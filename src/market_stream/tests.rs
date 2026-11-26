use super::fixture::{SEQ, delta, page, snapshot};
use super::*;
use crate::{auth::WriteAccess, config::Settings, database::Database, finance::Finance, router};
use axum::{
    body::{Body, to_bytes},
    http::{HeaderValue, Request, header, request::Builder},
};
use chrono::Utc;
use serde_json::Value;
use std::{
    cell::Cell, fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt, path::Path, sync::Arc,
    time::Duration,
};
use tower::ServiceExt;
use uuid::Uuid;

const SESSION_PATH: &str = "/api/v1/admin/session";
const ORIGIN: &str = "https://web.example.test";

struct StreamTestApp {
    state: AppState,
    bearer: HeaderValue,
    _directory: tempfile::TempDir,
}

impl StreamTestApp {
    fn new() -> Self {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let token_path = directory.path().join("admin.env");
        let token = Uuid::new_v4().to_string();
        let mut token_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&token_path)
            .unwrap();
        writeln!(token_file, "WEB_ADMIN_TOKEN={token}").unwrap();
        let mut bearer = HeaderValue::from_str(&format!("Bearer {token}")).unwrap();
        bearer.set_sensitive(true);

        let config = Settings::load(Path::new("config"), |_| None).unwrap();
        let access = WriteAccess::load(&token_path, 10, Duration::from_secs(1))
            .unwrap()
            .with_sessions(Some(ORIGIN), Duration::from_secs(10))
            .unwrap();
        let state = AppState {
            finance: Finance::new(&config.finance).unwrap(),
            db: Database::new(
                &format!(
                    "postgresql://localhost/web_stream_test?host={}",
                    directory.path().display()
                ),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .unwrap(),
            config: Arc::new(config),
            write_access: access,
            credential_store: None,
            started_at: Utc::now(),
        };
        Self {
            state,
            bearer,
            _directory: directory,
        }
    }
}

async fn send(app: &Router, builder: Builder) -> Response {
    app.clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn stream_request() -> Builder {
    Request::get(MARKET_STREAM)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
}

#[tokio::test]
async fn market_stream_handshake_reauthenticates_after_logout_and_checks_origin() {
    let fixture = StreamTestApp::new();
    let app = router(fixture.state);
    let anonymous = send(&app, stream_request()).await;
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(anonymous.headers()[header::CACHE_CONTROL], "no-store");

    let session = send(
        &app,
        Request::post(SESSION_PATH)
            .header(header::AUTHORIZATION, fixture.bearer)
            .header(header::ORIGIN, ORIGIN),
    )
    .await;
    assert_eq!(session.status(), StatusCode::CREATED);
    let mut cookie = HeaderValue::from_str(
        session.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap(),
    )
    .unwrap();
    cookie.set_sensitive(true);
    let body = to_bytes(session.into_body(), 4096).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    let csrf = body["csrf_token"].as_str().unwrap();

    let rejected = send(
        &app,
        stream_request()
            .header(header::COOKIE, cookie.clone())
            .header(header::ORIGIN, "https://other.example.test"),
    )
    .await;
    assert_eq!(rejected.status(), StatusCode::FORBIDDEN);

    let allowed = send(
        &app,
        stream_request()
            .header(header::COOKIE, cookie.clone())
            .header(header::ORIGIN, ORIGIN),
    )
    .await;
    assert_eq!(allowed.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(allowed.headers()[header::CACHE_CONTROL], "no-store");

    let deleted = send(
        &app,
        Request::delete(SESSION_PATH)
            .header(header::COOKIE, cookie.clone())
            .header(header::ORIGIN, ORIGIN)
            .header("x-csrf-token", csrf),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    let reconnect = send(
        &app,
        stream_request()
            .header(header::COOKIE, cookie)
            .header(header::ORIGIN, ORIGIN),
    )
    .await;
    assert_eq!(reconnect.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn market_stream_replay_reauthenticates_each_poll_and_lag_resets_the_reader() {
    let fixture = StreamTestApp::new();
    let partition = MarketPartition::new("synthetic", "book", "l2", "SYNTH.XNYS").unwrap();
    let mut replay = MarketReplay::new(partition.clone(), 0);
    let calls = Cell::new(0);
    let fetch = async |requested: &MarketPartition, after, through| {
        assert!(*requested == partition);
        assert_eq!(after, calls.get());
        assert_eq!(through, None);
        calls.set(after + 1);
        let reply = if after == 0 {
            page(1, 1, None, vec![snapshot(1, 1, SEQ)])
        } else {
            page(514, 514, Some(2), vec![delta(2, 1, SEQ + 1)])
        };
        Ok(serde_json::from_value(serde_json::to_value(reply).unwrap()).unwrap())
    };
    let access = &fixture.state.write_access;
    let mut headers = HeaderMap::new();
    assert_eq!(
        replay.poll_with(access, &headers, fetch).await,
        Err(ReplayError::Unauthorized)
    );
    assert_eq!(calls.get(), 0);

    headers.insert(header::AUTHORIZATION, fixture.bearer);
    let first = replay.poll_with(access, &headers, fetch).await.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].seq, SEQ.to_string());
    for _ in 0..2 {
        assert_eq!(
            replay.poll_with(access, &headers, fetch).await,
            Err(ReplayError::ResetRequired(ResetReason::ConsumerLagged))
        );
    }
    assert_eq!(calls.get(), 2);
    headers.remove(header::AUTHORIZATION);
    assert_eq!(
        replay.poll_with(access, &headers, fetch).await,
        Err(ReplayError::Unauthorized)
    );
}
