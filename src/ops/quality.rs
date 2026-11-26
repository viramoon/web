use super::{OpsConfig, read_snapshot};
use crate::{AppState, ErrorBody, api_error};
use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
    http::StatusCode,
    response::Response,
};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use std::{cmp::Reverse, collections::HashSet};
use utoipa::{IntoParams, ToSchema};

const QUALITY_MAX_BYTES: u64 = 1024 * 1024;
const MAX_STREAMS: usize = 64;

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct GapsQuery {
    source: Option<String>,
    product: Option<String>,
    #[param(format = DateTime)]
    from: Option<DateTime<Utc>>,
    #[param(format = DateTime)]
    to: Option<DateTime<Utc>>,
    #[param(minimum = 0, default = 0)]
    offset: Option<usize>,
    #[param(minimum = 1, maximum = 100, default = 20)]
    limit: Option<usize>,
}

impl GapsQuery {
    fn valid(&self) -> bool {
        self.source.as_deref().is_none_or(token)
            && self.product.as_deref().is_none_or(token)
            && self.from.zip(self.to).is_none_or(|(from, to)| from < to)
            && (1..=100).contains(&self.limit.unwrap_or(20))
    }
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum QualityState {
    Unavailable,
    Available,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct QualityGapsPage {
    state: QualityState,
    as_of: Option<DateTime<Utc>>,
    coverage: Vec<QualityCoverage>,
    items: Vec<QualityGap>,
    next_offset: Option<usize>,
}

#[derive(Serialize, ToSchema)]
struct QualityCoverage {
    source: String,
    product: String,
    license: String,
    watermarks: Option<MarketWatermarks>,
}

#[derive(Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct MarketWatermarks {
    source_latest: Option<SourceObservation>,
    local_durable: ExactCount,
    projected: ExactCount,
    remote_verified: Option<ExactCount>,
}

#[derive(Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct SourceObservation {
    channel: String,
    instrument_id: String,
    epoch: ExactCount,
    seq: ExactCount,
    /// UTC nanoseconds since the Unix epoch, as observed by the market source.
    exchange_ts: Option<ExactCount>,
    /// UTC nanoseconds since the Unix epoch, as observed by the collector.
    receive_ts: ExactCount,
}

#[derive(Serialize, ToSchema)]
struct QualityGap {
    source: String,
    product: String,
    channel: String,
    instrument_id: String,
    /// Inclusive UTC start of the gap.
    from: DateTime<Utc>,
    /// Exclusive UTC end of the gap.
    to: DateTime<Utc>,
    cause: GapCause,
    outcome: Option<GapRecovery>,
    count: Option<ExactCount>,
}

#[derive(Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum GapCause {
    LicenseSkip,
    Rotation,
    SourceRateLimit,
    Disconnected,
    SourceGap,
    StorageUnavailable,
}

#[derive(Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum GapRecovery {
    TickBackfilled,
    BarBackfilled,
    BookRestoredFromSnapshot,
    HistoryUnrecoverable,
}

/// An F3 unsigned 64-bit count, position or timestamp transported as decimal text.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize, ToSchema)]
#[serde(try_from = "String", into = "String")]
#[schema(value_type = String, pattern = r"^(?:0|[1-9][0-9]{0,19})(?![\s\S])")]
struct ExactCount(u64);

impl TryFrom<String> for ExactCount {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value
            .parse::<u64>()
            .ok()
            .filter(|count| count.to_string() == value)
            .map(Self)
            .ok_or("invalid F3 decimal integer")
    }
}

impl From<ExactCount> for String {
    fn from(value: ExactCount) -> Self {
        value.0.to_string()
    }
}

