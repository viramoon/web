pub mod auth;
mod backtests;
mod circuit;
pub mod config;
pub mod credentials;
pub mod database;
mod events;
pub mod finance;
mod integrations;
mod market;
#[cfg(test)]
mod market_gateway_tests;
mod market_pending;
pub mod market_preferences;
mod market_reads;
pub mod market_stream;
mod market_types;
pub mod notifications;
mod ops;
mod research;
mod spa;
pub mod telemetry;
#[cfg(test)]
mod w1_http_tests;
mod watchlists;

use axum::{
    Extension, Json, Router,
    extract::{MatchedPath, Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use config::Settings;
use database::{Database, DbStatus};
use finance::{Finance, FinanceStatus};
use integrations::StrictUpdateSchema;
use notifications::{
    http::{self as notification_http, silence as silence_http},
    worker,
};
use serde::Serialize;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tower_http::{catch_panic::CatchPanicLayer, limit::RequestBodyLimitLayer};
use tracing::Instrument;
use utoipa::{OpenApi, ToSchema};
use uuid::Uuid;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Settings>,
    pub db: Database,
    pub finance: Finance,
    pub write_access: auth::WriteAccess,
    pub credential_store: Option<credentials::CredentialStore>,
    pub started_at: DateTime<Utc>,
}

#[derive(Clone)]
pub(crate) struct StreamShutdown(pub(crate) tokio::sync::watch::Receiver<bool>);

#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Serialize, ToSchema)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
    /// JSON Pointer from the strategy compiler.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<ErrorLocation>,
}

impl ErrorDetail {
    fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            path: None,
            location: None,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct ErrorLocation {
    /// One-based line in the uploaded document.
    pub line: u64,
    /// One-based column in the uploaded document.
    pub column: u64,
}

pub fn api_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(ErrorBody {
            error: ErrorDetail::new(code, message),
        }),
    )
        .into_response()
}

#[derive(Serialize, ToSchema)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub required: bool,
}

#[derive(Serialize, ToSchema)]
pub struct Health {
    pub status: String,
    pub checks: Vec<Check>,
}

#[derive(Serialize, ToSchema)]
pub struct WebStatus {
    pub version: String,
    pub commit: String,
    pub started_at: DateTime<Utc>,
}

#[derive(Serialize, ToSchema)]
pub struct SystemStatus {
    pub web: WebStatus,
    pub database: DbStatus,
    pub finance: FinanceStatus,
    pub market: market::MarketSummary,
    pub notifications: worker::NotificationHealth,
}

#[utoipa::path(get, path="/healthz/live", responses((status=200, body=Health)))]
async fn live() -> Json<Health> {
    Json(Health {
        status: "ok".into(),
        checks: vec![Check {
            name: "process".into(),
            ok: true,
            required: true,
        }],
    })
}

#[utoipa::path(get, path="/healthz/ready", responses((status=200, body=Health),(status=503, body=Health)))]
async fn ready(State(s): State<AppState>) -> (StatusCode, Json<Health>) {
    let ((finance, _), static_ok) = tokio::join!(
        s.finance.probe(),
        spa::available(&s.config.paths.static_root)
    );
    let db = s.db.status();
    let checks = vec![
        Check {
            name: "database".into(),
            ok: db.ok,
            required: true,
        },
        Check {
            name: "static_index".into(),
            ok: static_ok,
            required: true,
        },
        Check {
            name: "finance".into(),
            ok: finance.ok,
            required: s.config.finance.required_for_ready,
        },
    ];
    let (code, status) = if checks.iter().any(|c| c.required && !c.ok) {
        (StatusCode::SERVICE_UNAVAILABLE, "not_ready")
    } else if !finance.ok {
        (StatusCode::OK, "degraded")
    } else {
        (StatusCode::OK, "ready")
    };
    (
        code,
        Json(Health {
            status: status.into(),
            checks,
        }),
    )
}

