use finance_client::models::{Accepted, BacktestSummary, Page};
use serde_json::{Value, json};

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