fn timestamp(nanos: ExactCount) -> DateTime<Utc> {
    const NANOS_PER_SECOND: u64 = 1_000_000_000;
    DateTime::from_timestamp(
        (nanos.0 / NANOS_PER_SECOND) as i64,
        (nanos.0 % NANOS_PER_SECOND) as u32,
    )
    .expect("u64 Unix nanoseconds fit in UTC timestamp")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportFile {
    channel: String,
    instrument_id: String,
    from_ts: ExactCount,
    to_ts: ExactCount,
    cause: GapCause,
    outcome: Option<GapRecovery>,
    count: Option<ExactCount>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StreamFile {
    source: String,
    product: String,
    license: String,
    watermarks: Option<MarketWatermarks>,
    reports: Vec<ReportFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QualityFile {
    version: u8,
    observed_at: DateTime<Utc>,
    streams: Vec<StreamFile>,
}

fn token(value: &str) -> bool {
    let lower_alphanumeric = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let mut bytes = value.bytes();
    value.len() <= 64
        && bytes.next().is_some_and(lower_alphanumeric)
        && bytes.all(|b| lower_alphanumeric(b) || matches!(b, b'_' | b'-' | b'.'))
}

fn instrument_id(value: &str) -> bool {
    let upper_alphanumeric = |b: u8| b.is_ascii_uppercase() || b.is_ascii_digit();
    let segment = |value: &str| {
        let mut bytes = value.bytes();
        bytes.next().is_some_and(upper_alphanumeric)
            && bytes.all(|b| upper_alphanumeric(b) || matches!(b, b'_' | b'-'))
    };
    value.rsplit_once('.').is_some_and(|(local, venue)| {
        local.len() <= 96 && local.split('.').all(segment) && venue.len() <= 32 && segment(venue)
    })
}

impl QualityFile {
    fn valid(&self) -> bool {
        let mut keys = HashSet::new();
        self.version == 1
            && self.streams.len() <= MAX_STREAMS
            && self.streams.iter().all(|stream| {
                token(&stream.source)
                    && token(&stream.product)
                    && token(&stream.license)
                    && keys.insert((&stream.source, &stream.product))
                    && stream.watermarks.as_ref().is_none_or(|marks| {
                        marks.projected <= marks.local_durable
                            && marks.source_latest.as_ref().is_none_or(|event| {
                                token(&event.channel) && instrument_id(&event.instrument_id)
                            })
                    })
                    && stream.reports.iter().all(|report| {
                        token(&report.channel)
                            && instrument_id(&report.instrument_id)
                            && report.from_ts <= report.to_ts
                    })
            })
    }
}

impl QualityGapsPage {
    fn unavailable() -> Self {
        Self {
            state: QualityState::Unavailable,
            as_of: None,
            coverage: Vec::new(),
            items: Vec::new(),
            next_offset: None,
        }
    }

    pub(super) fn read(config: &OpsConfig, query: GapsQuery) -> Self {
        let Some((file, _)) = config.quality_status_file.as_deref().and_then(|path| {
            read_snapshot::<QualityFile>(path, QUALITY_MAX_BYTES).filter(|(file, _)| file.valid())
        }) else {
            return Self::unavailable();
        };

        let mut coverage = Vec::new();
        let mut items = Vec::new();
        for stream in file.streams.into_iter().filter(|stream| {
            query
                .source
                .as_ref()
                .is_none_or(|source| source == &stream.source)
                && query
                    .product
                    .as_ref()
                    .is_none_or(|product| product == &stream.product)
        }) {
            for report in stream.reports {
                let from = timestamp(report.from_ts);
                // F3 uses a closed nanosecond interval. UTC DateTime can represent the tick
                // after u64::MAX, which keeps the half-open browser interval exact.
                let to = timestamp(report.to_ts) + TimeDelta::nanoseconds(1);
                if query.from.is_none_or(|start| to > start)
                    && query.to.is_none_or(|end| from < end)
                {
                    items.push(QualityGap {
                        source: stream.source.clone(),
                        product: stream.product.clone(),
                        channel: report.channel,
                        instrument_id: report.instrument_id,
                        from,
                        to,
                        cause: report.cause,
                        outcome: report.outcome,
                        count: report.count,
                    });
                }
            }
            coverage.push(QualityCoverage {
                source: stream.source,
                product: stream.product,
                license: stream.license,
                watermarks: stream.watermarks,
            });
        }
        items.sort_by_key(|gap| Reverse(gap.from));
        let offset = query.offset.unwrap_or(0).min(items.len());
        let end = (offset + query.limit.unwrap_or(20)).min(items.len());
        let next_offset = (end < items.len()).then_some(end);
        let items = items.drain(offset..end).collect();
        Self {
            state: QualityState::Available,
            as_of: Some(file.observed_at),
            coverage,
            items,
            next_offset,
        }
    }
}

#[utoipa::path(get, path = "/api/v1/quality/gaps", params(GapsQuery),
    security(("adminBearer" = []), ("adminSession" = [])),
    responses((status = 200, body = QualityGapsPage), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody), (status = 422, body = ErrorBody)))]
pub(crate) async fn quality_gaps(
    State(state): State<AppState>,
    query: Result<Query<GapsQuery>, QueryRejection>,
) -> Result<Json<QualityGapsPage>, Response> {
    let Query(query) = query
        .ok()
        .filter(|Query(query)| query.valid())
        .ok_or_else(|| {
            api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_request",
                "Invalid quality gaps query",
            )
        })?;
    let page = tokio::task::spawn_blocking(move || QualityGapsPage::read(&state.config.ops, query))
        .await
        .unwrap_or_else(|_| QualityGapsPage::unavailable());
    Ok(Json(page))
}
