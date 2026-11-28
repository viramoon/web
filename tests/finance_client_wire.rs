use finance_client::{
    apis::{configuration::Configuration, option_flow_stream_api},
    models::{Accepted, BacktestSummary, ChartBars, Event, MarketStatus, Page, Window},
};
use serde_json::{Value, json};

#[test]
fn option_flow_generated_requests_keep_cursor_and_last_event_id() {
    let config = Configuration {
        base_path: "http://127.0.0.1:1".into(),
        ..Configuration::new()
    };
    let cursor = "synthetic.cursor+/?=&";
    let last_id = "synthetic.last-event";
    let ws = option_flow_stream_api::ws(&config, 42, Some(cursor))
        .build()
        .unwrap();
    let sse = option_flow_stream_api::sse(&config, 42, Some(cursor), Some(last_id))
        .build()
        .unwrap();

    for (request, path) in [
        (&ws, "/api/v1/options/flow/ws"),
        (&sse, "/api/v1/options/flow/sse"),
    ] {
        assert_eq!(request.method(), reqwest::Method::GET);
        assert_eq!(request.url().path(), path);
        assert_eq!(
            request.url().query_pairs().into_owned().collect::<Vec<_>>(),
            [
                ("con_id".to_owned(), "42".to_owned()),
                ("cursor".to_owned(), cursor.to_owned())
            ]
        );
    }
    assert!(!ws.headers().contains_key("last-event-id"));
    assert_eq!(sse.headers()["last-event-id"], last_id);
}

#[test]
fn finance_read_stream_exports_replay_v1_frames_without_precision_loss() {
    let samples: Value =
        serde_json::from_str(include_str!("../openapi/finance.read-stream-examples.json")).unwrap();
    let ws: Value =
        serde_json::from_str(include_str!("../openapi/finance.market-ws.schema.json")).unwrap();
    let sse: Value = serde_json::from_str(include_str!(
        "../openapi/finance.market-status-sse.schema.json"
    ))
    .unwrap();
    for schema in [&ws, &sse] {
        assert_eq!(schema["$defs"]["SchemaVersion"]["enum"], json!(["1.0"]));
    }
    let mut big_seq_found = false;
    for wire in samples["market_ws"].as_array().unwrap() {
        let event: Event = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(wire["schema_version"], "1.0");
        big_seq_found |= event.seq.parse::<u64>().unwrap() > (1 << 53);
        assert_eq!(serde_json::to_value(event).unwrap(), *wire);
    }
    assert!(big_seq_found);

    let sse_messages = samples["market_status_sse"].as_array().unwrap();
    assert_eq!(sse_messages.len(), 2);
    for (wire, id) in sse_messages.iter().zip([u64::MAX - 1, u64::MAX]) {
        let status: MarketStatus = serde_json::from_value(wire["data"]["status"].clone()).unwrap();
        assert_eq!(wire["id"], id.to_string());
        assert_eq!(wire["event"], "market_status");
        assert_eq!(wire["data"]["schema_version"], "1.0");
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            wire["data"]["status"]
        );
    }
}

#[test]
fn generated_chart_models_preserve_ohlcv_and_indicator_decimal_text() {
    // Fixed synthetic close and volume above JavaScript's safe integer range.
    let price = "9007199254740993.000000000001";
    let size = "9007199254740993.0000";
    let opening = "2026-01-01T00:00:00Z";
    let closing = "2026-01-01T00:01:00Z";
    let mut wire = json!({
        "series": {"instrument_id": "SYNTH.XNYS", "source": "synthetic",
            "product": "bars", "adjustment": "raw", "timeframe": "1m"},
        "provenance": {"license": "fixture.synthetic", "delay_tier": "replay",
            "calendar_id": "XNYS", "market_timezone": "America/New_York",
            "display": "authorized_display"},
        "as_of": closing,
        "coverage": {"history_complete": true, "first_closed_open_ts": opening,
            "last_closed_end_ts": closing, "closed_bars": 1,
            "returned_closed_bars": 1, "returned_provisional": false},
        "next_start": null,
        "indicators": [{"use": "indicator.sma@1.0.0", "params": {"period": 2}}],
        "rows": [{
            "bar": {"open_ts": opening, "end_ts": closing, "first_available_ts": closing,
                "trading_day": "2026-01-01", "open": price, "high": price, "low": price,
                "close": price, "volume": size},
            "provisional": false,
            "indicator_values": [{"status": "warmup", "values": {}}]
        }]
    });
    let chart: ChartBars = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(chart.rows[0].bar.close, price);
    assert_eq!(chart.rows[0].bar.volume, size);
    assert_eq!(serde_json::to_value(chart).unwrap(), wire);
    wire["rows"][0]["indicator_values"] = json!([{"status": "ready", "values": {"sma": price}}]);
    let chart: ChartBars = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(chart.rows[0].indicator_values[0].values["sma"], price);
    assert_eq!(serde_json::to_value(chart).unwrap(), wire);

    wire["rows"][0]["indicator_values"][0]["values"]["sma"] = json!(1);
    assert!(serde_json::from_value::<ChartBars>(wire).is_err());
}

