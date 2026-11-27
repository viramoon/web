use super::*;

const CHAIN: &str =
    "/api/v1/options/chain?underlying_con_id=101&expiry=2026-01-02&after_con_id=200&limit=2";
const EXPOSURE: &str = "/api/v1/options/exposure?underlying_id=SYNTH.XNYS&as_of=2026-01-01T00%3A00%3A00Z&scenario=dealer_short";
const LEGACY_EXPOSURE: &str = "/api/v1/options/exposure?instrument_id=SYNTH.XNYS&as_of=2026-01-01T01%3A00%3A00%2B01%3A00&scenario=dealer_short";
const HISTORY: &str = "/api/v1/options/history?underlying_con_id=101&start=9007199254740993&end=9007199254740994&offset=1&limit=2";
const DEPTH: &str = "/api/v1/market/depth?symbol=QQQ";
const UPSTREAM_DEPTH: &str = "/api/v1/market/book?symbol=QQQ";
const BUDGET: &str = "/api/v1/market/subscriptions/budget";
const UPSTREAM_BUDGET: &str = "/api/v1/market/subscriptions";
const COVERAGE: &str = "/api/v1/market/subscriptions/coverage?con_id=201&start=2026-01-01T00%3A00%3A00Z&end=2026-01-01T01%3A00%3A00Z";
const FLOW: &str = "/api/v1/options/flow?instrument_id=SYNTH.XNYS&mode=verified";

const READS: [(&str, &str); 6] = [
    (CHAIN, CHAIN),
    (EXPOSURE, EXPOSURE),
    (HISTORY, HISTORY),
    (DEPTH, UPSTREAM_DEPTH),
    (BUDGET, UPSTREAM_BUDGET),
    (COVERAGE, COVERAGE),
];

fn display_access(product: &str) -> Value {
    json!({
        "source": "ibkr", "product": product, "license": LICENSE,
        "display_live": true, "record_local": false,
        "derive_non_display": false, "backup_personal_drive": false
    })
}

fn chain_page() -> Value {
    json!({
        "access": display_access("opra"), "as_of": AS_OF,
        "underlying_con_id": 101, "discovery_complete": false,
        "items": [{
            "con_id": 201, "symbol": "SYNTH", "primary_exchange": null,
            "expiry": "2026-01-02", "strike": PRICE, "multiplier": "100.00", "right": "call"
        }],
        "next_after_con_id": 201
    })
}

fn exposure_report() -> Value {
    let range = json!({"earliest": AS_OF, "latest": AS_OF});
    let coverage = json!({
        "universe_complete": false, "candidate_contracts": 8, "valid_contracts": 1,
        "partial": true, "eligible_open_interest": "0.000", "total_open_interest": null,
        "open_interest_dates": [], "unconfirmed_open_interest_dates": 1,
        "observed": range, "open_interest_observed": null, "gamma_observed": null,
        "max_oi_gamma_skew": null,
        "exclusions": {"missing_open_interest": 4, "missing_gamma": 3,
            "stale_spot": 0, "stale_gamma": 0, "spot_gamma_skew": 0}
    });
    let view = json!({
        "totals": {"gex": "-100.000", "absolute": "100.000"},
        "by_strike": [], "by_expiry": [],
        "walls": {"largest_absolute": null, "largest_positive": null, "most_negative": null},
        "coverage": coverage, "flip": {"status": "unavailable", "reason": "no_gamma_grid"}
    });
    json!({
        "access": display_access("opra"), "report": {
            "scenario": "dealer_short", "model_version": "synthetic", "underlying_id": INSTRUMENT,
            "as_of": AS_OF, "session_date": "2026-01-01", "gamma_grid": null,
            "spot": {"observed_at": AS_OF, "value": PRICE},
            "timing": {"max_spot_age_seconds": 30, "max_gamma_age_seconds": 30,
                "max_spot_gamma_skew_seconds": 5},
            "all": view, "zero_dte": view, "later": view
        }
    })
}

