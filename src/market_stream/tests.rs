use super::fixture::{SEQ, delta, page, reset, snapshot};
use super::*;
use crate::{
    auth::WriteAccess, config::Settings, database::Database, finance::Finance,
    market_preferences::MarketPreferences, router, test_postgres::Cluster,
    test_subscription_demand::DemandStub,
};
use axum::{
    Extension, Json,
    body::{Body, to_bytes},
    extract::Query,
    http::{HeaderMap, HeaderValue, Request, StatusCode, header, request::Builder},
    response::Response,
    routing::get,
};
use chrono::Utc;
use serde_json::Value;
use std::{
    cell::Cell, collections::BTreeMap, fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt,
    path::Path, sync::Arc, time::Duration,
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
        Self::with_origin(Some(ORIGIN))
    }

    fn with_origin(public_origin: Option<&str>) -> Self {
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
            .with_sessions(public_origin, Duration::from_secs(300))
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
    assert_eq!(allowed.status(), StatusCode::BAD_REQUEST);
    assert_eq!(allowed.headers()[header::CACHE_CONTROL], "no-store");
    let body = to_bytes(allowed.into_body(), 4096).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["code"], "invalid_market_websocket_upgrade");

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

const SUBSCRIBE: &str = r#"{"type":"subscribe","source":"synthetic","product":"book","channel":"l2","instrument_id":"SYNTH.XNYS"}"#;

#[tokio::test]
async fn market_websocket_resets_at_a_v2_control_without_delivering_it() {
    let fixture = StreamTestApp::new();
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, fixture.bearer);
    let preferences = Arc::new(MarketPreferences::default());
    let mut session =
        MarketStreamSession::open(fixture.state.write_access, &headers, preferences).unwrap();
    assert_eq!(session.command(SUBSCRIBE), Ok(None));

    let mut v2_reset = reset(1, 2);
    v2_reset.event.schema_version = finance_client::models::SchemaVersion::Variant20;
    v2_reset.event.asset_kind = finance_client::models::AssetKind::Perpetual;
    let frames = session
        .poll_with(async move |_, _, _| Ok(page(1, 1, None, vec![v2_reset])))
        .await;
    assert_eq!(frames.len(), 1);
    assert_eq!(
        frames[0].to_text().unwrap(),
        r#"{"type":"reset","reason":"unsupported_version"}"#
    );
    assert!(!session.is_subscribed());
}
const FOCUS_SUBSCRIPTION: &str = r#"{"type":"subscribe","source":"synthetic","product":"book","channel":"l2","use_depth_focus":true}"#;

#[tokio::test]
async fn market_stream_commands_share_quota_and_release_it_on_reset() {
    let fixture = StreamTestApp::new();
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, fixture.bearer);
    let preferences = Arc::new(MarketPreferences::default());
    let mut first = MarketStreamSession::open(
        fixture.state.write_access.clone(),
        &headers,
        preferences.clone(),
    )
    .unwrap();
    let mut second =
        MarketStreamSession::open(fixture.state.write_access, &headers, preferences.clone())
            .unwrap();
    let held: Vec<_> = std::iter::from_fn(|| preferences.acquire())
        .take(7)
        .collect();
    assert_eq!(held.len(), 7);
    assert_eq!(
        first.command(r#"{"type":"unsubscribe","unknown":1}"#),
        Err(MarketStreamError::InvalidSubscription)
    );
    assert!(preferences.acquire().is_some());
    assert_eq!(first.command(SUBSCRIBE), Ok(None));
    assert_eq!(first.command(SUBSCRIBE), Ok(None));
    assert_eq!(
        second.command(SUBSCRIBE),
        Err(MarketStreamError::SubscriptionQuotaExceeded)
    );

    let output = first
        .poll_with(async |_, after, through| {
            assert_eq!((after, through), (0, None));
            Ok(page(1, 1, None, vec![snapshot(1, 1, SEQ)]))
        })
        .await;
    assert_eq!(output.len(), 2);
    let frame: Value = serde_json::from_str(&output[0].to_text().unwrap()).unwrap();
    assert_eq!(frame["schema_version"], "1.0");
    assert_eq!(frame["kind"], "snapshot");
    assert_eq!(frame["seq"], SEQ.to_string());
    assert_eq!(frame["source"], "synthetic");
    assert_eq!(frame["product"], "book");
    assert_eq!(frame["exchange_ts"], Value::Null);
    assert_eq!(
        output[1].to_text().unwrap(),
        r#"{"type":"checkpoint","after":"1"}"#
    );
    let output = first
        .poll_with(async |_, after, through| {
            assert_eq!((after, through), (1, None));
            Ok(page(514, 514, Some(2), vec![delta(2, 1, SEQ + 1)]))
        })
        .await;
    assert_eq!(output.len(), 1);
    assert_eq!(
        output[0].to_text().unwrap(),
        r#"{"type":"reset","reason":"consumer_lagged"}"#
    );
    assert!(!first.is_subscribed());
    assert!(preferences.acquire().is_some());
    assert_eq!(second.command(SUBSCRIBE), Ok(None));
    assert_eq!(
        second.command(r#"{"type":"unsubscribe"}"#),
        Ok(Some(MarketStreamControl::Unsubscribed))
    );
    assert!(preferences.acquire().is_some());
}

#[tokio::test]
async fn loopback_cookie_market_stream_rechecks_authority_during_replay() {
    let fixture = StreamTestApp::with_origin(None);
    let access = fixture.state.write_access.clone();
    let app = router(fixture.state);
    let preferences = Arc::new(MarketPreferences::default());
    for host in ["127.0.0.1:8090", "localhost:8090"] {
        let origin = format!("http://{host}");
        let session = send(
            &app,
            Request::post(SESSION_PATH)
                .header(header::AUTHORIZATION, &fixture.bearer)
                .header(header::HOST, host)
                .header(header::ORIGIN, &origin),
        )
        .await;
        assert_eq!(session.status(), StatusCode::CREATED);
        let cookie = session.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static(host));
        headers.insert(header::ORIGIN, HeaderValue::from_str(&origin).unwrap());
        headers.insert(header::COOKIE, HeaderValue::from_str(cookie).unwrap());
        let mut stream =
            MarketStreamSession::open(access.clone(), &headers, preferences.clone()).unwrap();
        assert_eq!(stream.command(SUBSCRIBE), Ok(None));
        let frames = stream
            .poll_with(async |_, after, _| {
                assert_eq!(after, 0);
                Ok(page(1, 1, None, vec![snapshot(1, 1, SEQ)]))
            })
            .await;
        assert_eq!(frames.len(), 2);
        let snapshot: Value = serde_json::from_str(&frames[0].to_text().unwrap()).unwrap();
        assert_eq!(snapshot["seq"], SEQ.to_string());
        assert_eq!(
            frames[1].to_text().unwrap(),
            r#"{"type":"checkpoint","after":"1"}"#
        );
    }
}

