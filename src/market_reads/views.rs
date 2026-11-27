//! Browser read views over the F10a committed WAL and market status.

mod book;

use super::{MarketReadError, check_display, parsed, product_watermarks, wal_page};
use crate::{
    AppState, ErrorBody, market,
    market_types::{InstrumentId, ProductToken},
};
use axum::{
    Json, Router,
    extract::{Query, State, rejection::QueryRejection},
    routing::get,
};
use chrono::{DateTime, FixedOffset};
use finance_client::models::{self, MarketMessage, Message};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::{
    IntoParams, OpenApi, ToSchema,
    openapi::{
        Ref, RefOr,
        schema::{Array, Schema},
    },
};

const INSTRUMENTS: &str = "/api/v1/market/instruments";
const BARS: &str = "/api/v1/market/bars";
const TRADES: &str = "/api/v1/market/trades";
const QUOTES: &str = "/api/v1/market/quotes";
const BOOK: &str = "/api/v1/market/book";
const DERIVATIVES: &str = "/api/v1/market/derivatives";
const INDICATORS: &str = "/api/v1/indicators";
const INDICATOR_VALUES: &str = "/api/v1/indicators/values";
const WAL_PAGE_LIMIT: u16 = 128;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(INSTRUMENTS, get(instruments))
        .route(BARS, get(bars))
        .route(TRADES, get(trades))
        .route(DERIVATIVES, get(derivatives))
        .route(BOOK, get(book::book))
        .route(QUOTES, get(book::quotes))
        .route(INDICATORS, get(indicators))
        .route(INDICATOR_VALUES, get(indicator_values))
}

pub(super) fn is_private(path: &str) -> bool {
    matches!(
        path,
        INSTRUMENTS | BARS | TRADES | DERIVATIVES | BOOK | QUOTES | INDICATORS | INDICATOR_VALUES
    )
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
struct CatalogQuery {
    source: Option<ProductToken>,
    offset: Option<u32>,
    #[param(minimum = 1, maximum = 128)]
    limit: Option<u16>,
}

#[derive(Serialize, ToSchema)]
struct MarketInstruments {
    source: Option<String>,
    as_of: Option<String>,
    state: String,
    /// Observations from the finance collector status view.
    #[schema(schema_with = instrument_status_list)]
    items: Vec<models::InstrumentStatus>,
    next_offset: Option<usize>,
}

fn instrument_status_list() -> RefOr<Schema> {
    Array::new(Ref::from_schema_name("InstrumentStatus")).into()
}

#[utoipa::path(get, path = "/api/v1/market/instruments", operation_id = "marketInstruments", params(CatalogQuery),
    security(("adminSession" = []), ("adminBearer" = [])),
    responses((status = 200, body = MarketInstruments), (status = 400, body = ErrorBody),
        (status = 401, body = ErrorBody), (status = 403, body = ErrorBody),
        (status = 429, body = ErrorBody), (status = 502, body = ErrorBody),
        (status = 503, body = ErrorBody), (status = 504, body = ErrorBody)))]
