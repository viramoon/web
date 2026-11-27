use super::{options::display_access, *};

const LEGACY: &str = "/api/v1/options/flow";
const OBSERVATIONS: &str = "/api/v1/options/flow/observations";
const VERIFICATION: &str = "/api/v1/options/flow/verification";
const QUERY: &str = "con_id=201&start=2026-01-02T14%3A30%3A00Z&end=2026-01-02T14%3A31%3A00Z";
const LEGACY_QUERY: &str =
    "con_id=201&from=2026-01-02T09%3A30%3A00-05%3A00&until=2026-01-02T09%3A31%3A00-05%3A00";
const START: &str = "2026-01-02T14:30:00Z";
const END: &str = "2026-01-02T14:31:00Z";
const U64_MAX: &str = "18446744073709551615";

// Deterministic synthetic pages from fixed timestamps, contract 201, and the exact
// number literals in synthetic.rs. Vary only the fields tested by each case.
fn observation_page() -> Value {
    json!({
        "access": display_access("opra"), "con_id": 201,
        "start": START, "end": END, "as_of": END, "next_offset": 1,
        "items": [{
            "clue": {"con_id": 201, "generation": U64_MAX,
                "cohort": {"underlying_con_id": 101, "expiry": "2026-01-02", "session_segment": 0},
                "observed_at": START, "multiplier": "100.00", "rule_version": "fixture-v1",
                "status": "pending_verification", "observation": {"kind": "volume_increase",
                "evidence": {"earlier_cumulative_volume": {"value": "0.00", "received_at": START},
                    "later_cumulative_volume": {"value": SIZE, "received_at": END},
                    "delta_contracts": SIZE, "estimated_premium": null,
                    "comparison": {"elapsed_millis": 60000, "baseline_samples": 0, "elevated": null}}}},
            "budget": {"phase": "online", "as_of": START, "account_lines": 100,
                "reserve_lines": 30, "stock_lines": 9, "hot_lines": 16, "rotation_lines": 45,
                "confirmed_options": 8, "observed_options": 1, "discovery_complete": false},
            "coverage": observation_coverage(),
        }],
    })
}

fn observation_coverage() -> Value {
    json!({"access": display_access("opra"), "con_id": 201, "last_observed_at": null,
        "covered": [], "uncovered": [], "unknown": [{"start": START, "end": END}]})
}

fn verification_page() -> Value {
    json!({
        "access": display_access("opra-history"), "con_id": 201,
        "start": START, "end": END, "quote_start": START, "verified_at": END,
        "max_quote_age_ms": 500, "multiplier": "100.00", "next_offset": null,
        "durable_through": null, "observation_coverage": observation_coverage(),
        "counts": {"verified": 1, "unknown": 1, "pending_intervals": 0, "pending_trade_rows": 0,
            "unavailable_intervals": 1, "unresolved_amendments": 0, "non_trade_markers": 0},
        "issues": [{"kind": "bid_ask", "reason": "expired_contract", "start": START,
            "end": END, "observed_at": END}],
        "trades": [{"trade": {"occurrence": 1, "batch": {"page": 1, "kind": "trades",
            "requested_start": START, "obtained_at": END},
            "row": {"occurred_at": START, "price": PRICE, "quantity": SIZE, "exchange": "XTEST",
                "special_conditions": "", "past_limit": false, "unreported": false}},
            "status": "active", "condition": {"mapping_version": "fixture-v1", "meaning": "unknown"},
            "inference": {"direction": "unknown", "confidence": "unknown", "reason": "fixture_future_reason",
                "rule_version": "fixture-v1", "quote": null}, "identity": "unknown",
            "suspected_scale": {}, "premium": PRICE}],
    })
}

#[tokio::test]
async fn f10d_routes_forward_windows_and_preserve_evidence() {
    let observed = observation_page();
    let mut verified = verification_page();
    verified["access"]["record_local"] = json!(true);
    verified["durable_through"] = json!(U64_MAX);
    let mut empty = observed.clone();
    empty["items"] = json!([]);
    empty["next_offset"] = Value::Null;
    let exchange = |path: &str, params: &str, response: &Value| {
        Exchange::ok(format!("{path}?{params}"), response.clone())
    };
    let observed_page = format!("{QUERY}&offset=0&limit=1");
    let verified_page = format!("{QUERY}&offset={U64_MAX}&limit=100");
    let fixture = Fixture::start(vec![
        exchange(OBSERVATIONS, &observed_page, &observed),
        exchange(VERIFICATION, &verified_page, &verified),
        exchange(OBSERVATIONS, QUERY, &observed),
        exchange(VERIFICATION, QUERY, &verification_page()),
        exchange(OBSERVATIONS, QUERY, &empty),
    ])
    .await;
    for (request, expected) in [
        (format!("{OBSERVATIONS}?{observed_page}"), observed.clone()),
        (format!("{VERIFICATION}?{verified_page}"), verified),
        (format!("{LEGACY}?{LEGACY_QUERY}"), observed),
        (
            format!("{LEGACY}?instrument_id={INSTRUMENT}&mode=verified&{LEGACY_QUERY}"),
            verification_page(),
        ),
        (format!("{OBSERVATIONS}?{QUERY}"), empty),
    ] {
        assert_eq!(fixture.get_json(&request, StatusCode::OK).await, expected);
    }
    fixture.upstream_expectations_met();
}

