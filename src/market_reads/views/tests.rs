use super::*;

fn window_page(
    through: &str,
    durable_through: &str,
    next_after: Option<&str>,
    positions: &[&str],
) -> models::MarketWalPage {
    models::MarketWalPage::new(
        models::ProductPolicy::new(
            false,
            false,
            true,
            "synthetic".into(),
            "book".into(),
            true,
            "synthetic".into(),
        ),
        "2026-01-01T00:00:00Z".into(),
        durable_through.into(),
        next_after.map(str::to_owned),
        positions
            .iter()
            .map(|position| {
                models::WalRecord::new(models::Event::default(), "1".into(), (*position).into())
            })
            .collect(),
        through.into(),
    )
}

#[test]
fn market_view_preserves_f10a_partial_window_and_rejects_untrustworthy_positions() {
    let id = InstrumentId::parse("SYNTH.XNYS".into()).unwrap();
    let after = "9007199254740993";
    let position = "9007199254740994";
    let through = "9007199254740999";
    let durable_through = "9007199254740998";
    let valid = window_page(durable_through, through, Some(position), &[position]);
    let (metadata, records) =
        checked_window(id.clone(), after.into(), Some(durable_through), 1, valid)
            .ok()
            .unwrap();
    assert_eq!(metadata.coverage.after, after);
    assert_eq!(metadata.coverage.through, durable_through);
    assert_eq!(metadata.coverage.durable_through, through);
    assert_eq!(metadata.coverage.next_after.as_deref(), Some(position));
    assert_eq!(records[0].position, position);
    let hole = window_page(through, through, Some(position), &["9007199254740995"]);
    let result = checked_window(id, after.into(), Some(through), 1, hole);
    assert!(matches!(result, Err(MarketReadError::InvalidResponse)));
}

#[test]
fn market_views_openapi_distinguishes_explicit_queries_from_depth_focus() {
    let spec = crate::openapi();
    let is_required = |route: &str, name: &str| {
        spec["paths"][route]["get"]["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|parameter| parameter["name"] == name)
            .and_then(|parameter| parameter["required"].as_bool())
    };
    assert_ne!(is_required(BOOK, "instrument_id"), Some(true));
    assert_eq!(is_required(QUOTES, "instrument_id"), Some(true));
    assert_eq!(is_required(BOOK, "channel"), Some(true));
    assert_eq!(is_required(BARS, "indicator"), None);
    assert_eq!(is_required(INDICATOR_VALUES, "indicator"), Some(true));
    assert_eq!(
        spec["components"]["schemas"]["MarketHistory"]["allOf"][0]["$ref"],
        "#/components/schemas/MarketViewMetadata"
    );
}
