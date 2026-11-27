use super::*;
use crate::{auth::WriteAccess, config::Settings, database::Database, router};
use axum::{
    body::{Body, to_bytes},
    extract::OriginalUri,
    http::{HeaderMap, HeaderValue, Method, Request, header},
};
use chrono::Utc;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    fs::OpenOptions,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tower::ServiceExt;

mod synthetic;
use synthetic::*;

const ORIGIN: &str = "https://synthetic.example.test";

#[test]
fn generated_event_keeps_all_tagged_payloads_and_exact_numeric_text() {
    for (kind, payload) in [
        (
            "bar_closed",
            json!({"open_ts": "1", "open": PRICE, "high": PRICE, "low": PRICE,
                "close": PRICE, "volume": SIZE}),
        ),
        ("trade", trade()),
        (
            "snapshot",
            json!({"bids": [{"price": PRICE, "quantity": SIZE}], "asks": []}),
        ),
        (
            "delta",
            json!({"updates": [{"side": "bid", "price": PRICE, "quantity": SIZE}]}),
        ),
        (
            "correction",
            json!({"target_epoch": "7", "target_seq": POSITION,
                "replacement": {"kind": "trade", "payload": trade()}}),
        ),
        (
            "gap",
            json!({"from_ts": "1", "to_ts": "2", "reason": "rotation"}),
        ),
        ("reset", json!({"reason": "disconnected"})),
        ("heartbeat", json!({})),
        ("status", json!({"state": "degraded"})),
        (
            "perpetual_context",
            json!({"funding_rate": "-0.000100", "mark_price": PRICE, "open_interest": SIZE}),
        ),
    ] {
        let wire = event(kind, payload);
        let decoded: models::Event = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), wire, "{kind}");
    }
    let mut invalid = event("trade", trade());
    invalid["payload"]["price"] = json!(123);
    assert!(serde_json::from_value::<models::Event>(invalid).is_err());
    let mut invalid = event("trade", trade());
    invalid.as_object_mut().unwrap().remove("exchange_ts");
    assert!(serde_json::from_value::<models::Event>(invalid).is_err());
}

struct Exchange {
    request: String,
    status: StatusCode,
    value: Value,
}

impl Exchange {
    fn ok(request: String, value: Value) -> Self {
        Self {
            request,
            status: StatusCode::OK,
            value,
        }
    }
}

struct Fixture {
    server: tokio::task::JoinHandle<()>,
    script: Arc<Mutex<VecDeque<Exchange>>>,
    gateway: Gateway,
}