#[utoipa::path(get, path="/api/v1/system/status", responses((status=200, body=SystemStatus)))]
async fn status(State(s): State<AppState>) -> Json<SystemStatus> {
    let (mut finance, reachable) = s.finance.probe().await;
    let market = if reachable {
        market::read(&s.finance).await.ok()
    } else {
        None
    };
    finance.circuit = s.finance.circuit_state().into();
    let (database, notifications) = s.db.status_snapshot();
    Json(SystemStatus {
        web: WebStatus {
            version: env!("CARGO_PKG_VERSION").into(),
            commit: env!("WEB_GIT_SHA").into(),
            started_at: s.started_at,
        },
        database,
        finance,
        market: market::MarketSummary::from_status(market),
        notifications,
    })
}

#[utoipa::path(get, path = "/api/v1/backtests/sample",
    responses((status = 200, body = ref("#/components/schemas/BacktestSummary")),
        (status = 404, body = ErrorBody), (status = 502, body = ErrorBody),
        (status = 503, body = ErrorBody), (status = 504, body = ErrorBody)))]
async fn sample(
    State(s): State<AppState>,
) -> Result<Json<finance_client::models::BacktestSummary>, finance::Fault> {
    s.finance
        .read(|cfg| async move { finance_client::apis::default_api::sample(&cfg).await })
        .await
        .map(Json)
}

fn is_submission(req: &Request) -> bool {
    req.method() == axum::http::Method::POST && req.uri().path() == "/api/v1/backtests"
}

fn matched_path(req: &Request) -> Option<&str> {
    req.extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str)
}

fn is_integration_route(path: &str) -> bool {
    matches!(
        path,
        "/api/v1/integrations"
            | "/api/v1/integrations/catalog"
            | "/api/v1/integrations/{id}"
            | "/api/v1/integrations/{id}/credential"
            | "/api/v1/integrations/{id}/tests"
            | "/api/v1/integrations/{id}/tests/{testId}"
            | "/api/v1/integrations/{id}/activate"
            | "/api/v1/integrations/{id}/rollback"
            | "/api/v1/integration-tests/{id}"
    )
}

fn read_product(path: &str) -> Option<auth::Product> {
    use auth::Product;

    let product = match path {
        "/api/v1/system/status" => Product::System,
        "/api/v1/backtests/by-key" => Product::BacktestSubmission,
        "/api/v1/backtests"
        | "/api/v1/backtests/sample"
        | "/api/v1/backtests/datasets"
        | "/api/v1/backtests/{id}" => Product::Backtests,
        "/api/v1/market/status" => Product::MarketStatus,
        "/api/v1/integrations"
        | "/api/v1/integrations/catalog"
        | "/api/v1/integrations/{id}"
        | "/api/v1/integrations/{id}/tests/{testId}"
        | "/api/v1/integration-tests/{id}" => Product::Integrations,
        ops::BACKUP_STATUS => Product::Integrations,
        ops::QUALITY_GAPS => Product::RestrictedMarketData,
        notification_http::RULES
        | notification_http::RULE
        | notification_http::ALERTS
        | notification_http::ALERT
        | notification_http::CHANNEL_TEST
        | silence_http::RULE_SILENCE
        | silence_http::ALERT_SILENCE => Product::Notifications,
        _ => return None,
    };
    Some(product)
}

fn is_market_rest(path: &str) -> bool {
    market_pending::is_private(path)
        || market_reads::is_private(path)
        || matches!(
            path,
            watchlists::WATCHLIST
                | market_preferences::DEPTH_FOCUS
                | market_preferences::SUBSCRIPTIONS
        )
}

fn is_stream(path: &str) -> bool {
    matches!(path, market_stream::MARKET_STREAM | events::EVENTS)
}

