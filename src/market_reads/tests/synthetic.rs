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
