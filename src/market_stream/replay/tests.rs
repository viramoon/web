use super::*;
use crate::market_stream::fixture::{SEQ, delta, page, perpetual_context, reset, snapshot};
use serde_json::Value;

fn replay(after: u64) -> MarketReplay {
    let partition = MarketPartition::new("synthetic", "book", "l2", "SYNTH.XNYS").unwrap();
    MarketReplay::new(partition, after)
}

#[test]
fn replay_keeps_f1_precision_and_filters_other_partitions_across_pinned_pages() {
    let mut replay = replay(0);
    let mut unrelated = snapshot(2, 1, 1);
    unrelated.event.instrument_id = "OTHER.XNYS".into();
    let first = replay
        .accept(page(3, 3, Some(2), vec![snapshot(1, 1, SEQ), unrelated]))
        .unwrap();
    assert_eq!(first.len(), 1);
    let envelope = serde_json::to_value(&*first[0]).unwrap();
    assert_eq!(envelope["source"], "synthetic");
    assert_eq!(envelope["product"], "book");
    assert_eq!(envelope["license"], "synthetic");
    assert_eq!(envelope["seq"], SEQ.to_string());
    assert_eq!(envelope["exchange_ts"], Value::Null);
    assert_eq!(envelope["payload"]["bids"][0]["price"], "311.23900");
    assert_eq!(envelope["payload"]["bids"][0]["quantity"], "6.0100");

    let second = replay
        .accept(page(4, 3, None, vec![delta(3, 1, SEQ + 1)]))
        .unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].seq, (SEQ + 1).to_string());
    assert_eq!(replay.after, 3);
    assert_eq!(replay.through, None);
}

#[test]
fn replay_deduplicates_one_f1_position_even_when_receive_time_changes() {
    let mut replay = replay(0);
    let mut duplicate = snapshot(2, 1, SEQ);
    duplicate.event.receive_ts = "1760000000000000001".into();
    let delivered = replay
        .accept(page(
            3,
            3,
            None,
            vec![snapshot(1, 1, SEQ), duplicate, delta(3, 1, SEQ + 1)],
        ))
        .unwrap();
    assert_eq!(delivered.len(), 2);
    assert_eq!(delivered[0].receive_ts, "1760000000000000000");
    assert_eq!(replay.after, 3);
}

#[test]
fn replay_rejects_sequence_gaps_and_epoch_changes_without_reset() {
    for (epoch, seq) in [(1, SEQ + 2), (2, 0)] {
        let mut replay = replay(0);
        let error = replay
            .accept(page(
                2,
                2,
                None,
                vec![snapshot(1, 1, SEQ), delta(2, epoch, seq)],
            ))
            .unwrap_err();
        assert_eq!(
            error,
            ReplayError::ResetRequired(ResetReason::Discontinuity)
        );
        assert_eq!(replay.after, 0);
    }
}

#[test]
fn replay_drops_old_book_on_producer_reset_and_requires_fresh_snapshot() {
    let mut replay = replay(0);
    let first = replay
        .accept(page(2, 2, None, vec![snapshot(1, 1, SEQ), reset(2, 2)]))
        .unwrap();
    assert_eq!(first[1].epoch, "2");
    assert_eq!(first[1].seq, "0");
    assert_eq!(
        replay.accept(page(3, 3, None, vec![delta(3, 2, 1)])),
        Err(ReplayError::ResetRequired(ResetReason::SnapshotRequired))
    );
    assert_eq!(
        replay
            .accept(page(4, 4, None, vec![snapshot(3, 2, 1), delta(4, 2, 2)]))
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn replay_requires_reset_on_product_license_change() {
    let mut replay = replay(0);
    replay
        .accept(page(1, 1, None, vec![snapshot(1, 1, SEQ)]))
        .unwrap();
    let mut next = delta(2, 1, SEQ + 1);
    next.event.license = "other".into();
    let mut changed_license = page(2, 2, None, vec![next]);
    changed_license.access.license = "other".into();
    assert_eq!(
        replay.accept(changed_license),
        Err(ReplayError::ResetRequired(ResetReason::CursorUnavailable))
    );
    assert_eq!(replay.after, 1);
}

#[test]
fn replay_resets_slow_consumer_without_advancing_or_delivering_old_book() {
    let mut replay = replay(0);
    let _ = replay
        .accept(page(1, 1, None, vec![snapshot(1, 1, SEQ)]))
        .unwrap();
    let durable = MAX_BACKLOG + 2;
    assert_eq!(
        replay.accept(page(durable, durable, Some(2), vec![delta(2, 1, SEQ + 1)])),
        Err(ReplayError::ResetRequired(ResetReason::ConsumerLagged))
    );
    assert_eq!(replay.after, 1);
}

#[test]
fn replay_keeps_f10a_perpetual_context_numeric_text() {
    let partition = MarketPartition::new("synthetic", "context", "context", "SYNTH.XTEST").unwrap();
    let mut replay = MarketReplay::new(partition, 0);
    let mut page = page(1, 1, None, vec![perpetual_context(1, 2, SEQ)]);
    page.access.product = "context".into();
    let delivered = replay.accept(page).unwrap();
    let envelope = serde_json::to_value(&*delivered[0]).unwrap();
    assert_eq!(envelope["schema_version"], "2.0");
    assert_eq!(envelope["payload"]["funding_rate"], "-0.00100");
    assert_eq!(envelope["seq"], SEQ.to_string());
}
