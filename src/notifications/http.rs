use super::{
    model::{Alert, ChannelTest, Rule, RuleConfig},
    store::{self, StoreError},
};
use crate::{AppState, ErrorBody, api_error};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        Path, Query, RawQuery, State,
        rejection::{BytesRejection, JsonRejection, PathRejection, QueryRejection},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

pub(crate) mod silence;

pub(crate) const RULES: &str = "/api/v1/notification-rules";
pub(crate) const RULE: &str = "/api/v1/notification-rules/{id}";
pub(crate) const ALERTS: &str = "/api/v1/alerts";
pub(crate) const ALERT: &str = "/api/v1/alerts/{id}";
pub(crate) const ACK: &str = "/api/v1/alerts/{id}/ack";
pub(crate) const CHANNEL_TESTS: &str = "/api/v1/notification-channels/{id}/tests";
pub(crate) const CHANNEL_TEST: &str = "/api/v1/notification-channels/{id}/tests/{testId}";

pub(crate) fn is_private(path: &str) -> bool {
    matches!(
        path,
        RULES
            | RULE
            | ALERTS
            | ALERT
            | ACK
            | CHANNEL_TESTS
            | CHANNEL_TEST
            | silence::RULE_SILENCE
            | silence::ALERT_SILENCE
    )
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route(RULES, get(list_rules))
        .route(RULE, get(get_rule).put(put_rule))
        .route(ALERTS, get(list_alerts))
        .route(ALERT, get(get_alert))
        .route(ACK, post(ack_alert))
        .route(CHANNEL_TESTS, post(start_channel_test))
        .route(CHANNEL_TEST, get(get_channel_test))
        .merge(silence::routes())
}

pub(crate) enum ApiError {
    Invalid,
    RevisionRequired,
    TooLarge,
    Missing,
    Conflict,
    DatabaseUnavailable,
    Internal,
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::Conflict => Self::Conflict,
            StoreError::Invalid => Self::Invalid,
            StoreError::Database(error) => error.into(),
        }
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        match error {
            sqlx::Error::Io(_)
            | sqlx::Error::Tls(_)
            | sqlx::Error::PoolClosed
            | sqlx::Error::PoolTimedOut
            | sqlx::Error::WorkerCrashed => Self::DatabaseUnavailable,
            sqlx::Error::Database(error)
                if error.code().is_some_and(|code| {
                    code.starts_with("08") || matches!(code.as_ref(), "42P01" | "57P01" | "57P03")
                }) =>
            {
                Self::DatabaseUnavailable
            }
            _ => Self::Internal,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::Invalid => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_request",
                "Invalid notification request",
            ),
            Self::RevisionRequired => (
                StatusCode::PRECONDITION_REQUIRED,
                "revision_required",
                "If-Match with a quoted rule revision is required",
            ),
            Self::TooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                "Request body is too large",
            ),
            Self::Missing => (
                StatusCode::NOT_FOUND,
                "notification_not_found",
                "Notification resource was not found",
            ),
            Self::Conflict => (
                StatusCode::CONFLICT,
                "rule_conflict",
                "Notification rule revision changed",
            ),
            Self::DatabaseUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "database_unavailable",
                "Notification storage is unavailable",
            ),
            Self::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Internal server error",
            ),
        };
        api_error(status, code, message)
    }
}

fn uuid_path(path: Result<Path<String>, PathRejection>) -> Result<Uuid, ApiError> {
    let Path(id) = path.map_err(|_| ApiError::Invalid)?;
    parse_uuid(&id)
}

fn parse_uuid(value: &str) -> Result<Uuid, ApiError> {
    Uuid::try_parse(value)
        .ok()
        .filter(|id| !id.is_nil())
        .ok_or(ApiError::Invalid)
}

fn parse_number(value: &str) -> Option<i64> {
    value.parse::<i64>().ok().filter(|n| n.to_string() == value)
}

fn revision(headers: &HeaderMap) -> Result<i64, ApiError> {
    let mut values = headers.get_all(header::IF_MATCH).iter();
    values
        .next()
        .ok_or(ApiError::RevisionRequired)?
        .to_str()
        .ok()
        .and_then(|value| parse_number(value.strip_prefix('"')?.strip_suffix('"')?))
        .filter(|revision| *revision >= 0 && values.next().is_none())
        .ok_or(ApiError::Invalid)
}

fn rule_response(status: StatusCode, rule: Rule) -> Response {
    let etag = HeaderValue::from_str(&format!("\"{}\"", rule.revision))
        .expect("numeric revision is valid ASCII");
    (status, [(header::ETAG, etag)], Json(rule)).into_response()
}

fn require_empty(
    query: Option<String>,
    body: Result<Bytes, BytesRejection>,
) -> Result<(), ApiError> {
    match body {
        Ok(body) if body.is_empty() && query.is_none() => Ok(()),
        Err(error) if error.status() == StatusCode::PAYLOAD_TOO_LARGE => Err(ApiError::TooLarge),
        _ => Err(ApiError::Invalid),
    }
}

