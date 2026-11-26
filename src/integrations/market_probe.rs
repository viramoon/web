//! The fixed, local handoff to the finance market-data owner.
use super::{
    Connector, Symbol,
    store::{Outcome, TestInput},
};
use crate::finance::Finance;
use chrono::{DateTime, TimeDelta, Utc};
use reqwest::{Client, RequestBuilder, StatusCode};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use utoipa::ToSchema;
use uuid::Uuid;

const MAX_RESPONSE_BYTES: usize = 8192;
const PROBE_DEADLINE: Duration = Duration::from_secs(12);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const POLL_ATTEMPTS: usize = 2;
const CLOCK_SKEW: TimeDelta = TimeDelta::seconds(5);
const MAX_LIVE_AGE: TimeDelta = TimeDelta::seconds(120);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MarketResult {
    Verified,
    MarketClosed,
    Delayed,
    Frozen,
    NoPermission,
    LinesFull,
    SourceUnavailable,
    Disabled,
    AdapterUnavailable,
    ObservationPending,
    TimedOut,
    InvalidConfiguration,
    InvalidResponse,
    UnsafeBrokerSession,
    EvidenceIncomplete,
    StaleObservation,
    ReplayOnly,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub(crate) struct MarketEvidence {
    pub result: MarketResult,
    pub observation: Option<MarketObservation>,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct MarketObservation {
    pub origin: ObservationOrigin,
    pub observed_at: DateTime<Utc>,
    pub details: ObservationDetails,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservationOrigin {
    Live,
    Replay,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum ObservationDetails {
    IbkrPaper(PaperMarketEvidence),
    CryptoMarket(CryptoMarketEvidence),
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PaperMarketEvidence {
    pub paper_endpoint: bool,
    pub paper_account: bool,
    pub read_only: bool,
    pub unique_market_data_client: bool,
    #[schema(minimum = 1, maximum = 4)]
    pub stock_l1_market_data_type: Option<u8>,
    #[schema(minimum = 1, maximum = 4)]
    pub option_l1_market_data_type: Option<u8>,
    pub option_contract_id: Option<u32>,
    pub option_open_interest: bool,
    pub option_greeks: bool,
    pub nasdaq_depth_callback: bool,
    pub remaining_l1_lines: Option<u16>,
    pub remaining_depth_lines: Option<u16>,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CryptoMarketEvidence {
    pub metadata: bool,
    pub snapshot: bool,
    #[schema(maximum = 38)]
    pub price_precision: Option<u8>,
    pub event_at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
struct ProbeRequest {
    test_id: String,
    config_version: i64,
    connector: Connector,
    symbol: Symbol,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeResponse {
    integration_id: String,
    test_id: String,
    config_version: i64,
    connector: Connector,
    symbol: Symbol,
    status: ProbeStatus,
    observation: Option<MarketObservation>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProbeStatus {
    Verified,
    MarketClosed,
    Delayed,
    NoPermission,
    LinesFull,
    SourceUnavailable,
}

impl From<ProbeStatus> for MarketResult {
    fn from(status: ProbeStatus) -> Self {
        match status {
            ProbeStatus::Verified => Self::Verified,
            ProbeStatus::MarketClosed => Self::MarketClosed,
            ProbeStatus::Delayed => Self::Delayed,
            ProbeStatus::NoPermission => Self::NoPermission,
            ProbeStatus::LinesFull => Self::LinesFull,
            ProbeStatus::SourceUnavailable => Self::SourceUnavailable,
        }
    }
}

impl MarketEvidence {
    fn outcome(&self) -> Outcome {
        match self.result {
            MarketResult::Verified => Outcome::Passed,
            MarketResult::MarketClosed
            | MarketResult::LinesFull
            | MarketResult::SourceUnavailable
            | MarketResult::Disabled
            | MarketResult::AdapterUnavailable
            | MarketResult::ObservationPending
            | MarketResult::TimedOut
            | MarketResult::ReplayOnly => Outcome::Unavailable,
            MarketResult::Delayed
            | MarketResult::Frozen
            | MarketResult::NoPermission
            | MarketResult::InvalidConfiguration
            | MarketResult::InvalidResponse
            | MarketResult::UnsafeBrokerSession
            | MarketResult::EvidenceIncomplete
            | MarketResult::StaleObservation => Outcome::Failed,
        }
    }
}

fn result_only(result: MarketResult) -> MarketEvidence {
    MarketEvidence {
        result,
        observation: None,
    }
}

pub(super) async fn test(
    finance: &Finance,
    input: &TestInput,
    binance_enabled: bool,
) -> (Outcome, MarketEvidence) {
    let evidence = tokio::time::timeout(PROBE_DEADLINE, collect(finance, input, binance_enabled))
        .await
        .unwrap_or_else(|_| result_only(MarketResult::TimedOut));
    (evidence.outcome(), evidence)
}

async fn collect(finance: &Finance, input: &TestInput, binance_enabled: bool) -> MarketEvidence {
    let connector = match input.connector.as_str() {
        "ibkr_paper" => Connector::IbkrPaper,
        "hyperliquid_perp" => Connector::HyperliquidPerp,
        "binance_spot" => Connector::BinanceSpot,
        "binance_usd_m" => Connector::BinanceUsdM,
        _ => return result_only(MarketResult::InvalidConfiguration),
    };
    if matches!(connector, Connector::BinanceSpot | Connector::BinanceUsdM) && !binance_enabled {
        return result_only(MarketResult::Disabled);
    }

    let symbol = input
        .settings
        .get("symbol")
        .cloned()
        .and_then(|value| serde_json::from_value::<Symbol>(value).ok())
        .filter(|symbol| connector.symbols().contains(symbol));
    let (Ok(id), Ok(test_id), Some(symbol)) = (
        Uuid::parse_str(&input.test.integration_id),
        Uuid::parse_str(&input.test.id),
        symbol,
    ) else {
        return result_only(MarketResult::InvalidConfiguration);
    };
    let Some((client, base)) = finance.integration_probe_client() else {
        return result_only(MarketResult::AdapterUnavailable);
    };
    let path = format!("{base}/integrations/{id}/tests");
    let request = ProbeRequest {
        test_id: test_id.to_string(),
        config_version: input.test.config_version,
        connector,
        symbol,
    };
    let response = probe(client, &path, &request).await;
    match response {
        Ok(response) => verify(response, &id.to_string(), &request, input.test.started_at),
        Err(result) => result_only(result),
    }
}

async fn probe(
    client: &Client,
    path: &str,
    request: &ProbeRequest,
) -> Result<ProbeResponse, MarketResult> {
    match receive(client.post(path).json(request)).await {
        Ok(Some(response)) => return Ok(response),
        Ok(None) | Err(MarketResult::TimedOut) => {}
        Err(result) => return Err(result),
    }
    // A timed-out request can have started a job. Look up the same test ID
    // rather than risking another subscription by repeating the POST.
    let lookup = format!("{path}/{}", request.test_id);
    for _ in 0..POLL_ATTEMPTS {
        tokio::time::sleep(POLL_INTERVAL).await;
        if let Some(response) = receive(client.get(&lookup)).await? {
            return Ok(response);
        }
    }
    Err(MarketResult::ObservationPending)
}

async fn receive(request: RequestBuilder) -> Result<Option<ProbeResponse>, MarketResult> {
    let mut response = request.send().await.map_err(transport_error)?;
    match response.status() {
        StatusCode::OK => {}
        StatusCode::ACCEPTED => return Ok(None),
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED => {
            return Err(MarketResult::AdapterUnavailable);
        }
        StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => {
            return Err(MarketResult::TimedOut);
        }
        status if status.is_server_error() => return Err(MarketResult::SourceUnavailable),
        _ => return Err(MarketResult::InvalidResponse),
    }
    if response
        .content_length()
        .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
    {
        return Err(MarketResult::InvalidResponse);
    }
    let mut data = Vec::new();
    while let Some(part) = response.chunk().await.map_err(transport_error)? {
        if part.len() > MAX_RESPONSE_BYTES - data.len() {
            return Err(MarketResult::InvalidResponse);
        }
        data.extend_from_slice(&part);
    }
    serde_json::from_slice(&data)
        .map(Some)
        .map_err(|_| MarketResult::InvalidResponse)
}

fn transport_error(error: reqwest::Error) -> MarketResult {
    if error.is_timeout() {
        MarketResult::TimedOut
    } else {
        MarketResult::AdapterUnavailable
    }
}

fn verify(
    response: ProbeResponse,
    integration_id: &str,
    request: &ProbeRequest,
    started_at: DateTime<Utc>,
) -> MarketEvidence {
    if response.integration_id != integration_id
        || response.test_id != request.test_id
        || response.config_version != request.config_version
        || response.connector.id() != request.connector.id()
        || response.symbol != request.symbol
    {
        return result_only(MarketResult::InvalidResponse);
    }
    let reported = MarketResult::from(response.status);
    let Some(observation) = response.observation else {
        return result_only(if reported == MarketResult::Verified {
            MarketResult::EvidenceIncomplete
        } else {
            reported
        });
    };
    let result = match (&observation.details, request.connector) {
        (ObservationDetails::IbkrPaper(paper), Connector::IbkrPaper) => {
            check_paper(paper, reported)
        }
        (
            ObservationDetails::CryptoMarket(crypto),
            Connector::HyperliquidPerp | Connector::BinanceSpot | Connector::BinanceUsdM,
        ) => check_crypto(crypto, reported),
        _ => return result_only(MarketResult::InvalidResponse),
    };
    if result == MarketResult::InvalidResponse {
        return result_only(result);
    }
    let now = Utc::now();
    let result = if observation.origin == ObservationOrigin::Replay {
        MarketResult::ReplayOnly
    } else if !recent(observation.observed_at, started_at, now)
        || matches!(&observation.details, ObservationDetails::CryptoMarket(crypto)
            if crypto.event_at.is_some_and(|event| !recent(event, started_at, observation.observed_at)))
    {
        MarketResult::StaleObservation
    } else {
        result
    };
    MarketEvidence {
        result,
        observation: Some(observation),
    }
}

fn recent(at: DateTime<Utc>, started_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    at.signed_duration_since(started_at) >= -CLOCK_SKEW
        && (-CLOCK_SKEW..=MAX_LIVE_AGE).contains(&now.signed_duration_since(at))
}

fn check_paper(paper: &PaperMarketEvidence, reported: MarketResult) -> MarketResult {
    if !(paper.paper_endpoint
        && paper.paper_account
        && paper.read_only
        && paper.unique_market_data_client)
    {
        return MarketResult::UnsafeBrokerSession;
    }
    let types = [
        paper.stock_l1_market_data_type,
        paper.option_l1_market_data_type,
    ];
    if types.iter().flatten().any(|code| !(1..=4).contains(code)) {
        return MarketResult::InvalidResponse;
    }
    if reported != MarketResult::Verified {
        return reported;
    }
    if types.iter().flatten().any(|code| matches!(code, 3 | 4)) {
        return MarketResult::Delayed;
    }
    if types.contains(&Some(2)) {
        return MarketResult::Frozen;
    }
    if types == [Some(1), Some(1)]
        && paper.option_contract_id.is_some_and(|id| id > 0)
        && paper.option_open_interest
        && paper.option_greeks
        && paper.nasdaq_depth_callback
        && paper.remaining_l1_lines.is_some()
        && paper.remaining_depth_lines.is_some()
    {
        MarketResult::Verified
    } else {
        MarketResult::EvidenceIncomplete
    }
}

fn check_crypto(crypto: &CryptoMarketEvidence, reported: MarketResult) -> MarketResult {
    if crypto
        .price_precision
        .is_some_and(|precision| precision > 38)
    {
        return MarketResult::InvalidResponse;
    }
    if reported == MarketResult::Verified
        && !(crypto.metadata
            && crypto.snapshot
            && crypto.price_precision.is_some()
            && crypto.event_at.is_some())
    {
        return MarketResult::EvidenceIncomplete;
    }
    reported
}

#[cfg(test)]
mod tests;
