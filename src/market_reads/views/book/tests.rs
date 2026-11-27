use super::*;
use finance_client::models::{
    self, BookDelta, BookSnapshot, BookUpdate, Event, MarketMessageOneOf2, MarketMessageOneOf3,
    market_message_one_of_2, market_message_one_of_3,
};

fn query() -> WindowQuery {
    serde_json::from_value(serde_json::json!({
        "instrument_id": "SYNTH.XNYS", "source": "synthetic", "product": "book", "channel": "depth"
    }))
    .unwrap()
}

fn record(seq: u64, venue_instrument: &str, message: Message) -> WalRecord {
    WalRecord::new(
        Event {
            venue: "XNYS".into(),
            instrument_id: venue_instrument.into(),
            channel: "depth".into(),
            epoch: "1".into(),
            seq: seq.to_string(),
            message: Box::new(message),
            ..Event::default()
        },
        "1".into(),
        seq.to_string(),
    )
}

fn snapshot(seq: u64, venue_instrument: &str) -> WalRecord {
    record(
        seq,
        venue_instrument,
        Message::MarketMessageOneOf2(Box::new(MarketMessageOneOf2::new(
            market_message_one_of_2::Kind::Snapshot,
            BookSnapshot::new(
                vec![BookLevel::new("10.05".into(), "1.000".into())],
                vec![BookLevel::new("10.00".into(), "3.200".into())],
            ),
        ))),
    )
}

fn delta(seq: u64, venue_instrument: &str) -> WalRecord {
    record(
        seq,
        venue_instrument,
        Message::MarketMessageOneOf3(Box::new(MarketMessageOneOf3::new(
            market_message_one_of_3::Kind::Delta,
            BookDelta::new(vec![
                BookUpdate::new("10.000".into(), "0.000".into(), BookSide::Bid),
                BookUpdate::new("10.05".into(), "2.1000".into(), BookSide::Ask),
            ]),
        ))),
    )
}

#[test]
fn book_uses_exact_decimal_keys_and_does_not_mix_instruments() {
    let instrument = "SYNTH.XNYS";
    let query = query();
    let page = [
        snapshot(1, instrument),
        snapshot(99, "OTHER.XNYS"),
        delta(2, instrument),
    ];
    let book = FoldedBook::read(&query, &page).ok().unwrap();
    assert!(matches!(book.state(), BookState::Observed));
    let levels = book.levels.unwrap();
    assert!(levels.bids.is_empty());
    assert_eq!(levels.asks.first_key_value().unwrap().1.quantity, "2.1000");
    assert_eq!(book.observation.unwrap().seq, "2");
}

#[test]
fn book_drops_last_snapshot_on_sequence_gap_and_recovers_only_from_snapshot() {
    let instrument = "SYNTH.XNYS";
    let query = query();
    let page = [snapshot(1, instrument), delta(3, instrument)];
    let lost = FoldedBook::read(&query, &page).ok().unwrap();
    assert!(matches!(lost.state(), BookState::SnapshotRequired));
    assert!(lost.levels.is_none());
    let recovered = [delta(3, instrument), snapshot(5, instrument)];
    let available = FoldedBook::read(&query, &recovered).ok().unwrap();
    assert!(matches!(available.state(), BookState::Observed));
    assert_eq!(available.observation.unwrap().seq, "5");
}

#[test]
fn book_reset_clears_old_levels_even_without_a_followup_delta() {
    let instrument = "SYNTH.XNYS";
    let query = query();
    let reset = record(
        2,
        instrument,
        Message::MessageOneOf4(Box::new(models::MessageOneOf4::new(
            models::message_one_of_4::Kind::Reset,
            models::Reset::new("synthetic_reset".into()),
        ))),
    );
    let page = [snapshot(1, instrument), reset];
    let book = FoldedBook::read(&query, &page).ok().unwrap();
    assert!(matches!(book.state(), BookState::SnapshotRequired));
    assert!(book.levels.is_none());
}

#[test]
fn book_conflicting_or_regressed_snapshots_cannot_restore_synchronization() {
    let instrument = "SYNTH.XNYS";
    let query = query();
    let mut conflicting = snapshot(2, instrument);
    let Message::MarketMessageOneOf2(changed) = conflicting.event.message.as_mut() else {
        unreachable!();
    };
    changed.payload.bids[0].quantity = "4.00".into();
    let conflict = [snapshot(2, instrument), conflicting, delta(3, instrument)];
    let book = FoldedBook::read(&query, &conflict).ok().unwrap();
    assert!(matches!(book.state(), BookState::SnapshotRequired));
    assert!(book.levels.is_none());

    let mut current = snapshot(2, instrument);
    current.event.epoch = "2".into();
    let regained_old_epoch = [current, snapshot(99, instrument), snapshot(100, instrument)];
    let book = FoldedBook::read(&query, &regained_old_epoch).ok().unwrap();
    assert!(matches!(book.state(), BookState::SnapshotRequired));
    assert!(book.levels.is_none());
}