async fn instruments(
    State(state): State<AppState>,
    query: Result<Query<CatalogQuery>, QueryRejection>,
) -> Result<Json<MarketInstruments>, MarketReadError> {
    let query = parsed(query)?;
    let limit = page_limit(query.limit)? as usize;
    let offset = query.offset.unwrap_or(0) as usize;
    let status = market::read(&state.finance).await?;
    let source = status.source.flatten();
    let mut items = status
        .instruments
        .into_iter()
        .filter(|_| {
            query
                .source
                .as_ref()
                .is_none_or(|filter| source.as_deref() == Some(filter.as_str()))
        })
        .skip(offset);
    let page = items.by_ref().take(limit).collect();
    let next_offset = items.next().map(|_| offset + limit);
    Ok(Json(MarketInstruments {
        source,
        as_of: status.updated_at.flatten(),
        state: status.state,
        items: page,
        next_offset,
    }))
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
struct WindowQuery {
    source: ProductToken,
    product: ProductToken,
    /// Required except for depth book reads with an active focus.
    instrument_id: Option<InstrumentId>,
    /// Required for quotes and book, to avoid mixing channels.
    channel: Option<ProductToken>,
    /// The exclusive WAL position preceding this page. Defaults to the recent committed window.
    #[param(pattern = r"^(?:0|[1-9][0-9]{0,19})$")]
    after: Option<String>,
    /// Inclusive WAL cutoff. Preserve it when following next_after.
    #[param(pattern = r"^(?:0|[1-9][0-9]{0,19})$")]
    through: Option<String>,
    #[param(minimum = 1, maximum = 128)]
    limit: Option<u16>,
    from: Option<DateTime<FixedOffset>>,
    until: Option<DateTime<FixedOffset>>,
    as_of: Option<DateTime<FixedOffset>>,
    /// Required for the indicator values query.
    indicator: Option<String>,
}

impl WindowQuery {
    fn validate(&self) -> Result<(), MarketReadError> {
        if self
            .from
            .zip(self.until)
            .is_some_and(|(from, until)| from >= until)
        {
            return Err(MarketReadError::InvalidQuery);
        }
        for value in [&self.after, &self.through].into_iter().flatten() {
            parse_u64(value).ok_or(MarketReadError::InvalidQuery)?;
        }
        if self
            .after
            .as_deref()
            .and_then(parse_u64)
            .zip(self.through.as_deref().and_then(parse_u64))
            .is_some_and(|(after, through)| after > through)
        {
            return Err(MarketReadError::InvalidQuery);
        }
        Ok(())
    }

    fn includes(&self, event: &models::Event, timestamp: Option<&str>) -> bool {
        self.instrument_id
            .as_ref()
            .is_some_and(|instrument| instrument.as_str() == event.instrument_id)
            && self
                .channel
                .as_ref()
                .is_none_or(|channel| channel.as_str() == event.channel)
            && self.as_of.is_none_or(|as_of| {
                timestamp_at(&event.first_available_ts).is_some_and(|ts| ts <= as_of)
            })
            && ((self.from.is_none() && self.until.is_none())
                || timestamp.and_then(timestamp_at).is_some_and(|time| {
                    self.from.is_none_or(|from| time >= from)
                        && self.until.is_none_or(|until| time < until)
                }))
    }
}

fn parse_u64(value: &str) -> Option<u64> {
    let number: u64 = value.parse().ok()?;
    (number.to_string() == value).then_some(number)
}

fn timestamp_at(value: &str) -> Option<DateTime<chrono::Utc>> {
    const NANOS: u64 = 1_000_000_000;
    let nanos = parse_u64(value)?;
    DateTime::from_timestamp((nanos / NANOS) as i64, (nanos % NANOS) as u32)
}

fn page_limit(limit: Option<u16>) -> Result<u16, MarketReadError> {
    let limit = limit.unwrap_or(WAL_PAGE_LIMIT);
    (1..=WAL_PAGE_LIMIT)
        .contains(&limit)
        .then_some(limit)
        .ok_or(MarketReadError::InvalidQuery)
}

#[derive(Serialize, ToSchema)]
struct WalCoverage {
    /// Exclusive beginning of the observed WAL page.
    after: String,
    /// Inclusive cutoff for continuing the same WAL read.
    through: String,
    durable_through: String,
    /// Null when the requested cutoff was reached.
    next_after: Option<String>,
    scanned_records: usize,
}

#[derive(Serialize, ToSchema)]
struct MarketViewMetadata {
    /// When finance observed the WAL page.
    as_of: String,
    #[schema(schema_with = product_policy)]
    access: models::ProductPolicy,
    instrument_id: InstrumentId,
    coverage: WalCoverage,
}

fn product_policy() -> RefOr<Schema> {
    Ref::from_schema_name("ProductPolicy").into()
}

fn wal_record_list() -> RefOr<Schema> {
    Array::new(Ref::from_schema_name("WalRecord")).into()
}

async fn read_window(
    state: &AppState,
    query: &WindowQuery,
) -> Result<(MarketViewMetadata, Vec<models::WalRecord>), MarketReadError> {
    query.validate()?;
    let id = query
        .instrument_id
        .as_ref()
        .ok_or(MarketReadError::InvalidQuery)?;
    let limit = page_limit(query.limit)?;
    let mut cutoff = query.through.clone();
    let after = if let Some(after) = &query.after {
        after.clone()
    } else {
        if cutoff.is_none() {
            let head = product_watermarks(
                &state.finance,
                query.source.as_str(),
                query.product.as_str(),
            )
            .await?;
            check_display(&head.access)?;
            cutoff = Some(head.watermarks.local_durable);
        }
        parse_u64(cutoff.as_deref().expect("cutoff requested or observed"))
            .ok_or(MarketReadError::InvalidResponse)?
            .saturating_sub(u64::from(limit))
            .to_string()
    };
    let page = wal_page(
        &state.finance,
        query.source.as_str(),
        query.product.as_str(),
        Some(&after),
        cutoff.as_deref(),
        Some(u64::from(limit)),
    )
    .await?;
    checked_window(id.clone(), after, cutoff.as_deref(), limit, page)
}

fn checked_window(
    instrument_id: InstrumentId,
    after: String,
    fixed_cutoff: Option<&str>,
    limit: u16,
    page: models::MarketWalPage,
) -> Result<(MarketViewMetadata, Vec<models::WalRecord>), MarketReadError> {
    let number = |text: &str| parse_u64(text).ok_or(MarketReadError::InvalidResponse);
    let start = number(&after)?;
    let cutoff = number(&page.through)?;
    let durable = number(&page.durable_through)?;
    let count = cutoff
        .checked_sub(start)
        .ok_or(MarketReadError::InvalidResponse)?
        .min(u64::from(limit));
    let end = start + count;
    let positions_match = page
        .records
        .iter()
        .zip(start..end)
        .all(|(record, previous)| parse_u64(&record.position) == Some(previous + 1));
    if cutoff > durable
        || fixed_cutoff.is_some_and(|expected| expected != page.through)
        || (fixed_cutoff.is_none() && cutoff != durable)
        || page.records.len() as u64 != count
        || !positions_match
        || page.next_after != (end < cutoff).then(|| end.to_string())
    {
        return Err(MarketReadError::InvalidResponse);
    }
    let metadata = MarketViewMetadata {
        as_of: page.as_of,
        access: *page.access,
        instrument_id,
        coverage: WalCoverage {
            after,
            through: page.through,
            durable_through: page.durable_through,
            next_after: page.next_after,
            scanned_records: page.records.len(),
        },
    };
    Ok((metadata, page.records))
}

#[derive(Clone, Copy)]
enum HistoryKind {
    Bars,
    Trades,
    Derivatives,
}

impl HistoryKind {
    fn market_timestamp(self, event: &models::Event) -> Option<Option<&str>> {
        match (self, event.message.as_ref()) {
            (Self::Bars, Message::MarketMessageOneOf(bar)) => Some(Some(&bar.payload.open_ts)),
            (Self::Trades, Message::MarketMessageOneOf1(_))
            | (Self::Derivatives, Message::MessageOneOf7(_)) => Some(event.exchange_ts.as_deref()),
            (Self::Bars | Self::Trades, Message::MessageOneOf2(correction)) => {
                match (self, correction.payload.replacement.as_ref()) {
                    (Self::Bars, MarketMessage::MarketMessageOneOf(bar)) => {
                        Some(Some(&bar.payload.open_ts))
                    }
                    (Self::Trades, MarketMessage::MarketMessageOneOf1(_)) => {
                        Some(event.exchange_ts.as_deref())
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

#[derive(Serialize, ToSchema)]
struct MarketHistory {
    #[serde(flatten)]
    metadata: MarketViewMetadata,
    /// Matching F1 market events and their correction envelopes.
    #[schema(schema_with = wal_record_list)]
    records: Vec<models::WalRecord>,
}

async fn history(
    state: AppState,
    query: Result<Query<WindowQuery>, QueryRejection>,
    kind: HistoryKind,
) -> Result<Json<MarketHistory>, MarketReadError> {
    let query = parsed(query)?;
    if query.indicator.is_some() {
        return Err(MarketReadError::InvalidQuery);
    }
    let (metadata, records) = read_window(&state, &query).await?;
    let records = records
        .into_iter()
        .filter(|record| {
            kind.market_timestamp(&record.event)
                .is_some_and(|ts| query.includes(&record.event, ts))
        })
        .collect();
    Ok(Json(MarketHistory { metadata, records }))
}

macro_rules! history_route {
    ($name:ident, $path:literal, $operation:literal, $kind:ident) => {
        #[utoipa::path(get, path = $path, operation_id = $operation, params(WindowQuery),
            security(("adminSession" = []), ("adminBearer" = [])),
            responses((status = 200, body = MarketHistory), (status = 400, body = ErrorBody),
                (status = 401, body = ErrorBody), (status = 403, body = ErrorBody), (status = 404, body = ErrorBody),
                (status = 429, body = ErrorBody), (status = 502, body = ErrorBody),
                (status = 503, body = ErrorBody), (status = 504, body = ErrorBody)))]
        async fn $name(
            State(state): State<AppState>,
            query: Result<Query<WindowQuery>, QueryRejection>,
        ) -> Result<Json<MarketHistory>, MarketReadError> {
            history(state, query, HistoryKind::$kind).await
        }
    };
}

history_route!(bars, "/api/v1/market/bars", "marketBars", Bars);
history_route!(trades, "/api/v1/market/trades", "marketTrades", Trades);
history_route!(
    derivatives,
    "/api/v1/market/derivatives",
    "marketDerivatives",
    Derivatives
);

#[derive(Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum BarIndicator {
    Close,
    Volume,
}

impl BarIndicator {
    fn parse(id: &str) -> Option<Self> {
        match id {
            "close" => Some(Self::Close),
            "volume" => Some(Self::Volume),
            _ => None,
        }
    }

    fn value(self, bar: &models::ClosedBar) -> String {
        match self {
            Self::Close => bar.close.clone(),
            Self::Volume => bar.volume.clone(),
        }
    }
}

#[derive(Serialize, ToSchema)]
struct MarketIndicator {
    id: BarIndicator,
    source_kind: &'static str,
}

#[derive(Serialize, ToSchema)]
struct MarketIndicators {
    items: [MarketIndicator; 2],
}

#[utoipa::path(get, path = "/api/v1/indicators", operation_id = "marketIndicators",
    security(("adminSession" = []), ("adminBearer" = [])),
    responses((status = 200, body = MarketIndicators), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody), (status = 429, body = ErrorBody)))]
async fn indicators() -> Json<MarketIndicators> {
    Json(MarketIndicators {
        items: [BarIndicator::Close, BarIndicator::Volume].map(|id| MarketIndicator {
            id,
            source_kind: "bar_closed",
        }),
    })
}

#[derive(Serialize, ToSchema)]
struct MarketIndicatorValue {
    #[schema(schema_with = wal_record)]
    record: models::WalRecord,
    #[schema(schema_with = decimal_text)]
    value: String,
}

fn wal_record() -> RefOr<Schema> {
    Ref::from_schema_name("WalRecord").into()
}

fn decimal_text() -> RefOr<Schema> {
    Ref::from_schema_name("DecimalText").into()
}

#[derive(Serialize, ToSchema)]
struct MarketIndicatorValues {
    #[serde(flatten)]
    metadata: MarketViewMetadata,
    indicator: BarIndicator,
    items: Vec<MarketIndicatorValue>,
}

#[utoipa::path(get, path = "/api/v1/indicators/values", operation_id = "marketIndicatorValues", params(WindowQuery),
    security(("adminSession" = []), ("adminBearer" = [])),
    responses((status = 200, body = MarketIndicatorValues), (status = 400, body = ErrorBody),
        (status = 401, body = ErrorBody), (status = 403, body = ErrorBody), (status = 404, body = ErrorBody),
        (status = 429, body = ErrorBody), (status = 502, body = ErrorBody),
        (status = 503, body = ErrorBody), (status = 504, body = ErrorBody)))]
