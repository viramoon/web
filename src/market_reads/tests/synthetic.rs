use serde_json::{Value, json};

pub(super) const SOURCE: &str = "synthetic";
pub(super) const PRODUCT: &str = "fixture_l2";
pub(super) const LICENSE: &str = "fixture_display";
pub(super) const INSTRUMENT: &str = "SYNTH.XNYS";
pub(super) const AS_OF: &str = "2026-01-01T00:00:00Z";
pub(super) const POSITION: &str = "9007199254740993";
pub(super) const PRICE: &str = "1234.010000000000000001";
pub(super) const SIZE: &str = "0.000000000000000100";

pub(super) fn access(display_live: bool) -> Value {
    json!({
        "source": SOURCE, "product": PRODUCT, "license": LICENSE,
        "display_live": display_live, "record_local": true,
        "derive_non_display": false, "backup_personal_drive": false
    })
}

// All values are constructed from fixed literals. Each variation exercises a distinct F1 payload.
pub(super) fn event(kind: &str, payload: Value) -> Value {
    let is_perpetual = kind == "perpetual_context";
    json!({
        "schema_version": if is_perpetual { "2.0" } else { "1.0" },
        "channel": "fixture",
        "source": SOURCE,
        "product": PRODUCT,
        "instrument_id": if is_perpetual { "SYNTH.XHYP" } else { INSTRUMENT },
        "asset_kind": if is_perpetual { "perpetual" } else { "stock" },
        "venue": if is_perpetual { "XHYP" } else { "XNYS" },
        "exchange_ts": null,
        "receive_ts": POSITION,
        "first_available_ts": POSITION,
        "epoch": "7",
        "seq": POSITION,
        "cursor": "fixture-cursor",
        "revision": "0",
        "license": LICENSE,
        "kind": kind, "payload": payload
    })
}

pub(super) fn trade() -> Value {
    json!({"price": PRICE, "quantity": SIZE, "side": "unknown"})
}

pub(super) fn wal_reply(record: Value, display: bool) -> Value {
    json!({
        "as_of": AS_OF,
        "access": access(display),
        "durable_through": POSITION,
        "through": POSITION,
        "next_after": null,
        "records": [{"position": POSITION, "generation": "1", "event": record}]
    })
}

pub(super) fn wal_rows(events: Vec<Value>) -> Value {
    let through = events.len().to_string();
    let records: Vec<_> = events
        .into_iter()
        .enumerate()
        .map(|(index, mut event)| {
            let position = (index + 1).to_string();
            event["seq"] = json!((9_007_199_254_740_992_u64 + index as u64).to_string());
            json!({"position": position, "generation": "1", "event": event})
        })
        .collect();
    json!({
        "as_of": AS_OF,
        "access": access(true),
        "durable_through": through,
        "through": through,
        "next_after": null,
        "records": records
    })
}

pub(super) fn book_snapshot() -> Value {
    use finance_client::models::{BookLevel, BookSnapshot};
    let snapshot = BookSnapshot::new(
        vec![BookLevel::new("101.01".into(), "4.70".into())],
        vec![
            BookLevel::new("100.10000".into(), "5.20".into()),
            BookLevel::new("99.9".into(), "2.0".into()),
        ],
    );
    event("snapshot", serde_json::to_value(snapshot).unwrap())
}

pub(super) fn book_delta() -> Value {
    use finance_client::models::{BookDelta, BookSide, BookUpdate};
    let delta = BookDelta::new(vec![
        BookUpdate::new("100.10".into(), "0.000".into(), BookSide::Bid),
        BookUpdate::new("101.01".into(), SIZE.into(), BookSide::Ask),
    ]);
    event("delta", serde_json::to_value(delta).unwrap())
}
