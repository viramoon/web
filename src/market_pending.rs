//! Browser boundaries for domains waiting for a finance read or scheduling contract.
#![allow(
    dead_code,
    reason = "Typed pending requests are decoded before upstream dispatch exists"
)]

use axum::{
    Json, Router,
    extract::{
        Query,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
    response::Response,
    routing::{get, put},
};
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Deserializer, de};
use utoipa::{IntoParams, OpenApi, ToSchema};

use crate::{
    AppState, ErrorBody, api_error,
    market_types::{InstrumentId, valid_token},
};

pub(crate) const DEPTH_FOCUS: &str = "/api/v1/market/depth-focus";

#[derive(ToSchema)]
#[schema(value_type = String, pattern = r"^[a-z0-9][a-z0-9_.-]{0,63}(?![\s\S])")]
struct ProductToken(String);

impl<'de> Deserialize<'de> for ProductToken {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let token = String::deserialize(deserializer)?;
        if valid_token(&token) {
            Ok(Self(token))
        } else {
            Err(de::Error::custom("invalid product identifier"))
        }
    }
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
struct CatalogQuery {
    source: Option<ProductToken>,
    offset: Option<u32>,
    limit: Option<u16>,
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
struct InstrumentQuery {
    instrument_id: InstrumentId,
    source: Option<ProductToken>,
    product: Option<ProductToken>,
    as_of: Option<DateTime<FixedOffset>>,
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
struct HistoryQuery {
    instrument_id: InstrumentId,
    source: Option<ProductToken>,
    product: Option<ProductToken>,
    from: Option<DateTime<FixedOffset>>,
    until: Option<DateTime<FixedOffset>>,
    limit: Option<u16>,
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
struct IndicatorQuery {
    instrument_id: InstrumentId,
    indicator: ProductToken,
    from: Option<DateTime<FixedOffset>>,
    until: Option<DateTime<FixedOffset>>,
    limit: Option<u16>,
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
struct ChainQuery {
    instrument_id: InstrumentId,
    as_of: Option<DateTime<FixedOffset>>,
    offset: Option<u32>,
    limit: Option<u16>,
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum FlowMode {
    Observed,
    Verified,
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
struct FlowQuery {
    instrument_id: InstrumentId,
    mode: Option<FlowMode>,
    from: Option<DateTime<FixedOffset>>,
    until: Option<DateTime<FixedOffset>>,
    offset: Option<u32>,
    limit: Option<u16>,
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
struct ExposureQuery {
    instrument_id: InstrumentId,
    scenario: Option<ProductToken>,
    as_of: Option<DateTime<FixedOffset>>,
}

#[derive(Deserialize, ToSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum DepthFocus {
    Set { instrument_id: InstrumentId },
    Clear,
}

/// Fixed error responses keep unsupported products distinguishable from a
/// successful empty page, a stale reading, and an unknown source value.
fn pending() -> Response {
    api_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "finance_contract_pending",
        "The finance contract for this capability is pending",
    )
}

fn invalid() -> Response {
    api_error(
        StatusCode::BAD_REQUEST,
        "invalid_market_request",
        "Invalid market request",
    )
}

// Fields are consumed at the typed HTTP boundary until the matching finance
// operation is available. Temporal queries still enforce the interval rule.
fn window_valid(from: Option<DateTime<FixedOffset>>, until: Option<DateTime<FixedOffset>>) -> bool {
    from.zip(until).is_none_or(|(from, until)| from < until)
}

macro_rules! pending_read {
    ($name:ident, $path:literal, $operation:literal, $query:ident, $valid:expr) => {
        #[utoipa::path(get, path = $path, operation_id = $operation, params($query),
            security(("adminSession" = []), ("adminBearer" = [])),
            responses((status = 400, body = ErrorBody), (status = 401, body = ErrorBody),
                (status = 403, body = ErrorBody), (status = 429, body = ErrorBody),
                (status = 503, body = ErrorBody, description = "Finance contract pending")))]
        async fn $name(query: Result<Query<$query>, QueryRejection>) -> Response {
            match query {
                Ok(Query(query)) if ($valid)(&query) => pending(),
                _ => invalid(),
            }
        }
    };
}

pending_read!(
    instruments,
    "/api/v1/market/instruments",
    "marketInstruments",
    CatalogQuery,
    |q: &CatalogQuery| q.limit != Some(0)
);
pending_read!(
    bars,
    "/api/v1/market/bars",
    "marketBars",
    HistoryQuery,
    |q: &HistoryQuery| window_valid(q.from, q.until) && q.limit != Some(0)
);
pending_read!(
    quotes,
    "/api/v1/market/quotes",
    "marketQuotes",
    InstrumentQuery,
    |_: &InstrumentQuery| true
);
pending_read!(
    book,
    "/api/v1/market/book",
    "marketBook",
    InstrumentQuery,
    |_: &InstrumentQuery| true
);
pending_read!(
    trades,
    "/api/v1/market/trades",
    "marketTrades",
    HistoryQuery,
    |q: &HistoryQuery| window_valid(q.from, q.until) && q.limit != Some(0)
);
pending_read!(
    derivatives,
    "/api/v1/market/derivatives",
    "marketDerivatives",
    InstrumentQuery,
    |_: &InstrumentQuery| true
);
pending_read!(
    indicators,
    "/api/v1/indicators",
    "marketIndicators",
    CatalogQuery,
    |q: &CatalogQuery| q.limit != Some(0)
);
pending_read!(
    indicator_values,
    "/api/v1/indicators/values",
    "marketIndicatorValues",
    IndicatorQuery,
    |q: &IndicatorQuery| window_valid(q.from, q.until) && q.limit != Some(0)
);
pending_read!(
    chain,
    "/api/v1/options/chain",
    "optionsChain",
    ChainQuery,
    |q: &ChainQuery| q.limit != Some(0)
);
pending_read!(
    flow,
    "/api/v1/options/flow",
    "optionsFlow",
    FlowQuery,
    |q: &FlowQuery| window_valid(q.from, q.until) && q.limit != Some(0)
);
pending_read!(
    exposure,
    "/api/v1/options/exposure",
    "optionsExposure",
    ExposureQuery,
    |_: &ExposureQuery| true
);
pending_read!(
    subscriptions,
    "/api/v1/market/subscriptions",
    "marketSubscriptions",
    CatalogQuery,
    |q: &CatalogQuery| q.limit != Some(0)
);

#[utoipa::path(put, path = "/api/v1/market/depth-focus", operation_id = "setMarketDepthFocus",
    params(("X-CSRF-Token" = Option<String>, Header, description = "Required for browser sessions")),
    request_body = DepthFocus, security(("adminSession" = []), ("adminBearer" = [])),
    responses((status = 400, body = ErrorBody), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody), (status = 429, body = ErrorBody),
        (status = 503, body = ErrorBody, description = "Finance scheduling contract pending")))]
