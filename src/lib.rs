pub mod auth;
mod backtests;
mod circuit;
pub mod config;
pub mod database;
pub mod finance;
mod market;
mod spa;

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, Utc};
use config::Settings;
use database::{Database, DbStatus};
use finance::{Finance, FinanceStatus};
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
    pub started_at: DateTime<Utc>,
}

#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Serialize, ToSchema)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
}

pub fn api_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(ErrorBody {
            error: ErrorDetail {
                code: code.into(),
                message: message.into(),
            },
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
    Json(SystemStatus {
        web: WebStatus {
            version: env!("CARGO_PKG_VERSION").into(),
            commit: env!("WEB_GIT_SHA").into(),
            started_at: s.started_at,
        },
        database: s.db.status(),
        finance,
        market: market::MarketSummary::from_status(market),
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

fn is_key_lookup(req: &Request) -> bool {
    req.method() == axum::http::Method::GET && req.uri().path() == "/api/v1/backtests/by-key"
}

async fn check_write_access(State(s): State<AppState>, req: Request, next: Next) -> Response {
    let access = if is_submission(&req) {
        s.write_access.authorize(req.headers())
    } else if is_key_lookup(&req) {
        s.write_access.authenticate(req.headers())
    } else {
        Ok(())
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
    let span = tracing::info_span!("http", request_id = %id, method = %req.method(), path = %req.uri().path());
    let id = HeaderValue::from_str(&id.to_string()).expect("UUID is valid ASCII");
    req.headers_mut().insert("x-request-id", id.clone());
    async move {
        let start = Instant::now();
        let timeout = Duration::from_millis(s.config.server.request_timeout_ms);
        let submission = is_submission(&req);
        let key_lookup = is_key_lookup(&req);
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
                "Request deadline exceeded",
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
        if key_lookup {
            response.headers_mut().insert(
                axum::http::header::CACHE_CONTROL,
                HeaderValue::from_static("no-store"),
            );
        }
        response
    }
    .instrument(span)
    .await
}

pub fn router(s: AppState) -> Router {
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
        .route("/api/v1/market/status", get(market::status))
        .fallback(spa::serve);
    with_controls(routes, s)
}

fn with_controls(routes: Router<AppState>, s: AppState) -> Router {
    routes
        .layer(RequestBodyLimitLayer::new(s.config.server.max_body_bytes))
        .layer(middleware::from_fn_with_state(
            s.clone(),
            check_write_access,
        ))
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
        market::status
    ),
    components(schemas(
        ErrorBody,
        ErrorDetail,
        Check,
        Health,
        WebStatus,
        SystemStatus,
        DbStatus,
        FinanceStatus,
        market::MarketSummary
    ))
)]
pub struct WebApi;

pub fn openapi() -> utoipa::openapi::OpenApi {
    let mut spec = WebApi::openapi();
    // The repository has no declared license. OpenAPI requires a name if the object exists.
    if spec
        .info
        .license
        .as_ref()
        .is_some_and(|license| license.name.is_empty())
    {
        spec.info.license = None;
    }
    // Response models from the generated client have the same wire schema as finance.
    let upstream: utoipa::openapi::OpenApi =
        serde_json::from_str(include_str!("../openapi/finance.openapi.json"))
            .expect("checked-in finance OpenAPI is valid");
    let schemas = upstream
        .components
        .expect("finance declares model schemas")
        .schemas;
    let components = spec.components.get_or_insert_with(Default::default);
    for name in [
        "BacktestSummary",
        "Accepted",
        "JobRequest",
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
        "Coverage",
        "ExecutionAudit",
        "Fill",
        "BlockedOrder",
    ] {
        components
            .schemas
            .insert(name.into(), schemas[name].clone());
    }
    use utoipa::openapi::security::{Http, HttpAuthScheme, SecurityScheme};
    components.add_security_scheme(
        "adminBearer",
        SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
    );
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
    use tower::ServiceExt;

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