fn require_email(channel: &str) -> Result<(), ApiError> {
    if channel == "email" {
        Ok(())
    } else {
        Err(ApiError::Missing)
    }
}

fn limit(requested: Option<u16>) -> Result<usize, ApiError> {
    let limit = requested.unwrap_or(20);
    if (1..=100).contains(&limit) {
        Ok(usize::from(limit))
    } else {
        Err(ApiError::Invalid)
    }
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct RulesQuery {
    #[param(minimum = 1, maximum = 100, default = 20)]
    limit: Option<u16>,
    #[param(format = "uuid")]
    cursor: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct RulePage {
    items: Vec<Rule>,
    #[schema(format = "uuid")]
    next_cursor: Option<String>,
}

#[utoipa::path(get, path = "/api/v1/notification-rules", params(RulesQuery),
    security(("adminBearer" = []), ("adminSession" = [])),
    responses((status = 200, body = RulePage), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody), (status = 422, body = ErrorBody),
        (status = 503, body = ErrorBody)))]
pub(crate) async fn list_rules(
    State(s): State<AppState>,
    query: Result<Query<RulesQuery>, QueryRejection>,
) -> Result<Json<RulePage>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::Invalid)?;
    let limit = limit(query.limit)?;
    let cursor = query.cursor.as_deref().map(parse_uuid).transpose()?;
    let mut items = store::list_rules(&s.db, limit as i64 + 1, cursor).await?;
    let has_more = items.len() > limit;
    items.truncate(limit);
    let next_cursor = items
        .last()
        .filter(|_| has_more)
        .map(|rule| rule.id.clone());
    Ok(Json(RulePage { items, next_cursor }))
}

#[utoipa::path(get, path = "/api/v1/notification-rules/{id}", params(("id" = String, Path, format = "uuid")),
    security(("adminBearer" = []), ("adminSession" = [])),
    responses((status = 200, body = Rule, headers(("ETag" = String))),
        (status = 401, body = ErrorBody), (status = 403, body = ErrorBody),
        (status = 404, body = ErrorBody), (status = 422, body = ErrorBody),
        (status = 503, body = ErrorBody)))]
pub(crate) async fn get_rule(
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
) -> Result<Response, ApiError> {
    let rule = store::get_rule(&s.db, uuid_path(path)?)
        .await?
        .ok_or(ApiError::Missing)?;
    Ok(rule_response(StatusCode::OK, rule))
}

#[utoipa::path(put, path = "/api/v1/notification-rules/{id}", request_body = RuleConfig,
    security(("adminBearer" = []), ("adminSession" = [])),
    params(("id" = String, Path, format = "uuid"),
        ("If-Match" = String, Header, description = "Quoted current revision; use \"0\" to create"),
        ("Origin" = Option<String>, Header, description = "Required with the session cookie: configured HTTPS origin or matching HTTP loopback origin. With a bearer, if sent, must match the configured HTTPS origin"),
        ("X-CSRF-Token" = Option<String>, Header, description = "Required with the session cookie")),
    responses((status = 200, body = Rule, headers(("ETag" = String))),
        (status = 201, body = Rule, headers(("ETag" = String))),
        (status = 401, body = ErrorBody), (status = 403, body = ErrorBody),
        (status = 409, body = ErrorBody), (status = 413, body = ErrorBody),
        (status = 422, body = ErrorBody), (status = 428, body = ErrorBody),
        (status = 429, body = ErrorBody), (status = 503, body = ErrorBody),
        (status = 504, body = ErrorBody)))]
pub(crate) async fn put_rule(
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    body: Result<Json<RuleConfig>, JsonRejection>,
) -> Result<Response, ApiError> {
    let id = uuid_path(path)?;
    let revision = revision(&headers)?;
    let config = match body {
        Ok(Json(config)) => config,
        Err(error) if error.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return Err(ApiError::TooLarge);
        }
        Err(_) => return Err(ApiError::Invalid),
    };
    let rule = store::put_rule(&s.db, id, revision, config).await?;
    let status = if revision == 0 {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok(rule_response(status, rule))
}

#[derive(Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct AlertsQuery {
    #[param(minimum = 1, maximum = 100, default = 20)]
    limit: Option<u16>,
    /// Decimal cursor returned by the preceding page.
    #[param(pattern = "^[1-9][0-9]*$")]
    cursor: Option<String>,
    #[serde(default)]
    #[param(required = false, default = false)]
    unacknowledged: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct AlertPage {
    items: Vec<Alert>,
    #[schema(pattern = "^[1-9][0-9]*$")]
    next_cursor: Option<String>,
}

#[utoipa::path(get, path = "/api/v1/alerts", params(AlertsQuery),
    security(("adminBearer" = []), ("adminSession" = [])),
    responses((status = 200, body = AlertPage), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody), (status = 422, body = ErrorBody),
        (status = 503, body = ErrorBody)))]
