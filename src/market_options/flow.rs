use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
};
use chrono::{DateTime, Utc};
use finance_client::{apis::market_options_flow_api, models::ProductPolicy};
use serde::Deserialize;
use serde_json::{Value, json, value::RawValue};
use utoipa::{IntoParams, ToSchema};

use super::{
    DisplayAccess, OptionReadError, limited, parsed, positive_id, require_display, timestamp,
};
use crate::{AppState, ErrorBody, market_reads::MarketReadError, market_types::InstrumentId};

pub(super) const FLOW: &str = "/api/v1/options/flow";
pub(super) const OBSERVATIONS: &str = "/api/v1/options/flow/observations";
pub(super) const VERIFICATION: &str = "/api/v1/options/flow/verification";

#[derive(Default, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum FlowMode {
    #[default]
    Observed,
    Verified,
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(super) struct LegacyFlowQuery {
    /// Legacy identifier. Use con_id to select the confirmed broker contract.
    instrument_id: Option<InstrumentId>,
    mode: Option<FlowMode>,
    from: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    offset: Option<u32>,
    limit: Option<u16>,
    /// Required with from and until for a successful read.
    #[param(minimum = 1)]
    con_id: Option<i32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FlowQuery {
    con_id: i32,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    offset: Option<u64>,
    limit: Option<u16>,
}

#[derive(Deserialize)]
struct ObservationAccess {
    access: ProductPolicy,
    items: Vec<ObservationItemAccess>,
}

#[derive(Deserialize)]
struct ObservationItemAccess {
    coverage: Option<DisplayAccess>,
}

#[derive(Deserialize)]
struct VerificationAccess {
    access: ProductPolicy,
    observation_coverage: Option<DisplayAccess>,
    durable_through: Option<Box<RawValue>>,
}

impl FlowQuery {
    async fn read(
        self,
        state: AppState,
        mode: FlowMode,
    ) -> Result<Json<Box<RawValue>>, OptionReadError> {
        let con_id = positive_id(self.con_id)?;
        let limit = limited(self.limit, 100)?;
        if self.start >= self.end {
            return Err(OptionReadError::InvalidQuery);
        }
        let Self {
            start, end, offset, ..
        } = self;
        let raw = match mode {
            FlowMode::Observed => {
                state
                    .finance
                    .read(|cfg| async move {
                        market_options_flow_api::observations(
                            &cfg,
                            con_id,
                            timestamp(start),
                            timestamp(end),
                            offset,
                            limit,
                        )
                        .await
                    })
                    .await?
            }
            FlowMode::Verified => {
                state
                    .finance
                    .read(|cfg| async move {
                        market_options_flow_api::verification(
                            &cfg,
                            con_id,
                            timestamp(start),
                            timestamp(end),
                            offset,
                            limit,
                        )
                        .await
                    })
                    .await?
            }
        };
        mode.display(raw)
    }
}

impl FlowMode {
    fn display(self, raw: Box<RawValue>) -> Result<Json<Box<RawValue>>, OptionReadError> {
        let invalid_response = |_| OptionReadError::Market(MarketReadError::InvalidResponse);
        match self {
            Self::Observed => {
                let page: ObservationAccess =
                    serde_json::from_str(raw.get()).map_err(invalid_response)?;
                require_display(&page.access, "opra")?;
                for coverage in page.items.iter().filter_map(|item| item.coverage.as_ref()) {
                    require_display(&coverage.access, "opra")?;
                }
            }
            Self::Verified => {
                let page: VerificationAccess =
                    serde_json::from_str(raw.get()).map_err(invalid_response)?;
                require_display(&page.access, "opra-history")?;
                if let Some(coverage) = page.observation_coverage {
                    require_display(&coverage.access, "opra")?;
                }
                if !page.access.record_local && page.durable_through.is_some() {
                    return Err(OptionReadError::Market(MarketReadError::InvalidResponse));
                }
            }
        }
        Ok(Json(raw))
    }
}

/// Read options flow with the legacy mode and window parameter names.
#[utoipa::path(get, path = "/api/v1/options/flow", operation_id = "optionsFlow", params(LegacyFlowQuery),
    security(("adminSession" = []), ("adminBearer" = [])),
    responses((status = 400, body = ErrorBody), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody), (status = 429, body = ErrorBody),
        (status = 503, body = ErrorBody, description = "Finance unavailable, or legacy instrument_id query needs con_id and a window")))]
