use crate::{AppState, api_error};
use axum::{
    Json, Router,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use finance_client::{
    apis::{bundle_jobs_api, factor_read_api},
    models,
};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const STRATEGIES: &str = "/api/v1/strategies";
pub(crate) const VALIDATE: &str = "/api/v1/strategies/validate";
const VERSION: &str = "/api/v1/strategies/{id}/versions/{version}";
const FACTORS: &str = "/api/v1/factors";
const EVALUATIONS: &str = "/api/v1/factors/{id}/evaluations";
const EVALUATION: &str = "/api/v1/factors/{id}/evaluations/{evaluation_id}";
pub(crate) const REGISTER_TIMEOUT_HINT: &str =
    "Registration outcome unknown; read the strategy version before retrying";
pub(crate) const MAX_DOCUMENT_BYTES: usize = 256 * 1024;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route(STRATEGIES, get(versions).post(register))
        .route(VALIDATE, post(validate))
        .route(VERSION, get(version))
        .route(FACTORS, get(factors))
        .route(EVALUATIONS, get(evaluations))
        .route(EVALUATION, get(evaluation))
}

pub(crate) fn is_private(path: &str) -> bool {
    matches!(
        path,
        STRATEGIES | VALIDATE | VERSION | FACTORS | EVALUATIONS | EVALUATION
    )
}

pub(crate) fn document_too_large() -> Response {
    api_error(
        StatusCode::PAYLOAD_TOO_LARGE,
        "body_too_large",
        "Strategy document is too large",
    )
}

fn invalid_strategy() -> Response {
    api_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_request",
        "Invalid strategy document, version or pagination",
    )
}

fn invalid_factor() -> Response {
    api_error(
        StatusCode::BAD_REQUEST,
        "invalid_factor_query",
        "Invalid factor identifier, version or pagination",
    )
}

