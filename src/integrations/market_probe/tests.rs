use super::*;
use crate::{
    config::Settings,
    integrations::store::{TestStatus, TestView},
};
use axum::{
    Json, Router,
    extract::Path,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{net::TcpListener, task::JoinHandle};

const TESTS: &str = "/integrations/{id}/tests";
const TEST: &str = "/integrations/{id}/tests/{test_id}";

// All response data below is generated in-process: dates come from the test clock,
// numeric identifiers and counters are fixed sentinels, and no prices are recorded.
fn fixture(connector: &str, symbol: &str) -> TestInput {
    TestInput {
        connector: connector.into(),
        settings: json!({"symbol": symbol}),
        test: TestView {
            integration_id: Uuid::new_v4().to_string(),
            id: Uuid::new_v4().to_string(),
            config_version: 7,
            status: TestStatus::Pending,
            started_at: Utc::now(),
            finished_at: None,
            evidence: None,
        },
        credential: None,
    }
}

fn paper_response(input: &TestInput) -> Value {
    json!({
        "integration_id": input.test.integration_id,
        "test_id": input.test.id,
        "config_version": input.test.config_version,
        "connector": input.connector,
        "symbol": input.settings["symbol"],
        "status": "verified",
        "observation": {
            "origin": "live",
            "observed_at": Utc::now(),
            "details": {
                "kind": "ibkr_paper",
                "data": {
                    "paper_endpoint": true, "paper_account": true, "read_only": true,
                    "unique_market_data_client": true, "stock_l1_market_data_type": 1,
                    "option_l1_market_data_type": 1, "option_contract_id": 42,
                    "option_open_interest": true, "option_greeks": true, "nasdaq_depth_callback": true,
                    "remaining_l1_lines": 30, "remaining_depth_lines": 0
                }
            }
        }
    })
}

fn crypto_response(input: &TestInput) -> Value {
    let event = Utc::now();
    json!({
        "integration_id": input.test.integration_id,
        "test_id": input.test.id,
        "config_version": input.test.config_version,
        "connector": input.connector,
        "symbol": input.settings["symbol"],
        "status": "verified",
        "observation": {
            "origin": "live",
            "observed_at": event,
            "details": {
                "kind": "crypto_market",
                "data": {"metadata": true, "snapshot": true, "price_precision": 8, "event_at": event}
            }
        }
    })
}

fn finance_at(base_url: String) -> Finance {
    let mut config = Settings::load(std::path::Path::new("config"), |_| None)
        .unwrap()
        .finance;
    config.base_url = base_url;
    Finance::new(&config).unwrap()
}

struct Server(JoinHandle<()>);

impl Drop for Server {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn serve(app: Router) -> (Finance, Server) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let finance = finance_at(format!("http://{}", listener.local_addr().unwrap()));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (finance, Server(server))
}

#[tokio::test]
async fn paper_realtime_evidence_is_bound_to_the_candidate_and_read_only() {
    let input = fixture("ibkr_paper", "SPY");
    let response = paper_response(&input);
    let expected_id = input.test.integration_id.clone();
    let expected_request = json!({
        "test_id": input.test.id, "config_version": 7, "connector": "ibkr_paper", "symbol": "SPY"
    });
    let app = Router::new().route(
        TESTS,
        post(
            move |Path(id): Path<String>, Json(body): Json<Value>| async move {
                assert_eq!(id, expected_id);
                assert_eq!(body, expected_request);
                Json(response)
            },
        ),
    );
    let (finance, _server) = serve(app).await;
    let (outcome, evidence) = test(&finance, &input, false).await;
    assert!(matches!(outcome, Outcome::Passed));
    assert_paper_verified(evidence);
}

#[test]
fn synthetic_paper_callbacks_verify_as_realtime() {
    let input = fixture("ibkr_paper", "SPY");
    let request = ProbeRequest {
        connector: Connector::IbkrPaper,
        symbol: Symbol::Spy,
        config_version: input.test.config_version,
        test_id: input.test.id.clone(),
    };
    let evidence = verify(
        serde_json::from_value(paper_response(&input)).unwrap(),
        &input.test.integration_id,
        &request,
        input.test.started_at,
    );
    assert!(matches!(evidence.outcome(), Outcome::Passed));
    assert_paper_verified(evidence);
}

fn assert_paper_verified(evidence: MarketEvidence) {
    assert_eq!(evidence.result, MarketResult::Verified);
    let observation = evidence.observation.unwrap();
    assert_eq!(observation.origin, ObservationOrigin::Live);
    let ObservationDetails::IbkrPaper(paper) = observation.details else {
        panic!("expected IBKR paper proof")
    };
    assert!(paper.paper_endpoint && paper.paper_account && paper.read_only);
    assert!(paper.unique_market_data_client && paper.nasdaq_depth_callback);
    assert!(paper.option_open_interest && paper.option_greeks);
    assert_eq!(paper.stock_l1_market_data_type, Some(1));
    assert_eq!(paper.option_l1_market_data_type, Some(1));
    assert_eq!(paper.option_contract_id, Some(42));
    assert_eq!(paper.remaining_l1_lines, Some(30));
    assert_eq!(paper.remaining_depth_lines, Some(0));
}

#[tokio::test]
async fn disabled_binance_resolves_before_transport_selection() {
    let finance = finance_at("http://finance.invalid".into());
    assert!(finance.integration_probe_client().is_none());
    for connector in ["binance_spot", "binance_usd_m"] {
        let input = fixture(connector, "BTCUSDT");
        let (outcome, evidence) = test(&finance, &input, false).await;
        assert!(matches!(outcome, Outcome::Unavailable));
        assert_eq!(evidence.result, MarketResult::Disabled);
    }
}

#[test]
fn paper_separates_closed_delayed_permissions_and_capacity() {
    let input = fixture("ibkr_paper", "QQQ");
    let request = ProbeRequest {
        connector: Connector::IbkrPaper,
        symbol: Symbol::Qqq,
        config_version: input.test.config_version,
        test_id: input.test.id.clone(),
    };
    for (reported, expected) in [
        ("market_closed", MarketResult::MarketClosed),
        ("delayed", MarketResult::Delayed),
        ("no_permission", MarketResult::NoPermission),
        ("lines_full", MarketResult::LinesFull),
        ("source_unavailable", MarketResult::SourceUnavailable),
    ] {
        let mut response = paper_response(&input);
        response["status"] = json!(reported);
        response["observation"] = Value::Null;
        let evidence = verify(
            serde_json::from_value(response).unwrap(),
            &input.test.integration_id,
            &request,
            input.test.started_at,
        );
        assert_eq!(evidence.result, expected);
        assert!(!matches!(evidence.outcome(), Outcome::Passed));
    }
}

#[test]
fn incomplete_unsafe_mismatched_or_replay_paper_evidence_cannot_pass() {
    let input = fixture("ibkr_paper", "SPY");
    let request = ProbeRequest {
        connector: Connector::IbkrPaper,
        symbol: Symbol::Spy,
        config_version: input.test.config_version,
        test_id: input.test.id.clone(),
    };
    for (pointer, value, expected) in [
        ("/config_version", json!(8), MarketResult::InvalidResponse),
        (
            "/test_id",
            json!(Uuid::new_v4().to_string()),
            MarketResult::InvalidResponse,
        ),
        (
            "/integration_id",
            json!(Uuid::new_v4().to_string()),
            MarketResult::InvalidResponse,
        ),
        ("/symbol", json!("QQQ"), MarketResult::InvalidResponse),
        (
            "/observation/origin",
            json!("replay"),
            MarketResult::ReplayOnly,
        ),
        (
            "/observation/observed_at",
            json!(input.test.started_at - TimeDelta::seconds(10)),
            MarketResult::StaleObservation,
        ),
        (
            "/observation/details/data/paper_account",
            json!(false),
            MarketResult::UnsafeBrokerSession,
        ),
        (
            "/observation/details/data/read_only",
            json!(false),
            MarketResult::UnsafeBrokerSession,
        ),
        (
            "/observation/details/data/unique_market_data_client",
            json!(false),
            MarketResult::UnsafeBrokerSession,
        ),
        (
            "/observation/details/data/option_l1_market_data_type",
            json!(3),
            MarketResult::Delayed,
        ),
        (
            "/observation/details/data/stock_l1_market_data_type",
            json!(2),
            MarketResult::Frozen,
        ),
        (
            "/observation/details/data/stock_l1_market_data_type",
            json!(0),
            MarketResult::InvalidResponse,
        ),
        (
            "/observation/details/data/nasdaq_depth_callback",
            json!(false),
            MarketResult::EvidenceIncomplete,
        ),
        (
            "/observation/details/data/option_open_interest",
            json!(false),
            MarketResult::EvidenceIncomplete,
        ),
        (
            "/observation/details/data/option_greeks",
            json!(false),
            MarketResult::EvidenceIncomplete,
        ),
        (
            "/observation/details/data/remaining_l1_lines",
            Value::Null,
            MarketResult::EvidenceIncomplete,
        ),
    ] {
        let mut response = paper_response(&input);
        *response.pointer_mut(pointer).unwrap() = value;
        let evidence = verify(
            serde_json::from_value(response).unwrap(),
            &input.test.integration_id,
            &request,
            input.test.started_at,
        );
        assert_eq!(evidence.result, expected, "{pointer}");
        assert!(!matches!(evidence.outcome(), Outcome::Passed));
    }
}

#[tokio::test]
async fn disabled_binance_both_venues_make_zero_requests() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let finance = finance_at(format!("http://{}", listener.local_addr().unwrap()));
    for connector in ["binance_spot", "binance_usd_m"] {
        let input = fixture(connector, "BTCUSDT");
        let (outcome, evidence) = test(&finance, &input, false).await;
        assert!(matches!(outcome, Outcome::Unavailable));
        assert_eq!(evidence.result, MarketResult::Disabled);
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn crypto_returns_precision_and_new_event_for_allowlisted_sources() {
    for (connector, symbol) in [
        ("hyperliquid_perp", "BTC"),
        ("binance_spot", "BTCUSDT"),
        ("binance_usd_m", "ETHUSDT"),
    ] {
        let input = fixture(connector, symbol);
        let response = crypto_response(&input);
        let app = Router::new().route(TESTS, post(move || async move { Json(response) }));
        let (finance, _server) = serve(app).await;
        let (outcome, evidence) = test(&finance, &input, true).await;
        assert!(matches!(outcome, Outcome::Passed));
        let observation = evidence.observation.unwrap();
        let ObservationDetails::CryptoMarket(crypto) = observation.details else {
            panic!("expected crypto proof")
        };
        assert_eq!(crypto.price_precision, Some(8));
        assert!(crypto.metadata && crypto.snapshot);
        assert!(
            crypto
                .event_at
                .is_some_and(|at| at >= input.test.started_at)
        );
    }
}

#[test]
fn crypto_cached_event_or_invalid_precision_cannot_pass() {
    let input = fixture("hyperliquid_perp", "BTC");
    let request = ProbeRequest {
        connector: Connector::HyperliquidPerp,
        symbol: Symbol::Btc,
        config_version: input.test.config_version,
        test_id: input.test.id.clone(),
    };
    for (field, value, expected) in [
        (
            "event_at",
            json!(input.test.started_at - TimeDelta::minutes(1)),
            MarketResult::StaleObservation,
        ),
        ("event_at", Value::Null, MarketResult::EvidenceIncomplete),
        ("price_precision", json!(39), MarketResult::InvalidResponse),
        ("snapshot", json!(false), MarketResult::EvidenceIncomplete),
    ] {
        let mut response = crypto_response(&input);
        response["observation"]["details"]["data"][field] = value;
        let evidence = verify(
            serde_json::from_value(response).unwrap(),
            &input.test.integration_id,
            &request,
            input.test.started_at,
        );
        assert_eq!(evidence.result, expected, "{field}");
        assert!(!matches!(evidence.outcome(), Outcome::Passed));
    }
}

#[tokio::test]
async fn ambiguous_market_post_is_looked_up_without_retrying_the_write() {
    let input = fixture("ibkr_paper", "SPY");
    let response = paper_response(&input);
    let expected_path = (input.test.integration_id.clone(), input.test.id.clone());
    let posts = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            TESTS,
            post({
                let posts = posts.clone();
                move || async move {
                    posts.fetch_add(1, Ordering::Relaxed);
                    StatusCode::GATEWAY_TIMEOUT
                }
            }),
        )
        .route(
            TEST,
            get(move |Path(path): Path<(String, String)>| async move {
                assert_eq!(path, expected_path);
                Json(response)
            }),
        );
    let (finance, _server) = serve(app).await;
    let (outcome, evidence) = test(&finance, &input, false).await;
    assert!(matches!(outcome, Outcome::Passed));
    assert_eq!(evidence.result, MarketResult::Verified);
    assert_eq!(posts.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn market_transport_rejects_redirects_and_raw_provider_diagnostics() {
    let input = fixture("ibkr_paper", "SPY");
    let calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            TESTS,
            post(|| async { (StatusCode::TEMPORARY_REDIRECT, [("location", "/alternate")]) }),
        )
        .route(
            "/alternate",
            post({
                let calls = calls.clone();
                move || async move {
                    calls.fetch_add(1, Ordering::Relaxed);
                    StatusCode::OK
                }
            }),
        );
    let (finance, _server) = serve(app).await;
    let (_, evidence) = test(&finance, &input, false).await;
    assert_eq!(evidence.result, MarketResult::InvalidResponse);
    assert_eq!(calls.load(Ordering::Relaxed), 0);

    let mut response = paper_response(&input);
    let marker = Uuid::new_v4().to_string();
    response["diagnostic"] = json!(marker);
    let app = Router::new().route(TESTS, post(move || async move { Json(response) }));
    let (finance, _server) = serve(app).await;
    let (_, evidence) = test(&finance, &input, false).await;
    assert_eq!(evidence.result, MarketResult::InvalidResponse);
    assert!(!serde_json::to_string(&evidence).unwrap().contains(&marker));
}

#[tokio::test]
async fn absent_producer_does_not_fabricate_evidence() {
    let input = fixture("ibkr_paper", "SPY");
    let (finance, _server) = serve(Router::new()).await;
    let (outcome, evidence) = test(&finance, &input, false).await;
    assert!(matches!(outcome, Outcome::Unavailable));
    assert_eq!(evidence.result, MarketResult::AdapterUnavailable);
    assert!(evidence.observation.is_none());
}

#[tokio::test]
async fn nonlocal_base_does_not_fabricate_evidence() {
    let input = fixture("ibkr_paper", "SPY");
    for url in ["http://localhost:8081", "https://finance.invalid"] {
        let finance = finance_at(url.into());
        assert!(finance.integration_probe_client().is_none());
        let (outcome, evidence) = test(&finance, &input, false).await;
        assert!(matches!(outcome, Outcome::Unavailable));
        assert_eq!(evidence.result, MarketResult::AdapterUnavailable);
    }
}
