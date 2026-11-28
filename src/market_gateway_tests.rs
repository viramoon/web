use crate::{
    AppState, auth::WriteAccess, config::Settings, database::Database, finance::Finance,
    market_preferences::MarketPreferences, router_with_market_preferences, test_postgres::Cluster,
    test_subscription_demand::DemandStub,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderValue, Request, StatusCode, header, request::Builder},
    response::Response,
};
use serde_json::{Value, json};
use std::{io::Write, path::Path, sync::Arc, time::Duration};
use tower::ServiceExt;
use uuid::Uuid;

const ORIGIN: &str = "https://web.example.test";
const MAX_BODY_BYTES: usize = 512;

fn test_app() -> (
    Router,
    HeaderValue,
    tempfile::NamedTempFile,
    Arc<MarketPreferences>,
) {
    test_app_with_origin(Some(ORIGIN))
}

fn test_app_with_origin(
    public_origin: Option<&str>,
) -> (
    Router,
    HeaderValue,
    tempfile::NamedTempFile,
    Arc<MarketPreferences>,
) {
    build_test_app(public_origin, None, None)
}

async fn test_app_with_demand() -> (
    Router,
    HeaderValue,
    (tempfile::NamedTempFile, Cluster, DemandStub),
    Arc<MarketPreferences>,
) {
    let cluster = Cluster::create();
    let timeout = Duration::from_secs(1);
    let db = Database::new(&cluster.url(), timeout, timeout).unwrap();
    db.migrate().await.unwrap();
    let finance = DemandStub::enabled().await;
    let (app, admin, fixture, preferences) =
        build_test_app(Some(ORIGIN), Some(db), Some(&finance.url));
    (app, admin, (fixture, cluster, finance), preferences)
}

fn build_test_app(
    public_origin: Option<&str>,
    db: Option<Database>,
    finance_url: Option<&str>,
) -> (
    Router,
    HeaderValue,
    tempfile::NamedTempFile,
    Arc<MarketPreferences>,
) {
    let mut token_file = tempfile::NamedTempFile::new_in("/tmp").unwrap();
    let token = Uuid::new_v4();
    writeln!(token_file, "WEB_ADMIN_TOKEN={token}").unwrap();
    let timeout = Duration::from_secs(1);
    let mut config = Settings::load(Path::new("config"), |_| None).unwrap();
    config.server.max_body_bytes = MAX_BODY_BYTES;
    config.database.url = format!(
        "postgresql://localhost/market_gateway_test?host={}",
        token_file.path().display()
    );
    if let Some(finance_url) = finance_url {
        config.finance.base_url = finance_url.into();
    }
    let config = Arc::new(config);
    let state = AppState {
        db: db.unwrap_or_else(|| Database::new(&config.database.url, timeout, timeout).unwrap()),
        finance: Finance::new(&config.finance).unwrap(),
        write_access: WriteAccess::load(token_file.path(), 10, timeout)
            .unwrap()
            .with_sessions(public_origin, Duration::from_secs(120))
            .unwrap(),
        config,
        credential_store: None,
        started_at: chrono::Utc::now(),
    };
    let mut authorization = HeaderValue::from_str(&format!("Bearer {token}")).unwrap();
    authorization.set_sensitive(true);
    let preferences = Arc::new(MarketPreferences::default());
    (
        router_with_market_preferences(state, Arc::clone(&preferences)),
        authorization,
        token_file,
        preferences,
    )
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
    let (app, _, _fixture, _) = test_app();
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
        "/api/v1/market/depth-focus",
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
    let (app, admin, _fixture, _) = test_app_with_demand().await;
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
            200,
        ),
    ] {
        let response = send(&app, focus(origin), body).await;
        assert_eq!(response.status().as_u16(), expected);
        assert_no_store(&response);
        if expected == 200 {
            assert_eq!(
                response_json(response).await,
                json!({"instrument_id":"SYN-A.XTEST"})
            );
        }
    }
    let subscriptions = "/api/v1/market/subscriptions";
    let cross_origin = Request::get(subscriptions)
        .header(header::AUTHORIZATION, admin.clone())
        .header(header::ORIGIN, "https://foreign.example.test");
    assert_eq!(
        send(&app, cross_origin, Body::empty()).await.status(),
        StatusCode::FORBIDDEN
    );
    for _ in 0..120 {
        let request = Request::get(subscriptions).header(header::AUTHORIZATION, admin.clone());
        assert_eq!(
            send(&app, request, Body::empty()).await.status(),
            StatusCode::OK
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
    let (app, admin, _fixture, _) = test_app_with_demand().await;
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
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response_json(response).await,
        json!({"instrument_id": null})
    );
}

