use crate::{AppState, auth::WriteAccess, config::Settings, database::Database, finance::Finance};
use axum::{
    Json, Router,
    body::Body,
    extract::{
        Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header},
    response::{IntoResponse as _, Sse, sse::Event},
    routing::get,
};
use futures_util::{Stream, StreamExt as _, stream};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tempfile::{NamedTempFile, TempDir};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::watch,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_tungstenite::{
    WebSocketStream, client_async,
    tungstenite::{Error as SocketError, Message as ClientMessage, client::IntoClientRequest},
};
use tower::ServiceExt as _;

const WS_PATH: &str = "/api/v1/options/flow/ws";
const SSE_PATH: &str = "/api/v1/options/flow/sse";
const SNAPSHOT_PATH: &str = "/api/v1/options/flow/snapshot";
const SESSION_PATH: &str = "/api/v1/admin/session";
const ORIGIN: &str = "https://web.example.test";
const CONTRACT: i32 = 42;
const START: &str = "2025-10-09T08:53:20Z";
const END: &str = "2025-10-09T08:53:24Z";
const DEADLINE: Duration = Duration::from_secs(5);
type Socket = WebSocketStream<UnixStream>;

#[derive(Clone, Copy)]
enum Mode {
    Live,
    Disabled,
    Lagged,
    DeniedAccess,
    DeniedCoverage,
    WrongContract,
}

struct Replay {
    examples: Value,
    release: watch::Sender<bool>,
    calls: AtomicUsize,
    mode: Mode,
}

impl Replay {
    fn new(mode: Mode) -> Self {
        // F16 generates these observations from invented IDs, fixed epoch 1760000000
        // and literal decimal values. The fixture only changes fields under test.
        let mut examples: Value = serde_json::from_str(include_str!(
            "../../openapi/finance.option-flow-stream-examples.json"
        ))
        .unwrap();
        let observation = &mut examples["live"][1]["data"]["observation"];
        observation["clue"]["status"] = json!("synthetic_future_status");
        observation["extra_fixture_field"] = json!(["unknown", null]);
        match mode {
            Mode::DeniedAccess => {
                examples["live"][1]["data"]["access"]["display_live"] = json!(false);
            }
            Mode::DeniedCoverage => {
                observation["coverage"]["access"]["display_live"] = json!(false)
            }
            Mode::WrongContract => observation["clue"]["con_id"] = json!(CONTRACT + 1),
            Mode::Live | Mode::Disabled | Mode::Lagged => {}
        }
        Self {
            examples,
            release: watch::channel(false).0,
            calls: AtomicUsize::new(0),
            mode,
        }
    }

    fn receive(&self, headers: &HeaderMap, con_id: i32) {
        assert_eq!(con_id, CONTRACT);
        for name in [header::AUTHORIZATION, header::COOKIE, header::ORIGIN] {
            assert!(!headers.contains_key(name));
        }
        self.calls.fetch_add(1, Ordering::Relaxed);
    }

    fn frames(&self, cursor: Option<&str>) -> impl Stream<Item = Value> + use<> {
        let (initial, after_publish, hold_open) = if matches!(self.mode, Mode::Disabled) {
            (vec![self.examples["disabled"][0].clone()], None, true)
        } else if cursor == Some(self.examples["replayed"][0]["id"].as_str().unwrap()) {
            (
                self.examples["replayed"].as_array().unwrap().clone(),
                None,
                true,
            )
        } else if cursor.is_none() {
            let lagged = matches!(self.mode, Mode::Lagged);
            let next = if lagged {
                self.examples["expired"][0].clone()
            } else {
                self.examples["live"][1].clone()
            };
            (
                vec![self.examples["live"][0].clone()],
                Some((self.release.subscribe(), next)),
                !lagged,
            )
        } else {
            (vec![self.examples["expired"][0].clone()], None, false)
        };
        stream::iter(initial)
            .chain(stream::unfold(after_publish, |pending| async move {
                let (mut release, message) = pending?;
                release.wait_for(|ready| *ready).await.ok()?;
                Some((message, None))
            }))
            .chain(stream::pending().take(usize::from(hold_open)))
    }

