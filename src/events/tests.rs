use super::*;
use crate::{auth, config::Settings, database::Database, finance::Finance};
use axum::{
    Json,
    body::{Body, BodyDataStream, to_bytes},
    http::Request,
};
use finance_client::models::{Job, JobStatus, Page};
use futures_util::StreamExt;
use serde_json::Value;
use std::{fs, io::Write, os::unix::fs::PermissionsExt, path::Path};
use tempfile::{NamedTempFile, TempDir};
use tower::ServiceExt;

const ORIGIN: &str = "https://events.example.test";
const SESSION: &str = "/api/v1/admin/session";

fn fixture(finance_address: &str) -> (AppState, HeaderValue, TempDir) {
    fixture_with_origin(finance_address, Some(ORIGIN))
}

fn fixture_with_origin(
    finance_address: &str,
    public_origin: Option<&str>,
) -> (AppState, HeaderValue, TempDir) {
    let temporary = tempfile::tempdir_in("/tmp").unwrap();
    let mut token_file = NamedTempFile::new_in(&temporary).unwrap();
    fs::set_permissions(token_file.path(), fs::Permissions::from_mode(0o600)).unwrap();
    let token = Uuid::new_v4().simple().to_string();
    writeln!(token_file, "WEB_ADMIN_TOKEN={token}").unwrap();
    let static_root = temporary.path().join("static");
    let database_url = format!(
        "postgresql://localhost/events_test?host={}",
        temporary.path().display()
    );
    let mut settings = Settings::load(Path::new("config"), |name| match name {
        "WEB_ADMIN_TOKEN_FILE" => Some(token_file.path().display().to_string()),
        "WEB_PUBLIC_ORIGIN" => public_origin.map(str::to_owned),
        "WEB_STATIC_ROOT" => Some(static_root.display().to_string()),
        "DATABASE_URL" => Some(database_url.clone()),
        "FINANCE_BASE_URL" => Some(finance_address.to_string()),
        _ => None,
    })
    .unwrap();
    settings.finance.max_retries = 0;
    let db = Database::new(
        &settings.database.url,
        Duration::from_millis(50),
        Duration::from_millis(50),
    )
    .unwrap();
    let access = WriteAccess::load(token_file.path(), 10, Duration::from_secs(60))
        .unwrap()
        .with_sessions(public_origin, Duration::from_secs(120))
        .unwrap();
    let state = AppState {
        db,
        finance: Finance::new(&settings.finance).unwrap(),
        config: Arc::new(settings),
        write_access: access,
        credential_store: None,
        started_at: chrono::Utc::now(),
    };
    let mut authorization = HeaderValue::from_str(&format!("Bearer {token}")).unwrap();
    authorization.set_sensitive(true);
    (state, authorization, temporary)
}

fn with_replay(state: AppState, hub: &Arc<Hub>) -> Router {
    hub.journal().poller_running = true;
    routes_with_hub(Arc::clone(hub))
        .route(
            SESSION,
            axum::routing::post(auth::create_session).delete(auth::delete_session),
        )
        .with_state(state)
}

fn event_request(authorization: &HeaderValue, last_id: Option<&str>) -> Request<Body> {
    let mut request = Request::get(EVENTS).header(header::AUTHORIZATION, authorization);
    if let Some(last_id) = last_id {
        request = request.header(LAST_EVENT_ID, last_id);
    }
    request.body(Body::empty()).unwrap()
}