async fn check_access(State(s): State<AppState>, req: Request, next: Next) -> Response {
    let path = matched_path(&req).unwrap_or_default();
    let headers = req.headers();
    let access = match (req.method(), path) {
        (&Method::GET | &Method::HEAD, path) if is_stream(path) => s
            .write_access
            .authorize_stream(headers, path == market_stream::MARKET_STREAM),
        (&Method::GET | &Method::HEAD, path) if is_market_rest(path) => {
            s.write_access.authorize_market_read(headers)
        }
        (&Method::GET | &Method::HEAD, path) if research::is_private(path) => {
            s.write_access.authorize_stream(headers, false)
        }
        (&Method::GET | &Method::HEAD, path) => read_product(path).map_or(Ok(()), |product| {
            s.write_access.check_read(headers, product)
        }),
        (&Method::POST, "/api/v1/backtests") => s.write_access.authorize(headers),
        (&Method::POST, research::STRATEGIES | research::VALIDATE) => {
            s.write_access.authorize_same_origin(headers)
        }
        (&Method::POST, "/api/v1/admin/session") => s.write_access.check_session_creation(headers),
        (&Method::DELETE, "/api/v1/admin/session") => {
            s.write_access.check_session_deletion(headers)
        }
        (
            &Method::POST,
            "/api/v1/integrations"
            | "/api/v1/integrations/{id}/tests"
            | "/api/v1/integrations/{id}/activate"
            | "/api/v1/integrations/{id}/rollback",
        )
        | (&Method::PATCH, "/api/v1/integrations/{id}")
        | (&Method::PUT | &Method::DELETE, "/api/v1/integrations/{id}/credential")
        | (&Method::PUT, watchlists::WATCHLIST | market_preferences::DEPTH_FOCUS)
        | (&Method::PUT, notification_http::RULE)
        | (
            &Method::PUT | &Method::DELETE,
            silence_http::RULE_SILENCE | silence_http::ALERT_SILENCE,
        )
        | (&Method::POST, notification_http::ACK | notification_http::CHANNEL_TESTS) => {
            s.write_access.authorize_same_origin(headers)
        }
        _ => Ok(()),
    };
    if let Err(denied) = access {
        return denied.into_response();
    }
    next.run(req).await
}

async fn request_controls(State(s): State<AppState>, mut req: Request, next: Next) -> Response {
    let id = req
        .headers()
        .get("x-request-id")
        .and_then(|h| h.to_str().ok())
        .and_then(|id| Uuid::parse_str(id).ok())
        .unwrap_or_else(Uuid::new_v4);
    let path = matched_path(&req).unwrap_or("<unmatched>");
    let write_timeout_hint = match (req.method(), path) {
        (&Method::POST, research::STRATEGIES) => Some(research::REGISTER_TIMEOUT_HINT),
        (&Method::PUT, watchlists::WATCHLIST) => {
            Some("Watchlist update outcome unknown. Read the current revision before retrying")
        }
        (&Method::PUT, market_preferences::DEPTH_FOCUS) => {
            Some("Focus update outcome unknown. Read the current focus before retrying")
        }
        (&Method::PUT, notification_http::RULE) => {
            Some("Rule update outcome unknown; read the rule revision before retrying")
        }
        (&Method::POST, notification_http::ACK) => {
            Some("Acknowledgment outcome unknown; read the alert before retrying")
        }
        (
            &Method::PUT | &Method::DELETE,
            silence_http::RULE_SILENCE | silence_http::ALERT_SILENCE,
        ) => Some("Silence outcome unknown; read the silence resource before retrying"),
        (&Method::POST, notification_http::CHANNEL_TESTS) => {
            Some("Test outcome unknown; look up the original Idempotency-Key before retrying")
        }
        (&Method::POST, "/api/v1/integrations") => {
            Some("Creation outcome unknown. Read the integrations list before retrying")
        }
        (&Method::PATCH, "/api/v1/integrations/{id}")
        | (&Method::PUT | &Method::DELETE, "/api/v1/integrations/{id}/credential") => {
            Some("Update outcome unknown. Read the integration revision before retrying")
        }
        (&Method::POST, "/api/v1/integrations/{id}/tests") => Some(
            "Test outcome unknown. Read the integration revision and latest test before retrying",
        ),
        (
            &Method::POST,
            "/api/v1/integrations/{id}/activate" | "/api/v1/integrations/{id}/rollback",
        ) => Some(
            "Update outcome unknown. Read the integration revision and active version before retrying",
        ),
        _ => None,
    };
    let no_store = is_integration_route(path)
        || ops::is_private(path)
        || notification_http::is_private(path)
        || is_market_rest(path)
        || is_stream(path)
        || research::is_private(path)
        || matches!(
            path,
            "/api/v1/backtests"
                | "/api/v1/backtests/{id}"
                | "/api/v1/backtests/by-key"
                | "/api/v1/admin/session"
                | "/api/v1/me/capabilities"
        );
    let method = match *req.method() {
        Method::GET
        | Method::HEAD
        | Method::POST
        | Method::PUT
        | Method::PATCH
        | Method::DELETE
        | Method::OPTIONS
        | Method::CONNECT
        | Method::TRACE => req.method().as_str(),
        _ => "OTHER",
    };
    let span = tracing::info_span!("http", request_id = %id, method, path);
    let id = HeaderValue::from_str(&id.to_string()).expect("UUID is valid ASCII");
    req.headers_mut().insert("x-request-id", id.clone());
    async move {
        let start = Instant::now();
        let timeout = Duration::from_millis(s.config.server.request_timeout_ms);
        let submission = is_submission(&req);
        let mut response = match tokio::time::timeout(timeout, next.run(req)).await {
            Ok(r) => r,
            Err(_) if submission => api_error(
                StatusCode::GATEWAY_TIMEOUT,
                "finance_timeout",
                "Submission outcome unknown; query by Idempotency-Key before retrying",
            ),
            Err(_) => api_error(
                StatusCode::GATEWAY_TIMEOUT,
                "request_timeout",
                write_timeout_hint.unwrap_or("Request deadline exceeded"),
            ),
        };
        if response.status() == StatusCode::PAYLOAD_TOO_LARGE {
            response = api_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                "Request body is too large",
            );
        }
        tracing::info!(
            status = response.status().as_u16(),
            latency_ms = start.elapsed().as_millis() as u64,
            "request complete"
        );
        response.headers_mut().insert("x-request-id", id);
        if no_store {
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        }
        response
    }
    .instrument(span)
    .await
}