impl Fixture {
    async fn start(script: Vec<Exchange>) -> Self {
        let script = Arc::new(Mutex::new(VecDeque::from(script)));
        let upstream = Router::new()
            .fallback(
                |State(script): State<Arc<Mutex<VecDeque<Exchange>>>>,
                 method: Method,
                 OriginalUri(uri): OriginalUri,
                 headers: HeaderMap| async move {
                    assert_eq!(method, Method::GET);
                    assert!(!headers.contains_key(header::AUTHORIZATION));
                    assert!(!headers.contains_key(header::COOKIE));
                    assert!(!headers.contains_key(header::ORIGIN));
                    let exchange = script
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("unexpected upstream call");
                    assert_eq!(uri.to_string(), exchange.request);
                    (exchange.status, Json(exchange.value))
                },
            )
            .with_state(script.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        Self {
            server,
            script,
            gateway: Gateway::new(format!("http://{address}")),
        }
    }

    async fn get_json(&self, path: &str, status: StatusCode) -> Value {
        self.gateway.get_json(path, status).await
    }

    fn upstream_expectations_met(&self) {
        assert!(self.script.lock().unwrap().is_empty());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

struct Gateway {
    _directory: tempfile::TempDir,
    app: Router,
    authorization: HeaderValue,
}

impl Gateway {
    fn new(finance_url: String) -> Self {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let token_file = directory.path().join("fixture.env");
        let token = uuid::Uuid::new_v4().to_string();
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}")).unwrap();
        authorization.set_sensitive(true);
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&token_file)
            .unwrap()
            .write_all(format!("WEB_ADMIN_TOKEN={token}").as_bytes())
            .unwrap();
        let mut config = Settings::load(Path::new("config"), |key| {
            (key == "WEB_PUBLIC_ORIGIN").then(|| ORIGIN.into())
        })
        .unwrap();
        config.finance.base_url = finance_url;
        config.finance.max_retries = 0;
        let write_access = WriteAccess::load(&token_file, 32, Duration::from_secs(5))
            .unwrap()
            .with_sessions(
                config.security.public_origin.as_deref(),
                Duration::from_secs(300),
            )
            .unwrap();
        let db = Database::new(
            &format!(
                "postgresql://localhost/test?host={}",
                directory.path().display()
            ),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap();
        let app = router(AppState {
            db,
            finance: Finance::new(&config.finance).unwrap(),
            config: Arc::new(config),
            write_access,
            credential_store: None,
            started_at: Utc::now(),
        });
        Self {
            _directory: directory,
            app,
            authorization,
        }
    }

    async fn request(&self, method: Method, path: &str, authenticated: bool) -> Response {
        let mut request = Request::builder().method(method).uri(path);
        if authenticated {
            request = request
                .header(header::AUTHORIZATION, self.authorization.clone())
                .header(header::ORIGIN, ORIGIN)
                .header(header::COOKIE, "synthetic=browser");
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        response
    }

    async fn get_json(&self, path: &str, status: StatusCode) -> Value {
        let response = self.request(Method::GET, path, true).await;
        assert_eq!(response.status(), status);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
}

#[tokio::test]
async fn restricted_market_rest_authenticates_before_query_parsing() {
    let gateway = Gateway::new("http://127.0.0.1:1".into());
    for path in [PRODUCTS, WAL, PROJECTION, WATERMARKS, FINANCE_GAPS] {
        for method in [Method::GET, Method::HEAD] {
            let response = gateway.request(method, path, false).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
    }
    let bad_query = format!("{WAL}?source={SOURCE}&product={PRODUCT}&unknown=discard");
    let body = gateway.get_json(&bad_query, StatusCode::BAD_REQUEST).await;
    assert_eq!(body["error"]["code"], "market_query_invalid");
    assert!(!body.to_string().contains("discard"));
}

fn gaps_reply() -> Value {
    json!({
        "as_of": AS_OF, "access": access(true), "next_offset": null,
        "items": [
            {"channel": "fixture", "instrument_id": INSTRUMENT, "from_ts": "1", "to_ts": "1",
                "cause": "rotation", "outcome": null, "count": null},
            {"channel": "fixture", "instrument_id": INSTRUMENT, "from_ts": "2", "to_ts": "2",
                "cause": "source_gap", "outcome": "history_unrecoverable", "count": "0"}
        ]
    })
}

#[test]
fn generated_read_models_preserve_unknown_coverage_and_distinguish_display_permissions() {
    let gaps = gaps_reply();
    let page: models::MarketQualityPage = serde_json::from_value(gaps.clone()).unwrap();
    assert_eq!(page.items[0].count, None);
    assert_eq!(page.items[1].count.as_deref(), Some("0"));
    assert_eq!(serde_json::to_value(page).unwrap(), gaps);

    let wal = wal_reply(event("trade", trade()), false);
    let page: models::MarketWalPage = serde_json::from_value(wal.clone()).unwrap();
    assert_eq!(page.through, POSITION);
    assert!(check_product(&page.access, SOURCE, PRODUCT).is_ok());
    assert!(matches!(
        check_display(&page.access),
        Err(MarketReadError::DisplayDenied)
    ));
    assert!(check_record(&page.access, &page.records[0]).is_ok());
    assert_eq!(serde_json::to_value(page).unwrap(), wal);
}

#[test]
fn public_openapi_preserves_f1_composition_and_declares_restricted_reads() {
    let spec = crate::openapi();
    let finance: Value =
        serde_json::from_str(include_str!("../../openapi/finance.openapi.json")).unwrap();
    for name in ["Event", "Message", "MarketWalPage", "MarketQualityPage"] {
        assert_eq!(
            spec["components"]["schemas"][name],
            finance["components"]["schemas"][name]
        );
    }
    for path in [PRODUCTS, WAL, PROJECTION, WATERMARKS, FINANCE_GAPS] {
        let operation = &spec["paths"][path]["get"];
        assert_eq!(
            operation["security"],
            json!([{"adminBearer": []}, {"adminSession": []}])
        );
        assert!(operation["responses"].get("429").is_some());
    }
}

#[tokio::test]
async fn market_rest_preserves_licensed_metadata_partial_coverage_and_precision() {
    let feed = format!("source={SOURCE}&product={PRODUCT}");
    let products = format!("{PRODUCTS}?source={SOURCE}&offset=1&limit=2");
    let wal = format!("{WAL}?{feed}&after=9007199254740992&through={POSITION}&limit=1");
    let projection = format!(
        "{PROJECTION}?{feed}&channel=fixture&instrument_id={INSTRUMENT}&epoch=7&seq={POSITION}"
    );
    let watermarks = format!("{WATERMARKS}?{feed}");
    let gaps_query = format!("{feed}&from_ts=1&to_ts=3");
    let products_value = json!({"as_of": AS_OF, "items": [access(true)], "next_offset": null});
    let wal_value = wal_reply(event("trade", trade()), true);
    let projected_value = json!({
        "as_of": AS_OF, "access": access(true), "projected_through": POSITION,
        "record": wal_value["records"][0],
    });
    let watermarks_value = json!({
        "as_of": AS_OF, "access": access(true),
        "watermarks": {"source_latest": null, "local_durable": POSITION,
            "projected": "7", "remote_verified": null}
    });
    let gaps_value = gaps_reply();
    let fixture = Fixture::start(vec![
        Exchange::ok(products.clone(), products_value.clone()),
        Exchange::ok(wal.clone(), wal_value.clone()),
        Exchange::ok(projection.clone(), projected_value.clone()),
        Exchange::ok(watermarks.clone(), watermarks_value.clone()),
        Exchange::ok(format!("{UPSTREAM_GAPS}?{gaps_query}"), gaps_value.clone()),
    ])
    .await;
    for (path, expected) in [
        (products, products_value),
        (wal, wal_value),
        (projection, projected_value),
        (watermarks, watermarks_value),
        (format!("{FINANCE_GAPS}?{gaps_query}"), gaps_value),
    ] {
        assert_eq!(fixture.get_json(&path, StatusCode::OK).await, expected);
    }
    fixture.upstream_expectations_met();
}

#[tokio::test]
async fn market_event_bodies_require_display_license_and_failures_are_redacted() {
    let wal = format!("{WAL}?source={SOURCE}&product={PRODUCT}");
    let mut unlicensed = event("trade", trade());
    unlicensed["license"] = json!("fixture_wrong");
    let fixture = Fixture::start(vec![
        Exchange::ok(wal.clone(), wal_reply(event("trade", trade()), false)),
        Exchange::ok(wal.clone(), wal_reply(unlicensed, true)),
        Exchange {
            request: wal.clone(),
            status: StatusCode::NOT_FOUND,
            value: json!({"error": {"code": "source-internal-fixture", "message": "internal/fixture/path"}}),
        },
    ]).await;
    for (status, code) in [
        (StatusCode::FORBIDDEN, "market_display_denied"),
        (StatusCode::BAD_GATEWAY, "finance_invalid_response"),
        (StatusCode::NOT_FOUND, "market_data_not_found"),
    ] {
        let body = fixture.get_json(&wal, status).await;
        assert_eq!(body["error"]["code"], code);
        for marker in [
            PRICE,
            "fixture_wrong",
            "source-internal-fixture",
            "internal/fixture/path",
        ] {
            assert!(!body.to_string().contains(marker));
        }
    }
    fixture.upstream_expectations_met();
}

#[tokio::test]
async fn browser_market_views_query_real_f10a_data_and_keep_exact_values() {
    let path = format!("{WAL}?source={SOURCE}&product={PRODUCT}&after=0&limit=128");
    let query = format!("source={SOURCE}&product={PRODUCT}&channel=fixture&after=0");
    let stock = format!("{query}&instrument_id={INSTRUMENT}");
    let perpetual = format!("{query}&instrument_id=SYNTH.XHYP");
    let rows = wal_rows(vec![
        book_snapshot(),
        book_delta(),
        event(
            "bar_closed",
            json!({
                "open_ts": "1", "open": PRICE, "high": PRICE, "low": PRICE,
                "close": PRICE, "volume": SIZE
            }),
        ),
        event("trade", trade()),
        event(
            "perpetual_context",
            json!({
                "funding_rate": "-0.000100", "mark_price": PRICE, "open_interest": SIZE
            }),
        ),
    ]);
    let mut script = vec![Exchange::ok(
        "/api/v1/market/status".into(),
        json!({
            "source": SOURCE, "state": "ready", "updated_at": AS_OF,
            "instruments": [{"instrument": INSTRUMENT, "age_seconds": null, "stale": false}]
        }),
    )];
    script.extend((0..5).map(|_| Exchange::ok(path.clone(), rows.clone())));
    script.push(Exchange::ok(
        format!("{WATERMARKS}?source={SOURCE}&product={PRODUCT}"),
        json!({
            "as_of": AS_OF, "access": access(true),
            "watermarks": {"local_durable": "5", "projected": "0", "remote_verified": null, "source_latest": null}
        }),
    ));
    script.push(Exchange::ok(
        format!("{WAL}?source={SOURCE}&product={PRODUCT}&after=3&through=5&limit=2"),
        json!({
            "as_of": AS_OF, "access": access(true),
            "durable_through": "5", "through": "5", "next_after": null,
            "records": [rows["records"][3], rows["records"][4]]
        }),
    ));
    let fixture = Fixture::start(script).await;
    let instruments = fixture
        .get_json("/api/v1/market/instruments?limit=1", StatusCode::OK)
        .await;
    assert_eq!(instruments["items"][0]["instrument"], INSTRUMENT);
    assert_eq!(instruments["items"][0]["age_seconds"], Value::Null);
    assert_eq!(instruments["next_offset"], Value::Null);
    assert_eq!(instruments["as_of"], AS_OF);
    let bars = fixture
        .get_json(&format!("/api/v1/market/bars?{stock}"), StatusCode::OK)
        .await;
    assert_eq!(bars["records"], json!([rows["records"][2]]));
    assert_eq!(bars["coverage"]["scanned_records"], 5);
    assert_eq!(bars["access"]["license"], LICENSE);
    let trades = fixture
        .get_json(&format!("/api/v1/market/trades?{stock}"), StatusCode::OK)
        .await;
    assert_eq!(trades["records"], json!([rows["records"][3]]));
    let derivatives = fixture
        .get_json(
            &format!("/api/v1/market/derivatives?{perpetual}"),
            StatusCode::OK,
        )
        .await;
    assert_eq!(derivatives["records"], json!([rows["records"][4]]));
    let indicators = fixture.get_json("/api/v1/indicators", StatusCode::OK).await;
    assert_eq!(indicators["items"][0]["id"], "close");
    let values = fixture
        .get_json(
            &format!("/api/v1/indicators/values?{stock}&indicator=close"),
            StatusCode::OK,
        )
        .await;
    assert_eq!(values["items"][0]["value"], PRICE);
    assert_eq!(values["items"][0]["record"], rows["records"][2]);
    let quotes = fixture
        .get_json(&format!("/api/v1/market/quotes?{stock}"), StatusCode::OK)
        .await;
    assert_eq!(quotes["state"], "observed");
    assert_eq!(quotes["observation"]["seq"], POSITION);
    assert_eq!(quotes["bid"]["price"], "99.9");
    assert_eq!(quotes["ask"]["quantity"], SIZE);
    let recent = format!(
        "source={SOURCE}&product={PRODUCT}&channel=fixture&instrument_id={INSTRUMENT}&limit=2"
    );
    let book = fixture
        .get_json(&format!("/api/v1/market/book?{recent}"), StatusCode::OK)
        .await;
    assert_eq!(book["coverage"]["after"], "3");
    assert_eq!(book["coverage"]["through"], "5");
    assert_eq!(book["state"], "unknown");
    assert_eq!(book["bids"], Value::Null);
    fixture.upstream_expectations_met();
}

#[tokio::test]
async fn depth_focus_selects_a_real_wal_book_then_clear_requires_an_instrument() {
    const FOCUS: &str = "/api/v1/market/depth-focus";
    let path = format!("{WAL}?source={SOURCE}&product={PRODUCT}&after=0&limit=128");
    let fixture = Fixture::start(vec![Exchange::ok(path, wal_rows(vec![book_snapshot()]))]).await;
    async fn put(gateway: &Gateway, body: Value) -> Value {
        let request = Request::put(FOCUS)
            .header(header::AUTHORIZATION, gateway.authorization.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = gateway.app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap()
    }
    assert_eq!(
        put(
            &fixture.gateway,
            json!({"action": "set", "instrument_id": INSTRUMENT})
        )
        .await,
        json!({"instrument_id": INSTRUMENT})
    );
    let query =
        format!("/api/v1/market/book?source={SOURCE}&product={PRODUCT}&channel=fixture&after=0");
    let book = fixture.get_json(&query, StatusCode::OK).await;
    assert_eq!(book["instrument_id"], INSTRUMENT);
    assert_eq!(book["state"], "observed");
    assert_eq!(book["observation"]["venue"], "XNYS");
    let cleared = put(&fixture.gateway, json!({"action": "clear"})).await;
    assert_eq!(cleared, json!({"instrument_id": null}));
    let missing = fixture.get_json(&query, StatusCode::BAD_REQUEST).await;
    assert_eq!(missing["error"]["code"], "depth_focus_required");
    fixture.upstream_expectations_met();
}

#[tokio::test]
async fn browser_market_views_reject_incomplete_queries_before_reading_finance() {
    let gateway = Gateway::new("http://127.0.0.1:1".into());
    let base = format!("instrument_id={INSTRUMENT}&source={SOURCE}&product={PRODUCT}");
    for route in [
        format!("/api/v1/market/trades?{base}&limit=129"),
        format!("/api/v1/market/trades?{base}&after=2&through=1"),
        format!(
            "/api/v1/market/trades?{base}&from=2026-01-02T00%3A00%3A00Z&until=2026-01-01T00%3A00%3A00Z"
        ),
        format!("/api/v1/market/quotes?{base}"),
    ] {
        let value = gateway.get_json(&route, StatusCode::BAD_REQUEST).await;
        assert_eq!(value["error"]["code"], "market_query_invalid");
    }
    let missing_indicator = gateway
        .get_json(
            &format!("/api/v1/indicators/values?{base}&indicator=not_present"),
            StatusCode::NOT_FOUND,
        )
        .await;
    assert_eq!(
        missing_indicator["error"]["code"],
        "market_indicator_not_found"
    );
}