#[tokio::test]
async fn market_stream_application_reauthenticates_subscription_and_resume() {
    let fixture = StreamTestApp::new();
    let access = fixture.state.write_access.clone();
    let app = router(fixture.state);
    let csrf_cookie = send(
        &app,
        Request::post(SESSION_PATH)
            .header(header::AUTHORIZATION, fixture.bearer)
            .header(header::ORIGIN, ORIGIN),
    )
    .await;
    assert_eq!(csrf_cookie.status(), StatusCode::CREATED);
    let cookie = csrf_cookie.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let credentials: Value =
        serde_json::from_slice(&to_bytes(csrf_cookie.into_body(), 4096).await.unwrap()).unwrap();
    let csrf = credentials["csrf_token"].as_str().unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(header::COOKIE, cookie.parse().unwrap());
    headers.insert(header::ORIGIN, ORIGIN.parse().unwrap());
    let preferences = Arc::new(MarketPreferences::default());

    let mut first =
        MarketStreamSession::open(access.clone(), &headers, preferences.clone()).unwrap();
    assert_eq!(first.command(SUBSCRIBE), Ok(None));
    drop(first);
    let mut resumed =
        MarketStreamSession::open(access.clone(), &headers, preferences.clone()).unwrap();
    let resume = SUBSCRIBE.replace('}', ",\"after\":\"3\"}");
    assert_eq!(resumed.command(&resume), Ok(None));
    let output = resumed
        .poll_with(async |_, after, _| {
            assert_eq!(after, 3);
            let mut unrelated = snapshot(4, 1, SEQ);
            unrelated.event.instrument_id = "OTHER.XNYS".into();
            Ok(page(5, 5, None, vec![unrelated, snapshot(5, 1, SEQ)]))
        })
        .await;
    assert_eq!(output.len(), 2);
    let delivered: Value = serde_json::from_str(&output[0].to_text().unwrap()).unwrap();
    assert_eq!(delivered["instrument_id"], "SYNTH.XNYS");
    assert_eq!(
        output[1].to_text().unwrap(),
        r#"{"type":"checkpoint","after":"5"}"#
    );
    let resume = SUBSCRIBE.replace('}', ",\"after\":\"5\"}");
    assert_eq!(resumed.command(&resume), Ok(None));
    let output = resumed
        .poll_with(async |_, after, _| {
            assert_eq!(after, 5);
            Ok(page(6, 6, None, vec![delta(6, 1, SEQ + 1)]))
        })
        .await;
    assert_eq!(output.len(), 1);
    assert_eq!(
        output[0].to_text().unwrap(),
        r#"{"type":"reset","reason":"snapshot_required"}"#
    );
    assert_eq!(resumed.command(SUBSCRIBE), Ok(None));
    let deleted = send(
        &app,
        Request::delete(SESSION_PATH)
            .header(header::COOKIE, cookie)
            .header(header::ORIGIN, ORIGIN)
            .header("x-csrf-token", csrf),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        resumed.command(SUBSCRIBE),
        Err(MarketStreamError::AuthorizationRevoked)
    );
    assert!(matches!(
        MarketStreamSession::open(access, &headers, preferences.clone()),
        Err(MarketStreamError::AuthorizationRevoked)
    ));
    assert!(preferences.acquire().is_some());
}

