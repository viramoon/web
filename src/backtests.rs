use crate::{AppState, ErrorBody, api_error};
use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use finance_client::{
    apis::{jobs_api, research_api},
    models,
};
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Submission {
    strategy: String,
    instrument: String,
    start: String,
    end: String,
    params: Map<String, Value>,
}

impl Submission {
    fn into_request(self) -> Option<models::JobRequest> {
        if self
            .params
            .keys()
            .any(|k| !matches!(k.as_str(), "fast_period" | "slow_period" | "shares"))
        {
            return None;
        }
        let params =
            serde_json::from_value::<models::StrategyParams>(Value::Object(self.params)).ok()?;
        Some(models::JobRequest::new(
            self.end,
            self.instrument,
            params,
            self.start,
            self.strategy,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pagination {
    limit: Option<u32>,
    cursor: Option<i64>,
}

fn invalid(message: &str) -> Response {
    api_error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_request", message)
}

fn lookup_key(headers: &HeaderMap) -> Option<&str> {
    let mut keys = headers.get_all("Idempotency-Key").iter();
    keys.next().and_then(|v| v.to_str().ok()).filter(|v| {
        (1..=128).contains(&v.len())
            && v.bytes().all(|c| c.is_ascii_graphic())
            && keys.next().is_none()
    })
}

#[utoipa::path(post, path = "/api/v1/backtests", security(("adminBearer" = []), ("adminSession" = [])),
    params(("Idempotency-Key" = String, Header, description="Stable key, 1..128 visible ASCII characters"),
        ("Origin" = Option<String>, Header, description="Configured HTTPS origin with the session cookie"),
        ("X-CSRF-Token" = Option<String>, Header, description="Required with the session cookie")),
    request_body = ref("#/components/schemas/JobRequest"),
    responses((status=202,body=ref("#/components/schemas/Accepted")), (status=401,body=ErrorBody),
    (status=403,body=ErrorBody),(status=409,body=ErrorBody),(status=413,body=ErrorBody),
    (status=422,body=ErrorBody),(status=429,body=ErrorBody),(status=502,body=ErrorBody),
    (status=503,body=ErrorBody),(status=504,body=ErrorBody)))]
pub(crate) async fn submit(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<Submission>, JsonRejection>,
) -> Response {
    let Some(key) = lookup_key(&headers) else {
        return invalid("One Idempotency-Key with 1..128 visible ASCII characters is required");
    };
    let input = match body {
        Ok(Json(input)) => input.into_request(),
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => return e.into_response(),
        _ => None,
    };
    let Some(input) = input else {
        return invalid("Invalid backtest submission");
    };
    let input = models::SubmitRequest::JobRequest(Box::new(input));
    match s
        .finance
        .write(|cfg| {
            let request = input.clone();
            async move { jobs_api::submit(&cfg, key, request).await }
        })
        .await
    {
        Ok(accepted) => (StatusCode::ACCEPTED, Json(accepted)).into_response(),
        Err(e) => e.into_response(),
    }
}

#[utoipa::path(get, path = "/api/v1/backtests/by-key", security(("adminBearer" = []), ("adminSession" = [])),
    params(("Idempotency-Key" = String, Header, description="Exact attempted submission key")),
    responses((status=200,body=ref("#/components/schemas/Accepted")),(status=401,body=ErrorBody),
    (status=403,body=ErrorBody),(status=404,body=ErrorBody),(status=422,body=ErrorBody),
    (status=502,body=ErrorBody),(status=503,body=ErrorBody),(status=504,body=ErrorBody)))]
pub(crate) async fn by_key(State(s): State<AppState>, headers: HeaderMap) -> Response {
    let Some(key) = lookup_key(&headers) else {
        return invalid("One Idempotency-Key with 1..128 visible ASCII characters is required");
    };
    s.finance
        .read(|cfg| async move { jobs_api::by_key(&cfg, key).await })
        .await
        .map(Json)
        .into_response()
}

#[utoipa::path(get, path = "/api/v1/backtests/datasets",
    responses((status=200,body=ref("#/components/schemas/DatasetList")),(status=502,body=ErrorBody),
        (status=503,body=ErrorBody),(status=504,body=ErrorBody)))]
pub(crate) async fn datasets(State(s): State<AppState>) -> Response {
    s.finance
        .read(|cfg| async move { research_api::datasets(&cfg).await })
        .await
        .map(Json)
        .into_response()
}

#[utoipa::path(get, path = "/api/v1/backtests/{id}", params(("id" = i64, Path)),
    responses((status=200,body=ref("#/components/schemas/Job")),(status=404,body=ErrorBody),
    (status=422,body=ErrorBody),(status=502,body=ErrorBody),(status=503,body=ErrorBody),(status=504,body=ErrorBody)))]
pub(crate) async fn detail(
    State(s): State<AppState>,
    path: Result<Path<i64>, PathRejection>,
) -> Response {
    let Ok(Path(id)) = path else {
        return invalid("Invalid backtest id");
    };
    s.finance
        .read(|cfg| async move { jobs_api::detail(&cfg, id).await })
        .await
        .map(Json)
        .into_response()
}

#[utoipa::path(get, path = "/api/v1/backtests",
    params(("limit" = Option<u32>, Query, description="1..100, default 20"),
        ("cursor" = Option<i64>, Query, description="Exclusive last id in ascending order")),
    responses((status=200,body=ref("#/components/schemas/Page")),(status=422,body=ErrorBody),
    (status=502,body=ErrorBody),(status=503,body=ErrorBody),(status=504,body=ErrorBody)))]
pub(crate) async fn list(
    State(s): State<AppState>,
    query: Result<Query<Pagination>, QueryRejection>,
) -> Response {
    let Ok(Query(p)) = query else {
        return invalid("Invalid pagination parameters");
    };
    s.finance
        .read(|cfg| async move { jobs_api::list(&cfg, p.limit, p.cursor).await })
        .await
        .map(Json)
        .into_response()
}