#[tokio::test]
async fn market_gateway_persists_focus_in_process_and_reports_shared_subscription_quota() {
    let (app, admin, _fixture, preferences) = test_app_with_demand().await;
    let focus = "/api/v1/market/depth-focus";
    let read = |path| Request::get(path).header(header::AUTHORIZATION, admin.clone());
    let change = || {
        Request::put(focus)
            .header(header::AUTHORIZATION, admin.clone())
            .header(header::CONTENT_TYPE, "application/json")
    };
    for (command, expected) in [
        (
            r#"{"action":"set","instrument_id":"SYN-B.XTEST"}"#,
            json!("SYN-B.XTEST"),
        ),
        (r#"{"action":"clear"}"#, Value::Null),
    ] {
        let updated = send(&app, change(), command).await;
        assert_eq!(updated.status(), StatusCode::OK);
        assert_eq!(response_json(updated).await["instrument_id"], expected);
        let readback = send(&app, read(focus), Body::empty()).await;
        assert_eq!(readback.status(), StatusCode::OK);
        assert_eq!(response_json(readback).await["instrument_id"], expected);
    }

    let subscriptions = "/api/v1/market/subscriptions";
    let quota = response_json(send(&app, read(subscriptions), Body::empty()).await).await;
    assert_eq!(
        quota,
        json!({"web": {"limit": 8, "available": 8, "active": 0},
        "finance_lines": "finance_contract_pending"})
    );
    let mut leases: Vec<_> = (0..8).map(|_| preferences.acquire().unwrap()).collect();
    assert!(preferences.acquire().is_none());
    let saturated = response_json(send(&app, read(subscriptions), Body::empty()).await).await;
    assert_eq!(
        saturated["web"],
        json!({"limit": 8, "available": 0, "active": 8})
    );
    drop(leases.pop());
    let released = response_json(send(&app, read(subscriptions), Body::empty()).await).await;
    assert_eq!(
        released["web"],
        json!({"limit": 8, "available": 1, "active": 7})
    );
    assert!(preferences.acquire().is_some());
}

#[tokio::test]
async fn market_gateway_documents_and_returns_413_for_oversize_focus_and_watchlists() {
    let (app, admin, _fixture, _) = test_app();
    let spec = crate::openapi();
    for (url, doc_path) in [
        ("/api/v1/watchlists/default", "/api/v1/watchlists/{id}"),
        ("/api/v1/market/depth-focus", "/api/v1/market/depth-focus"),
    ] {
        let response = send(
            &app,
            Request::put(url)
                .header(header::AUTHORIZATION, admin.clone())
                .header(header::CONTENT_TYPE, "application/json"),
            " ".repeat(MAX_BODY_BYTES + 1),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_no_store(&response);
        assert_eq!(
            response_json(response).await["error"]["code"],
            "body_too_large"
        );
        assert_eq!(
            spec["paths"][doc_path]["put"]["responses"]["413"]["content"]["application/json"]["schema"]
                ["$ref"],
            "#/components/schemas/ErrorBody"
        );
    }
}

#[test]
fn market_demand_openapi_documents_state_header_and_error_envelopes() {
    let spec = crate::openapi();
    for path in ["/api/v1/watchlists/{id}", "/api/v1/market/depth-focus"] {
        let put = &spec["paths"][path]["put"];
        assert_eq!(
            put["security"],
            json!([{"adminSession": []}, {"adminBearer": []}])
        );
        let responses = &put["responses"];
        assert_eq!(
            responses["200"]["headers"]["X-Finance-Subscription-State"]["schema"]["type"],
            "string"
        );
        for status in ["400", "401", "502", "503", "504"] {
            assert_eq!(
                responses[status]["content"]["application/json"]["schema"]["$ref"],
                "#/components/schemas/ErrorBody"
            );
        }
    }
}

#[tokio::test]
async fn loopback_cookie_rest_requires_same_origin_and_csrf() {
    const ADMIN_SESSION: &str = "/api/v1/admin/session";
    const CATALOG: &str = "/api/v1/integrations/catalog";
    const CAPABILITIES: &str = "/api/v1/me/capabilities";
    const INTEGRATIONS: &str = "/api/v1/integrations";
    const CSRF_HEADER: &str = "x-csrf-token";
    let (app, admin, _fixture, _) = test_app_with_origin(None);
    let host = "localhost:8090";
    let origin = "http://localhost:8090";
    for (host, rejected_origin) in [
        (None, origin),
        (Some(host), "https://localhost:8090"),
        (Some(host), "http://localhost:8091"),
        (Some(host), "http://user@localhost:8090"),
        (Some("localhost.evil:8090"), "http://localhost.evil:8090"),
    ] {
        let mut request = Request::post(ADMIN_SESSION)
            .header(header::AUTHORIZATION, &admin)
            .header(header::ORIGIN, rejected_origin)
            .header("x-forwarded-host", "localhost:8090");
        if let Some(host) = host {
            request = request.header(header::HOST, host);
        }
        let response = send(&app, request, Body::empty()).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response_json(response).await["error"]["code"],
            "origin_rejected"
        );
    }

    let session = send(
        &app,
        Request::post(ADMIN_SESSION)
            .header(header::AUTHORIZATION, &admin)
            .header(header::HOST, host)
            .header(header::ORIGIN, origin),
        Body::empty(),
    )
    .await;
    assert_eq!(session.status(), StatusCode::CREATED);
    let set_cookie = session.headers()[header::SET_COOKIE].to_str().unwrap();
    for attribute in ["Secure", "HttpOnly", "SameSite=Strict", "Path=/"] {
        assert!(set_cookie.split("; ").any(|part| part == attribute));
    }
    let cookie = set_cookie.split(';').next().unwrap().to_owned();
    let session = response_json(session).await;
    let csrf = session["csrf_token"].as_str().unwrap();

    for path in [CATALOG, "/api/v1/market/subscriptions", CAPABILITIES] {
        let response = send(
            &app,
            Request::get(path)
                .header(header::COOKIE, &cookie)
                .header(header::HOST, host)
                .header("sec-fetch-site", "same-origin"),
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        if path == CAPABILITIES {
            assert_eq!(response_json(response).await["csrf_token"], csrf);
        }
    }
    for path in [CATALOG, "/api/v1/market/subscriptions"] {
        let response = send(
            &app,
            Request::get(path)
                .header(header::COOKIE, &cookie)
                .header(header::HOST, host)
                .header(header::ORIGIN, "http://127.0.0.1:8090"),
            Body::empty(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
        assert_eq!(
            response_json(response).await["error"]["code"],
            "origin_rejected"
        );
    }

    for (request_origin, token, reason) in [
        (None, Some(csrf), "origin_rejected"),
        (Some("http://localhost:8091"), Some(csrf), "origin_rejected"),
        (Some(origin), None, "csrf_rejected"),
        (Some(origin), Some("wrong"), "csrf_rejected"),
    ] {
        let mut request = Request::post(INTEGRATIONS)
            .header(header::HOST, host)
            .header(header::COOKIE, &cookie)
            .header("sec-fetch-site", "same-origin")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(origin) = request_origin {
            request = request.header(header::ORIGIN, origin);
        }
        if let Some(token) = token {
            request = request.header(CSRF_HEADER, token);
        }
        let response = send(&app, request, "{").await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response_json(response).await["error"]["code"], reason);
    }
    let response = send(
        &app,
        Request::post(INTEGRATIONS)
            .header(header::HOST, host)
            .header(header::ORIGIN, origin)
            .header(header::COOKIE, &cookie)
            .header(CSRF_HEADER, csrf)
            .header(header::CONTENT_TYPE, "application/json"),
        "{",
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let response = send(
        &app,
        Request::delete(ADMIN_SESSION)
            .header(header::HOST, host)
            .header(header::ORIGIN, origin)
            .header(header::COOKIE, &cookie)
            .header(CSRF_HEADER, csrf),
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}