#[tokio::test]
async fn market_stream_focused_selection_uses_http_preference_and_updates_quota() {
    let mut fixture = StreamTestApp::new();
    let cluster = Cluster::create();
    let timeout = Duration::from_secs(2);
    fixture.state.db = Database::new(&cluster.url(), timeout, timeout).unwrap();
    fixture.state.db.migrate().await.unwrap();
    let finance = DemandStub::enabled().await;
    let config = Arc::make_mut(&mut fixture.state.config);
    config.finance.base_url.clone_from(&finance.url);
    fixture.state.finance = Finance::new(&config.finance).unwrap();
    let preferences = Arc::new(MarketPreferences::default());
    let app = crate::with_controls(
        crate::market_preferences::routes().layer(Extension(preferences.clone())),
        fixture.state.clone(),
    );
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, fixture.bearer.clone());
    let mut session =
        MarketStreamSession::open(fixture.state.write_access, &headers, preferences).unwrap();
    assert_eq!(
        session.command(FOCUS_SUBSCRIPTION),
        Err(MarketStreamError::InvalidSubscription)
    );
    let selection = app
        .clone()
        .oneshot(
            Request::put(crate::market_preferences::DEPTH_FOCUS)
                .header(header::AUTHORIZATION, fixture.bearer.clone())
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"action":"set","instrument_id":"SYNTH.XNYS"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(selection.status(), StatusCode::OK);
    assert_eq!(session.command(FOCUS_SUBSCRIPTION), Ok(None));
    let output = session
        .poll_with(async |partition, _, _| {
            let expected = MarketPartition::new("synthetic", "book", "l2", "SYNTH.XNYS").unwrap();
            assert!(*partition == expected);
            Ok(page(1, 1, None, vec![snapshot(1, 1, SEQ)]))
        })
        .await;
    let wire: Value = serde_json::from_str(&output[0].to_text().unwrap()).unwrap();
    assert_eq!(wire["instrument_id"], "SYNTH.XNYS");
    assert_eq!(wire["seq"], SEQ.to_string());

    for active in [1, 0] {
        let slots = send(
            &app,
            Request::get(crate::market_preferences::SUBSCRIPTIONS)
                .header(header::AUTHORIZATION, fixture.bearer.clone()),
        )
        .await;
        assert_eq!(slots.status(), StatusCode::OK);
        let slots: Value =
            serde_json::from_slice(&to_bytes(slots.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(slots["web"]["active"], active);
        assert_eq!(slots["web"]["available"], 8 - active);
        assert_eq!(
            session.command(r#"{"type":"unsubscribe"}"#),
            Ok(Some(MarketStreamControl::Unsubscribed))
        );
    }
}

#[tokio::test]
async fn market_stream_application_connects_to_synthetic_finance_and_filters_display_policy() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture_app = Router::new().route(
        "/api/v1/market/wal",
        get(
            |Query(params): Query<BTreeMap<String, String>>, headers: HeaderMap| async move {
                assert!(!headers.contains_key(header::AUTHORIZATION));
                assert!(!headers.contains_key(header::COOKIE));
                assert_eq!(params["source"], "synthetic");
                assert_eq!(params["after"], "0");
                assert_eq!(params["limit"], "64");
                let mut reply = page(1, 1, None, vec![snapshot(1, 1, SEQ)]);
                if params["product"] == "record_only" {
                    reply.access.display_live = false;
                    reply.access.product = "record_only".into();
                    reply.records[0].event.product = "record_only".into();
                } else {
                    assert_eq!(params["product"], "book");
                }
                Json(reply)
            },
        ),
    );
    let fixture = StreamTestApp::new();
    let mut config = fixture.state.config.finance.clone();
    config.base_url = format!("http://{}", listener.local_addr().unwrap());
    config.max_retries = 0;
    let finance = Finance::new(&config).unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, fixture_app).await.unwrap() });
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, fixture.bearer);
    let preferences = Arc::new(MarketPreferences::default());
    let mut session =
        MarketStreamSession::open(fixture.state.write_access, &headers, preferences).unwrap();
    assert_eq!(session.command(SUBSCRIBE), Ok(None));
    let output = session.poll(&finance).await;
    let snapshot: Value = serde_json::from_str(&output[0].to_text().unwrap()).unwrap();
    assert_eq!(snapshot["seq"], SEQ.to_string());
    assert_eq!(snapshot["kind"], "snapshot");
    assert_eq!(
        session.command(&SUBSCRIBE.replace("\"book\"", "\"record_only\"")),
        Ok(None)
    );
    let denied = session.poll(&finance).await;
    assert_eq!(denied.len(), 1);
    assert_eq!(
        denied[0].to_text().unwrap(),
        r#"{"type":"error","code":"product_unavailable"}"#
    );
    assert!(!session.is_subscribed());
    server.abort();
}

#[path = "transport_rejection_tests.rs"]
mod transport_rejection_tests;
#[path = "transport_tests.rs"]
mod transport_tests;
