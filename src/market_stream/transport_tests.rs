use super::*;
use crate::market_preferences::SUBSCRIPTIONS;
use futures_util::{SinkExt as _, StreamExt as _};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::{net::TcpListener, task::JoinHandle, time::timeout};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Error as SocketError, Message as SocketMessage, client::IntoClientRequest},
};

type Client = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

struct Browser {
    cookie: HeaderValue,
    csrf: String,
}

struct Fixture {
    gateway: String,
    client: reqwest::Client,
    bearer: HeaderValue,
    lagging: Arc<AtomicBool>,
    upstream_available: Arc<AtomicBool>,
    upstream_task: JoinHandle<()>,
    gateway_task: JoinHandle<()>,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn start(allow_second_snapshot: bool) -> Self {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_url = format!("http://{}", gateway.local_addr().unwrap());
        let lagging = Arc::new(AtomicBool::new(false));
        let upstream_available = Arc::new(AtomicBool::new(true));
        let upstream_app = Router::new()
            .route(
                "/healthz/ready",
                get({
                    let upstream_available = upstream_available.clone();
                    move |headers: HeaderMap| {
                        let upstream_available = upstream_available.clone();
                        async move {
                            assert_private_proxy(&headers);
                            let status = if upstream_available.load(Ordering::SeqCst) {
                                StatusCode::OK
                            } else {
                                StatusCode::BAD_GATEWAY
                            };
                            (
                                status,
                                Json(finance_client::models::Health::new(Vec::new(), "ok".into())),
                            )
                        }
                    }
                }),
            )
            .route(
                "/api/v1/market/wal",
                get({
                    let lagging = lagging.clone();
                    move |Query(params): Query<BTreeMap<String, String>>, headers: HeaderMap| {
                        let lagging = lagging.clone();
                        async move {
                            assert_private_proxy(&headers);
                            assert_eq!(params["source"], "synthetic");
                            assert_eq!(params["limit"], "64");
                            let mut reply = match params["after"].as_str() {
                                "0" => page(1, 1, None, vec![snapshot(1, 1, SEQ)]),
                                "1" if allow_second_snapshot => {
                                    page(2, 2, None, vec![snapshot(2, 1, SEQ + 1)])
                                }
                                "1" => page(1, 1, None, Vec::new()),
                                "2" if lagging.load(Ordering::SeqCst) => {
                                    page(516, 516, Some(3), vec![delta(3, 1, SEQ + 2)])
                                }
                                "2" => page(2, 2, None, Vec::new()),
                                _ => panic!("unexpected WAL resume position"),
                            };
                            if params["product"] == "record_only" {
                                reply.access.display_live = false;
                                reply.access.product = "record_only".into();
                                for record in &mut reply.records {
                                    record.event.product = "record_only".into();
                                }
                            } else {
                                assert_eq!(params["product"], "book");
                            }
                            Json(reply)
                        }
                    }
                }),
            );
        let mut fixture = StreamTestApp::new();
        let mut config = fixture.state.config.finance.clone();
        config.base_url = format!("http://{}", upstream.local_addr().unwrap());
        config.max_retries = 0;
        fixture.state.finance = Finance::new(&config).unwrap();
        let app = router(fixture.state);
        let upstream_task =
            tokio::spawn(async move { axum::serve(upstream, upstream_app).await.unwrap() });
        let gateway_task = tokio::spawn(async move { axum::serve(gateway, app).await.unwrap() });
        Self {
            gateway: gateway_url,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(IO_TIMEOUT)
                .build()
                .unwrap(),
            bearer: fixture.bearer,
            lagging,
            upstream_available,
            upstream_task,
            gateway_task,
            _directory: fixture._directory,
        }
    }

    async fn login(&self) -> Browser {
        let response = self
            .client
            .post(format!("{}{SESSION_PATH}", self.gateway))
            .header(header::AUTHORIZATION, self.bearer.clone())
            .header(header::ORIGIN, ORIGIN)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let mut cookie = HeaderValue::from_str(
            response.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap(),
        )
        .unwrap();
        cookie.set_sensitive(true);
        Browser {
            cookie,
            csrf: response.json::<Value>().await.unwrap()["csrf_token"]
                .as_str()
                .unwrap()
                .into(),
        }
    }