fn history_page() -> Value {
    json!({
        "access": display_access("opra"), "as_of": AS_OF, "underlying_con_id": 101,
        "start": POSITION, "end": "9007199254740994", "next_offset": null,
        "items": [
            {"instrument_id": "SYNTH-1.XTEST", "result": {"status": "available", "bars": [{
                "close_ts": "9007199254740994", "data": {"open_ts": POSITION,
                "open": PRICE, "high": PRICE, "low": PRICE, "close": PRICE, "volume": SIZE}
            }]}},
            {"instrument_id": "SYNTH-2.XTEST", "result": {"status": "expired"}}
        ]
    })
}

fn venue_book() -> Value {
    json!({
        "access": display_access("nasdaq-totalview"), "symbol": "QQQ", "venue": "XNAS",
        "permissions": {"equity_l1_confirmed": true, "totalview_confirmed": true, "eds_confirmed": true},
        "state": "receiving", "failure": null, "requested_rows": 5, "returned_rows": 1,
        "received_at": AS_OF, "market_data_type": null,
        "bids": [{"price": PRICE, "size": SIZE}], "asks": []
    })
}

fn line_budget() -> Value {
    json!({
        "access": display_access("opra"), "as_of": AS_OF, "phase": "online",
        "account_lines": 100, "reserve_lines": 30, "stock_lines": 9,
        "hot_lines": 16, "rotation_lines": 45, "confirmed_options": 200,
        "observed_options": 60, "discovery_complete": false
    })
}

fn coverage_report() -> Value {
    json!({
        "access": display_access("opra"), "con_id": 201, "last_observed_at": null,
        "covered": [], "uncovered": [],
        "unknown": [{"start": AS_OF, "end": "2026-01-01T01:00:00Z"}]
    })
}

#[tokio::test]
async fn f10c_reads_forward_queries_and_preserve_display_evidence() {
    let mut default_exposure = exposure_report();
    default_exposure["report"]["scenario"] = json!("absolute");
    let payloads = [
        chain_page(),
        exposure_report(),
        history_page(),
        venue_book(),
        line_budget(),
        coverage_report(),
    ];
    let script = READS
        .iter()
        .zip(&payloads)
        .map(|((_, upstream), payload)| Exchange::ok((*upstream).to_owned(), payload.clone()))
        .chain([
            Exchange::ok(EXPOSURE.into(), exposure_report()),
            Exchange::ok(
                format!("/api/v1/options/exposure?underlying_id={INSTRUMENT}"),
                default_exposure.clone(),
            ),
        ])
        .collect();
    let fixture = Fixture::start(script).await;
    for ((path, _), expected) in READS.iter().zip(payloads) {
        assert_eq!(
            fixture.get_json(path, StatusCode::OK).await,
            expected,
            "{path}"
        );
    }
    assert_eq!(
        fixture.get_json(LEGACY_EXPOSURE, StatusCode::OK).await,
        exposure_report()
    );
    assert_eq!(
        fixture
            .get_json(
                &format!("/api/v1/options/exposure?instrument_id={INSTRUMENT}"),
                StatusCode::OK,
            )
            .await,
        default_exposure
    );
    fixture.upstream_expectations_met();
}

#[tokio::test]
async fn f10c_preserves_future_values_nulls_missing_fields_and_numeric_text() {
    let mut unseen = line_budget();
    unseen["phase"] = json!("future_scheduler_phase");
    unseen["new_decimal"] = json!(PRICE);
    unseen["explicit_unknown"] = Value::Null;
    let mut partial = exposure_report();
    partial["report"]["all"]["flip"]["reason"] = json!("future_reason");
    partial["report"]["all"]["coverage"]
        .as_object_mut()
        .unwrap()
        .remove("total_open_interest");
    let fixture = Fixture::start(vec![
        Exchange::ok(UPSTREAM_BUDGET.into(), unseen.clone()),
        Exchange::ok(EXPOSURE.into(), partial.clone()),
    ])
    .await;
    assert_eq!(fixture.get_json(BUDGET, StatusCode::OK).await, unseen);
    assert_eq!(fixture.get_json(EXPOSURE, StatusCode::OK).await, partial);
    fixture.upstream_expectations_met();
}