async fn focus(request: Result<Json<DepthFocus>, JsonRejection>) -> Response {
    match request {
        Ok(_) => pending(),
        Err(error) if error.status() == StatusCode::PAYLOAD_TOO_LARGE => api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            "Request body is too large",
        ),
        _ => invalid(),
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/market/instruments", get(instruments))
        .route("/api/v1/market/bars", get(bars))
        .route("/api/v1/market/quotes", get(quotes))
        .route("/api/v1/market/book", get(book))
        .route("/api/v1/market/trades", get(trades))
        .route("/api/v1/market/derivatives", get(derivatives))
        .route("/api/v1/indicators", get(indicators))
        .route("/api/v1/indicators/values", get(indicator_values))
        .route("/api/v1/options/chain", get(chain))
        .route("/api/v1/options/flow", get(flow))
        .route("/api/v1/options/exposure", get(exposure))
        .route("/api/v1/market/subscriptions", get(subscriptions))
        .route(DEPTH_FOCUS, put(focus))
}

pub(crate) fn is_private(path: &str) -> bool {
    path.starts_with("/api/v1/options/")
        || path.starts_with("/api/v1/indicators")
        || matches!(
            path,
            "/api/v1/market/instruments"
                | "/api/v1/market/bars"
                | "/api/v1/market/quotes"
                | "/api/v1/market/book"
                | "/api/v1/market/trades"
                | "/api/v1/market/derivatives"
                | "/api/v1/market/subscriptions"
                | DEPTH_FOCUS
        )
}

#[derive(OpenApi)]
#[openapi(
    paths(
        instruments,
        bars,
        quotes,
        book,
        trades,
        derivatives,
        indicators,
        indicator_values,
        chain,
        flow,
        exposure,
        subscriptions,
        focus
    ),
    components(schemas(InstrumentId, ProductToken, FlowMode, DepthFocus))
)]
pub(crate) struct MarketPendingApi;
