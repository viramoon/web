use super::{
    postgres::Cluster,
    subscription_demand_fixture::{DemandStub, PATH, subscription_status, upstream_error_body},
};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Method, Request, StatusCode, header},
    routing::put,
};
use serde_json::{Value, json};
use std::{io::Write, path::Path, sync::Arc, time::Duration};
use tokio::sync::{Notify, mpsc};
use tower::ServiceExt;
use uuid::Uuid;
use web::{AppState, auth::WriteAccess, config::Settings, database::Database, finance::Finance};

async fn request(
    app: &Router,
    authorization: &str,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (status, _, value) = market_request(
        app,
        Some(authorization),
        method,
        &format!("/api/v1/watchlists/{path}"),
        body,
    )
    .await;
    (status, value)
}

async fn market_request(
    app: &Router,
    authorization: Option<&str>,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(authorization) = authorization {
        request = request.header(header::AUTHORIZATION, authorization);
    }
    let request = request
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, 8192).await.unwrap();
    (
        parts.status,
        parts.headers,
        serde_json::from_slice(&bytes).unwrap(),
    )
}

async fn gateway(finance: &DemandStub) -> (AppState, String, Cluster, tempfile::NamedTempFile) {
    let cluster = Cluster::create();
    let mut fixture = tempfile::NamedTempFile::new().unwrap();
    let token = Uuid::new_v4();
    writeln!(fixture, "WEB_ADMIN_TOKEN={token}").unwrap();
    let mut config = Settings::load(Path::new("config"), |_| None).unwrap();
    config.finance.base_url.clone_from(&finance.url);
    let timeout = Duration::from_secs(2);
    let db = Database::new(&cluster.url(), timeout, timeout).unwrap();
    db.migrate().await.unwrap();
    let state = AppState {
        db,
        finance: Finance::new(&config.finance).unwrap(),
        write_access: WriteAccess::load(fixture.path(), 10, Duration::from_secs(60)).unwrap(),
        config: Arc::new(config),
        credential_store: None,
        started_at: chrono::Utc::now(),
    };
    (state, format!("Bearer {token}"), cluster, fixture)
}

#[tokio::test]
async fn postgres_watchlists_persist_order_and_compare_revisions_atomically() {
    let finance = DemandStub::enabled().await;
    let (state, auth, _cluster, _fixture) = gateway(&finance).await;
    let app = web::router(state.clone());
    let read = |app, id| request(app, &auth, Method::GET, id, None);
    assert_eq!(
        read(&app, "default").await,
        (
            StatusCode::OK,
            json!({"id":"default","revision":0,"instrument_ids":[]})
        )
    );
    let body = |revision, ids| Some(json!({"revision":revision,"instrument_ids":ids}));
    let ids = ["SYN.B.XTEST", "SYN-A.XTEST"];
    let other = ["SYN-C.XTEST", "SYN-D.XTEST"];
    let put = |body| request(&app, &auth, Method::PUT, "default", body);
    let (left, right) = tokio::join!(put(body(0, ids)), put(body(0, other)));
    let (winner, loser) = if left.0 == StatusCode::OK {
        (left, right)
    } else {
        (right, left)
    };
    assert_eq!(winner.0, StatusCode::OK);
    assert_eq!(winner.1["revision"], 1);
    assert_eq!(loser.0, StatusCode::CONFLICT);

    let fresh_router = web::router(state);
    assert_eq!(read(&fresh_router, "default").await, winner);
    assert_eq!(
        put(body(1, ["SYN-A.XTEST", "SYN-A.XTEST"])).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        put(body(1, ["SYN-A.XTEST", "alias"])).await.0,
        StatusCode::BAD_REQUEST
    );
    let (status, updated) = put(body(1, ids)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["revision"], 2);
    assert_eq!(updated["instrument_ids"], json!(ids));
    assert_eq!(read(&fresh_router, "default").await, (status, updated));
    assert_eq!(read(&fresh_router, "another").await.1["revision"], 0);
}

const DEFAULT: &str = "/api/v1/watchlists/default";
const FOCUS: &str = "/api/v1/market/depth-focus";
const DEMAND_STATE: &str = "x-finance-subscription-state";