pub(crate) async fn list_alerts(
    State(s): State<AppState>,
    query: Result<Query<AlertsQuery>, QueryRejection>,
) -> Result<Json<AlertPage>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::Invalid)?;
    let limit = limit(query.limit)?;
    let cursor = query
        .cursor
        .map(|cursor| {
            parse_number(&cursor)
                .filter(|n| *n > 0)
                .ok_or(ApiError::Invalid)
        })
        .transpose()?;
    let mut items =
        store::list_alerts(&s.db, limit as i64 + 1, cursor, query.unacknowledged).await?;
    let has_more = items.len() > limit;
    items.truncate(limit);
    let next_cursor = items
        .last()
        .filter(|_| has_more)
        .map(|alert| alert.sequence.to_string());
    Ok(Json(AlertPage { items, next_cursor }))
}

#[utoipa::path(get, path = "/api/v1/alerts/{id}", params(("id" = String, Path, format = "uuid")),
    security(("adminBearer" = []), ("adminSession" = [])),
    responses((status = 200, body = Alert), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody), (status = 404, body = ErrorBody),
        (status = 422, body = ErrorBody), (status = 503, body = ErrorBody)))]
pub(crate) async fn get_alert(
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
) -> Result<Json<Alert>, ApiError> {
    Ok(Json(
        store::get_alert(&s.db, uuid_path(path)?)
            .await?
            .ok_or(ApiError::Missing)?,
    ))
}

#[utoipa::path(post, path = "/api/v1/alerts/{id}/ack",
    security(("adminBearer" = []), ("adminSession" = [])),
    params(("id" = String, Path, format = "uuid"),
        ("Origin" = Option<String>, Header, description = "Required with the session cookie: configured HTTPS origin or matching HTTP loopback origin. With a bearer, if sent, must match the configured HTTPS origin"),
        ("X-CSRF-Token" = Option<String>, Header, description = "Required with the session cookie")),
    responses((status = 200, body = Alert), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody), (status = 404, body = ErrorBody),
        (status = 413, body = ErrorBody), (status = 422, body = ErrorBody),
        (status = 429, body = ErrorBody), (status = 503, body = ErrorBody),
        (status = 504, body = ErrorBody)))]
pub(crate) async fn ack_alert(
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    RawQuery(query): RawQuery,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<Alert>, ApiError> {
    let id = uuid_path(path)?;
    require_empty(query, body)?;
    Ok(Json(
        store::ack_alert(&s.db, id)
            .await?
            .ok_or(ApiError::Missing)?,
    ))
}

#[utoipa::path(post, path = "/api/v1/notification-channels/{id}/tests",
    security(("adminBearer" = []), ("adminSession" = [])),
    params(("id" = String, Path, description = "Email channel ID: email"),
        ("Idempotency-Key" = String, Header, format = "uuid", description = "Client-generated UUID for this test"),
        ("Origin" = Option<String>, Header, description = "Required with the session cookie: configured HTTPS origin or matching HTTP loopback origin. With a bearer, if sent, must match the configured HTTPS origin"),
        ("X-CSRF-Token" = Option<String>, Header, description = "Required with the session cookie")),
    responses((status = 202, body = ChannelTest, headers(("Location" = String))),
        (status = 401, body = ErrorBody), (status = 403, body = ErrorBody),
        (status = 404, body = ErrorBody), (status = 413, body = ErrorBody),
        (status = 422, body = ErrorBody), (status = 429, body = ErrorBody),
        (status = 503, body = ErrorBody), (status = 504, body = ErrorBody)))]
pub(crate) async fn start_channel_test(
    State(s): State<AppState>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let Path(channel) = path.map_err(|_| ApiError::Invalid)?;
    require_email(&channel)?;
    require_empty(query, body)?;
    let mut keys = headers.get_all("idempotency-key").iter();
    let key = keys
        .next()
        .and_then(|key| key.to_str().ok())
        .filter(|_| keys.next().is_none())
        .ok_or(ApiError::Invalid)?;
    let test_id = parse_uuid(key)?;
    let test = store::enqueue_test(&s.db, test_id).await?;
    let location = format!("/api/v1/notification-channels/email/tests/{test_id}");
    Ok((
        StatusCode::ACCEPTED,
        [(header::LOCATION, location)],
        Json(test),
    )
        .into_response())
}

#[utoipa::path(get, path = "/api/v1/notification-channels/{id}/tests/{testId}",
    security(("adminBearer" = []), ("adminSession" = [])),
    params(("id" = String, Path, description = "Email channel ID: email"),
        ("testId" = String, Path, format = "uuid", description = "Original Idempotency-Key UUID")),
    responses((status = 200, body = ChannelTest), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody), (status = 404, body = ErrorBody),
        (status = 422, body = ErrorBody), (status = 503, body = ErrorBody)))]
pub(crate) async fn get_channel_test(
    State(s): State<AppState>,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<Json<ChannelTest>, ApiError> {
    let Path((channel, test_id)) = path.map_err(|_| ApiError::Invalid)?;
    require_email(&channel)?;
    Ok(Json(
        store::get_test(&s.db, parse_uuid(&test_id)?)
            .await?
            .ok_or(ApiError::Missing)?,
    ))
}
