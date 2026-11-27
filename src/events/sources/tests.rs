use super::*;

fn synthetic_job(id: i64, status: JobStatus) -> JobUpdate {
    JobUpdate::from(Job {
        id,
        status,
        error: Some(Some("synthetic diagnostic".into())),
        ..Job::default()
    })
}

#[test]
fn job_poll_tracks_transitions_and_uses_only_operational_fields() {
    let mut jobs = Jobs::default();
    let queued = synthetic_job(9_007_199_254_740_993, JobStatus::Queued);
    let running = synthetic_job(queued.id, JobStatus::Running);
    let completed = synthetic_job(queued.id, JobStatus::Succeeded);

    let first = jobs.record(Vec::new(), [queued]);
    let projected = serde_json::to_value(&first[0]).unwrap();
    assert_eq!(
        projected,
        serde_json::json!({
            "kind": "job",
            "job": {"id": "9007199254740993", "status": "queued", "attempt": 0},
        }),
    );
    assert!(
        jobs.record(vec![JobObservation::Unavailable], [])
            .is_empty()
    );
    let next = jobs.record(vec![JobObservation::Found(running)], []);
    assert!(matches!(&next[..], [EventPayload::Job { job }] if *job == running));
    let done = jobs.record(vec![JobObservation::Found(completed)], [queued]);
    assert!(matches!(&done[..], [EventPayload::Job { job }] if *job == completed));
    assert!(jobs.active.is_empty());
    assert_eq!(jobs.cursor, Some(queued.id));
}

fn synthetic_alert(sequence: i64) -> AlertUpdate {
    AlertUpdate {
        id: uuid::Uuid::from_u128(sequence as u128).to_string(),
        sequence,
        condition: Condition::SourceUnavailable,
        phase: Phase::Triggered,
        severity: Severity::Important,
        disposition: Disposition::InApp,
        observed_at: DateTime::UNIX_EPOCH,
        acknowledged_at: None,
        duplicate_count: 0,
        last_duplicate_at: None,
    }
}

#[test]
fn alert_poll_emits_changes_and_resets_after_the_window_is_replaced() {
    let mut alerts = Alerts::default();
    let mut events = Vec::new();
    let first = synthetic_alert(1);
    alerts.record(vec![first.clone()], &mut events);
    assert_eq!(events.len(), 1);
    events.clear();
    alerts.record(vec![first.clone()], &mut events);
    assert!(events.is_empty());

    let mut acknowledged = first;
    acknowledged.acknowledged_at = Some(DateTime::UNIX_EPOCH);
    alerts.record(vec![acknowledged], &mut events);
    assert!(
        matches!(&events[..], [EventPayload::Alert { alert }] if alert.acknowledged_at.is_some())
    );
    events.clear();

    alerts.record(
        (1..=ALERT_WINDOW + 1).rev().map(synthetic_alert).collect(),
        &mut events,
    );
    assert_eq!(events.len(), ALERT_WINDOW as usize);
    assert!(
        events
            .iter()
            .all(|event| matches!(event, EventPayload::Alert { .. }))
    );
    events.clear();

    alerts.record(
        (ALERT_WINDOW + 2..=2 * ALERT_WINDOW + 2)
            .rev()
            .map(synthetic_alert)
            .collect(),
        &mut events,
    );
    assert!(matches!(
        events.first(),
        Some(EventPayload::Reset {
            reason: EventResetReason::AlertWindowOverrun
        })
    ));
    assert_eq!(events.len(), ALERT_WINDOW as usize + 1);
}

#[tokio::test]
async fn alert_poll_from_empty_sends_sse_reset_only_when_window_overruns() {
    use axum::{
        body::to_bytes,
        response::{IntoResponse, Sse},
    };
    use std::convert::Infallible;

    let window = |count| (1..=count).rev().map(synthetic_alert).collect();
    let mut events = Vec::new();
    let mut alerts = Alerts::default();
    alerts.record(window(ALERT_WINDOW + 1), &mut events);
    assert_eq!(events.len(), ALERT_WINDOW as usize);
    assert!(
        events
            .iter()
            .all(|item| matches!(item, EventPayload::Alert { .. }))
    );

    alerts = Alerts::default();
    events.clear();
    alerts.record(Vec::new(), &mut events);
    assert!(events.is_empty());
    alerts.record(window(ALERT_WINDOW), &mut events);
    assert_eq!(events.len(), ALERT_WINDOW as usize);
    assert!(
        events
            .iter()
            .all(|item| matches!(item, EventPayload::Alert { .. }))
    );

    alerts = Alerts::default();
    events.clear();
    alerts.record(Vec::new(), &mut events);
    assert!(events.is_empty());
    alerts.record(window(ALERT_WINDOW + 1), &mut events);
    assert_eq!(events.len(), ALERT_WINDOW as usize + 1);
    let response = Sse::new(futures_util::stream::iter([Ok::<_, Infallible>(
        super::super::frame(events.remove(0), None),
    )]))
    .into_response();
    let body = to_bytes(response.into_body(), 1024).await.unwrap();
    let text = std::str::from_utf8(&body).unwrap();
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("event: reset"));
    let data: serde_json::Value =
        serde_json::from_str(lines.next().unwrap().strip_prefix("data: ").unwrap()).unwrap();
    assert_eq!(
        data,
        serde_json::json!({
            "schema_version": "1.0",
            "kind": "reset",
            "reason": "alert_window_overrun"
        })
    );
    for (expected, event) in (2..=ALERT_WINDOW + 1).zip(&events) {
        assert!(matches!(event, EventPayload::Alert { alert } if alert.sequence == expected));
    }
    events.clear();
    alerts.record(window(ALERT_WINDOW + 1), &mut events);
    assert!(events.is_empty());
}
