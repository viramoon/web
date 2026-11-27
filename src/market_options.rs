//! Administrator-only finance option and market-owner reads.

use axum::{
    Json, Router,
    extract::{Query, State, rejection::QueryRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, FixedOffset, NaiveDate, SecondsFormat, Utc};
use finance_client::{
    apis::{
        market_options_analytics_api as analytics, market_options_depth_api as depth,
        market_options_lines_api as lines,
    },
    models::{DealerScenario, ProductPolicy},
};
use serde::Deserialize;
use serde::de::value::{Error as ValueError, StrDeserializer};
use serde_json::{Value, json, value::RawValue};
use utoipa::OpenApi;

use crate::{
    AppState, api_error,
    finance::Fault,
    market_reads::MarketReadError,
    market_types::{InstrumentId, ProductToken, parse_u64, valid_token},
};

mod flow;

const CHAIN: &str = "/api/v1/options/chain";
const DEPTH: &str = "/api/v1/market/depth";
const EXPOSURE: &str = "/api/v1/options/exposure";
const HISTORY: &str = "/api/v1/options/history";
const BUDGET: &str = "/api/v1/market/subscriptions/budget";
const COVERAGE: &str = "/api/v1/market/subscriptions/coverage";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChainQuery {
    underlying_con_id: Option<i32>,
    expiry: Option<NaiveDate>,
    after_con_id: Option<i32>,
    limit: Option<u16>,
    instrument_id: Option<InstrumentId>,
    as_of: Option<DateTime<FixedOffset>>,
    offset: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DepthQuery {
    symbol: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    underlying_con_id: i32,
    start: String,
    end: String,
    offset: Option<u64>,
    limit: Option<u16>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CoverageQuery {
    con_id: i32,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExposureQuery {
    instrument_id: Option<InstrumentId>,
    underlying_id: Option<InstrumentId>,
    scenario: Option<ProductToken>,
    as_of: Option<DateTime<Utc>>,
}

impl ExposureQuery {
    fn finance_scenario(&self) -> Result<Option<&str>, OptionReadError> {
        let Some(scenario) = self.scenario.as_ref().map(ProductToken::as_str) else {
            return Ok(None);
        };
        DealerScenario::deserialize(StrDeserializer::<ValueError>::new(scenario))
            .map_err(|_| OptionReadError::ContractPending)?;
        Ok(Some(scenario))
    }
}

enum OptionReadError {
    InvalidQuery,
    ContractPending,
    FlowQueryUpgradeRequired,
    Market(MarketReadError),
    Upstream(Fault),
}

impl IntoResponse for OptionReadError {
    fn into_response(self) -> Response {
        match self {
            Self::InvalidQuery => api_error(
                StatusCode::BAD_REQUEST,
                "invalid_market_request",
                "Invalid market request",
            ),
            Self::ContractPending => api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "finance_contract_pending",
                "The finance contract for this capability is pending",
            ),
            Self::FlowQueryUpgradeRequired => api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "finance_flow_query_upgrade_required",
                "Use a confirmed con_id and the from and until boundaries",
            ),
            Self::Market(error) => error.into_response(),
            Self::Upstream(fault) => fault.into_response(),
        }
    }
}

impl From<Fault> for OptionReadError {
    fn from(fault: Fault) -> Self {
        Self::Upstream(fault)
    }
}

fn parsed<T>(query: Result<Query<T>, QueryRejection>) -> Result<T, OptionReadError> {
    query
        .map(|Query(query)| query)
        .map_err(|_| OptionReadError::InvalidQuery)
}

fn positive_id(id: i32) -> Result<u32, OptionReadError> {
    u32::try_from(id)
        .ok()
        .filter(|id| *id > 0)
        .ok_or(OptionReadError::InvalidQuery)
}

fn timestamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

fn limited(limit: Option<u16>, max: u16) -> Result<Option<u64>, OptionReadError> {
    if limit.is_some_and(|limit| !(1..=max).contains(&limit)) {
        return Err(OptionReadError::InvalidQuery);
    }
    Ok(limit.map(u64::from))
}

#[derive(Deserialize)]
struct DisplayAccess {
    access: ProductPolicy,
}

fn display(raw: Box<RawValue>, product: &str) -> Result<Json<Box<RawValue>>, OptionReadError> {
    let DisplayAccess { access } = serde_json::from_str(raw.get())
        .map_err(|_| OptionReadError::Market(MarketReadError::InvalidResponse))?;
    require_display(&access, product)?;
    Ok(Json(raw))
}

fn require_display(access: &ProductPolicy, product: &str) -> Result<(), OptionReadError> {
    if access.source != "ibkr" || access.product != product || !valid_token(&access.license) {
        return Err(OptionReadError::Market(MarketReadError::InvalidResponse));
    }
    if !access.display_live {
        return Err(OptionReadError::Market(MarketReadError::DisplayDenied));
    }
    Ok(())
}

async fn chain(
    State(state): State<AppState>,
    query: Result<Query<ChainQuery>, QueryRejection>,
) -> Result<Json<Box<RawValue>>, OptionReadError> {
    let query = parsed(query)?;
    let Some(underlying_con_id) = query.underlying_con_id else {
        return Err(
            if query.instrument_id.is_some()
                && query.expiry.is_none()
                && query.after_con_id.is_none()
                && query.limit != Some(0)
            {
                OptionReadError::ContractPending
            } else {
                OptionReadError::InvalidQuery
            },
        );
    };
    if query.instrument_id.is_some() || query.as_of.is_some() || query.offset.is_some() {
        return Err(OptionReadError::InvalidQuery);
    }
    let underlying_con_id = positive_id(underlying_con_id)?;
    let after_con_id = query.after_con_id.map(positive_id).transpose()?;
    let limit = limited(query.limit, 500)?;
    let raw = state
        .finance
        .read(|cfg| async move {
            lines::chain(
                &cfg,
                underlying_con_id,
                query.expiry.map(|date| date.to_string()),
                after_con_id,
                limit,
            )
            .await
        })
        .await?;
    display(raw, "opra")
}

async fn exposure(
    State(state): State<AppState>,
    query: Result<Query<ExposureQuery>, QueryRejection>,
) -> Result<Json<Box<RawValue>>, OptionReadError> {
    let query = parsed(query)?;
    let underlying = match (&query.underlying_id, &query.instrument_id) {
        (Some(id), Some(alias)) if id != alias => return Err(OptionReadError::InvalidQuery),
        (Some(id), _) | (_, Some(id)) => id.as_str(),
        _ => return Err(OptionReadError::InvalidQuery),
    };
    let scenario = query.finance_scenario()?;
    let raw = state
        .finance
        .read(|cfg| async move {
            analytics::exposure(&cfg, underlying, query.as_of.map(timestamp), scenario).await
        })
        .await?;
    display(raw, "opra")
}

async fn history(
    State(state): State<AppState>,
    query: Result<Query<HistoryQuery>, QueryRejection>,
) -> Result<Json<Box<RawValue>>, OptionReadError> {
    let query = parsed(query)?;
    let underlying_con_id = positive_id(query.underlying_con_id)?;
    let start = parse_u64(&query.start).ok_or(OptionReadError::InvalidQuery)?;
    let end = parse_u64(&query.end).ok_or(OptionReadError::InvalidQuery)?;
    if start >= end {
        return Err(OptionReadError::InvalidQuery);
    }
    let start = query.start.as_str();
    let end = query.end.as_str();
    let limit = limited(query.limit, 50)?;
    let raw = state
        .finance
        .read(|cfg| async move {
            analytics::history(&cfg, underlying_con_id, start, end, query.offset, limit).await
        })
        .await?;
    display(raw, "opra")
}

async fn depth(
    State(state): State<AppState>,
    query: Result<Query<DepthQuery>, QueryRejection>,
) -> Result<Json<Box<RawValue>>, OptionReadError> {
    let query = parsed(query)?;
    let symbol = query.symbol.as_str();
    let raw = state
        .finance
        .read(|cfg| async move { depth::book(&cfg, symbol).await })
        .await?;
    display(raw, "nasdaq-totalview")
}

async fn budget(State(state): State<AppState>) -> Result<Json<Box<RawValue>>, OptionReadError> {
    let raw = state
        .finance
        .read(|cfg| async move { lines::subscriptions(&cfg).await })
        .await?;
    display(raw, "opra")
}

async fn coverage(
    State(state): State<AppState>,
    query: Result<Query<CoverageQuery>, QueryRejection>,
) -> Result<Json<Box<RawValue>>, OptionReadError> {
    let query = parsed(query)?;
    let con_id = positive_id(query.con_id)?;
    if query.start >= query.end {
        return Err(OptionReadError::InvalidQuery);
    }
    let raw = state
        .finance
        .read(|cfg| async move {
            lines::coverage(&cfg, con_id, timestamp(query.start), timestamp(query.end)).await
        })
        .await?;
    display(raw, "opra")
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route(CHAIN, get(chain))
        .route(DEPTH, get(depth))
        .route(EXPOSURE, get(exposure))
        .route(flow::FLOW, get(flow::legacy))
        .route(flow::OBSERVATIONS, get(flow::observations))
        .route(flow::VERIFICATION, get(flow::verification))
        .route(HISTORY, get(history))
        .route(BUDGET, get(budget))
        .route(COVERAGE, get(coverage))
}

pub(crate) fn is_private(path: &str) -> bool {
    matches!(
        path,
        CHAIN
            | DEPTH
            | EXPOSURE
            | HISTORY
            | BUDGET
            | COVERAGE
            | flow::FLOW
            | flow::OBSERVATIONS
            | flow::VERIFICATION
    )
}

#[derive(OpenApi)]
#[openapi(
    paths(flow::legacy),
    components(schemas(InstrumentId, ProductToken, flow::FlowMode))
)]
pub(crate) struct MarketOptionsApi;