async fn message(body: &mut BodyDataStream) -> String {
    let bytes = tokio::time::timeout(Duration::from_secs(8), body.next())
        .await
        .expect("event received before timeout")
        .expect("stream is open")
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn id(frame: &str) -> &str {
    frame
        .lines()
        .find_map(|line| line.strip_prefix("id: "))
        .unwrap()
}

fn synthetic_job(id: i64) -> EventPayload {
    EventPayload::Job {
        job: JobUpdate {
            id,
            status: JobStatus::Queued,
            attempt: 0,
        },
    }
}

#[test]
fn export_describes_sse_authentication_and_operational_frames() {
    let api = serde_json::to_value(EventsApi::openapi()).unwrap();
    let operation = &api["paths"][EVENTS]["get"];
    assert!(operation["responses"]["200"]["content"]["text/event-stream"].is_object());
    assert_eq!(
        operation["security"],
        serde_json::json!([{"adminBearer": []}, {"adminSession": []}])
    );
    let schemas = &api["components"]["schemas"];
    assert!(
        schemas["EventData"]["allOf"]
            .as_array()
            .unwrap()
            .iter()
            .any(|part| part["$ref"] == "#/components/schemas/EventPayload")
    );
    assert_eq!(
        schemas["EventPayload"]["oneOf"].as_array().unwrap().len(),
        5
    );
}

#[tokio::test]
async fn mounted_route_denies_anonymous_reconnect_and_head_without_starting_poller() {
    let (state, admin, _temporary) = fixture("http://127.0.0.1:1");
    let hub = Arc::new(Hub::new());
    let app = crate::with_controls(routes_with_hub(Arc::clone(&hub)), state);
    for method in ["GET", "HEAD"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(EVENTS)
                    .header(LAST_EVENT_ID, "claimed-prior-event")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
    let response = app
        .oneshot(
            Request::head(EVENTS)
                .header(header::AUTHORIZATION, admin)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );
    assert!(to_bytes(response.into_body(), 0).await.unwrap().is_empty());
    assert!(!hub.journal().poller_running);
    assert_eq!(hub.connections.available_permits(), MAX_CONNECTIONS);
}

#[tokio::test]
async fn resume_replays_exactly_after_cursor_and_slow_consumer_resets_to_head() {
    let (state, admin, _temporary) = fixture("http://127.0.0.1:1");
    let hub = Arc::new(Hub::new());
    let app = with_replay(state, &hub);
    let response = app
        .clone()
        .oneshot(event_request(&admin, None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let mut body = response.into_body().into_data_stream();
    let permissions = message(&mut body).await;
    assert!(permissions.contains("event: permissions"));
    assert!(permissions.contains("\"state\":\"authorized\""));
    assert!(!permissions.contains("csrf_token"));
    let frame = message(&mut body).await;
    assert!(frame.contains("snapshot_required"));
    let original = id(&frame).to_owned();

    hub.publish(vec![synthetic_job(1), synthetic_job(2)]);
    let first = message(&mut body).await;
    assert!(first.contains("\"id\":\"1\""));
    let first_id = id(&first);
    drop(body);

    let response = app
        .clone()
        .oneshot(event_request(&admin, Some(first_id)))
        .await
        .unwrap();
    let mut resumed = response.into_body().into_data_stream();
    assert!(message(&mut resumed).await.contains("event: permissions"));
    assert!(message(&mut resumed).await.contains("\"id\":\"2\""));

    for id in 0..=RETAINED_EVENTS {
        hub.publish(vec![synthetic_job(id as i64 + 3)]);
    }
    let reset = message(&mut resumed).await;
    assert!(reset.contains("cursor_unavailable"));
    let head = hub.journal().head.to_string();
    assert_eq!(id(&reset), head);
    drop(resumed);

    for cursor in [
        original.as_str(),
        "not-a-cursor",
        "00000000-0000-0000-0000-000000000000:0",
    ] {
        let response = app
            .clone()
            .oneshot(event_request(&admin, Some(cursor)))
            .await
            .unwrap();
        let mut body = response.into_body().into_data_stream();
        message(&mut body).await;
        assert!(message(&mut body).await.contains("cursor_unavailable"));
    }
}

#[tokio::test]
async fn private_session_origin_and_revocation_are_rechecked_on_existing_and_new_streams() {
    let (state, admin, _temporary) = fixture("http://127.0.0.1:1");
    let app = with_replay(state, &Arc::new(Hub::new()));
    let session = app
        .clone()
        .oneshot(
            Request::post(SESSION)
                .header(header::AUTHORIZATION, admin)
                .header(header::ORIGIN, ORIGIN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(session.status(), StatusCode::CREATED);
    let cookie = session.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let created: Value =
        serde_json::from_slice(&to_bytes(session.into_body(), 4096).await.unwrap()).unwrap();
    let csrf = created["csrf_token"].as_str().unwrap();

    for (name, value) in [
        (header::ORIGIN.as_str(), "https://cross.example.test"),
        ("sec-fetch-site", "cross-site"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::get(EVENTS)
                    .header(header::COOKIE, &cookie)
                    .header(name, value)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    let response = app
        .clone()
        .oneshot(
            Request::get(EVENTS)
                .header(header::COOKIE, &cookie)
                .header("sec-fetch-site", "same-origin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let mut body = response.into_body().into_data_stream();
    assert!(
        message(&mut body)
            .await
            .contains("\"state\":\"authorized\"")
    );
    let reset = message(&mut body).await;
    let response = app
        .clone()
        .oneshot(
            Request::delete(SESSION)
                .header(header::COOKIE, &cookie)
                .header(header::ORIGIN, ORIGIN)
                .header("x-csrf-token", csrf)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(message(&mut body).await.contains("\"state\":\"revoked\""));
    assert!(body.next().await.is_none());

    let response = app
        .oneshot(
            Request::get(EVENTS)
                .header(header::COOKIE, &cookie)
                .header(LAST_EVENT_ID, id(&reset))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn loopback_cookie_sse_streams_only_for_same_origin_requests() {
    let (state, admin, _temporary) = fixture_with_origin("http://127.0.0.1:1", None);
    let app = with_replay(state, &Arc::new(Hub::new()));
    for host in ["127.0.0.1:8090", "localhost:8090"] {
        let origin = format!("http://{host}");
        let session = app
            .clone()
            .oneshot(
                Request::post(SESSION)
                    .header(header::AUTHORIZATION, &admin)
                    .header(header::HOST, host)
                    .header(header::ORIGIN, &origin)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(session.status(), StatusCode::CREATED);
        let cookie = session.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap();

        for (request_origin, fetch_site) in
            [(Some(origin.as_str()), None), (None, Some("same-origin"))]
        {
            let mut request = Request::get(EVENTS)
                .header(header::HOST, host)
                .header(header::COOKIE, cookie);
            if let Some(origin) = request_origin {
                request = request.header(header::ORIGIN, origin);
            }
            if let Some(site) = fetch_site {
                request = request.header("sec-fetch-site", site);
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "text/event-stream"
            );
            let mut body = response.into_body().into_data_stream();
            assert!(
                message(&mut body)
                    .await
                    .contains("\"state\":\"authorized\"")
            );
            assert!(message(&mut body).await.contains("snapshot_required"));
        }
        for (request_host, request_origin, fetch_site) in [
            (host, None, None),
            (host, None, Some("same-site")),
            (
                host,
                Some("http://example.invalid:8090"),
                Some("same-origin"),
            ),
            (host, Some("http://localhost:8091"), None),
            (
                "127.0.0.1.evil:8090",
                Some("http://127.0.0.1.evil:8090"),
                None,
            ),
        ] {
            let mut request = Request::get(EVENTS)
                .header(header::HOST, request_host)
                .header(header::COOKIE, cookie);
            if let Some(origin) = request_origin {
                request = request.header(header::ORIGIN, origin);
            }
            if let Some(site) = fetch_site {
                request = request.header("sec-fetch-site", site);
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            let body = to_bytes(response.into_body(), 4096).await.unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["code"], "origin_rejected");
        }
    }
}

#[tokio::test]
async fn sse_connection_limit_releases_on_disconnect_and_shutdown_ends_the_body() {
    let (state, admin, _temporary) = fixture("http://127.0.0.1:1");
    let hub = Arc::new(Hub::new());
    let (stop, shutdown) = watch::channel(false);
    let app = with_replay(state, &hub).layer(Extension(StreamShutdown(shutdown)));
    let mut open = Vec::new();
    for _ in 0..MAX_CONNECTIONS {
        let response = app
            .clone()
            .oneshot(event_request(&admin, None))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        open.push(response.into_body());
    }
    let exceeded = app
        .clone()
        .oneshot(event_request(&admin, None))
        .await
        .unwrap();
    assert_eq!(exceeded.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(exceeded.headers()[header::RETRY_AFTER], "3");
    drop(open.pop());
    let next = app.oneshot(event_request(&admin, None)).await.unwrap();
    assert_eq!(next.status(), StatusCode::OK);
    let mut body = next.into_body().into_data_stream();
    assert!(message(&mut body).await.contains("event: permissions"));
    stop.send_replace(true);
    assert!(body.next().await.is_none());
}

#[tokio::test]
async fn production_poller_projects_job_transitions_and_backup_without_upstream_payloads() {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", socket.local_addr().unwrap());
    let (state, admin, _temporary) = fixture(&base);
    let (stop, shutdown) = watch::channel(false);
    let (upstream_stop, mut upstream_shutdown) = watch::channel(false);
    let job = |status| Job {
        id: 92,
        status,
        error: Some(Some("synthetic internal detail".into())),
        ..Job::default()
    };
    let upstream = Router::new()
        .route(
            "/api/v1/backtests",
            get(move || async move { Json(Page::new(vec![job(JobStatus::Queued)])) }),
        )
        .route(
            "/api/v1/backtests/{id}",
            get(move || async move { Json(job(JobStatus::Running)) }),
        );
    let server = tokio::spawn(async move {
        axum::serve(socket, upstream)
            .with_graceful_shutdown(async move {
                let _ = upstream_shutdown.wait_for(|stopping| *stopping).await;
            })
            .await
            .unwrap();
    });

    let app = crate::router_with_shutdown(state, shutdown);
    let response = app.oneshot(event_request(&admin, None)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    assert!(message(&mut body).await.contains("event: permissions"));
    assert!(message(&mut body).await.contains("snapshot_required"));
    let queued = message(&mut body).await;
    assert!(queued.contains("\"id\":\"92\""));
    assert!(queued.contains("\"status\":\"queued\""));
    assert!(!queued.contains("synthetic internal detail"));
    let backup = message(&mut body).await;
    assert!(backup.contains("event: backup"));
    assert!(backup.contains("\"state\":\"disabled\""));
    let running = message(&mut body).await;
    assert!(running.contains("\"status\":\"running\""));
    assert!(!running.contains("synthetic internal detail"));
    stop.send_replace(true);
    assert!(body.next().await.is_none());
    upstream_stop.send_replace(true);
    server.await.unwrap();
}
