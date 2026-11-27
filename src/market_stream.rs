//! Market stream authorization and bounded replay over the committed finance WAL.

use crate::{AppState, ErrorBody, api_error};
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use utoipa::OpenApi;

mod replay;
mod session;
pub use replay::{MarketPartition, MarketReplay, ReplayError, ResetReason};
pub use session::{
    MAX_COMMAND_BYTES, MarketStreamCommand, MarketStreamControl, MarketStreamError,
    MarketStreamFrame, MarketStreamSession,
};

pub(crate) const MARKET_STREAM: &str = "/api/v1/stream/market";

pub(crate) fn routes() -> Router<AppState> {
    Router::new().route(MARKET_STREAM, get(market_stream))
}

#[utoipa::path(get, path = "/api/v1/stream/market", operation_id = "marketStream",
    description = "Reserved WebSocket endpoint. The transport is currently unavailable.",
    security(("adminSession" = []), ("adminBearer" = [])),
    responses((status = 401, body = ErrorBody), (status = 403, body = ErrorBody),
        (status = 503, body = ErrorBody)))]
async fn market_stream(State(s): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(denied) = s.write_access.authorize_stream(&headers, true) {
        return denied.into_response();
    }
    api_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "market_stream_unavailable",
        "Market WebSocket transport is unavailable",
    )
}

#[derive(OpenApi)]
#[openapi(
    paths(market_stream),
    components(schemas(
        MarketStreamCommand,
        MarketStreamControl,
        MarketStreamError,
        ResetReason
    ))
)]
pub(crate) struct MarketStreamApi;

#[cfg(test)]
mod fixture;
#[cfg(test)]
mod tests;