pub(crate) fn register_openapi(spec: &mut Value, upstream: &Value) {
    for (path, source, operation_id) in [
        (CHAIN, CHAIN, "optionsChain"),
        (DEPTH, "/api/v1/market/book", "marketVenueDepth"),
        (EXPOSURE, EXPOSURE, "optionsExposure"),
        (HISTORY, HISTORY, "optionsHistory"),
        (
            BUDGET,
            "/api/v1/market/subscriptions",
            "marketSubscriptionBudget",
        ),
        (COVERAGE, COVERAGE, "marketSubscriptionCoverage"),
        (
            flow::OBSERVATIONS,
            flow::OBSERVATIONS,
            "optionsFlowObservations",
        ),
        (
            flow::VERIFICATION,
            flow::VERIFICATION,
            "optionsFlowVerification",
        ),
    ] {
        crate::market_reads::register_read_operation(spec, upstream, path, source, operation_id);
        let operation = &mut spec["paths"][path]["get"];
        if path == CHAIN {
            let parameters = operation["parameters"]
                .as_array_mut()
                .expect("chain parameters");
            for parameter in parameters.iter_mut() {
                match parameter["name"].as_str() {
                    Some("underlying_con_id") => {
                        parameter["description"] =
                            json!("Confirmed broker identifier. Required for a successful read.");
                        parameter["required"] = json!(false);
                    }
                    Some("limit") => {
                        parameter["description"] = json!(
                            "F10c page size: 1–500, defaults to 100. Legacy queries retain their original range."
                        );
                        parameter["schema"] =
                            json!({"type": "integer", "format": "int32", "minimum": 0});
                    }
                    _ => {}
                }
            }
            for (name, schema) in [
                (
                    "instrument_id",
                    json!({"$ref": "#/components/schemas/InstrumentId"}),
                ),
                ("as_of", json!({"type": "string", "format": "date-time"})),
                (
                    "offset",
                    json!({"type": "integer", "format": "int32", "minimum": 0}),
                ),
            ] {
                parameters.push(json!({
                    "name": name, "in": "query", "required": false, "deprecated": true,
                    "description": "Legacy placeholder parameter. Use the F10c chain query.",
                    "schema": schema,
                }));
            }
            operation["responses"]["503"]["description"] =
                json!("Finance unavailable, or finance contract pending for a legacy query");
        }
        if path == EXPOSURE {
            let parameters = operation["parameters"]
                .as_array_mut()
                .expect("exposure parameters");
            for parameter in parameters.iter_mut() {
                match parameter["name"].as_str() {
                    Some("underlying_id") => parameter["required"] = json!(false),
                    Some("scenario") => {
                        parameter["schema"] = json!({"$ref": "#/components/schemas/ProductToken"});
                        parameter["description"] = json!(
                            "Use absolute (default), dealer_short, dealer_long, or call_long_put_short. Unsupported ProductToken values return 503 finance_contract_pending."
                        );
                    }
                    _ => {}
                }
            }
            parameters.push(json!({
                "name": "instrument_id", "in": "query", "required": false,
                "description": "Alias for underlying_id. Supply at least one.",
                "schema": {"$ref": "#/components/schemas/InstrumentId"}
            }));
            operation["responses"]["503"]["description"] = json!(
                "Finance unavailable, or finance contract pending for an unsupported scenario"
            );
        }
        let success = operation["responses"]["200"].clone();
        register_referenced_schemas(
            &success,
            &mut spec["components"]["schemas"],
            &upstream["components"]["schemas"],
        );
    }
    flow::register_legacy_openapi(spec);
}

