use crate::{
    AppState, auth::WriteAccess, config::Settings, database::Database, finance::Finance, router,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderValue, Request, StatusCode, header, request::Builder},
    response::Response,
};
use serde_json::Value;
use std::{io::Write, path::Path, sync::Arc, time::Duration};
use tower::ServiceExt;
use uuid::Uuid;

const ORIGIN: &str = "https://web.example.test";

fn test_app() -> (Router, HeaderValue, tempfile::NamedTempFile) {
    let mut token_file = tempfile::NamedTempFile::new_in("/tmp").unwrap();
    let token = Uuid::new_v4();
    writeln!(token_file, "WEB_ADMIN_TOKEN={token}").unwrap();
    let timeout = Duration::from_secs(1);
    let config = Arc::new(Settings::load(Path::new("config"), |_| None).unwrap());
    let state = AppState {
        db: Database::new(&config.database.url, timeout, timeout).unwrap(),
        finance: Finance::new(&config.finance).unwrap(),
        write_access: WriteAccess::load(token_file.path(), 10, timeout)
            .unwrap()
            .with_sessions(Some(ORIGIN), Duration::from_secs(120))
            .unwrap(),
        config,
        credential_store: None,
        started_at: chrono::Utc::now(),
    };
    let mut authorization = HeaderValue::from_str(&format!("Bearer {token}")).unwrap();
    authorization.set_sensitive(true);
    (router(state), authorization, token_file)
}

async fn send(app: &Router, request: Builder, body: impl Into<Body>) -> Response {
    app.clone()
        .oneshot(request.body(body.into()).unwrap())
        .await
        .unwrap()
}

fn assert_no_store(response: &Response) {
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
}

async fn response_json(response: Response) -> Value {
    let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn market_gateway_blocks_anonymous_routes_and_cache_bypass() {
    let (app, _, _fixture) = test_app();
    for path in [
        "/api/v1/market/products",
        "/api/v1/market/wal",
        "/api/v1/market/projection/latest",
        "/api/v1/market/watermarks",
        "/api/v1/quality/finance-gaps",
        "/api/v1/market/instruments",
        "/api/v1/market/bars",
        "/api/v1/market/quotes",
        "/api/v1/market/book",
        "/api/v1/market/trades",
        "/api/v1/market/derivatives",
        "/api/v1/market/subscriptions",
        "/api/v1/indicators",
        "/api/v1/indicators/values",
        "/api/v1/options/chain",
        "/api/v1/options/flow",
        "/api/v1/options/exposure",
        "/api/v1/watchlists/default",
        "/api/v1/stream/market",
        "/api/v1/events",
    ] {
        for method in ["GET", "HEAD"] {
            let response = send(
                &app,
                Request::builder().method(method).uri(path),
                Body::empty(),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {path}"
            );
            assert_no_store(&response);
        }
    }
    for path in ["/api/v1/watchlists/default", "/api/v1/market/depth-focus"] {
        let response = send(&app, Request::put(path), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_no_store(&response);
    }
}

#[tokio::test]
async fn market_gateway_checks_focus_contract_and_throttles_authorized_reads() {
    let (app, admin, _fixture) = test_app();
    let focus = |request| {
        Request::put("/api/v1/market/depth-focus")
            .header(header::AUTHORIZATION, admin.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ORIGIN, request)
    };
    for (origin, body, expected) in [
        ("https://foreign.example.test", r#"{"action":"clear"}"#, 403),
        (ORIGIN, r#"{"action":"set","instrument_id":"alias"}"#, 400),
        (
            ORIGIN,
            r#"{"action":"set","instrument_id":"SYN-A.XTEST"}"#,
            503,
        ),
    ] {
        let response = send(&app, focus(origin), body).await;
        assert_eq!(response.status().as_u16(), expected);
        assert_no_store(&response);
        if expected == 503 {
            assert_eq!(
                response_json(response).await["error"]["code"],
                "finance_contract_pending"
            );
        }
    }
    let pending = "/api/v1/market/subscriptions";
    let cross_origin = Request::get(pending)
        .header(header::AUTHORIZATION, admin.clone())
        .header(header::ORIGIN, "https://foreign.example.test");
    assert_eq!(
        send(&app, cross_origin, Body::empty()).await.status(),
        StatusCode::FORBIDDEN
    );
    for _ in 0..120 {
        let request = Request::get(pending).header(header::AUTHORIZATION, admin.clone());
        assert_eq!(
            send(&app, request, Body::empty()).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
    let request = Request::get("/api/v1/options/chain?instrument_id=SYN-A.XTEST")
        .header(header::AUTHORIZATION, admin);
    let response = send(&app, request, Body::empty()).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(response.headers().contains_key(header::RETRY_AFTER));
    assert_no_store(&response);
    assert_eq!(
        response_json(response).await["error"]["message"],
        "Market read rate limit reached"
    );
}

#[tokio::test]
async fn market_gateway_checks_browser_origin_and_csrf_before_focus_and_watchlist_changes() {
    let (app, admin, _fixture) = test_app();
    let session = send(
        &app,
        Request::post("/api/v1/admin/session")
            .header(header::AUTHORIZATION, admin)
            .header(header::ORIGIN, ORIGIN),
        Body::empty(),
    )
    .await;
    assert_eq!(session.status(), StatusCode::CREATED);
    let cookie = session.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split_once(';')
        .unwrap()
        .0
        .to_owned();
    let session = response_json(session).await;
    let csrf = session["csrf_token"].as_str().unwrap();
    let base = |path| {
        Request::put(path)
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ORIGIN, ORIGIN)
    };
    for path in ["/api/v1/watchlists/default", "/api/v1/market/depth-focus"] {
        let denied = send(&app, base(path), r#"{"action":"clear"}"#).await;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response_json(denied).await["error"]["code"],
            "csrf_rejected"
        );
    }
    let response = send(
        &app,
        base("/api/v1/market/depth-focus").header("x-csrf-token", csrf),
        r#"{"action":"clear"}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}
