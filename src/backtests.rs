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
use finance_client::{apis::jobs_api, models};
use serde::{Deserialize, de::IgnoredAny};
use std::collections::BTreeMap;

#[derive(Deserialize)]
pub(crate) struct Submission {
    // The generated model ignores unknown properties. Preserve finance's strict input contract.
    #[serde(flatten)]
    request: models::JobRequest,
    #[serde(flatten)]
    extra: BTreeMap<String, IgnoredAny>,
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

#[utoipa::path(post, path = "/api/v1/backtests", security(("adminBearer" = [])),
    params(("Idempotency-Key" = String, Header, description="Stable key, 1..128 visible ASCII characters")),
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
    let mut keys = headers.get_all("Idempotency-Key").iter();
    let key = keys.next().and_then(|v| v.to_str().ok()).filter(|v| {
        (1..=128).contains(&v.len())
            && v.bytes().all(|c| c.is_ascii_graphic())
            && keys.next().is_none()
    });
    let Some(key) = key else {
        return invalid("One Idempotency-Key with 1..128 visible ASCII characters is required");
    };
    let input = match body {
        Ok(Json(input))
            if input.extra.is_empty()
                && input
                    .request
                    .params
                    .as_object()
                    .is_some_and(|params| params.is_empty()) =>
        {
            input.request
        }
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => return e.into_response(),
        _ => return invalid("Invalid backtest submission"),
    };
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