#[tokio::test]
async fn f10c_maps_upstream_errors_on_every_read() {
    for (upstream_status, upstream_error, web_status, web_error) in [
        (
            StatusCode::BAD_REQUEST,
            ("market_read_query_invalid", "Invalid market read query"),
            StatusCode::BAD_REQUEST,
            ("market_read_query_invalid", "Invalid market read query"),
        ),
        (
            StatusCode::NOT_FOUND,
            (
                "market_read_not_found",
                "Requested market data is unavailable",
            ),
            StatusCode::NOT_FOUND,
            (
                "market_read_not_found",
                "Requested market data is unavailable",
            ),
        ),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            ("market_read_failed", "Could not read market data"),
            StatusCode::BAD_GATEWAY,
            ("finance_upstream_error", "Finance request failed"),
        ),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            (
                "market_read_source_unavailable",
                "Market read source is not connected",
            ),
            StatusCode::SERVICE_UNAVAILABLE,
            ("finance_unavailable", "Finance is unavailable"),
        ),
        (
            StatusCode::GATEWAY_TIMEOUT,
            ("gateway_timeout", "Synthetic upstream gateway timed out"),
            StatusCode::GATEWAY_TIMEOUT,
            ("finance_timeout", "Finance request timed out"),
        ),
    ] {
        for (path, upstream) in READS {
            let fixture = Fixture::start(vec![Exchange {
                request: upstream.into(),
                status: upstream_status,
                value: json!({"error": {"code": upstream_error.0, "message": upstream_error.1}}),
            }])
            .await;
            let body = fixture.get_json(path, web_status).await;
            assert_eq!(
                body,
                json!({"error": {"code": web_error.0, "message": web_error.1}}),
                "{path} with upstream status {upstream_status}"
            );
            fixture.upstream_expectations_met();
        }
    }
}

#[tokio::test]
async fn f10c_display_gate_checks_product_and_license_without_requiring_recording() {
    let mut denied = chain_page();
    denied["access"]["display_live"] = json!(false);
    let mut wrong_product = venue_book();
    wrong_product["access"]["product"] = json!("opra");
    let mut wrong_source = line_budget();
    wrong_source["access"]["source"] = json!("synthetic");
    let mut invalid_license = history_page();
    invalid_license["access"]["license"] = json!("");
    let fixture = Fixture::start(vec![
        Exchange::ok(CHAIN.into(), denied),
        Exchange::ok(UPSTREAM_DEPTH.into(), wrong_product),
        Exchange::ok(UPSTREAM_BUDGET.into(), wrong_source),
        Exchange::ok(HISTORY.into(), invalid_license),
        Exchange::ok(COVERAGE.into(), json!({"unknown": []})),
    ])
    .await;
    assert_eq!(
        fixture.get_json(CHAIN, StatusCode::FORBIDDEN).await["error"]["code"],
        "market_display_denied"
    );
    for path in [DEPTH, BUDGET, HISTORY, COVERAGE] {
        assert_eq!(
            fixture.get_json(path, StatusCode::BAD_GATEWAY).await["error"]["code"],
            "finance_invalid_response"
        );
    }
    fixture.upstream_expectations_met();
}

#[tokio::test]
async fn f10c_routes_share_anonymous_denial_and_throttle_before_finance() {
    let gateway = Gateway::new("http://127.0.0.1:1".into());
    for path in READS.map(|(path, _)| path).into_iter().chain([FLOW]) {
        for method in [Method::GET, Method::HEAD] {
            assert_eq!(
                gateway.request(method, path, false).await.status(),
                StatusCode::UNAUTHORIZED
            );
        }
    }
    for _ in 0..120 {
        assert_eq!(
            gateway
                .request(Method::GET, UPSTREAM_BUDGET, true)
                .await
                .status(),
            StatusCode::OK
        );
    }
    for path in READS.map(|(path, _)| path).into_iter().chain([FLOW]) {
        let response = gateway.request(Method::GET, path, true).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(response.headers().contains_key(header::RETRY_AFTER));
    }
}