#[test]
fn preserves_decimal_text_and_unsigned_counts_across_the_full_finance_range() {
    let mut wire = json!({
        "currency": "USD",
        "start": "2026-01-01",
        "end": "2026-01-02",
        "generated_at": "2026-01-02T00:00:00Z",
        "instrument": "SPY.ARCA",
        "strategy": "sample",
        "run_id": "sample-1",
        "pnl": "9007199254740993.000000000000000001",
        "total_orders": u64::MAX,
        "total_positions": u64::MAX - 1
    });
    let model: BacktestSummary = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(model.total_orders, u64::MAX);
    assert_eq!(serde_json::to_value(model).unwrap(), wire);

    wire["total_orders"] = json!(-1);
    assert!(serde_json::from_value::<BacktestSummary>(wire.clone()).is_err());
    wire["total_orders"] = json!(0);
    wire["pnl"] = json!(123);
    assert!(serde_json::from_value::<BacktestSummary>(wire).is_err());
}

#[test]
fn job_ids_and_pagination_cursors_keep_their_signed_wire_type() {
    let accepted: Accepted = serde_json::from_value(json!({ "id": i64::MIN })).unwrap();
    assert_eq!(accepted.id, i64::MIN);
    let wire = json!({"items": [], "next_cursor": -10});
    let page: Page = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(page.next_cursor, Some(Some(-10)));
    assert_eq!(serde_json::to_value(page).unwrap(), wire);

    assert!(serde_json::from_value::<Accepted>(json!({ "id": u64::MAX })).is_err());
    assert!(serde_json::from_value::<Page>(json!({"items": [], "next_cursor": u64::MAX})).is_err());
    let omitted: Page = serde_json::from_value(json!({"items": []})).unwrap();
    let explicit_null: Page =
        serde_json::from_value(json!({"items": [], "next_cursor": Value::Null})).unwrap();
    assert_eq!(omitted.next_cursor, None);
    assert_eq!(explicit_null.next_cursor, Some(None));
}

#[test]
fn research_windows_preserve_unavailable_metrics_and_calendar_fields() {
    let wire = json!({
        "window": "50y",
        "label": "50y (insufficient)",
        "requested_years": 50,
        "requested_start": "1976-11-20",
        "calendar_start": null,
        "calendar_end": null,
        "coverage": "insufficient",
        "coverage_reason": "calendar_unavailable",
        "calendar_diagnostic": "XNYS 1976-11-20 to 2026-11-20: calendar unavailable",
        "expected_sessions": null,
        "missing_sessions": null,
        "metrics": null
    });
    let window: Window = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(window.metrics, Some(None));
    assert_eq!(window.expected_sessions, Some(None));
    assert_eq!(serde_json::to_value(window).unwrap(), wire);

    let omitted = json!({"coverage": "insufficient"});
    let window: Window = serde_json::from_value(omitted.clone()).unwrap();
    assert_eq!(window.metrics, None);
    assert_eq!(serde_json::to_value(window).unwrap(), omitted);
}

#[test]
fn research_window_metrics_keep_exact_decimal_text_and_optional_values() {
    let wire = json!({
        "window": "longest_sample",
        "label": "longest_sample",
        "requested_years": null,
        "requested_start": "2026-11-18",
        "calendar_start": "2026-11-18",
        "calendar_end": "2026-11-20",
        "observations": 3,
        "expected_sessions": 3,
        "missing_sessions": 0,
        "coverage": "full",
        "rf_source": "rf=0",
        "metrics": {
            "return_pct": "9007199254740993.000000000000000001",
            "max_drawdown_pct": "0.0000",
            "cagr_pct": "12.345600",
            "sharpe": null,
            "volatility_pct": "4.432100",
            "turnover_pct": null
        }
    });
    let window: Window = serde_json::from_value(wire.clone()).unwrap();
    let metrics = window.metrics.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(metrics.sharpe, Some(None));
    assert_eq!(serde_json::to_value(window).unwrap(), wire);

    let legacy_wire = json!({
        "requested_years": 5,
        "requested_start": "2021-11-20",
        "calendar_start": "2021-11-20",
        "expected_sessions": 1257,
        "missing_sessions": 0,
        "coverage": "full",
        "metrics": {"return_pct": "1.2345", "max_drawdown_pct": "0.0000"}
    });
    let window: Window = serde_json::from_value(legacy_wire.clone()).unwrap();
    assert_eq!(serde_json::to_value(window).unwrap(), legacy_wire);
}

#[test]
fn public_openapi_exports_window_metrics_and_report_metrics() {
    let spec = serde_json::to_value(web::openapi()).unwrap();
    let upstream: Value =
        serde_json::from_str(include_str!("../openapi/finance.openapi.json")).unwrap();
    let schemas = &spec["components"]["schemas"];
    let upstream_schemas = upstream["components"]["schemas"].as_object().unwrap();
    for name in ["ResearchReport", "Window", "WindowMetrics", "Metrics"] {
        assert_eq!(
            schemas[name], upstream_schemas[name],
            "public schema differs from finance for {name}"
        );
    }
}
