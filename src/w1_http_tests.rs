use crate::{AppState, auth, config::Settings, database::Database, finance::Finance, router};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderValue, Request, StatusCode, header, request::Builder},
    response::Response,
};
use chrono::Utc;
use serde_json::{Value, json};
use std::{
    fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt, path::Path, sync::Arc, time::Duration,
};
use tower::ServiceExt;
use uuid::Uuid;

const ORIGIN: &str = "https://web.example.test";
const ADMIN_SESSION: &str = "/api/v1/admin/session";
const CAPABILITIES: &str = "/api/v1/me/capabilities";
const CATALOG: &str = "/api/v1/integrations/catalog";
const INTEGRATIONS: &str = "/api/v1/integrations";
const ALERTS: &str = "/api/v1/alerts";
const RULES: &str = "/api/v1/notification-rules";
const EMAIL_TESTS: &str = "/api/v1/notification-channels/email/tests";
const CSRF_HEADER: &str = "x-csrf-token";
const MAX_BODY_BYTES: usize = 512;

fn test_app() -> (Router, HeaderValue, tempfile::TempDir) {
    app_with_deadline(None)
}

fn app_with_deadline(deadline_ms: Option<u64>) -> (Router, HeaderValue, tempfile::TempDir) {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let token_path = directory.path().join("web.env");
    let token = Uuid::new_v4().to_string();
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&token_path)
        .unwrap()
        .write_all(format!("WEB_ADMIN_TOKEN={token}").as_bytes())
        .unwrap();
    let mut config = Settings::load(Path::new("config"), |key| match key {
        "WEB_ADMIN_TOKEN_FILE" => Some(token_path.to_str().unwrap().into()),
        "WEB_PUBLIC_ORIGIN" => Some(ORIGIN.into()),
        "WEB_ADMIN_SESSION_TTL_SECONDS" => Some("120".into()),
        _ => None,
    })
    .unwrap();
    config.server.max_body_bytes = MAX_BODY_BYTES;
    if let Some(deadline_ms) = deadline_ms {
        config.server.request_timeout_ms = deadline_ms;
    }
    config.paths.static_root = directory.path().join("static");
    std::fs::create_dir(&config.paths.static_root).unwrap();
    std::fs::write(
        config.paths.static_root.join("index.html"),
        "<main>web</main>",
    )
    .unwrap();
    config.database.url = format!(
        "postgresql://localhost/web_w1_test?host={}",
        directory.path().display()
    );
    let write_access = auth::WriteAccess::load(
        &config.security.admin_token_file,
        config.security.max_requests,
        Duration::from_millis(config.security.window_ms),
    )
    .unwrap()
    .with_sessions(
        config.security.public_origin.as_deref(),
        Duration::from_secs(config.security.session_ttl_seconds),
    )
    .unwrap();
    let db = Database::new(
        &config.database.url,
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .unwrap();
    let db = if deadline_ms.is_some() {
        db.with_stalled_connections()
    } else {
        db
    };
    let app = router(AppState {
        db,
        finance: Finance::new(&config.finance).unwrap(),
        config: Arc::new(config),
        write_access,
        credential_store: None,
        started_at: Utc::now(),
    });
    let mut authorization = HeaderValue::from_str(&format!("Bearer {token}")).unwrap();
    authorization.set_sensitive(true);
    (app, authorization, directory)
}

async fn send(app: &Router, builder: Builder, body: impl Into<Body>) -> Response {
    let request = builder.body(body.into()).unwrap();
    app.clone().oneshot(request).await.unwrap()
}

fn assert_no_store(response: &Response) {
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
}