// Keep user input within a single path segment before the client builds a URL.
// Finance checks the domain-specific identifier and release version formats.
fn safe_segment(segment: &str) -> bool {
    segment
        .as_bytes()
        .split_first()
        .is_some_and(|(first, rest)| {
            first.is_ascii_alphanumeric()
                && rest
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    format: models::StrategyFormat,
    content: String,
}

enum DocumentError {
    TooLarge,
    Invalid,
}

impl IntoResponse for DocumentError {
    fn into_response(self) -> Response {
        match self {
            Self::TooLarge => document_too_large(),
            Self::Invalid => invalid_strategy(),
        }
    }
}

fn document(
    body: Result<Json<Document>, JsonRejection>,
) -> Result<models::StrategyDocument, DocumentError> {
    match body {
        Ok(Json(input)) if input.content.len() <= MAX_DOCUMENT_BYTES => {
            Ok(models::StrategyDocument::new(input.content, input.format))
        }
        Ok(_) => Err(DocumentError::TooLarge),
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => Err(DocumentError::TooLarge),
        Err(_) => Err(DocumentError::Invalid),
    }
}

async fn validate(
    State(s): State<AppState>,
    body: Result<Json<Document>, JsonRejection>,
) -> Response {
    let input = match document(body) {
        Ok(input) => input,
        Err(error) => return error.into_response(),
    };
    s.finance
        .post_once(|cfg| {
            let input = input.clone();
            async move { bundle_jobs_api::validate_strategy(&cfg, input).await }
        })
        .await
        .map(Json)
        .into_response()
}

async fn register(
    State(s): State<AppState>,
    body: Result<Json<Document>, JsonRejection>,
) -> Response {
    let input = match document(body) {
        Ok(input) => input,
        Err(error) => return error.into_response(),
    };
    s.finance
        .post_once(|cfg| {
            let input = input.clone();
            async move { bundle_jobs_api::register(&cfg, input).await }
        })
        .await
        .map(Json)
        .map_err(|fault| fault.with_timeout_hint(REGISTER_TIMEOUT_HINT))
        .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrategyQuery {
    id: Option<String>,
    after: Option<String>,
    limit: Option<u32>,
}

async fn versions(
    State(s): State<AppState>,
    query: Result<Query<StrategyQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return invalid_strategy();
    };
    s.finance
        .read(|cfg| {
            let query = &query;
            async move {
                bundle_jobs_api::strategy_versions(
                    &cfg,
                    query.id.as_deref(),
                    query.after.as_deref(),
                    query.limit,
                )
                .await
            }
        })
        .await
        .map(Json)
        .into_response()
}

async fn version(
    State(s): State<AppState>,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Response {
    let Ok(Path((id, version))) = path else {
        return invalid_strategy();
    };
    if !safe_segment(&id) || !safe_segment(&version) {
        return invalid_strategy();
    }
    s.finance
        .read(|cfg| {
            let (id, version) = (&id, &version);
            async move { bundle_jobs_api::strategy_version(&cfg, id, version).await }
        })
        .await
        .map(Json)
        .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FactorQuery {
    factor_id: Option<String>,
    version: Option<String>,
    after: Option<String>,
    limit: Option<u32>,
}

async fn factors(
    State(s): State<AppState>,
    query: Result<Query<FactorQuery>, QueryRejection>,
) -> Response {
    let Ok(Query(query)) = query else {
        return invalid_factor();
    };
    s.finance
        .read(|cfg| {
            let query = &query;
            async move {
                factor_read_api::factor_list(
                    &cfg,
                    query.factor_id.as_deref(),
                    query.version.as_deref(),
                    query.after.as_deref(),
                    query.limit,
                )
                .await
            }
        })
        .await
        .map(Json)
        .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvaluationQuery {
    version: Option<String>,
    after: Option<String>,
    limit: Option<u32>,
}

async fn evaluations(
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    query: Result<Query<EvaluationQuery>, QueryRejection>,
) -> Response {
    let (Ok(Path(id)), Ok(Query(query))) = (path, query) else {
        return invalid_factor();
    };
    if !safe_segment(&id) {
        return invalid_factor();
    }
    s.finance
        .read(|cfg| {
            let (id, query) = (&id, &query);
            async move {
                factor_read_api::factor_evaluations(
                    &cfg,
                    id,
                    query.version.as_deref(),
                    query.after.as_deref(),
                    query.limit,
                )
                .await
            }
        })
        .await
        .map(Json)
        .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExactEvaluationQuery {
    version: String,
}

async fn evaluation(
    State(s): State<AppState>,
    path: Result<Path<(String, String)>, PathRejection>,
    query: Result<Query<ExactEvaluationQuery>, QueryRejection>,
) -> Response {
    let (Ok(Path((id, evaluation_id))), Ok(Query(query))) = (path, query) else {
        return invalid_factor();
    };
    if !safe_segment(&id) || !safe_segment(&evaluation_id) {
        return invalid_factor();
    }
    s.finance
        .read(|cfg| {
            let (id, evaluation_id, version) = (&id, &evaluation_id, &query.version);
            async move { factor_read_api::factor_evaluation(&cfg, id, evaluation_id, version).await }
        })
        .await
        .map(Json)
        .into_response()
}

pub(crate) fn register_openapi(spec: &mut Value, upstream: &Value) {
    for path in [
        STRATEGIES,
        VALIDATE,
        VERSION,
        FACTORS,
        EVALUATIONS,
        EVALUATION,
    ] {
        let mut item = upstream["paths"][path].clone();
        let operations = item.as_object_mut().expect("finance research path");
        for (method, operation) in operations {
            operation["security"] = json!([{"adminBearer": []}, {"adminSession": []}]);
            let responses = operation["responses"]
                .as_object_mut()
                .expect("finance research responses");
            responses.remove("500");
            for (status, description) in [
                ("401", "Authentication required"),
                ("403", "Access denied"),
                ("502", "Finance response error"),
                ("503", "Finance unavailable"),
                ("504", "Finance request timed out"),
            ] {
                responses.insert(status.into(), error_response(description));
            }
            if method == "post" {
                responses.insert(
                    "413".into(),
                    error_response("Request or strategy document too large"),
                );
                responses.insert("429".into(), error_response("Write rate limit exceeded"));
                operation["parameters"] = json!([
                    {"name":"Origin", "in":"header", "description":"Configured HTTPS origin with the session cookie", "schema":{"type":"string"}},
                    {"name":"X-CSRF-Token", "in":"header", "description":"Required with the session cookie", "schema":{"type":"string"}}
                ]);
            }
        }
        spec["paths"][path] = item;
    }

    // W4 integration evidence already uses the name StrategyPreview.
    for (path, method) in [(VALIDATE, "post"), (VERSION, "get")] {
        let reference = &mut spec["paths"][path][method]["responses"]["200"]["content"]["application/json"]
            ["schema"]["$ref"];
        assert_eq!(*reference, "#/components/schemas/StrategyPreview");
        *reference = json!("#/components/schemas/StrategyBundlePreview");
    }
    let schemas = spec["components"]["schemas"]
        .as_object_mut()
        .expect("web schemas");
    let upstream_schemas = &upstream["components"]["schemas"];
    for name in [
        "Availability",
        "Bars",
        "BundleSubmission",
        "CompiledComponent",
        "CompiledPlan",
        "CompiledSleeve",
        "ConflictRules",
        "EvaluationPhase",
        "FactorDecimal",
        "FactorDetails",
        "FactorDistribution",
        "FactorEvaluationDetail",
        "FactorEvaluationItem",
        "FactorEvaluationPage",
        "FactorInteger",
        "FactorPeer",
        "FactorPortfolioSummary",
        "FactorReadAccess",
        "FactorReadItem",
        "FactorReadPage",
        "FactorState",
        "FactorWindowBounds",
        "FactorWindowSummary",
        "HistoricalDecision",
        "Parameter",
        "RegisteredStrategy",
        "ResolvedInputs",
        "SharpeComparison",
        "StrategyDocument",
        "StrategyFormat",
        "StrategySource",
        "StrategyVersionPage",
        "SubmitRequest",
        "Transition",
        "Universe",
    ] {
        let schema = &upstream_schemas[name];
        assert!(schema.is_object(), "finance research schema {name}");
        let previous = schemas.insert(name.to_owned(), schema.clone());
        assert!(
            previous.is_none(),
            "web already defines research schema {name}"
        );
    }
    schemas.insert(
        "StrategyBundlePreview".into(),
        upstream_schemas["StrategyPreview"].clone(),
    );
    for source in schemas
        .get_mut("StrategySource")
        .expect("copied strategy source schema")["oneOf"]
        .as_array_mut()
        .expect("strategy source variants")
    {
        source["additionalProperties"] = json!(false);
    }
}

fn error_response(description: &str) -> Value {
    json!({
        "description": description,
        "content": {
            "application/json": {"schema": {"$ref": "#/components/schemas/ErrorBody"}}
        }
    })
}

#[cfg(test)]
mod tests;