async fn next_request(requests: &mut mpsc::UnboundedReceiver<Value>) -> Value {
    tokio::time::timeout(Duration::from_secs(2), requests.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn market_demand_forwards_the_current_union_and_focus_in_serial_order() {
    let (captured, mut requests) = mpsc::unbounded_channel();
    let release = Arc::new(Notify::new());
    let wait_for_release = release.clone();
    let finance = DemandStub::start(Router::new().route(
        PATH,
        put(
            move |headers: HeaderMap, Json(body): Json<Value>| async move {
                assert!(!headers.contains_key(header::AUTHORIZATION));
                assert!(!headers.contains_key(header::COOKIE));
                assert!(!headers.contains_key(header::ORIGIN));
                assert!(!headers.contains_key("x-csrf-token"));
                captured.send(body.clone()).unwrap();
                if body == json!({"watchlist": ["SYN-A.XTEST", "SYN-B.XTEST"], "depth_focus": null})
                {
                    wait_for_release.notified().await;
                }
                Json(subscription_status(&body, "enabled"))
            },
        ),
    ))
    .await;
    let (state, auth, cluster, _fixture) = gateway(&finance).await;
    // Seed a saved pre-W15 list without any previous F15 request.
    super::postgres::checked(cluster.client("psql").args([
        "-d",
        "web_test_db",
        "-v",
        "ON_ERROR_STOP=1",
        "-c",
        "INSERT INTO app.market_watchlists VALUES ('other', 1, ARRAY['SYN-A.XTEST'])",
    ]));
    let app = web::router(state);
    let write_app = app.clone();
    let write_auth = auth.clone();
    let watchlist = tokio::spawn(async move {
        market_request(
            &write_app,
            Some(&write_auth),
            Method::PUT,
            DEFAULT,
            Some(json!({"revision": 0, "instrument_ids": ["SYN-B.XTEST", "SYN-A.XTEST"]})),
        )
        .await
    });
    let first = next_request(&mut requests).await;
    assert_eq!(
        first,
        json!({"watchlist": ["SYN-A.XTEST", "SYN-B.XTEST"], "depth_focus": null})
    );

    let focus = market_request(
        &app,
        Some(&auth),
        Method::PUT,
        FOCUS,
        Some(json!({"action": "set", "instrument_id": "SYN-C.XTEST"})),
    );
    tokio::pin!(focus);
    tokio::select! {
        biased;
        _ = &mut focus => panic!("focus write completed before the preceding demand"),
        _ = std::future::ready(()) => {}
    }
    assert_eq!(
        market_request(&app, Some(&auth), Method::GET, FOCUS, None)
            .await
            .2,
        json!({"instrument_id": null})
    );
    release.notify_one();

    let (status, headers, saved) = watchlist.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[DEMAND_STATE], "enabled");
    assert_eq!(
        saved,
        json!({"id": "default", "revision": 1,
        "instrument_ids": ["SYN-B.XTEST", "SYN-A.XTEST"]})
    );
    let (status, headers, selection) = focus.await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[DEMAND_STATE], "enabled");
    assert_eq!(selection, json!({"instrument_id": "SYN-C.XTEST"}));
    assert_eq!(
        next_request(&mut requests).await,
        json!({"watchlist": ["SYN-A.XTEST", "SYN-B.XTEST"], "depth_focus": "SYN-C.XTEST"})
    );

    let (status, _, _) = market_request(
        &app,
        Some(&auth),
        Method::PUT,
        DEFAULT,
        Some(json!({"revision": 1, "instrument_ids": []})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        next_request(&mut requests).await,
        json!({"watchlist": ["SYN-A.XTEST"], "depth_focus": "SYN-C.XTEST"})
    );
    let (status, _, _) = market_request(
        &app,
        Some(&auth),
        Method::PUT,
        FOCUS,
        Some(json!({"action": "clear"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        next_request(&mut requests).await,
        json!({"watchlist": ["SYN-A.XTEST"], "depth_focus": null})
    );
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn market_demand_keeps_saved_preferences_and_sanitizes_finance_errors() {
    let (captured, mut requests) = mpsc::unbounded_channel();
    let finance = DemandStub::start(Router::new().route(
        PATH,
        put(move |Json(body): Json<Value>| async move {
            let status = match body["depth_focus"].as_str() {
                None => StatusCode::BAD_REQUEST,
                Some("SYN-TIMEOUT.XTEST") => StatusCode::GATEWAY_TIMEOUT,
                Some(_) => StatusCode::SERVICE_UNAVAILABLE,
            };
            captured.send(body).unwrap();
            (status, Json(upstream_error_body()))
        }),
    ))
    .await;
    let (state, auth, _cluster, _fixture) = gateway(&finance).await;
    let app = web::router(state.clone());
    let body = json!({"revision": 0, "instrument_ids": ["UNREGISTERED.XTEST"]});
    let (status, headers, error) =
        market_request(&app, Some(&auth), Method::PUT, DEFAULT, Some(body.clone())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!headers.contains_key(DEMAND_STATE));
    assert_eq!(
        error,
        json!({"error": {"code": "subscription_demand_invalid",
        "message": "Finance rejected the saved subscription demand"}})
    );
    assert_eq!(
        requests.try_recv().unwrap(),
        json!({"watchlist": ["UNREGISTERED.XTEST"], "depth_focus": null})
    );
    assert_eq!(
        read_watchlist(&web::router(state), &auth).await,
        json!({"id": "default", "revision": 1, "instrument_ids": ["UNREGISTERED.XTEST"]})
    );
    assert_eq!(
        market_request(&app, Some(&auth), Method::PUT, DEFAULT, Some(body))
            .await
            .0,
        StatusCode::CONFLICT
    );

    let (status, headers, error) = market_request(
        &app,
        Some(&auth),
        Method::PUT,
        FOCUS,
        Some(json!({"action": "set", "instrument_id": "SYN-C.XTEST"})),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!headers.contains_key(DEMAND_STATE));
    assert_eq!(
        error,
        json!({"error": {"code": "finance_unavailable", "message": "Finance is unavailable"}})
    );
    assert_eq!(
        requests.try_recv().unwrap(),
        json!({"watchlist": ["UNREGISTERED.XTEST"], "depth_focus": "SYN-C.XTEST"})
    );
    assert!(requests.try_recv().is_err());
    assert_eq!(
        market_request(&app, Some(&auth), Method::GET, FOCUS, None)
            .await
            .2,
        json!({"instrument_id": "SYN-C.XTEST"})
    );

    let (status, _, error) = market_request(
        &app,
        Some(&auth),
        Method::PUT,
        FOCUS,
        Some(json!({"action": "set", "instrument_id": "SYN-TIMEOUT.XTEST"})),
    )
    .await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(
        error,
        json!({"error": {"code": "finance_timeout", "message":
        "Web market preferences were saved. Check subscription demand before retrying"}})
    );
    assert_eq!(
        requests.try_recv().unwrap(),
        json!({"watchlist": ["UNREGISTERED.XTEST"], "depth_focus": "SYN-TIMEOUT.XTEST"})
    );
    assert!(requests.try_recv().is_err());
    assert_eq!(
        market_request(&app, Some(&auth), Method::GET, FOCUS, None)
            .await
            .2,
        json!({"instrument_id": "SYN-TIMEOUT.XTEST"})
    );
}

async fn read_watchlist(app: &Router, auth: &str) -> Value {
    let (status, _, body) = market_request(app, Some(auth), Method::GET, DEFAULT, None).await;
    assert_eq!(status, StatusCode::OK);
    body
}

#[tokio::test]
async fn market_demand_rejects_unexpected_status_with_a_valid_success_body() {
    let (captured, mut requests) = mpsc::unbounded_channel();
    let redirected = captured.clone();
    let finance = DemandStub::start(
        Router::new()
            .route(
                PATH,
                put(move |Json(body): Json<Value>| async move {
                    let status = match body["watchlist"][0].as_str() {
                        Some("SYN-A.XTEST") => StatusCode::FOUND,
                        Some("SYN-B.XTEST") => StatusCode::CREATED,
                        _ => panic!("unexpected synthetic watchlist"),
                    };
                    captured.send(body.clone()).unwrap();
                    (
                        status,
                        [(header::LOCATION, "/redirect-target")],
                        Json(subscription_status(&body, "enabled")),
                    )
                }),
            )
            .fallback(move || async move {
                redirected.send(json!({"redirect_followed": true})).unwrap();
                StatusCode::NO_CONTENT
            }),
    )
    .await;
    let (mut state, auth, _cluster, _fixture) = gateway(&finance).await;
    // Send all four unexpected responses before the circuit can open.
    let config = Arc::make_mut(&mut state.config);
    config.finance.circuit_failure_threshold = 5;
    state.finance = Finance::new(&config.finance).unwrap();
    let app = web::router(state);
    let invalid_response = json!({"error": {"code": "finance_invalid_response",
        "message": "Invalid finance response"}});
    let mut previous_focus = Value::Null;

    for (revision, instrument, focus) in [
        (0, "SYN-A.XTEST", "SYN-C.XTEST"),
        (1, "SYN-B.XTEST", "SYN-D.XTEST"),
    ] {
        let cases = [
            (
                DEFAULT,
                json!({"revision": revision, "instrument_ids": [instrument]}),
                json!({"watchlist": [instrument], "depth_focus": previous_focus}),
                json!({"id": "default", "revision": revision + 1, "instrument_ids": [instrument]}),
            ),
            (
                FOCUS,
                json!({"action": "set", "instrument_id": focus}),
                json!({"watchlist": [instrument], "depth_focus": focus}),
                json!({"instrument_id": focus}),
            ),
        ];
        for (path, change, demand, saved) in cases {
            let (status, headers, error) =
                market_request(&app, Some(&auth), Method::PUT, path, Some(change)).await;
            assert_eq!(status, StatusCode::BAD_GATEWAY, "{path}");
            assert_eq!(error, invalid_response);
            assert!(!headers.contains_key(DEMAND_STATE));
            assert!(!headers.contains_key(header::LOCATION));
            assert_eq!(requests.try_recv().unwrap(), demand);
            assert!(requests.try_recv().is_err());
            let (status, _, actual) =
                market_request(&app, Some(&auth), Method::GET, path, None).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(actual, saved);
        }
        previous_focus = json!(focus);
    }
}

#[tokio::test]
async fn market_demand_propagates_disabled_and_rejects_anonymous_writes() {
    let (captured, mut requests) = mpsc::unbounded_channel();
    let finance = DemandStub::start(Router::new().route(
        PATH,
        put(move |Json(body): Json<Value>| async move {
            captured.send(body.clone()).unwrap();
            Json(subscription_status(&body, "disabled"))
        }),
    ))
    .await;
    let (state, auth, _cluster, _fixture) = gateway(&finance).await;
    let app = web::router(state);
    let changes = [
        (
            DEFAULT,
            json!({"revision": 0, "instrument_ids": ["SYN-A.XTEST"]}),
            json!({"watchlist": ["SYN-A.XTEST"], "depth_focus": null}),
            json!({"id": "default", "revision": 1, "instrument_ids": ["SYN-A.XTEST"]}),
        ),
        (
            FOCUS,
            json!({"action": "set", "instrument_id": "SYN-B.XTEST"}),
            json!({"watchlist": ["SYN-A.XTEST"], "depth_focus": "SYN-B.XTEST"}),
            json!({"instrument_id": "SYN-B.XTEST"}),
        ),
    ];
    for (path, body, _, _) in &changes {
        let (status, _, _) =
            market_request(&app, None, Method::PUT, path, Some(body.clone())).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    assert!(requests.try_recv().is_err());
    assert_eq!(read_watchlist(&app, &auth).await["revision"], 0);
    assert_eq!(
        market_request(&app, Some(&auth), Method::GET, FOCUS, None)
            .await
            .2,
        json!({"instrument_id": null})
    );
    for (path, body, demand, expected) in changes {
        let (status, headers, saved) =
            market_request(&app, Some(&auth), Method::PUT, path, Some(body)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[DEMAND_STATE], "disabled");
        assert_eq!(saved, expected);
        assert_eq!(requests.try_recv().unwrap(), demand);
    }
    assert!(requests.try_recv().is_err());
}