pub fn router(s: AppState) -> Router {
    router_with_market_preferences(
        s,
        Arc::new(market_preferences::MarketPreferences::default()),
    )
}

fn router_with_market_preferences(
    s: AppState,
    market_preferences: Arc<market_preferences::MarketPreferences>,
) -> Router {
    let routes = Router::new()
        .route("/healthz/live", get(live))
        .route("/healthz/ready", get(ready))
        .route("/api/v1/system/status", get(status))
        .route("/api/v1/backtests/sample", get(sample))
        .route(
            "/api/v1/backtests",
            get(backtests::list).post(backtests::submit),
        )
        .route("/api/v1/backtests/{id}", get(backtests::detail))
        .route("/api/v1/backtests/by-key", get(backtests::by_key))
        .route("/api/v1/backtests/datasets", get(backtests::datasets))
        .merge(research::routes())
        .route("/api/v1/market/status", get(market::status))
        .route(
            "/api/v1/admin/session",
            post(auth::create_session).delete(auth::delete_session),
        )
        .route("/api/v1/me/capabilities", get(auth::capabilities))
        .merge(integrations::routes())
        .merge(notification_http::routes())
        .merge(ops::routes())
        .merge(watchlists::routes())
        .merge(market_pending::routes())
        .merge(market_preferences::routes())
        .merge(market_reads::routes())
        .merge(market_stream::routes())
        .merge(events::routes())
        .fallback(spa::serve)
        .layer(Extension(market_preferences));
    with_controls(routes, s)
}

pub fn router_with_shutdown(s: AppState, shutdown: tokio::sync::watch::Receiver<bool>) -> Router {
    router(s).layer(Extension(StreamShutdown(shutdown)))
}

fn with_controls(routes: Router<AppState>, s: AppState) -> Router {
    routes
        .layer(RequestBodyLimitLayer::new(s.config.server.max_body_bytes))
        .layer(middleware::from_fn_with_state(s.clone(), check_access))
        .layer(CatchPanicLayer::custom(|_| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "Internal server error",
            )
        }))
        .layer(middleware::from_fn_with_state(s.clone(), request_controls))
        .with_state(s)
}