async fn indicator_values(
    State(state): State<AppState>,
    query: Result<Query<WindowQuery>, QueryRejection>,
) -> Result<Json<MarketIndicatorValues>, MarketReadError> {
    let query = parsed(query)?;
    let indicator = BarIndicator::parse(
        query
            .indicator
            .as_deref()
            .ok_or(MarketReadError::InvalidQuery)?,
    )
    .ok_or(MarketReadError::IndicatorNotFound)?;
    let (metadata, records) = read_window(&state, &query).await?;
    let items = records
        .into_iter()
        .filter_map(|record| {
            let bar = match record.event.message.as_ref() {
                Message::MarketMessageOneOf(bar) => &bar.payload,
                Message::MessageOneOf2(correction) => {
                    match correction.payload.replacement.as_ref() {
                        MarketMessage::MarketMessageOneOf(bar) => &bar.payload,
                        _ => return None,
                    }
                }
                _ => return None,
            };
            let value = indicator.value(bar);
            query
                .includes(&record.event, Some(&bar.open_ts))
                .then_some(MarketIndicatorValue { record, value })
        })
        .collect();
    Ok(Json(MarketIndicatorValues {
        metadata,
        indicator,
        items,
    }))
}

#[derive(OpenApi)]
#[openapi(
    paths(
        instruments,
        bars,
        trades,
        derivatives,
        book::book,
        book::quotes,
        indicators,
        indicator_values
    ),
    components(schemas(
        MarketInstruments,
        MarketViewMetadata,
        WalCoverage,
        MarketHistory,
        MarketIndicators,
        MarketIndicator,
        MarketIndicatorValues,
        MarketIndicatorValue,
        BarIndicator,
        book::MarketBook,
        book::MarketQuote,
        book::Observation,
        book::BookState
    ))
)]
struct MarketViewsApi;

