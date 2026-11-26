use super::{ApiError, require_empty, uuid_path};
use crate::{
    AppState, ErrorBody,
    notifications::store::silence::{self, SilenceRequest, SilenceStatus, Target},
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        Path, RawQuery, State,
        rejection::{BytesRejection, JsonRejection, PathRejection},
    },
    http::StatusCode,
    routing::get,
};

pub(crate) const RULE_SILENCE: &str = "/api/v1/notification-rules/{id}/silence";
pub(crate) const ALERT_SILENCE: &str = "/api/v1/alerts/{id}/silence";

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            RULE_SILENCE,
            get(get_rule).put(put_rule).delete(delete_rule),
        )
        .route(
            ALERT_SILENCE,
            get(get_alert).put(put_alert).delete(delete_alert),
        )
}

async fn get_status(state: &AppState, target: Target) -> Result<Json<SilenceStatus>, ApiError> {
    silence::get(&state.db, target)
        .await?
        .map(Json)
        .ok_or(ApiError::Missing)
}

async fn change(
    state: &AppState,
    target: Target,
    request: Option<SilenceRequest>,
) -> Result<Json<SilenceStatus>, ApiError> {
    silence::change(&state.db, target, request)
        .await?
        .map(Json)
        .ok_or(ApiError::Missing)
}

fn parse_request(
    query: Option<String>,
    body: Result<Json<SilenceRequest>, JsonRejection>,
) -> Result<SilenceRequest, ApiError> {
    match body {
        Ok(Json(request)) if query.is_none() => Ok(request),
        Err(error) if error.status() == StatusCode::PAYLOAD_TOO_LARGE => Err(ApiError::TooLarge),
        _ => Err(ApiError::Invalid),
    }
}

/// Read the rule's latest manual silence.
#[utoipa::path(get, tag = "notifications", path = RULE_SILENCE, operation_id = "get_rule_silence",
    params(("id" = String, Path, format = "uuid")),
    security(("adminBearer" = []), ("adminSession" = [])),
    responses((status = 200, body = SilenceStatus), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody), (status = 404, body = ErrorBody),
        (status = 422, body = ErrorBody), (status = 503, body = ErrorBody),
        (status = 504, body = ErrorBody)))]
pub(crate) async fn get_rule(
    State(state): State<AppState>,
    path: Result<Path<String>, PathRejection>,
) -> Result<Json<SilenceStatus>, ApiError> {
    get_status(&state, Target::Rule(uuid_path(path)?)).await
}

/// Create or replace the rule's manual silence.
#[utoipa::path(put, tag = "notifications", path = RULE_SILENCE, operation_id = "put_rule_silence",
    request_body = SilenceRequest, security(("adminBearer" = []), ("adminSession" = [])),
    params(("id" = String, Path, format = "uuid"),
        ("Origin" = Option<String>, Header, description = "Matches the configured HTTPS origin when sent and is required with the session cookie"),
        ("X-CSRF-Token" = Option<String>, Header, description = "Required with the session cookie")),
    responses((status = 200, body = SilenceStatus), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody), (status = 404, body = ErrorBody),
        (status = 413, body = ErrorBody), (status = 422, body = ErrorBody),
        (status = 429, body = ErrorBody), (status = 503, body = ErrorBody),
        (status = 504, body = ErrorBody)))]
pub(crate) async fn put_rule(
    State(state): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    RawQuery(query): RawQuery,
    body: Result<Json<SilenceRequest>, JsonRejection>,
) -> Result<Json<SilenceStatus>, ApiError> {
    let target = Target::Rule(uuid_path(path)?);
    change(&state, target, Some(parse_request(query, body)?)).await
}

/// Release the rule's manual silence.
#[utoipa::path(delete, tag = "notifications", path = RULE_SILENCE, operation_id = "delete_rule_silence",
    security(("adminBearer" = []), ("adminSession" = [])),
    params(("id" = String, Path, format = "uuid"),
        ("Origin" = Option<String>, Header, description = "Matches the configured HTTPS origin when sent and is required with the session cookie"),
        ("X-CSRF-Token" = Option<String>, Header, description = "Required with the session cookie")),
    responses((status = 200, body = SilenceStatus), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody), (status = 404, body = ErrorBody),
        (status = 413, body = ErrorBody), (status = 422, body = ErrorBody),
        (status = 429, body = ErrorBody), (status = 503, body = ErrorBody),
        (status = 504, body = ErrorBody)))]
pub(crate) async fn delete_rule(
    State(state): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    RawQuery(query): RawQuery,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<SilenceStatus>, ApiError> {
    let target = Target::Rule(uuid_path(path)?);
    require_empty(query, body)?;
    change(&state, target, None).await
}

/// Read the alert instance's latest manual silence.
#[utoipa::path(get, tag = "notifications", path = ALERT_SILENCE, operation_id = "get_alert_silence",
    params(("id" = String, Path, format = "uuid")),
    security(("adminBearer" = []), ("adminSession" = [])),
    responses((status = 200, body = SilenceStatus), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody), (status = 404, body = ErrorBody),
        (status = 422, body = ErrorBody), (status = 503, body = ErrorBody),
        (status = 504, body = ErrorBody)))]
pub(crate) async fn get_alert(
    State(state): State<AppState>,
    path: Result<Path<String>, PathRejection>,
) -> Result<Json<SilenceStatus>, ApiError> {
    get_status(&state, Target::Alert(uuid_path(path)?)).await
}

/// Create or replace the alert instance's manual silence.
#[utoipa::path(put, tag = "notifications", path = ALERT_SILENCE, operation_id = "put_alert_silence",
    request_body = SilenceRequest, security(("adminBearer" = []), ("adminSession" = [])),
    params(("id" = String, Path, format = "uuid"),
        ("Origin" = Option<String>, Header, description = "Matches the configured HTTPS origin when sent and is required with the session cookie"),
        ("X-CSRF-Token" = Option<String>, Header, description = "Required with the session cookie")),
    responses((status = 200, body = SilenceStatus), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody), (status = 404, body = ErrorBody),
        (status = 413, body = ErrorBody), (status = 422, body = ErrorBody),
        (status = 429, body = ErrorBody), (status = 503, body = ErrorBody),
        (status = 504, body = ErrorBody)))]
pub(crate) async fn put_alert(
    State(state): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    RawQuery(query): RawQuery,
    body: Result<Json<SilenceRequest>, JsonRejection>,
) -> Result<Json<SilenceStatus>, ApiError> {
    let target = Target::Alert(uuid_path(path)?);
    change(&state, target, Some(parse_request(query, body)?)).await
}

/// Release the alert instance's manual silence.
#[utoipa::path(delete, tag = "notifications", path = ALERT_SILENCE, operation_id = "delete_alert_silence",
    security(("adminBearer" = []), ("adminSession" = [])),
    params(("id" = String, Path, format = "uuid"),
        ("Origin" = Option<String>, Header, description = "Matches the configured HTTPS origin when sent and is required with the session cookie"),
        ("X-CSRF-Token" = Option<String>, Header, description = "Required with the session cookie")),
    responses((status = 200, body = SilenceStatus), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody), (status = 404, body = ErrorBody),
        (status = 413, body = ErrorBody), (status = 422, body = ErrorBody),
        (status = 429, body = ErrorBody), (status = 503, body = ErrorBody),
        (status = 504, body = ErrorBody)))]
pub(crate) async fn delete_alert(
    State(state): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    RawQuery(query): RawQuery,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<SilenceStatus>, ApiError> {
    let target = Target::Alert(uuid_path(path)?);
    require_empty(query, body)?;
    change(&state, target, None).await
}