#[derive(OpenApi)]
#[openapi(
    paths(
        live,
        ready,
        status,
        sample,
        backtests::submit,
        backtests::detail,
        backtests::list,
        backtests::by_key,
        backtests::datasets,
        market::status,
        auth::create_session,
        auth::delete_session,
        auth::capabilities,
        integrations::catalog,
        integrations::list,
        integrations::get_integration,
        integrations::create,
        integrations::update,
        integrations::put_credential,
        integrations::delete_credential,
        integrations::start_test,
        integrations::get_test,
        integrations::get_scoped_test,
        integrations::activate,
        integrations::rollback,
        ops::backup_status,
        ops::quality_gaps,
        notification_http::list_rules,
        notification_http::get_rule,
        notification_http::put_rule,
        notification_http::list_alerts,
        notification_http::get_alert,
        notification_http::ack_alert,
        notification_http::start_channel_test,
        notification_http::get_channel_test,
        silence_http::get_rule,
        silence_http::put_rule,
        silence_http::delete_rule,
        silence_http::get_alert,
        silence_http::put_alert,
        silence_http::delete_alert
    ),
    components(schemas(
        ErrorBody,
        ErrorDetail,
        ErrorLocation,
        Check,
        Health,
        WebStatus,
        SystemStatus,
        DbStatus,
        FinanceStatus,
        market::MarketSummary,
        worker::NotificationHealth
    )),
    modifiers(&StrictUpdateSchema)
)]
pub struct WebApi;

