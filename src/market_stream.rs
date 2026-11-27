//! Market stream authorization and bounded replay over the committed finance WAL.

use crate::AppState;
use axum::{Extension, Router, routing::get};
use utoipa::OpenApi;

mod replay;
mod session;
mod transport;
pub use replay::{MarketPartition, MarketReplay, ReplayError, ResetReason};
pub use session::{
    MAX_COMMAND_BYTES, MarketStreamCommand, MarketStreamControl, MarketStreamError,
    MarketStreamFrame, MarketStreamSession,
};

pub(crate) const MARKET_STREAM: &str = "/api/v1/stream/market";

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route(MARKET_STREAM, get(transport::upgrade))
        .layer(Extension(transport::Connections::new()))
}

#[derive(OpenApi)]
#[openapi(
    paths(transport::upgrade),
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