#[tokio::test]
async fn f10c_validates_query_bounds_and_preserves_legacy_pending() {
    let gateway = Gateway::new("http://127.0.0.1:1".into());
    for path in [
        "/api/v1/options/chain?underlying_con_id=2147483648",
        "/api/v1/options/chain?underlying_con_id=101&limit=501",
        "/api/v1/options/chain?underlying_con_id=101&after_con_id=0",
        "/api/v1/options/chain?underlying_con_id=101&expiry=2100-02-29",
        "/api/v1/options/chain?offset=1",
        "/api/v1/options/chain?instrument_id=SYNTH.XNYS&limit=0",
        "/api/v1/options/chain?instrument_id=SYNTH.XNYS&expiry=2026-01-02",
        "/api/v1/options/chain?instrument_id=SYNTH.XNYS&underlying_con_id=101",
        "/api/v1/options/exposure?underlying_id=SYNTH.XNYS&instrument_id=SYNTH.XTEST",
        "/api/v1/options/history?underlying_con_id=101&start=1&end=1",
        "/api/v1/options/history?underlying_con_id=101&start=01&end=10",
        "/api/v1/options/history?underlying_con_id=101&start=0&end=18446744073709551616",
        "/api/v1/options/history?underlying_con_id=101&start=0&end=1&limit=51",
        "/api/v1/market/subscriptions/coverage?con_id=201&start=2026-01-02T00:00:00Z&end=2026-01-01T00:00:00Z",
        "/api/v1/options/flow?instrument_id=SYNTH.XNYS&limit=0",
    ] {
        assert_eq!(
            gateway.get_json(path, StatusCode::BAD_REQUEST).await["error"]["code"],
            "invalid_market_request"
        );
    }
    for path in [
        FLOW,
        "/api/v1/options/chain?instrument_id=SYNTH.XNYS&offset=1",
        "/api/v1/options/chain?instrument_id=SYNTH.XNYS&limit=65535",
    ] {
        assert_eq!(
            gateway
                .get_json(path, StatusCode::SERVICE_UNAVAILABLE)
                .await,
            json!({"error": {
                "code": "finance_contract_pending",
                "message": "The finance contract for this capability is pending"
            }}),
            "{path}"
        );
    }
}

#[tokio::test]
async fn f10c_unsupported_exposure_scenario_preserves_pending_before_finance() {
    let gateway = Gateway::new("http://127.0.0.1:1".into());
    for parameter in ["instrument_id", "underlying_id"] {
        let path = format!("/api/v1/options/exposure?{parameter}={INSTRUMENT}&scenario=fixture");
        assert_eq!(
            gateway
                .get_json(&path, StatusCode::SERVICE_UNAVAILABLE)
                .await,
            json!({"error": {
                "code": "finance_contract_pending",
                "message": "The finance contract for this capability is pending"
            }}),
            "{path}"
        );
        let malformed =
            format!("/api/v1/options/exposure?{parameter}={INSTRUMENT}&scenario=Fixture");
        assert_eq!(
            gateway.get_json(&malformed, StatusCode::BAD_REQUEST).await,
            json!({"error": {"code": "invalid_market_request", "message": "Invalid market request"}}),
            "{malformed}"
        );
    }
}

