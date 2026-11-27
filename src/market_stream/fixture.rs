//! Deterministic synthetic snapshots and WAL positions from typed protocol constructors.

use finance_client::models::{
    AssetKind, BookDelta, BookLevel, BookSide, BookSnapshot, BookUpdate, Event,
    MarketMessageOneOf2, MarketMessageOneOf3, MarketWalPage, Message, MessageOneOf4, MessageOneOf7,
    PerpetualContext, ProductPolicy, Reset, SchemaVersion, WalRecord, market_message_one_of_2,
    market_message_one_of_3, message_one_of_4, message_one_of_7,
};

pub(super) const SEQ: u64 = 9_007_199_254_740_993;
const PRICE: &str = "311.23900";
const QUANTITY: &str = "6.0100";

fn record(position: u64, epoch: u64, seq: u64, message: Message) -> WalRecord {
    let event = Event {
        schema_version: SchemaVersion::Variant10,
        channel: "l2".into(),
        source: "synthetic".into(),
        product: "book".into(),
        instrument_id: "SYNTH.XNYS".into(),
        asset_kind: AssetKind::Stock,
        venue: "XNYS".into(),
        exchange_ts: None,
        receive_ts: "1760000000000000000".into(),
        first_available_ts: "1760000000000000000".into(),
        epoch: epoch.to_string(),
        seq: seq.to_string(),
        cursor: format!("position-{epoch}-{seq}"),
        revision: "0".into(),
        license: "synthetic".into(),
        native_id: None,
        message: Box::new(message),
    };
    WalRecord::new(event, "1".into(), position.to_string())
}

pub(super) fn snapshot(position: u64, epoch: u64, seq: u64) -> WalRecord {
    let bid = BookLevel::new(PRICE.into(), QUANTITY.into());
    let message = Message::MarketMessageOneOf2(Box::new(MarketMessageOneOf2::new(
        market_message_one_of_2::Kind::Snapshot,
        BookSnapshot::new(vec![], vec![bid]),
    )));
    record(position, epoch, seq, message)
}

pub(super) fn delta(position: u64, epoch: u64, seq: u64) -> WalRecord {
    let update = BookUpdate::new(PRICE.into(), "0.0000".into(), BookSide::Bid);
    let message = Message::MarketMessageOneOf3(Box::new(MarketMessageOneOf3::new(
        market_message_one_of_3::Kind::Delta,
        BookDelta::new(vec![update]),
    )));
    record(position, epoch, seq, message)
}

pub(super) fn reset(position: u64, epoch: u64) -> WalRecord {
    let message = Message::MessageOneOf4(Box::new(MessageOneOf4::new(
        message_one_of_4::Kind::Reset,
        Reset::new("synthetic_reset".into()),
    )));
    record(position, epoch, 0, message)
}

pub(super) fn perpetual_context(position: u64, epoch: u64, seq: u64) -> WalRecord {
    let message = Message::MessageOneOf7(Box::new(MessageOneOf7::new(
        message_one_of_7::Kind::PerpetualContext,
        PerpetualContext::new("-0.00100".into(), PRICE.into(), QUANTITY.into()),
    )));
    let mut record = record(position, epoch, seq, message);
    record.event.schema_version = SchemaVersion::Variant20;
    record.event.instrument_id = "SYNTH.XTEST".into();
    record.event.venue = "XTEST".into();
    record.event.asset_kind = AssetKind::Perpetual;
    record.event.product = "context".into();
    record.event.channel = "context".into();
    record
}

pub(super) fn page(
    durable: u64,
    through: u64,
    next: Option<u64>,
    records: Vec<WalRecord>,
) -> MarketWalPage {
    MarketWalPage::new(
        ProductPolicy::new(
            false,
            false,
            true,
            "synthetic".into(),
            "book".into(),
            true,
            "synthetic".into(),
        ),
        "2025-10-09T08:53:20Z".into(),
        durable.to_string(),
        next.map(|value| value.to_string()),
        records,
        through.to_string(),
    )
}