    async fn logout(&self, browser: &Browser) {
        let response = self
            .client
            .delete(format!("{}{SESSION_PATH}", self.gateway))
            .header(header::COOKIE, browser.cookie.clone())
            .header(header::ORIGIN, ORIGIN)
            .header("x-csrf-token", &browser.csrf)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    fn upgrade_request(&self, browser: Option<&Browser>) -> Request<()> {
        let mut request = format!(
            "ws://{}{MARKET_STREAM}",
            self.gateway.strip_prefix("http://").unwrap()
        )
        .into_client_request()
        .unwrap();
        if let Some(browser) = browser {
            request
                .headers_mut()
                .insert(header::COOKIE, browser.cookie.clone());
            request
                .headers_mut()
                .insert(header::ORIGIN, HeaderValue::from_static(ORIGIN));
        }
        request
    }

    async fn connect(&self, browser: &Browser) -> Client {
        let (client, response) = timeout(
            IO_TIMEOUT,
            connect_async(self.upgrade_request(Some(browser))),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        client
    }

    async fn quota_active(&self, browser: &Browser) -> usize {
        let response = self
            .client
            .get(format!("{}{SUBSCRIPTIONS}", self.gateway))
            .header(header::COOKIE, browser.cookie.clone())
            .header(header::ORIGIN, ORIGIN)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.json::<Value>().await.unwrap();
        assert_eq!(body["web"]["limit"], 8);
        let active = body["web"]["active"].as_u64().unwrap();
        assert_eq!(body["web"]["available"], 8 - active);
        active.try_into().unwrap()
    }

    async fn wait_for_quota(&self, browser: &Browser, active: usize) {
        timeout(IO_TIMEOUT, async {
            loop {
                if self.quota_active(browser).await == active {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.upstream_task.abort();
        self.gateway_task.abort();
    }
}

fn assert_private_proxy(headers: &HeaderMap) {
    assert!(!headers.contains_key(header::AUTHORIZATION));
    assert!(!headers.contains_key(header::COOKIE));
}

async fn expect_rejected(request: Request<()>, status: StatusCode) {
    let response = timeout(IO_TIMEOUT, connect_async(request)).await.unwrap();
    match response {
        Err(SocketError::Http(response)) => {
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        }
        _ => panic!("expected WebSocket upgrade rejection"),
    }
}

async fn command(client: &mut Client, text: &str) {
    timeout(IO_TIMEOUT, client.send(SocketMessage::Text(text.into())))
        .await
        .unwrap()
        .unwrap();
}

async fn frame(client: &mut Client) -> Value {
    let message = timeout(IO_TIMEOUT, client.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}

async fn expect_snapshot_and_checkpoint(client: &mut Client, sequence: u64, after: &str) {
    let snapshot = frame(client).await;
    assert_eq!(snapshot["kind"], "snapshot");
    assert_eq!(snapshot["seq"], sequence.to_string());
    let checkpoint = frame(client).await;
    assert_eq!(checkpoint["type"], "checkpoint");
    assert_eq!(checkpoint["after"], after);
}

#[tokio::test]
async fn websocket_upgrades_reauthenticates_reconnect_and_releases_on_lag_reset() {
    let fixture = Fixture::start(true).await;
    expect_rejected(fixture.upgrade_request(None), StatusCode::UNAUTHORIZED).await;
    let browser = fixture.login().await;
    let mut cross_origin = fixture.upgrade_request(Some(&browser));
    cross_origin.headers_mut().insert(
        header::ORIGIN,
        HeaderValue::from_static("https://other.example.test"),
    );
    expect_rejected(cross_origin, StatusCode::FORBIDDEN).await;

    let mut first = fixture.connect(&browser).await;
    command(&mut first, SUBSCRIBE).await;
    expect_snapshot_and_checkpoint(&mut first, SEQ, "1").await;
    drop(first);
    fixture.wait_for_quota(&browser, 0).await;

    let mut resumed = fixture.connect(&browser).await;
    let subscribe = SUBSCRIBE.replace('}', ",\"after\":\"1\"}");
    command(&mut resumed, &subscribe).await;
    expect_snapshot_and_checkpoint(&mut resumed, SEQ + 1, "2").await;
    fixture.lagging.store(true, Ordering::SeqCst);
    let reset = frame(&mut resumed).await;
    assert_eq!(reset["type"], "reset");
    assert_eq!(reset["reason"], "consumer_lagged");
    assert_eq!(fixture.quota_active(&browser).await, 0);

    fixture.logout(&browser).await;
    assert_eq!(frame(&mut resumed).await["code"], "authorization_revoked");
    expect_rejected(
        fixture.upgrade_request(Some(&browser)),
        StatusCode::UNAUTHORIZED,
    )
    .await;
}

#[tokio::test]
async fn websocket_browser_subscriptions_exhaust_and_release_shared_quota() {
    let fixture = Fixture::start(false).await;
    let browser = fixture.login().await;
    let another_session = fixture.login().await;
    let mut clients = Vec::new();
    for _ in 0..8 {
        let mut client = fixture.connect(&browser).await;
        command(&mut client, SUBSCRIBE).await;
        expect_snapshot_and_checkpoint(&mut client, SEQ, "1").await;
        clients.push(client);
    }
    assert_eq!(fixture.quota_active(&another_session).await, 8);

    let mut waiting = fixture.connect(&another_session).await;
    command(&mut waiting, SUBSCRIBE).await;
    assert_eq!(
        frame(&mut waiting).await["code"],
        "subscription_quota_exceeded"
    );
    assert_eq!(fixture.quota_active(&browser).await, 8);

    command(&mut clients[0], r#"{"type":"unsubscribe"}"#).await;
    assert_eq!(frame(&mut clients[0]).await["type"], "unsubscribed");
    assert_eq!(fixture.quota_active(&another_session).await, 7);
    command(&mut waiting, SUBSCRIBE).await;
    expect_snapshot_and_checkpoint(&mut waiting, SEQ, "1").await;
    assert_eq!(fixture.quota_active(&browser).await, 8);

    let denied = SUBSCRIBE.replace("\"book\"", "\"record_only\"");
    command(&mut waiting, &denied).await;
    let error = frame(&mut waiting).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["code"], "product_unavailable");
    assert_eq!(fixture.quota_active(&another_session).await, 7);
}

#[tokio::test]
async fn websocket_upgrade_reports_503_only_after_finance_failure() {
    let fixture = Fixture::start(false).await;
    let browser = fixture.login().await;
    fixture.upstream_available.store(false, Ordering::SeqCst);
    expect_rejected(
        fixture.upgrade_request(Some(&browser)),
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
}
