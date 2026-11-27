//! Options read boundaries waiting for a finance contract.
#![allow(
    dead_code,
    reason = "Typed pending requests are decoded before upstream dispatch exists"
)]

use axum::{
    Router,
    extract::{Query, rejection::QueryRejection},
    http::StatusCode,
    response::Response,
    routing::get,
};
use chrono::{DateTime, FixedOffset};
use serde::Deserialize;
use utoipa::{IntoParams, OpenApi, ToSchema};

use crate::{
    AppState, ErrorBody, api_error,
    market_types::{InstrumentId, ProductToken},
};

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
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/options/chain", get(chain))
        .route("/api/v1/options/flow", get(flow))
        .route("/api/v1/options/exposure", get(exposure))
}

pub(crate) fn is_private(path: &str) -> bool {
    matches!(
        path,
        "/api/v1/options/chain" | "/api/v1/options/flow" | "/api/v1/options/exposure"
    )
}

#[derive(OpenApi)]
#[openapi(
    paths(chain, flow, exposure),
    components(schemas(InstrumentId, ProductToken, FlowMode))
)]
pub(crate) struct MarketPendingApi;