pub fn openapi() -> serde_json::Value {
    let mut spec = WebApi::openapi();
    spec.merge(watchlists::WatchlistApi::openapi());
    spec.merge(market_pending::MarketPendingApi::openapi());
    spec.merge(market_preferences::MarketPreferencesApi::openapi());
    spec.merge(market_stream::MarketStreamApi::openapi());
    spec.merge(events::EventsApi::openapi());
    // The repository has no declared license. OpenAPI requires a name if the object exists.
    if spec
        .info
        .license
        .as_ref()
        .is_some_and(|license| license.name.is_empty())
    {
        spec.info.license = None;
    }
    let components = spec.components.get_or_insert_with(Default::default);
    use utoipa::openapi::security::{ApiKey, ApiKeyValue, Http, HttpAuthScheme, SecurityScheme};
    components.add_security_scheme(
        "adminBearer",
        SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
    );
    components.add_security_scheme(
        "adminSession",
        SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::new(auth::SESSION_COOKIE_NAME))),
    );
    let mut spec = serde_json::to_value(spec).expect("web OpenAPI is serializable");
    // Retain JSON Schema keywords such as const and unevaluatedProperties on
    // upstream F1 components. Utoipa's schema abstraction cannot round-trip them.
    let upstream: serde_json::Value =
        serde_json::from_str(include_str!("../openapi/finance.openapi.json"))
            .expect("checked-in finance OpenAPI is valid");
    market_reads::register_openapi(&mut spec, &upstream);
    research::register_openapi(&mut spec, &upstream);
    let schemas = upstream["components"]["schemas"]
        .as_object()
        .expect("finance schemas");
    let web_schemas = spec["components"]["schemas"]
        .as_object_mut()
        .expect("web schemas");
    for name in [
        "BacktestSummary",
        "Accepted",
        "AcceptedSource",
        "BundleRequestView",
        "BundleStrategy",
        "JobRequest",
        "JobRequestView",
        "StrategyParams",
        "Job",
        "JobStatus",
        "Page",
        "MarketStatus",
        "InstrumentStatus",
        "Dataset",
        "DatasetList",
        "ResearchReport",
        "Metrics",
        "Window",
        "WindowMetrics",
        "Coverage",
        "ExecutionAudit",
        "Fill",
        "BlockedOrder",
    ] {
        web_schemas.insert(name.into(), schemas[name].clone());
    }
    backtests::register_openapi(&mut spec);
    spec
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
        routing::post,
    };
    use std::path::Path;
    use tokio::sync::Notify;
    use tower::ServiceExt;

    #[tokio::test]
    async fn system_status_returns_while_a_half_open_finance_trial_is_held_and_db_is_unavailable() {
        let fixture = tempfile::tempdir_in("/tmp").unwrap();
        let mut cfg = Settings::load(Path::new("config"), |_| None).unwrap();
        cfg.finance.base_url = "http://127.0.0.1:1".into();
        cfg.finance.circuit_failure_threshold = 1;
        cfg.finance.circuit_open_ms = 0;
        cfg.finance.max_retries = 0;
        cfg.finance.request_timeout_ms = 1_500;
        let finance = Finance::new(&cfg.finance).unwrap();
        let app = router(AppState {
            finance: finance.clone(),
            write_access: auth::WriteAccess::load(
                &fixture.path().join("unused-token"),
                10,
                Duration::from_secs(1),
            )
            .unwrap(),
            credential_store: None,
            db: Database::new(
                &format!(
                    "postgresql://localhost/test?host={}",
                    fixture.path().display()
                ),
                Duration::from_secs(2),
                Duration::from_secs(2),
            )
            .unwrap()
            .with_stalled_connections(),
            config: Arc::new(cfg),
            started_at: Utc::now(),
        });

        let opened = finance
            .read::<(), (), _, _>(|_| async {
                let invalid = serde_json::from_str::<u64>("fixture").unwrap_err();
                Err(finance_client::apis::Error::Serde(invalid))
            })
            .await;
        assert!(opened.is_err());
        assert_eq!(finance.circuit_state(), "open");
        let ready = Notify::new();
        let release = Notify::new();
        let trial = finance.read(|_| async {
            ready.notify_one();
            release.notified().await;
            Ok::<(), finance_client::apis::Error<()>>(())
        });
        tokio::pin!(trial);
        tokio::select! {
            _ = ready.notified() => {}
            _ = &mut trial => panic!("fixture trial completed before it was held"),
        }

        let request = Request::get("/api/v1/system/status")
            .body(Body::empty())
            .unwrap();
        let response = tokio::select! {
            response = tokio::time::timeout(Duration::from_millis(500), app.oneshot(request)) => {
                response.expect("system status must return while the trial is held").unwrap()
            }
            _ = &mut trial => panic!("system status waited for the finance trial to finish"),
        };
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["finance"]["circuit"], "half_open");
        assert_eq!(body["finance"]["ok"], false);
        assert_eq!(body["database"]["ok"], false);
        assert_eq!(body["notifications"]["state"], "unavailable");
        release.notify_one();
        assert!(trial.await.is_ok());
    }

    #[tokio::test]
    async fn bounds_panics_deadlines_and_payloads_without_losing_request_id() {
        let mut cfg = Settings::load(Path::new("config"), |_| None).unwrap();
        cfg.server.max_body_bytes = 4;
        cfg.server.request_timeout_ms = 10;
        let state = AppState {
            finance: Finance::new(&cfg.finance).unwrap(),
            write_access: auth::WriteAccess::load(
                &std::env::temp_dir().join(Uuid::new_v4().to_string()),
                10,
                Duration::from_secs(1),
            )
            .unwrap(),
            credential_store: None,
            db: Database::new(
                &cfg.database.url,
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .unwrap(),
            config: Arc::new(cfg),
            started_at: Utc::now(),
        };
        async fn panic_handler() {
            panic!("test panic");
        }
        let routes = Router::new()
            .route("/panic", post(panic_handler))
            .route(
                "/slow",
                post(|| async { std::future::pending::<()>().await }),
            )
            .route("/echo", post(|body: String| async { body }));
        let app = with_controls(routes, state);
        for (path, body, status, code) in [
            ("/panic", "", 500, "internal_error"),
            ("/slow", "", 504, "request_timeout"),
            ("/echo", "12345", 413, "body_too_large"),
        ] {
            let request = Request::post(path).body(Body::from(body)).unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status().as_u16(), status);
            Uuid::parse_str(response.headers()["x-request-id"].to_str().unwrap()).unwrap();
            let bytes = to_bytes(response.into_body(), 1000).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["error"]["code"], code);
        }
    }
}