#[tokio::test]
async fn f10d_preserves_upstream_read_errors() {
    for (upstream_status, upstream_code, status, code) in [
        (
            400,
            "market_read_query_invalid",
            400,
            "market_read_query_invalid",
        ),
        (404, "market_read_not_found", 404, "market_read_not_found"),
        (500, "market_read_failed", 502, "finance_upstream_error"),
        (
            503,
            "market_read_source_unavailable",
            503,
            "finance_unavailable",
        ),
        (504, "gateway_timeout", 504, "finance_timeout"),
    ] {
        let upstream_status = StatusCode::from_u16(upstream_status).unwrap();
        let status = StatusCode::from_u16(status).unwrap();
        let exchanges = [OBSERVATIONS, VERIFICATION].map(|path| Exchange {
            request: format!("{path}?{QUERY}"),
            status: upstream_status,
            value: json!({"error": {"code": upstream_code, "message": "Synthetic failure"}}),
        });
        let fixture = Fixture::start(Vec::from(exchanges)).await;
        for request in [
            format!("{OBSERVATIONS}?{QUERY}"),
            format!("{LEGACY}?{LEGACY_QUERY}&mode=verified"),
        ] {
            let reply = fixture.get_json(&request, status).await;
            assert_eq!(reply["error"]["code"], code);
        }
        fixture.upstream_expectations_met();
    }
}

#[tokio::test]
async fn f10d_authenticates_and_throttles_all_paths_before_finance() {
    let gateway = Gateway::new("http://127.0.0.1:1".into());
    let paths = [
        format!("{OBSERVATIONS}?{QUERY}"),
        format!("{VERIFICATION}?{QUERY}"),
        format!("{LEGACY}?{LEGACY_QUERY}"),
    ];
    for path in &paths {
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
                .request(Method::GET, "/api/v1/market/subscriptions", true)
                .await
                .status(),
            StatusCode::OK
        );
    }
    for path in &paths {
        let response = gateway.request(Method::GET, path, true).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(response.headers().contains_key(header::RETRY_AFTER));
    }
}

#[tokio::test]
async fn f10d_validates_windows_and_preserves_legacy_error_shape() {
    let gateway = Gateway::new("http://127.0.0.1:1".into());
    for request in [
        format!("{OBSERVATIONS}?{QUERY}&limit=101"),
        format!("{OBSERVATIONS}?{}", QUERY.replace("201", "0")),
        format!("{VERIFICATION}?{}", QUERY.replace("14%3A31", "14%3A30")),
        format!("{VERIFICATION}?con_id=201"),
        format!("{VERIFICATION}?{QUERY}&mode=verified"),
        format!("{LEGACY}?con_id=201"),
        format!("{LEGACY}?{LEGACY_QUERY}&mode=invalid"),
    ] {
        assert_eq!(
            gateway.get_json(&request, StatusCode::BAD_REQUEST).await["error"]["code"],
            "invalid_market_request"
        );
    }
    let old_request = format!("{LEGACY}?instrument_id={INSTRUMENT}&mode=verified");
    assert_eq!(
        gateway
            .get_json(&old_request, StatusCode::SERVICE_UNAVAILABLE)
            .await,
        json!({
            "error": {"code": "finance_flow_query_upgrade_required", "message": "Use a confirmed con_id and the from and until boundaries"}
        })
    );
}

#[test]
fn f10d_openapi_preserves_legacy_client_and_registers_display_reads() {
    let spec = crate::openapi();
    let schemas = &spec["components"]["schemas"];
    let legacy = &spec["paths"][LEGACY]["get"];
    assert_eq!(legacy["operationId"], "optionsFlow");
    assert_eq!(
        legacy["security"],
        json!([{"adminSession": []}, {"adminBearer": []}])
    );
    let parameters = legacy["parameters"].as_array().unwrap();
    for (name, schema) in [
        (
            "instrument_id",
            json!({"$ref": "#/components/schemas/InstrumentId"}),
        ),
        ("mode", json!({"$ref": "#/components/schemas/FlowMode"})),
        ("from", json!({"format": "date-time", "type": "string"})),
        ("until", json!({"format": "date-time", "type": "string"})),
        (
            "offset",
            json!({"format": "int32", "minimum": 0, "type": "integer"}),
        ),
        (
            "limit",
            json!({"format": "int32", "minimum": 0, "type": "integer"}),
        ),
    ] {
        assert!(
            parameters
                .iter()
                .any(|param| param["name"] == name && param["schema"] == schema)
        );
    }
    assert_eq!(schemas["FlowMode"]["enum"], json!(["observed", "verified"]));
    let legacy_responses = &legacy["responses"];
    for status in ["400", "401", "403", "429", "503"] {
        assert_eq!(
            legacy_responses[status]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ErrorBody"
        );
    }
    let upstream: Value =
        serde_json::from_str(include_str!("../../../openapi/finance.openapi.json")).unwrap();
    for (index, (path, operation_id, schema)) in [
        (
            OBSERVATIONS,
            "optionsFlowObservations",
            "OptionFlowObservationPage",
        ),
        (
            VERIFICATION,
            "optionsFlowVerification",
            "OptionFlowVerificationPage",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let operation = &spec["paths"][path]["get"];
        let success = &operation["responses"]["200"]["content"]["application/json"]["schema"];
        assert_eq!(operation["operationId"], operation_id);
        assert_eq!(
            operation["security"],
            json!([{"adminBearer": []}, {"adminSession": []}])
        );
        assert_eq!(
            operation["parameters"],
            upstream["paths"][path]["get"]["parameters"]
        );
        assert_eq!(success["$ref"], format!("#/components/schemas/{schema}"));
        assert_eq!(
            *success,
            legacy_responses["200"]["content"]["application/json"]["schema"]["oneOf"][index]
        );
        assert_eq!(schemas[schema], upstream["components"]["schemas"][schema]);
        for status in ["400", "401", "403", "404", "429", "502", "503", "504"] {
            assert_eq!(
                operation["responses"][status]["content"]["application/json"]["schema"]["$ref"],
                "#/components/schemas/ErrorBody"
            );
        }
    }
}
