use crate::{
    AppState, ErrorBody,
    finance::{Fault, Finance},
};
use axum::{Json, extract::State};
use finance_client::{apis::market_api, models::MarketStatus};
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
pub struct MarketSummary {
    pub state: String,
    pub source: Option<String>,
    pub updated_at: Option<String>,
    pub fresh: bool,
    pub instrument_count: usize,
    pub stale_count: usize,
}

impl MarketSummary {
    pub fn from_status(report: Option<MarketStatus>) -> Self {
        let Some(m) = report else {
            return Self {
                state: "unavailable".into(),
                fresh: false,
                source: None,
                updated_at: None,
                instrument_count: 0,
                stale_count: 0,
            };
        };
        let instrument_count = m.instruments.len();
        let stale_count = m.instruments.iter().filter(|i| i.stale).count();
        Self {
            fresh: m.state == "ready" && instrument_count > 0 && stale_count == 0,
            state: m.state,
            source: m.source.flatten(),
            updated_at: m.updated_at.flatten(),
            instrument_count,
            stale_count,
        }
    }
}

pub async fn read(finance: &Finance) -> Result<MarketStatus, Fault> {
    finance
        .read(|cfg| async move { market_api::status(&cfg).await })
        .await
}

#[utoipa::path(get, path="/api/v1/market/status", operation_id="marketStatus", responses((status=200,body=ref("#/components/schemas/MarketStatus")),
    (status=502,body=ErrorBody),(status=503,body=ErrorBody),(status=504,body=ErrorBody)))]
pub(crate) async fn status(State(s): State<AppState>) -> Result<Json<MarketStatus>, Fault> {
    read(&s.finance).await.map(Json)
}