fn register_referenced_schemas(value: &Value, schemas: &mut Value, upstream: &Value) {
    match value {
        Value::Object(fields) => {
            if let Some(name) = fields
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|reference| reference.strip_prefix("#/components/schemas/"))
                && schemas.get(name).is_none()
            {
                let schema = upstream.get(name).expect("finance response schema");
                schemas[name] = schema.clone();
                register_referenced_schemas(schema, schemas, upstream);
            }
            for field in fields.values() {
                register_referenced_schemas(field, schemas, upstream);
            }
        }
        Value::Array(items) => {
            for item in items {
                register_referenced_schemas(item, schemas, upstream);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f10c_exposure_query_accepts_finance_scenarios_and_omission() {
        for scenario in [
            DealerScenario::Absolute,
            DealerScenario::DealerShort,
            DealerScenario::DealerLong,
            DealerScenario::CallLongPutShort,
        ] {
            let scenario = scenario.to_string();
            let uri = format!("{EXPOSURE}?underlying_id=SYNTH.XNYS&scenario={scenario}")
                .parse()
                .unwrap();
            let Query(query) = Query::<ExposureQuery>::try_from_uri(&uri).unwrap();
            assert_eq!(query.finance_scenario().ok(), Some(Some(scenario.as_str())));
        }
        let uri = format!("{EXPOSURE}?instrument_id=SYNTH.XNYS")
            .parse()
            .unwrap();
        let Query(query) = Query::<ExposureQuery>::try_from_uri(&uri).unwrap();
        assert_eq!(query.finance_scenario().ok(), Some(None));
    }

    #[test]
    fn f10c_display_validation_preserves_raw_json_and_denies_bad_access() {
        let reply = |source: &str, product: &str, license: &str, display_live| {
            format!(
                r#"{{"access":{{"source":"{source}","product":"{product}","license":"{license}","display_live":{display_live},"record_local":false,"derive_non_display":false,"backup_personal_drive":false}},"future":null,"exact":"9007199254740993.0000","extra_number":1.234567890123456789}}"#
            )
        };
        let allowed = reply("ibkr", "opra", "synthetic", true);
        let raw = RawValue::from_string(allowed.clone()).unwrap();
        assert_eq!(display(raw, "opra").ok().unwrap().0.get(), allowed);

        for (body, expected) in [
            (
                reply("ibkr", "opra", "synthetic", false),
                StatusCode::FORBIDDEN,
            ),
            (reply("ibkr", "opra", "", true), StatusCode::BAD_GATEWAY),
            (
                reply("ibkr", "nasdaq-totalview", "synthetic", true),
                StatusCode::BAD_GATEWAY,
            ),
            (
                reply("synthetic", "opra", "synthetic", true),
                StatusCode::BAD_GATEWAY,
            ),
            ("null".into(), StatusCode::BAD_GATEWAY),
        ] {
            let raw = RawValue::from_string(body).unwrap();
            assert_eq!(
                display(raw, "opra").unwrap_err().into_response().status(),
                expected
            );
        }
    }
}
