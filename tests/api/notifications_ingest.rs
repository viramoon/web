use super::postgres::Cluster;
use chrono::{DateTime, TimeDelta, Utc};
use std::time::Duration;
use uuid::Uuid;
use web::{
    database::Database,
    notifications::{
        model::{AlertSignal, Condition, Disposition, Phase},
        store::{RecordError, record_signal},
    },
};

// Synthetic fixture. The subject is an opaque test UUID and observations use fixed UTC offsets.
fn signal(phase: Phase, minute: i64) -> AlertSignal {
    let epoch: DateTime<Utc> = "2026-01-01T00:00:00Z".parse().unwrap();
    let time = epoch + TimeDelta::minutes(minute);
    AlertSignal {
        condition: Condition::SourceUnavailable,
        incident_key: Uuid::from_u128(26),
        window_start: time,
        observed_at: time,
        phase,
    }
}

#[tokio::test]
async fn postgres_notification_ingest_is_atomic_and_deduplicates_replays() {
    let cluster = Cluster::create();
    let url = cluster.url();
    let db = Database::new(&url, Duration::from_secs(2), Duration::from_secs(2)).unwrap();
    db.migrate().await.unwrap();
    let pool = sqlx::PgPool::connect(&url).await.unwrap();
    let rule_id = Uuid::from_u128(2601);
    sqlx::query(
        "INSERT INTO app.notification_rules (id, condition, severity, enabled, email_enabled,
             reminder_seconds, min_occurrences)
         VALUES ($1, 'source_unavailable', 'urgent', true, true, 3600, 1)",
    )
    .bind(rule_id)
    .execute(&pool)
    .await
    .unwrap();

    // Fail the outbox insert after the event and incident have already been written.
    sqlx::query("ALTER TABLE app.notification_outbox ADD CONSTRAINT reject_test_alert CHECK (kind <> 'alert')")
        .execute(&pool)
        .await
        .unwrap();
    let active = signal(Phase::Triggered, 0);
    assert!(matches!(
        record_signal(&db, rule_id, active).await,
        Err(RecordError::Database(_))
    ));
    let state: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM app.alert_events),
             (SELECT count(*) FROM app.notification_outbox),
             (SELECT count(*) FROM app.notification_incidents)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, (0, 0, 0));
    sqlx::query("ALTER TABLE app.notification_outbox DROP CONSTRAINT reject_test_alert")
        .execute(&pool)
        .await
        .unwrap();

    let (first, second) = tokio::join!(
        record_signal(&db, rule_id, active),
        record_signal(&db, rule_id, active),
    );
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!(first.id, second.id);
    assert_ne!(first.duplicate, second.duplicate);
    assert_eq!(first.disposition, Disposition::Pending);
    assert_eq!(second.disposition, Disposition::Pending);
    let audit: (i64, bool, i64) = sqlx::query_as(
        "SELECT duplicate_count, last_duplicate_at IS NOT NULL,
             (SELECT count(*) FROM app.notification_outbox WHERE event_id = app.alert_events.id)
         FROM app.alert_events WHERE id = $1",
    )
    .bind(first.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audit, (1, true, 1));

    let reminder = record_signal(&db, rule_id, signal(Phase::Triggered, 60))
        .await
        .unwrap();
    let recovered = record_signal(&db, rule_id, signal(Phase::Recovered, 61))
        .await
        .unwrap();
    assert_eq!(recovered.disposition, Disposition::Pending);
    let deliveries: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT kind, status, suppression_reason FROM app.notification_outbox
         WHERE event_id = ANY($1) ORDER BY kind",
    )
    .bind([reminder.id, recovered.id].as_slice())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        deliveries,
        [
            ("recovery".into(), "pending".into(), None),
            (
                "reminder".into(),
                "suppressed".into(),
                Some("incident_recovered".into())
            ),
        ]
    );
    let replay = record_signal(&db, rule_id, signal(Phase::Recovered, 61))
        .await
        .unwrap();
    assert!(replay.duplicate);
    assert_eq!(replay.id, recovered.id);
    pool.close().await;
    db.close().await;
    cluster.stop();
}