pub(super) async fn legacy(
    State(state): State<AppState>,
    query: Result<Query<LegacyFlowQuery>, QueryRejection>,
) -> Result<Json<Box<RawValue>>, OptionReadError> {
    let query = parsed(query)?;
    if query
        .from
        .zip(query.until)
        .is_some_and(|(from, until)| from >= until)
        || query.limit == Some(0)
    {
        return Err(OptionReadError::InvalidQuery);
    }
    if query.con_id.is_none() && query.instrument_id.is_some() {
        return Err(OptionReadError::FlowQueryUpgradeRequired);
    }
    let (Some(con_id), Some(start), Some(end)) = (query.con_id, query.from, query.until) else {
        return Err(OptionReadError::InvalidQuery);
    };
    let query_window = FlowQuery {
        con_id,
        start,
        end,
        offset: query.offset.map(u64::from),
        limit: query.limit,
    };
    query_window
        .read(state, query.mode.unwrap_or_default())
        .await
}

pub(super) async fn observations(
    State(state): State<AppState>,
    query: Result<Query<FlowQuery>, QueryRejection>,
) -> Result<Json<Box<RawValue>>, OptionReadError> {
    parsed(query)?.read(state, FlowMode::Observed).await
}

pub(super) async fn verification(
    State(state): State<AppState>,
    query: Result<Query<FlowQuery>, QueryRejection>,
) -> Result<Json<Box<RawValue>>, OptionReadError> {
    parsed(query)?.read(state, FlowMode::Verified).await
}

pub(super) fn register_legacy_openapi(spec: &mut Value) {
    let responses = spec["paths"][OBSERVATIONS]["get"]["responses"].clone();
    let legacy = &mut spec["paths"][FLOW]["get"]["responses"];
    for status in ["404", "502", "504"] {
        legacy[status] = responses[status].clone();
    }
    legacy["200"] = json!({
        "description": "Observations (default) or historical verification, selected by mode",
        "content": {"application/json": {"schema": {"oneOf": [
            {"$ref": "#/components/schemas/OptionFlowObservationPage"},
            {"$ref": "#/components/schemas/OptionFlowVerificationPage"}
        ]}}},
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::StatusCode, response::IntoResponse};

    fn access(product: &str, display: bool) -> Value {
        json!({"source": "ibkr", "product": product, "license": "synthetic",
            "display_live": display, "record_local": false,
            "derive_non_display": false, "backup_personal_drive": false})
    }

    #[test]
    fn f10d_display_checks_both_products_and_recording_permission() {
        let denied = json!({"access": access("opra", false)});
        let incorrect = json!({"access": access("opra-history", true)});
        for (mode, body, status) in [
            (
                FlowMode::Observed,
                json!({"access": access("opra", false), "items": []}),
                StatusCode::FORBIDDEN,
            ),
            (
                FlowMode::Observed,
                json!({"access": access("opra", true), "items": [{"coverage": denied}]}),
                StatusCode::FORBIDDEN,
            ),
            (
                FlowMode::Verified,
                json!({"access": access("opra-history", true), "observation_coverage": denied}),
                StatusCode::FORBIDDEN,
            ),
            (
                FlowMode::Verified,
                json!({"access": access("opra-history", true), "observation_coverage": incorrect}),
                StatusCode::BAD_GATEWAY,
            ),
            (
                FlowMode::Verified,
                json!({"access": access("opra", true)}),
                StatusCode::BAD_GATEWAY,
            ),
            (
                FlowMode::Verified,
                json!({"access": access("opra-history", true), "durable_through": "18446744073709551615"}),
                StatusCode::BAD_GATEWAY,
            ),
        ] {
            let raw = RawValue::from_string(body.to_string()).unwrap();
            assert_eq!(
                mode.display(raw).unwrap_err().into_response().status(),
                status
            );
        }
    }

    #[test]
    fn f10d_display_retains_raw_decimal_text_and_unknown_fields() {
        let body = format!(
            r#"{{"access":{}, "durable_through":null, "observation_coverage":null,
            "trades":[{{"premium":"100.000000000000000001","status":"fixture_future"}}],
            "extra":1.234567891234567891}}"#,
            access("opra-history", true)
        );
        let raw = RawValue::from_string(body.clone()).unwrap();
        assert_eq!(FlowMode::Verified.display(raw).ok().unwrap().0.get(), body);
    }
}