async fn response_json(response: Response) -> Value {
    let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn restricted_catalog_and_capabilities_distinguish_anonymous_and_admin() {
    let (app, admin, _fixture) = test_app();
    let response = send(&app, Request::get(CAPABILITIES), Body::empty()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_no_store(&response);
    let anonymous = response_json(response).await;
    assert_eq!(anonymous["subject"], "anonymous");
    assert_eq!(
        anonymous["products"],
        json!(["system", "backtests", "market_status"])
    );
    assert!(anonymous.get("csrf_token").is_none());

    for path in [CATALOG, INTEGRATIONS] {
        let response = send(&app, Request::get(path), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_no_store(&response);
    }
    let response = send(
        &app,
        Request::get(CAPABILITIES).header(header::AUTHORIZATION, admin.clone()),
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_no_store(&response);
    let capabilities = response_json(response).await;
    assert_eq!(capabilities["subject"], "admin");
    assert!(capabilities.get("csrf_token").is_none());
    let products = capabilities["products"].as_array().unwrap();
    assert!(products.contains(&json!("integrations")));
    assert!(products.contains(&json!("restricted_market_data")));

    let response = send(
        &app,
        Request::get(CATALOG).header(header::AUTHORIZATION, admin),
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_no_store(&response);
    let catalog = response_json(response).await;
    let entries = catalog.as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|entry| entry["connector"] == "binance_spot")
    );
    assert!(entries.iter().all(|entry| entry["test_available"] == false));
}

#[tokio::test]
async fn authorization_precedes_body_limit_and_deserialization() {
    let (app, admin, _fixture) = test_app();
    let oversized = "x".repeat(MAX_BODY_BYTES + 1);
    for path in [ADMIN_SESSION, INTEGRATIONS, "/api/v1/backtests"] {
        let response = send(
            &app,
            Request::post(path)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONTENT_LENGTH, oversized.len()),
            oversized.clone(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(
            response_json(response).await["error"]["code"],
            "unauthorized"
        );
    }
    let response = send(
        &app,
        Request::delete(ADMIN_SESSION).header(header::CONTENT_LENGTH, oversized.len()),
        oversized.clone(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = send(
        &app,
        Request::post(INTEGRATIONS)
            .header(header::AUTHORIZATION, admin.clone())
            .header(header::CONTENT_LENGTH, oversized.len()),
        oversized,
    )
    .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let response = send(
        &app,
        Request::post(INTEGRATIONS)
            .header(header::AUTHORIZATION, admin)
            .header(header::CONTENT_TYPE, "application/json"),
        "{",
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn notifications_require_admin_and_reject_client_smtp_settings() {
    let (app, admin, _fixture) = test_app();
    let id = Uuid::new_v4();
    let rule = format!("{RULES}/{id}");
    let alert = format!("{ALERTS}/{id}");
    let ack = format!("{alert}/ack");
    let test = format!("{EMAIL_TESTS}/{id}");
    for path in [RULES, rule.as_str(), ALERTS, alert.as_str(), test.as_str()] {
        for request in [Request::get(path), Request::head(path)] {
            let response = send(&app, request, Body::empty()).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
            assert_no_store(&response);
        }
    }
    for request in [
        Request::post(EMAIL_TESTS),
        Request::post(&ack),
        Request::put(&rule),
    ] {
        let response = send(&app, request, "x".repeat(MAX_BODY_BYTES + 1)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_no_store(&response);
    }
    let response = send(
        &app,
        Request::get(CAPABILITIES).header(header::AUTHORIZATION, admin.clone()),
        Body::empty(),
    )
    .await;
    assert!(
        response_json(response).await["products"]
            .as_array()
            .unwrap()
            .contains(&json!("notifications"))
    );

    let marker = "synthetic.test.invalid";
    let with_query = format!("{EMAIL_TESTS}?host={marker}");
    for (path, body, status) in [
        (
            EMAIL_TESTS,
            json!({"recipient": marker}).to_string(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            with_query.as_str(),
            String::new(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "/api/v1/notification-channels/other/tests",
            String::new(),
            StatusCode::NOT_FOUND,
        ),
    ] {
        let response = send(
            &app,
            Request::post(path)
                .header(header::AUTHORIZATION, admin.clone())
                .header("Idempotency-Key", id.to_string()),
            body,
        )
        .await;
        assert_eq!(response.status(), status, "{path}");
        assert_no_store(&response);
        assert!(!response_json(response).await.to_string().contains(marker));
    }
    for path in [
        format!("{RULES}?limit=101"),
        format!("{ALERTS}?cursor=-1"),
        format!("{ALERTS}?host={marker}"),
    ] {
        let response = send(
            &app,
            Request::get(&path).header(header::AUTHORIZATION, admin.clone()),
            Body::empty(),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{path}"
        );
        assert_no_store(&response);
        assert!(!response_json(response).await.to_string().contains(marker));
    }
    let response = send(
        &app,
        Request::put(&rule).header(header::AUTHORIZATION, admin),
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PRECONDITION_REQUIRED);
    assert_eq!(
        response_json(response).await["error"]["code"],
        "revision_required"
    );
}

#[tokio::test]
async fn notification_cookie_writes_check_origin_and_csrf_before_parsing() {
    let (app, admin, _fixture) = test_app();
    let session = send(
        &app,
        Request::post(ADMIN_SESSION)
            .header(header::AUTHORIZATION, admin)
            .header(header::ORIGIN, ORIGIN),
        Body::empty(),
    )
    .await;
    assert_eq!(session.status(), StatusCode::CREATED);
    let cookie = session.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let session = response_json(session).await;
    let csrf = session["csrf_token"].as_str().unwrap();
    let ack = format!("{ALERTS}/{}/ack", Uuid::new_v4());
    let rule = format!("{RULES}/{}", Uuid::new_v4());
    for request in [
        Request::put(&rule),
        Request::post(&ack),
        Request::post(EMAIL_TESTS),
    ] {
        let response = send(
            &app,
            request
                .header(header::COOKIE, &cookie)
                .header(CSRF_HEADER, csrf),
            "{",
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response_json(response).await["error"]["code"],
            "origin_rejected"
        );
    }
    for request in [
        Request::put(&rule),
        Request::post(&ack),
        Request::post(EMAIL_TESTS),
    ] {
        let response = send(
            &app,
            request
                .header(header::COOKIE, &cookie)
                .header(header::ORIGIN, ORIGIN),
            "{",
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response_json(response).await["error"]["code"],
            "csrf_rejected"
        );
    }
    for request in [
        Request::put(&rule),
        Request::post(&ack),
        Request::post(EMAIL_TESTS),
    ] {
        let response = send(
            &app,
            request
                .header(header::COOKIE, &cookie)
                .header(header::ORIGIN, ORIGIN)
                .header(CSRF_HEADER, csrf)
                .header(header::IF_MATCH, "\"0\"")
                .header(header::CONTENT_TYPE, "application/json"),
            "{",
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_no_store(&response);
    }
}

#[tokio::test]
async fn legacy_backtests_page_and_bearer_requests_reach_their_handlers() {
    let (app, admin, _fixture) = test_app();
    let page = send(&app, Request::get("/backtests"), Body::empty()).await;
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(page.into_body(), 100).await.unwrap(),
        "<main>web</main>"
    );
    let submission = send(
        &app,
        Request::post("/api/v1/backtests")
            .header(header::AUTHORIZATION, admin.clone())
            .header(header::ORIGIN, "https://legacy.example.test")
            .header("Idempotency-Key", "legacy-submit")
            .header(header::CONTENT_TYPE, "application/json"),
        "{}",
    )
    .await;
    assert_eq!(submission.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let key_path = "/api/v1/backtests/by-key";
    let anonymous = send(&app, Request::head(key_path), Body::empty()).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let lookup = send(
        &app,
        Request::get(key_path).header(header::AUTHORIZATION, admin.clone()),
        Body::empty(),
    )
    .await;
    assert_eq!(lookup.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_no_store(&lookup);
    let test_url = format!("{INTEGRATIONS}/{}/tests?target=external", Uuid::new_v4());
    let target = send(
        &app,
        Request::post(test_url)
            .header(header::AUTHORIZATION, admin)
            .header(header::IF_MATCH, "\"1\""),
        Body::empty(),
    )
    .await;
    assert_eq!(target.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn integration_patch_rejects_fields_outside_the_selected_action() {
    let (app, admin, _fixture) = test_app();
    let path = format!("{INTEGRATIONS}/{}", Uuid::new_v4());
    let marker = format!("w1-extra-field-{}", Uuid::new_v4().simple());
    for (case, body) in [
        (
            "disable with an unexpected target",
            json!({"action": "disable", "target": marker}),
        ),
        (
            "disable with configure settings",
            json!({"action": "disable", "settings": {"symbol": "SPX"}}),
        ),
        (
            "configure with an unexpected target",
            json!({"action": "configure", "settings": {"symbol": "SPX"}, "target": marker}),
        ),
        (
            "configure with an unexpected settings field",
            json!({"action": "configure", "settings": {"symbol": "SPX", "path": marker}}),
        ),
    ] {
        let response = send(
            &app,
            Request::patch(&path)
                .header(header::AUTHORIZATION, admin.clone())
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, "\"1\""),
            body.to_string(),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{case}"
        );
        let body = response_json(response).await;
        assert_eq!(body["error"]["code"], "invalid_request", "{case}");
        assert!(!body.to_string().contains(&marker), "{case}");
    }
}

#[tokio::test]
async fn versioned_integration_routes_require_admin_origin_and_revision() {
    let (app, admin, _fixture) = test_app();
    let path = format!("{INTEGRATIONS}/{}", Uuid::new_v4());
    let test_id = Uuid::new_v4();
    for path in [&path, &format!("{path}/tests/{test_id}")] {
        let response = send(&app, Request::get(path), Body::empty()).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_no_store(&response);
    }
    for (action, body) in [
        ("activate", json!({"test_id": test_id.to_string()})),
        ("rollback", json!({"version": 1})),
    ] {
        let action = format!("{path}/{action}");
        let response = send(
            &app,
            Request::post(&action)
                .header(header::AUTHORIZATION, admin.clone())
                .header(header::ORIGIN, "https://other.example.invalid")
                .header(header::CONTENT_TYPE, "application/json"),
            body.to_string(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_no_store(&response);
        let response = send(
            &app,
            Request::post(action)
                .header(header::AUTHORIZATION, admin.clone())
                .header(header::ORIGIN, ORIGIN)
                .header(header::CONTENT_TYPE, "application/json"),
            body.to_string(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PRECONDITION_REQUIRED);
        assert_no_store(&response);
    }
}

#[tokio::test]
async fn integration_write_timeouts_match_the_exported_contract() {
    let (app, admin, _fixture) = app_with_deadline(Some(5));
    let spec = serde_json::to_value(crate::openapi()).unwrap();
    let id = Uuid::new_v4();
    let test_id = Uuid::new_v4();
    for (action, body, hint) in [
        ("tests", String::new(), "latest test"),
        (
            "activate",
            json!({"test_id": test_id.to_string()}).to_string(),
            "active version",
        ),
        (
            "rollback",
            json!({"version": 1}).to_string(),
            "active version",
        ),
    ] {
        let response = send(
            &app,
            Request::post(format!("{INTEGRATIONS}/{id}/{action}"))
                .header(header::AUTHORIZATION, admin.clone())
                .header(header::IF_MATCH, "\"1\"")
                .header(header::CONTENT_TYPE, "application/json"),
            body,
        )
        .await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_no_store(&response);
        let problem = response_json(response).await;
        assert_eq!(problem["error"]["code"], "request_timeout");
        assert!(problem["error"]["message"].as_str().unwrap().contains(hint));
        let declared =
            &spec["paths"][format!("{INTEGRATIONS}/{{id}}/{action}")]["post"]["responses"]["504"];
        assert!(declared["description"].as_str().unwrap().contains(hint));
        assert_eq!(
            declared["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ErrorBody"
        );
    }
}

#[tokio::test]
async fn credential_upload_rejects_path_dotenv_and_pem_token_forms() {
    let (app, admin, _fixture) = test_app();
    let path = format!("{INTEGRATIONS}/{}/credential", Uuid::new_v4());
    let marker = format!("w1-invalid-token-{}", Uuid::new_v4().simple());
    for (case, token) in [
        ("absolute path", format!("/tmp/{marker}.env")),
        ("relative path", format!("../{marker}.env")),
        ("dotenv filename", format!(".env.{marker}")),
        ("dotenv assignment", format!("WEB_ADMIN_TOKEN={marker}\n")),
        (
            "PEM contents",
            format!("-----BEGIN CERTIFICATE-----\n{marker}\n-----END CERTIFICATE-----"),
        ),
    ] {
        let response = send(
            &app,
            Request::put(&path)
                .header(header::AUTHORIZATION, admin.clone())
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, "\"1\""),
            json!({"api_token": token}).to_string(),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{case}"
        );
        let body = response_json(response).await;
        assert_eq!(body["error"]["code"], "invalid_request", "{case}");
        assert!(!body.to_string().contains(&marker), "{case}");
    }
}

#[tokio::test]
async fn session_cookie_same_origin_csrf_and_logout_work_through_router() {
    let (app, admin, _fixture) = test_app();
    for origin in [None, Some("https://other.example.test")] {
        let mut request = Request::post(ADMIN_SESSION).header(header::AUTHORIZATION, admin.clone());
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        let response = send(&app, request, Body::empty()).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response_json(response).await["error"]["code"],
            "origin_rejected"
        );
    }
    let response = send(
        &app,
        Request::post(ADMIN_SESSION)
            .header(header::AUTHORIZATION, admin)
            .header(header::ORIGIN, ORIGIN),
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_no_store(&response);
    let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
    let mut parts = cookie.split("; ");
    let session_cookie = parts.next().unwrap().to_string();
    assert!(session_cookie.starts_with(&format!("{}=", auth::SESSION_COOKIE_NAME)));
    let attributes: Vec<_> = parts.collect();
    for attribute in [
        "Path=/",
        "HttpOnly",
        "Secure",
        "SameSite=Strict",
        "Max-Age=120",
    ] {
        assert!(attributes.contains(&attribute));
    }
    assert!(!attributes.iter().any(|part| part.starts_with("Domain=")));
    let session = response_json(response).await;
    assert_eq!(session["expires_in"], 120);
    let csrf = session["csrf_token"].as_str().unwrap();

    let response = send(
        &app,
        Request::get(CAPABILITIES).header(header::COOKIE, &session_cookie),
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_no_store(&response);
    let capabilities = response_json(response).await;
    assert_eq!(capabilities["subject"], "admin");
    assert_eq!(capabilities["csrf_token"], csrf);
    let response = send(
        &app,
        Request::get(CATALOG).header(header::COOKIE, &session_cookie),
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    for (origin, token, reason) in [
        (None, Some(csrf), "origin_rejected"),
        (
            Some("https://other.example.test"),
            Some(csrf),
            "origin_rejected",
        ),
        (Some(ORIGIN), None, "csrf_rejected"),
        (Some(ORIGIN), Some("wrong"), "csrf_rejected"),
    ] {
        let mut request = Request::post(INTEGRATIONS)
            .header(header::COOKIE, &session_cookie)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        if let Some(token) = token {
            request = request.header(CSRF_HEADER, token);
        }
        let response = send(&app, request, "{").await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response_json(response).await["error"]["code"], reason);
    }
    for path in [INTEGRATIONS, "/api/v1/backtests"] {
        let response = send(
            &app,
            Request::post(path)
                .header(header::COOKIE, &session_cookie)
                .header(header::ORIGIN, ORIGIN)
                .header(CSRF_HEADER, csrf)
                .header(header::CONTENT_TYPE, "application/json"),
            "{",
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{path}"
        );
    }

    let response = send(
        &app,
        Request::delete(ADMIN_SESSION)
            .header(header::COOKIE, &session_cookie)
            .header(header::ORIGIN, ORIGIN),
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = send(
        &app,
        Request::delete(ADMIN_SESSION)
            .header(header::COOKIE, &session_cookie)
            .header(header::ORIGIN, ORIGIN)
            .header(CSRF_HEADER, csrf),
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_no_store(&response);
    let expiration = response.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(expiration.starts_with(&format!("{}=; ", auth::SESSION_COOKIE_NAME)));
    assert!(expiration.split("; ").any(|part| part == "Max-Age=0"));
    let replay = send(
        &app,
        Request::get(CATALOG).header(header::COOKIE, &session_cookie),
        Body::empty(),
    )
    .await;
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn credential_errors_and_http_logs_do_not_echo_request_secrets() {
    let logs = tempfile::NamedTempFile::new_in("/tmp").unwrap();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(logs.reopen().unwrap())
        .finish();
    // A global subscriber makes HTTP callsite registration consistent across parallel unit tests.
    tracing::subscriber::set_global_default(subscriber).unwrap();
    let (app, admin, _fixture) = test_app();
    let secret = format!("w1-test-secret-{}", Uuid::new_v4().simple());
    let token = admin.to_str().unwrap().strip_prefix("Bearer ").unwrap();
    let path = format!("{INTEGRATIONS}/{}/credential", Uuid::new_v4());
    let credential = json!({"api_token": secret}).to_string();
    let unexpected = json!({"api_token": secret, (secret.as_str()): secret}).to_string();
    for (path, body, status, code) in [
        (
            format!("{INTEGRATIONS}/{secret}/credential?probe={secret}"),
            credential.clone(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
        ),
        (
            path.clone(),
            unexpected,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
        ),
        (
            path,
            credential,
            StatusCode::SERVICE_UNAVAILABLE,
            "credential_store_unavailable",
        ),
    ] {
        let response = send(
            &app,
            Request::put(path)
                .header(header::AUTHORIZATION, admin.clone())
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, "\"1\""),
            body,
        )
        .await;
        assert_eq!(response.status(), status);
        assert_no_store(&response);
        for value in response
            .headers()
            .values()
            .filter_map(|value| value.to_str().ok())
        {
            assert!(!value.contains(&secret) && !value.contains(token));
        }
        let response = response_json(response).await;
        assert_eq!(response["error"]["code"], code);
        let response = response.to_string();
        assert!(!response.contains(&secret) && !response.contains(token));
    }
    let response = send(
        &app,
        Request::get(format!("/api/{secret}?probe={secret}")),
        Body::empty(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(!response_json(response).await.to_string().contains(&secret));
    let captured = std::fs::read_to_string(logs.path()).unwrap();
    assert!(captured.contains("/api/v1/integrations/{id}/credential"));
    assert!(!captured.contains(&secret) && !captured.contains(token));
}
