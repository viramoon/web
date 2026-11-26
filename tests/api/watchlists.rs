use super::postgres::Cluster;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use serde_json::{Value, json};
use std::{io::Write, path::Path, sync::Arc, time::Duration};
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
    let request = Request::builder()
        .method(method)
        .uri(format!("/api/v1/watchlists/{path}"))
        .header(header::AUTHORIZATION, authorization)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 8192).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn postgres_watchlists_persist_order_and_compare_revisions_atomically() {
    let cluster = Cluster::create();
    let mut fixture = tempfile::NamedTempFile::new().unwrap();
    let token = Uuid::new_v4();
    writeln!(fixture, "WEB_ADMIN_TOKEN={token}").unwrap();
    let config = Arc::new(Settings::load(Path::new("config"), |_| None).unwrap());
    let timeout = Duration::from_secs(2);
    let db = Database::new(&cluster.url(), timeout, timeout).unwrap();
    db.migrate().await.unwrap();
    let state = AppState {
        db,
        finance: Finance::new(&config.finance).unwrap(),
        write_access: WriteAccess::load(fixture.path(), 10, Duration::from_secs(60)).unwrap(),
        config,
        credential_store: None,
        started_at: chrono::Utc::now(),
    };
    let app = web::router(state.clone());
    let auth = format!("Bearer {token}");
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