#[test]
fn f10c_openapi_keeps_legacy_views_and_exports_read_schemas() {
    let spec = crate::openapi();
    let paths = &spec["paths"];
    let schemas = &spec["components"]["schemas"];
    assert_eq!(
        paths["/api/v1/market/book"]["get"]["operationId"],
        "marketBook"
    );
    assert_eq!(
        paths["/api/v1/market/subscriptions"]["get"]["operationId"],
        "marketSubscriptions"
    );
    assert_eq!(
        schemas["FinanceLineState"]["enum"],
        json!(["finance_contract_pending"])
    );
    assert_eq!(
        paths["/api/v1/options/flow"]["get"]["operationId"],
        "optionsFlow"
    );
    let exposure = &paths["/api/v1/options/exposure"]["get"];
    assert_eq!(exposure["operationId"], "optionsExposure");
    assert!(
        exposure["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|parameter| parameter["name"] == "instrument_id")
    );
    let scenario = exposure["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|parameter| parameter["name"] == "scenario")
        .unwrap();
    // The query parameter and its component in 70119a2:openapi/web.openapi.json.
    assert_eq!(scenario["in"], "query");
    assert_eq!(scenario["required"], false);
    assert_eq!(
        scenario["schema"],
        json!({"$ref": "#/components/schemas/ProductToken"})
    );
    assert_eq!(
        schemas["ProductToken"],
        json!({"type": "string", "pattern": r"^[a-z0-9][a-z0-9_.-]{0,63}(?![\s\S])"})
    );

    for ((path, _), expected) in READS.into_iter().zip([
        "OptionChainPage",
        "OptionExposureResponse",
        "OptionHistoryPage",
        "DepthBook",
        "SubscriptionBudget",
        "SubscriptionCoverage",
    ]) {
        let operation = &paths[path.split('?').next().unwrap()]["get"];
        assert_eq!(
            operation["security"],
            json!([{"adminBearer": []}, {"adminSession": []}])
        );
        assert_eq!(
            operation["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            format!("#/components/schemas/{expected}")
        );
        assert!(
            schemas[expected]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("access"))
        );
        for status in ["401", "403", "429", "502", "503", "504"] {
            assert!(
                operation["responses"][status].is_object(),
                "{path} {status}"
            );
        }
    }
}

fn option_history_quote() -> Value {
    let batch =
        json!({"request_id": null, "page": "1", "occurrence": "1", "requested_start_ts": "1"});
    option_history_event(
        "option_history_quote",
        json!({"batch": batch, "bid": null, "ask": PRICE,
            "bid_quantity": null, "ask_quantity": SIZE, "bid_past_low": false, "ask_past_high": false}),
    )
}

fn option_history_event(kind: &str, payload: Value) -> Value {
    let mut wire = event(kind, payload);
    wire["schema_version"] = json!("3.0");
    wire["asset_kind"] = json!("option");
    wire["exchange_ts"] = json!("1000000000");
    wire
}

#[test]
fn f10c_generated_wal_decodes_v3_evidence_without_changing_v1() {
    let batch =
        json!({"request_id": null, "page": "1", "occurrence": "1", "requested_start_ts": "1"});
    for wire in [
        option_history_event(
            "option_history_trade",
            json!({"batch": batch, "condition_mapping_version": "fixture-v1",
            "contract_multiplier": "100.00", "exchange": "XTEST", "past_limit": false,
            "price": PRICE, "quantity": SIZE, "special_conditions": "", "unreported": true}),
        ),
        option_history_quote(),
    ] {
        let mut decoded: models::Event = serde_json::from_value(wire.clone()).unwrap();
        assert!(event_version_matches(&decoded));
        assert_eq!(serde_json::to_value(&decoded).unwrap(), wire);
        let legacy_page: models::MarketWalPage =
            serde_json::from_value(wal_reply(wire, true)).unwrap();
        assert!(matches!(
            check_record(&legacy_page.access, &legacy_page.records[0]),
            Err(MarketReadError::InvalidResponse)
        ));
        for invalid in [None, Some("01"), Some("+1"), Some("18446744073709551616")] {
            decoded.exchange_ts = invalid.map(str::to_owned);
            assert!(!event_version_matches(&decoded), "{invalid:?}");
        }
        decoded.exchange_ts = Some("1000000000".into());
        decoded.schema_version = models::SchemaVersion::Variant10;
        assert!(!event_version_matches(&decoded));
    }
}

#[tokio::test]
async fn f10c_legacy_wal_and_projection_reject_v3_evidence() {
    let wal = format!("{WAL}?source={SOURCE}&product={PRODUCT}");
    let projected = format!(
        "{PROJECTION}?source={SOURCE}&product={PRODUCT}&channel=fixture&instrument_id={INSTRUMENT}&epoch=7&seq={POSITION}"
    );
    let valid = option_history_quote();
    let mut missing_timestamp = valid.clone();
    missing_timestamp["exchange_ts"] = Value::Null;
    let mut script = Vec::new();
    for record in [valid, missing_timestamp] {
        let page = wal_rows(vec![event("trade", trade()), record]);
        let projection = json!({
            "as_of": AS_OF, "access": access(true),
            "projected_through": page["through"],
            "record": page["records"][1],
        });
        script.push(Exchange::ok(wal.clone(), page));
        script.push(Exchange::ok(projected.clone(), projection));
    }
    let fixture = Fixture::start(script).await;
    for path in [&wal, &projected, &wal, &projected] {
        assert_eq!(
            fixture.get_json(path, StatusCode::BAD_GATEWAY).await,
            json!({"error": {
                "code": "finance_invalid_response",
                "message": "Invalid finance response"
            }})
        );
    }
    fixture.upstream_expectations_met();
}