pub(super) fn register_openapi(spec: &mut Value) {
    let mut api = serde_json::to_value(MarketViewsApi::openapi()).expect("market views OpenAPI");
    for path in [BARS, TRADES, DERIVATIVES, BOOK, QUOTES, INDICATOR_VALUES] {
        let query = api["paths"][path]["get"]["parameters"]
            .as_array_mut()
            .expect("view query parameters");
        query.retain_mut(|param| match param["name"].as_str() {
            Some("indicator") if path != INDICATOR_VALUES => false,
            Some("from" | "until") if matches!(path, BOOK | QUOTES) => false,
            Some("indicator") => {
                param["required"] = true.into();
                true
            }
            Some("instrument_id") if path != BOOK => {
                param["required"] = true.into();
                true
            }
            Some("channel") if matches!(path, BOOK | QUOTES) => {
                param["required"] = true.into();
                true
            }
            _ => true,
        });
    }
    for key in ["/paths", "/components/schemas"] {
        let source = api
            .pointer_mut(key)
            .and_then(Value::as_object_mut)
            .expect("market views OpenAPI section");
        spec.pointer_mut(key)
            .and_then(Value::as_object_mut)
            .expect("web OpenAPI destination")
            .append(source);
    }
}

#[cfg(test)]
mod tests;