    fn snapshot(&self) -> Value {
        let data = &self.examples["live"][1]["data"];
        json!({
            "cursor": self.examples["live"][1]["id"],
            "snapshot": {
                "con_id": CONTRACT, "access": data["access"], "start": START, "end": END,
                "as_of": END, "items": [data["observation"]], "next_offset": null
            }
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Subscription {
    con_id: i32,
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Window {
    con_id: i32,
    start: String,
    end: String,
}

async fn finance_ws(
    State(replay): State<Arc<Replay>>,
    headers: HeaderMap,
    Query(query): Query<Subscription>,
    upgrade: WebSocketUpgrade,
) -> axum::response::Response {
    replay.receive(&headers, query.con_id);
    upgrade.on_upgrade(move |mut peer: WebSocket| async move {
        let frames = replay.frames(query.cursor.as_deref());
        tokio::pin!(frames);
        loop {
            tokio::select! {
                frame = frames.next() => {
                    let Some(frame) = frame else {
                        let _ = peer.send(Message::Close(None)).await;
                        break;
                    };
                    if peer.send(Message::Text(frame.to_string().into())).await.is_err() {
                        break;
                    }
                }
                frame = peer.recv() => match frame {
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {},
                    _ => break,
                }
            }
        }
    })
}

async fn finance_sse(
    State(replay): State<Arc<Replay>>,
    headers: HeaderMap,
    Query(query): Query<Subscription>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>> + use<>> {
    replay.receive(&headers, query.con_id);
    let cursor = headers
        .get("last-event-id")
        .map(|value| value.to_str().unwrap())
        .filter(|value| !value.is_empty())
        .or(query.cursor.as_deref());
    Sse::new(replay.frames(cursor).map(|frame| {
        let mut event = Event::default()
            .event(frame["event"].as_str().unwrap())
            .data(frame["data"].to_string());
        if let Some(id) = frame["id"].as_str() {
            event = event.id(id);
        }
        Ok(event)
    }))
}

async fn finance_snapshot(
    State(replay): State<Arc<Replay>>,
    headers: HeaderMap,
    Query(query): Query<Window>,
) -> Json<Value> {
    replay.receive(&headers, query.con_id);
    assert_eq!(query.start, START);
    assert_eq!(query.end, END);
    Json(replay.snapshot())
}

struct Browser {
    cookie: HeaderValue,
    csrf: String,
}

struct Fixture {
    _directory: TempDir,
    web_socket: PathBuf,
    bearer: HeaderValue,
    client: reqwest::Client,
    replay: Arc<Replay>,
    tasks: [JoinHandle<()>; 2],
    stop: watch::Sender<bool>,
}

fn test_state(
    directory: &Path,
    ttl: Duration,
    finance_socket: Option<&Path>,
) -> (AppState, HeaderValue) {
    let mut admin = NamedTempFile::new_in(directory).unwrap();
    let token = uuid::Uuid::new_v4().simple().to_string();
    writeln!(admin, "WEB_ADMIN_TOKEN={token}").unwrap();
    let mut bearer = HeaderValue::from_str(&format!("Bearer {token}")).unwrap();
    bearer.set_sensitive(true);
    let write_access = WriteAccess::load(admin.path(), 10, Duration::from_secs(60))
        .unwrap()
        .with_sessions(Some(ORIGIN), ttl)
        .unwrap();
    let mut settings = Settings::load(Path::new("config"), |key| match key {
        "FINANCE_BASE_URL" => Some("http://finance.example.test".into()),
        "WEB_ADMIN_TOKEN_FILE" => Some(admin.path().display().to_string()),
        "WEB_PUBLIC_ORIGIN" => Some(ORIGIN.into()),
        "WEB_STATIC_ROOT" => Some(directory.join("static").display().to_string()),
        _ => None,
    })
    .unwrap();
    settings.finance.max_retries = 0;
    settings.finance.request_timeout_ms = 700;
    let finance = match finance_socket {
        Some(socket) => Finance::new_for_unix_socket(&settings.finance, socket),
        None => {
            settings.finance.base_url = "http://127.0.0.1:1".into();
            Finance::new(&settings.finance)
        }
    }
    .unwrap();
    let state = AppState {
        finance,
        config: Arc::new(settings),
        db: Database::new(
            &format!(
                "postgresql://localhost/web_w16?host={}",
                directory.display()
            ),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap(),
        write_access,
        credential_store: None,
        started_at: chrono::Utc::now(),
    };
    (state, bearer)
}

impl Fixture {
    fn start(mode: Mode, ttl: Duration) -> Self {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let finance_socket = directory.path().join("finance.sock");
        let web_socket = directory.path().join("web.sock");
        let finance_listener = UnixListener::bind(&finance_socket).unwrap();
        let web_listener = UnixListener::bind(&web_socket).unwrap();
        let replay = Arc::new(Replay::new(mode));
        let upstream = Router::new()
            .route(WS_PATH, get(finance_ws))
            .route(SSE_PATH, get(finance_sse))
            .route(SNAPSHOT_PATH, get(finance_snapshot))
            .with_state(Arc::clone(&replay));

        let (state, bearer) = test_state(directory.path(), ttl, Some(&finance_socket));
        let (stop, shutdown) = watch::channel(false);
        let gateway = crate::router_with_shutdown(state, shutdown);
        let tasks = [
            tokio::spawn(async move { axum::serve(finance_listener, upstream).await.unwrap() }),
            tokio::spawn(async move { axum::serve(web_listener, gateway).await.unwrap() }),
        ];
        let client = reqwest::Client::builder()
            .unix_socket(web_socket.as_path())
            .build()
            .unwrap();
        Self {
            _directory: directory,
            web_socket,
            bearer,
            client,
            replay,
            tasks,
            stop,
        }
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("http://web.example.test{path}"))
    }

    fn subscribe(
        &self,
        request: reqwest::RequestBuilder,
        cursor: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let request = request.query(&[("con_id", CONTRACT)]);
        if let Some(cursor) = cursor {
            return request.query(&[("cursor", cursor)]);
        }
        request
    }

    async fn login(&self) -> Browser {
        let response = send(
            self.request(Method::POST, SESSION_PATH)
                .header(header::AUTHORIZATION, &self.bearer)
                .header(header::ORIGIN, ORIGIN),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let cookie = session_cookie(response.headers());
        let body: Value = response.json().await.unwrap();
        Browser {
            cookie,
            csrf: body["csrf_token"].as_str().unwrap().into(),
        }
    }

    fn ws_request(&self, cursor: Option<&str>) -> Request<()> {
        let mut url = url::Url::parse(&format!("ws://web.example.test{WS_PATH}")).unwrap();
        url.query_pairs_mut()
            .append_pair("con_id", &CONTRACT.to_string());
        if let Some(cursor) = cursor {
            url.query_pairs_mut().append_pair("cursor", cursor);
        }
        url.as_str().into_client_request().unwrap()
    }

    async fn ws_handshake(
        &self,
        request: Request<()>,
    ) -> Result<
        (
            Socket,
            tokio_tungstenite::tungstenite::handshake::client::Response,
        ),
        SocketError,
    > {
        timeout(DEADLINE, async {
            client_async(
                request,
                UnixStream::connect(&self.web_socket).await.unwrap(),
            )
            .await
        })
        .await
        .unwrap()
    }

    async fn websocket(&self, authorization: HeaderMap, cursor: Option<&str>) -> Socket {
        let mut request = self.ws_request(cursor);
        request.headers_mut().extend(authorization);
        let (socket, response) = self.ws_handshake(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        socket
    }

    async fn snapshot(&self) -> reqwest::Response {
        send(
            self.request(Method::GET, SNAPSHOT_PATH)
                .header(header::AUTHORIZATION, &self.bearer)
                .query(&[
                    ("con_id", CONTRACT.to_string()),
                    ("start", START.into()),
                    ("end", END.into()),
                ]),
        )
        .await
    }

    fn bearer_auth(&self) -> HeaderMap {
        HeaderMap::from_iter([(header::AUTHORIZATION, self.bearer.clone())])
    }

    fn sse_request(
        &self,
        authorization: HeaderMap,
        cursor: Option<&str>,
    ) -> reqwest::RequestBuilder {
        self.subscribe(self.request(Method::GET, SSE_PATH), cursor)
            .headers(authorization)
    }

    fn calls(&self) -> usize {
        self.replay.calls.load(Ordering::Relaxed)
    }
}

fn session_cookie(headers: &HeaderMap) -> HeaderValue {
    let value = headers[header::SET_COOKIE].to_str().unwrap();
    let mut cookie = HeaderValue::from_str(value.split(';').next().unwrap()).unwrap();
    cookie.set_sensitive(true);
    cookie
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Browser {
    fn auth(&self, origin: &'static str) -> HeaderMap {
        HeaderMap::from_iter([
            (header::COOKIE, self.cookie.clone()),
            (header::ORIGIN, HeaderValue::from_static(origin)),
        ])
    }
}

async fn send(request: reqwest::RequestBuilder) -> reqwest::Response {
    timeout(DEADLINE, request.send()).await.unwrap().unwrap()
}

async fn socket_message(socket: &mut Socket) -> Option<Result<ClientMessage, SocketError>> {
    timeout(DEADLINE, socket.next()).await.unwrap()
}

async fn ws_frame(socket: &mut Socket) -> Value {
    let message = socket_message(socket).await.unwrap().unwrap();
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}

async fn ws_closed(socket: &mut Socket) {
    loop {
        match socket_message(socket).await {
            None
            | Some(Ok(ClientMessage::Close(_)))
            | Some(Err(SocketError::Protocol(
                tokio_tungstenite::tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
            ))) => break,
            Some(Ok(ClientMessage::Ping(_) | ClientMessage::Pong(_))) => {}
            _ => panic!("expected the stream to close without additional data"),
        }
    }
}

struct SseBody {
    response: reqwest::Response,
    buffered: Vec<u8>,
}

impl SseBody {
    async fn open(request: reqwest::RequestBuilder) -> Self {
        let response = send(request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        Self {
            response,
            buffered: Vec::new(),
        }
    }

    async fn next(&mut self) -> Option<Value> {
        timeout(DEADLINE, async {
            loop {
                if let Some(end) = self.buffered.windows(2).position(|pair| pair == b"\n\n") {
                    let bytes: Vec<u8> = self.buffered.drain(..end + 2).collect();
                    let text = std::str::from_utf8(&bytes).unwrap();
                    let values =
                        |name| text.lines().filter_map(move |line| line.strip_prefix(name));
                    let data = values("data: ").collect::<Vec<_>>().join("\n");
                    if data.is_empty() {
                        continue;
                    }
                    let mut frame = json!({
                        "event": values("event: ").next().unwrap(),
                        "data": serde_json::from_str::<Value>(&data).unwrap(),
                    });
                    if let Some(id) = values("id: ").next() {
                        frame["id"] = json!(id);
                    }
                    return Some(frame);
                }
                let chunk = self.response.chunk().await.unwrap()?;
                self.buffered.extend_from_slice(&chunk);
            }
        })
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn pushes_live_observations_and_resumes_both_transports_without_losing_precision() {
    let fixture = Fixture::start(Mode::Live, Duration::from_secs(60));
    let mut socket = fixture.websocket(fixture.bearer_auth(), None).await;
    let mut sse = SseBody::open(fixture.sse_request(fixture.bearer_auth(), None)).await;
    let initial = &fixture.replay.examples["live"][0];
    assert_eq!(&ws_frame(&mut socket).await, initial);
    assert_eq!(sse.next().await.as_ref(), Some(initial));

    // The upstream sends its next observation only after both downstreams see status.
    fixture.replay.release.send_replace(true);
    let expected = &fixture.replay.examples["live"][1];
    let observed = ws_frame(&mut socket).await;
    assert_eq!(&observed, expected);
    assert_eq!(sse.next().await.as_ref(), Some(expected));
    assert_eq!(observed["data"]["seq"], "18446744073709551614");
    assert_eq!(
        observed["data"]["observation"]["clue"]["observation"]["evidence"]["sampled_last_price"]["value"],
        "1.000000000000000001"
    );

    drop(socket);
    drop(sse);
    let cursor = observed["id"].as_str().unwrap();
    let mut socket = fixture.websocket(fixture.bearer_auth(), Some(cursor)).await;
    let query = fixture.sse_request(fixture.bearer_auth(), Some(cursor));
    let mut fallback = SseBody::open(query.header("last-event-id", "")).await;
    let precedence = fixture.sse_request(fixture.bearer_auth(), Some("stale"));
    let mut sse = SseBody::open(precedence.header("last-event-id", cursor)).await;
    for expected in fixture.replay.examples["replayed"].as_array().unwrap() {
        assert_eq!(&ws_frame(&mut socket).await, expected);
        assert_eq!(sse.next().await.as_ref(), Some(expected));
        assert_eq!(fallback.next().await.as_ref(), Some(expected));
    }
}

#[tokio::test]
async fn expired_cursor_resets_both_transports_and_snapshot_restores_resume() {
    let fixture = Fixture::start(Mode::Live, Duration::from_secs(60));
    let mut socket = fixture
        .websocket(fixture.bearer_auth(), Some("stale"))
        .await;
    let request = fixture.sse_request(fixture.bearer_auth(), None);
    let mut sse = SseBody::open(request.header("last-event-id", "stale")).await;
    let expected = &fixture.replay.examples["expired"][0];
    assert_eq!(&ws_frame(&mut socket).await, expected);
    assert_eq!(sse.next().await.as_ref(), Some(expected));
    assert_eq!(expected["data"]["snapshot"], SNAPSHOT_PATH);
    ws_closed(&mut socket).await;
    assert_eq!(sse.next().await, None);

    let response = fixture.snapshot().await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let snapshot: Value = response.json().await.unwrap();
    assert_eq!(snapshot, fixture.replay.snapshot());
    let cursor = snapshot["cursor"].as_str().unwrap();
    let mut resumed = fixture.websocket(fixture.bearer_auth(), Some(cursor)).await;
    assert_eq!(ws_frame(&mut resumed).await["id"], cursor);
    assert_eq!(
        ws_frame(&mut resumed).await,
        fixture.replay.examples["replayed"][1]
    );
}

#[tokio::test]
async fn relays_upstream_lag_reset_during_an_active_subscription() {
    let fixture = Fixture::start(Mode::Lagged, Duration::from_secs(60));
    let mut socket = fixture.websocket(fixture.bearer_auth(), None).await;
    let mut sse = SseBody::open(fixture.sse_request(fixture.bearer_auth(), None)).await;
    let initial = &fixture.replay.examples["live"][0];
    assert_eq!(&ws_frame(&mut socket).await, initial);
    assert_eq!(sse.next().await.as_ref(), Some(initial));

    fixture.replay.release.send_replace(true);
    let reset = &fixture.replay.examples["expired"][0];
    assert_eq!(&ws_frame(&mut socket).await, reset);
    assert_eq!(sse.next().await.as_ref(), Some(reset));
    ws_closed(&mut socket).await;
    assert_eq!(sse.next().await, None);
}

#[tokio::test]
async fn reauthenticates_after_logout_and_closes_idle_sessions_for_both_transports() {
    let fixture = Fixture::start(Mode::Disabled, Duration::from_secs(60));
    let browser = fixture.login().await;
    let mut socket = fixture.websocket(browser.auth(ORIGIN), None).await;
    let mut sse = SseBody::open(fixture.sse_request(browser.auth(ORIGIN), None)).await;
    let disabled = &fixture.replay.examples["disabled"][0];
    assert_eq!(&ws_frame(&mut socket).await, disabled);
    assert_eq!(sse.next().await.as_ref(), Some(disabled));
    // A disabled feed can outlive the ordinary upstream HTTP request deadline.
    sleep(Duration::from_millis(800)).await;
    assert!(
        timeout(Duration::from_millis(30), socket.next())
            .await
            .is_err()
    );
    assert!(
        timeout(Duration::from_millis(30), sse.response.chunk())
            .await
            .is_err()
    );

    let logout = send(
        fixture
            .request(Method::DELETE, SESSION_PATH)
            .headers(browser.auth(ORIGIN))
            .header("x-csrf-token", browser.csrf.as_str()),
    )
    .await;
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);
    ws_closed(&mut socket).await;
    assert_eq!(sse.next().await, None);
    let mut request = fixture.ws_request(None);
    request.headers_mut().extend(browser.auth(ORIGIN));
    expect_ws_denied(&fixture, request, StatusCode::UNAUTHORIZED).await;
    let resumed = fixture.sse_request(browser.auth(ORIGIN), Some("stale"));
    assert_eq!(send(resumed).await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(fixture.calls(), 2);
}

#[tokio::test]
async fn session_expiry_and_shutdown_end_active_streams() {
    let fixture = Fixture::start(Mode::Disabled, Duration::from_secs(2));
    let browser = fixture.login().await;
    let mut expiring = SseBody::open(fixture.sse_request(browser.auth(ORIGIN), None)).await;
    assert_eq!(expiring.next().await.unwrap()["data"]["state"], "disabled");
    assert_eq!(expiring.next().await, None);
    let reconnect = fixture.sse_request(browser.auth(ORIGIN), Some("stale"));
    assert_eq!(send(reconnect).await.status(), StatusCode::UNAUTHORIZED);

    let mut sse = SseBody::open(fixture.sse_request(fixture.bearer_auth(), None)).await;
    let mut socket = fixture.websocket(fixture.bearer_auth(), None).await;
    assert!(sse.next().await.is_some());
    assert_eq!(ws_frame(&mut socket).await["data"]["state"], "disabled");
    fixture.stop.send_replace(true);
    assert_eq!(sse.next().await, None);
    ws_closed(&mut socket).await;
}

async fn expect_ws_denied(fixture: &Fixture, request: Request<()>, status: StatusCode) {
    let Err(SocketError::Http(response)) = fixture.ws_handshake(request).await else {
        panic!("expected HTTP error before websocket upgrade");
    };
    assert_eq!(response.status(), status);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn rejects_anonymous_cross_origin_and_ambiguous_requests_before_upstream() {
    let fixture = Fixture::start(Mode::Live, Duration::from_secs(60));
    expect_ws_denied(&fixture, fixture.ws_request(None), StatusCode::UNAUTHORIZED).await;
    let browser = fixture.login().await;
    let cross_origin = browser.auth("https://other.example.test");
    let mut request = fixture.ws_request(None);
    request.headers_mut().extend(cross_origin.clone());
    expect_ws_denied(&fixture, request, StatusCode::FORBIDDEN).await;
    for path in [WS_PATH, SSE_PATH, SNAPSHOT_PATH] {
        for method in [Method::GET, Method::HEAD] {
            for (auth, status) in [
                (HeaderMap::new(), StatusCode::UNAUTHORIZED),
                (cross_origin.clone(), StatusCode::FORBIDDEN),
            ] {
                let response = send(fixture.request(method.clone(), path).headers(auth)).await;
                assert_eq!(response.status(), status);
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            }
        }
    }
    let duplicate = fixture
        .sse_request(fixture.bearer_auth(), None)
        .header("last-event-id", "first")
        .header("last-event-id", "second");
    assert_eq!(send(duplicate).await.status(), StatusCode::BAD_REQUEST);
    let unknown = fixture
        .sse_request(fixture.bearer_auth(), None)
        .query(&[("unrecognized", "fixture")]);
    assert_eq!(send(unknown).await.status(), StatusCode::BAD_REQUEST);
    let mut malformed = fixture.ws_request(None);
    malformed.headers_mut().extend(fixture.bearer_auth());
    *malformed.uri_mut() = format!("ws://web.example.test{WS_PATH}?con_id=0")
        .parse()
        .unwrap();
    expect_ws_denied(&fixture, malformed, StatusCode::BAD_REQUEST).await;
    assert_eq!(fixture.calls(), 0);
}

#[tokio::test]
async fn shares_connection_quota_and_releases_slots_on_disconnect() {
    let fixture = Fixture::start(Mode::Disabled, Duration::from_secs(60));
    let mut sockets = Vec::new();
    let mut streams = Vec::new();
    for _ in 0..8 {
        let mut socket = fixture.websocket(fixture.bearer_auth(), None).await;
        assert_eq!(ws_frame(&mut socket).await["data"]["state"], "disabled");
        sockets.push(socket);
        let mut sse = SseBody::open(fixture.sse_request(fixture.bearer_auth(), None)).await;
        assert_eq!(sse.next().await.unwrap()["data"]["state"], "disabled");
        streams.push(sse);
    }
    let rejected = send(fixture.sse_request(fixture.bearer_auth(), None)).await;
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    let mut request = fixture.ws_request(None);
    request.headers_mut().extend(fixture.bearer_auth());
    expect_ws_denied(&fixture, request, StatusCode::TOO_MANY_REQUESTS).await;
    assert_eq!(fixture.calls(), 16);

    drop(sockets.pop());
    timeout(DEADLINE, async {
        loop {
            let response = send(fixture.sse_request(fixture.bearer_auth(), None)).await;
            if response.status() == StatusCode::OK {
                break;
            }
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(fixture.calls(), 17);
}

#[tokio::test]
async fn throttles_reconnects_separately_from_the_market_rest_quota() {
    let fixture = Fixture::start(Mode::Live, Duration::from_secs(60));
    for _ in 0..120 {
        let request = fixture.sse_request(fixture.bearer_auth(), Some("stale"));
        let mut sse = SseBody::open(request).await;
        assert_eq!(sse.next().await.unwrap()["event"], "reset_required");
        assert_eq!(sse.next().await, None);
    }
    let rejected = send(fixture.sse_request(fixture.bearer_auth(), None)).await;
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(rejected.headers().contains_key(header::RETRY_AFTER));
    let mut request = fixture.ws_request(None);
    request.headers_mut().extend(fixture.bearer_auth());
    expect_ws_denied(&fixture, request, StatusCode::TOO_MANY_REQUESTS).await;
    assert_eq!(fixture.calls(), 120);

    assert_eq!(fixture.snapshot().await.status(), StatusCode::OK);
    for _ in 1..120 {
        let request = fixture
            .request(Method::GET, crate::market_preferences::SUBSCRIPTIONS)
            .headers(fixture.bearer_auth());
        assert_eq!(send(request).await.status(), StatusCode::OK);
    }
    let rejected = fixture.snapshot().await;
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(rejected.headers().contains_key(header::RETRY_AFTER));
    assert_eq!(fixture.calls(), 121);
}

#[tokio::test]
async fn refuses_non_displayable_and_mismatched_observations_and_snapshots() {
    for (mode, status) in [
        (Mode::DeniedAccess, StatusCode::FORBIDDEN),
        (Mode::DeniedCoverage, StatusCode::FORBIDDEN),
        (Mode::WrongContract, StatusCode::BAD_GATEWAY),
    ] {
        let fixture = Fixture::start(mode, Duration::from_secs(60));
        let mut socket = fixture.websocket(fixture.bearer_auth(), None).await;
        let initial = &fixture.replay.examples["live"][0];
        assert_eq!(&ws_frame(&mut socket).await, initial);
        let mut sse = SseBody::open(fixture.sse_request(fixture.bearer_auth(), None)).await;
        assert_eq!(sse.next().await.as_ref(), Some(initial));

        fixture.replay.release.send_replace(true);
        ws_closed(&mut socket).await;
        assert_eq!(sse.next().await, None);
        assert_eq!(fixture.snapshot().await.status(), status);
    }
}

#[tokio::test]
async fn router_enforces_stream_authorization_before_parsing_queries() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let (state, bearer) = test_state(directory.path(), Duration::from_secs(60), None);
    let app = crate::router(state);
    let send = |request: axum::http::request::Builder| {
        app.clone().oneshot(request.body(Body::empty()).unwrap())
    };
    let login = send(
        Request::post(SESSION_PATH)
            .header(header::AUTHORIZATION, &bearer)
            .header(header::ORIGIN, ORIGIN),
    )
    .await
    .unwrap();
    assert_eq!(login.status(), StatusCode::CREATED);
    let cookie = session_cookie(login.headers());
    for path in [WS_PATH, SSE_PATH, SNAPSHOT_PATH] {
        for method in [Method::GET, Method::HEAD] {
            let request = || Request::builder().method(method.clone()).uri(path);
            let anonymous = send(request()).await.unwrap();
            assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(anonymous.headers()[header::CACHE_CONTROL], "no-store");
            let cross_origin = send(
                request()
                    .header(header::COOKIE, &cookie)
                    .header(header::ORIGIN, "https://other.example.test"),
            )
            .await
            .unwrap();
            assert_eq!(cross_origin.status(), StatusCode::FORBIDDEN);
            let bearer = send(request().header(header::AUTHORIZATION, &bearer))
                .await
                .unwrap();
            assert_eq!(bearer.status(), StatusCode::BAD_REQUEST);
        }
    }
}

#[test]
fn f16_wire_gates_observation_access_and_preserves_exact_values_in_its_envelope() {
    let query = super::SnapshotQuery {
        con_id: CONTRACT,
        start: START.parse().unwrap(),
        end: END.parse().unwrap(),
    };
    let replay = Replay::new(Mode::Live);
    let json = replay.examples["live"][1].to_string();
    let envelope: super::wire::Envelope<'_> = serde_json::from_str(&json).unwrap();
    assert_eq!(
        super::wire::check(envelope.event, envelope.id, envelope.data.get(), CONTRACT).ok(),
        Some(false)
    );
    assert!(envelope.data.get().contains("1.000000000000000001"));
    assert!(envelope.data.get().contains("18446744073709551614"));
    assert!(envelope.data.get().contains("synthetic_future_status"));
    assert!(super::wire::check_snapshot(&replay.snapshot().to_string(), &query).is_ok());

    for (mode, status) in [
        (Mode::DeniedAccess, StatusCode::FORBIDDEN),
        (Mode::DeniedCoverage, StatusCode::FORBIDDEN),
        (Mode::WrongContract, StatusCode::BAD_GATEWAY),
    ] {
        let replay = Replay::new(mode);
        let json = replay.examples["live"][1].to_string();
        let envelope: super::wire::Envelope<'_> = serde_json::from_str(&json).unwrap();
        let error = super::wire::check(envelope.event, envelope.id, envelope.data.get(), CONTRACT)
            .unwrap_err();
        assert_eq!(error.into_response().status(), status);
        let error =
            super::wire::check_snapshot(&replay.snapshot().to_string(), &query).unwrap_err();
        assert_eq!(error.into_response().status(), status);
    }
}

#[test]
fn f16_limits_reserve_active_slots_and_throttle_repeat_openings() {
    let limits = super::Limits {
        connections: Arc::new(tokio::sync::Semaphore::new(16)),
        opens: std::sync::Mutex::default(),
    };
    let mut active = (0..16)
        .map(|_| limits.acquire().unwrap())
        .collect::<Vec<_>>();
    let rejected = limits.acquire().unwrap_err().into_response();
    assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
    drop(active.pop());
    for _ in 16..120 {
        drop(limits.acquire().unwrap());
    }
    let throttled = limits.acquire().unwrap_err().into_response();
    assert_eq!(throttled.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(throttled.headers().contains_key(header::RETRY_AFTER));
}

#[test]
fn openapi_documents_authenticated_f16_transports_and_recovery() {
    let spec = crate::openapi();
    for path in [WS_PATH, SSE_PATH, SNAPSHOT_PATH] {
        let operation = &spec["paths"][path]["get"];
        let authorization = operation["security"].as_array().unwrap();
        assert_eq!(authorization.len(), 2);
        assert!(authorization.contains(&json!({"adminSession": []})));
        assert!(authorization.contains(&json!({"adminBearer": []})));
        for code in ["400", "401", "403", "429"] {
            assert!(operation["responses"][code].is_object());
        }
    }
    let paths = &spec["paths"];
    assert!(paths[WS_PATH]["get"]["responses"]["101"].is_object());
    let sse = &paths[SSE_PATH]["get"];
    assert!(sse["responses"]["200"]["content"]["text/event-stream"].is_object());
    assert!(
        sse["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|parameter| parameter["name"] == "Last-Event-ID" && parameter["in"] == "header")
    );
    assert_eq!(
        paths[SNAPSHOT_PATH]["get"]["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/OptionFlowStreamSnapshot"
    );
}
