//! Synthetic alert metadata with a process-local SMTP substitute and isolated PostgreSQL.

use super::postgres::Cluster;
use chrono::{DateTime, TimeDelta, Utc};
use sqlx::PgPool;
use std::{collections::VecDeque, sync::Mutex, time::Duration};
use tokio::sync::Notify;
use uuid::Uuid;
use web::{
    database::Database,
    notifications::{
        channel::{ChannelConfigError, DeliveryOutcome, MailKind, NotificationChannel},
        model::{AttemptOutcome, Condition},
        worker::{NotificationPolicy, NotificationState, dispatch_one, health},
    },
};

fn at(value: &str) -> DateTime<Utc> {
    value.parse().unwrap()
}

fn later(time: DateTime<Utc>, seconds: i64) -> DateTime<Utc> {
    time + TimeDelta::seconds(seconds)
}

struct Fixture {
    db: Database,
    pool: PgPool,
    _cluster: Cluster,
}

impl Fixture {
    async fn create() -> Self {
        let cluster = Cluster::create();
        let db = Database::new(
            &cluster.url(),
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .unwrap();
        db.migrate().await.unwrap();
        let pool = PgPool::connect(&cluster.url()).await.unwrap();
        Self {
            db,
            pool,
            _cluster: cluster,
        }
    }

    async fn rule(&self, urgent: bool) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO app.notification_rules
             (id, condition, severity, enabled, email_enabled, min_occurrences)
             VALUES ($1, 'source_unavailable', $2, true, true, 1)",
        )
        .bind(id)
        .bind(if urgent { "urgent" } else { "important" })
        .execute(&self.pool)
        .await
        .unwrap();
        id
    }

    async fn queue_event(
        &self,
        rule: Uuid,
        incident: Uuid,
        kind: &str,
        now: DateTime<Utc>,
    ) -> Uuid {
        let phase = if kind == "recovery" {
            "recovered"
        } else {
            "triggered"
        };
        let event_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO app.alert_events
                (id, rule_id, condition, severity, phase, incident_key, business_window, observed_at, disposition)
             SELECT $1, id, condition, severity, $3, $4, $5, $5, 'pending'
             FROM app.notification_rules WHERE id = $2",
        )
        .bind(event_id)
        .bind(rule)
        .bind(phase)
        .bind(incident)
        .bind(now)
        .execute(&self.pool)
        .await
        .unwrap();
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO app.notification_outbox
                (id, event_id, rule_id, kind, priority, created_at, available_at, updated_at)
             SELECT $1, $2, id, $4, CASE WHEN severity = 'urgent' THEN 'urgent' ELSE 'normal' END, $5, $5, $5
             FROM app.notification_rules WHERE id = $3",
        )
        .bind(id)
        .bind(event_id)
        .bind(rule)
        .bind(kind)
        .bind(now)
        .execute(&self.pool)
        .await
        .unwrap();
        id
    }

    async fn next(&self, channel: &impl NotificationChannel, time: DateTime<Utc>) -> bool {
        dispatch_one(&self.db, &NotificationPolicy::default(), channel, || time)
            .await
            .unwrap()
    }

    async fn attempts(&self, id: Uuid) -> Vec<AttemptOutcome> {
        sqlx::query_scalar(
            "SELECT outcome FROM app.notification_attempts WHERE outbox_id = $1 ORDER BY attempt",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }

    async fn deferrals(&self, id: Uuid) -> Vec<(String, DateTime<Utc>)> {
        sqlx::query_as(
            "SELECT reason, available_at FROM app.notification_deferrals WHERE outbox_id = $1 ORDER BY sequence",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

#[derive(Default)]
struct FakeSmtp {
    outcomes: Mutex<VecDeque<DeliveryOutcome>>,
    sent: Mutex<Vec<(Uuid, Option<MailKind>)>>,
}

impl FakeSmtp {
    fn with_outcomes(outcomes: impl IntoIterator<Item = DeliveryOutcome>) -> Self {
        Self {
            outcomes: Mutex::new(outcomes.into_iter().collect()),
            ..Self::default()
        }
    }

    fn record(&self, id: Uuid, mail: Option<MailKind>) -> DeliveryOutcome {
        self.sent.lock().unwrap().push((id, mail));
        self.outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(DeliveryOutcome::Accepted)
    }
}

impl NotificationChannel for FakeSmtp {
    fn validate(&self) -> Result<(), ChannelConfigError> {
        Ok(())
    }

    async fn test(&self, id: Uuid) -> DeliveryOutcome {
        self.record(id, None)
    }

    async fn send(&self, id: Uuid, mail: MailKind) -> DeliveryOutcome {
        self.record(id, Some(mail))
    }
}

#[tokio::test]
async fn smtp_timeout_retries_with_same_id_and_outage_recovery_is_audited() {
    let fixture = Fixture::create().await;
    let mail = FakeSmtp::with_outcomes([
        DeliveryOutcome::Timeout,
        DeliveryOutcome::Rejected,
        DeliveryOutcome::Accepted,
    ]);
    let start = at("2026-11-27T01:00:00Z");
    let initial_rule = fixture.rule(false).await;
    let failing_rule = fixture.rule(false).await;
    let initial = fixture
        .queue_event(initial_rule, Uuid::new_v4(), "alert", start)
        .await;
    let failing = fixture
        .queue_event(failing_rule, Uuid::new_v4(), "alert", later(start, 1))
        .await;
    assert_eq!(health(&fixture.db).await.state, NotificationState::Idle);

    assert!(fixture.next(&mail, start).await);
    let smtp = health(&fixture.db).await;
    assert_eq!(smtp.state, NotificationState::Degraded);
    assert_eq!(smtp.last_outcome, Some(AttemptOutcome::Timeout));
    assert_eq!(smtp.failure_since, Some(start));
    assert!(fixture.next(&mail, later(start, 1)).await);
    assert!(!fixture.next(&mail, later(start, 29)).await);
    assert!(fixture.next(&mail, later(start, 30)).await);
    assert_eq!(health(&fixture.db).await.state, NotificationState::Healthy);
    assert_eq!(
        fixture.attempts(initial).await,
        [AttemptOutcome::Timeout, AttemptOutcome::Accepted]
    );
    assert_eq!(fixture.attempts(failing).await, [AttemptOutcome::Rejected]);
    let status: String =
        sqlx::query_scalar("SELECT status FROM app.notification_outbox WHERE id = $1")
            .bind(failing)
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert_eq!(status, "failed");

    assert!(fixture.next(&mail, later(start, 31)).await);
    assert!(!fixture.next(&mail, later(start, 32)).await);
    {
        let calls = mail.sent.lock().unwrap();
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0], calls[2]);
        assert_eq!(calls[0].0, initial);
        assert_eq!(calls[3].1, Some(MailKind::TransportRecovered { count: 2 }));
    }
    let health_phases: Vec<String> = sqlx::query_scalar(
        "SELECT phase FROM app.alert_events WHERE condition = 'smtp_unavailable' ORDER BY sequence",
    )
    .fetch_all(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(health_phases, ["triggered", "recovered"]);
}

#[tokio::test]
async fn quiet_hours_audit_digest_and_urgent_bypass() {
    let fixture = Fixture::create().await;
    let mail = FakeSmtp::default();
    let quiet = at("2026-11-27T15:00:00Z"); // 23:00 Asia/Shanghai
    let morning = at("2026-11-28T00:00:00Z"); // 08:00 Asia/Shanghai
    let first_rule = fixture.rule(false).await;
    let urgent_rule = fixture.rule(true).await;
    let first = fixture
        .queue_event(first_rule, Uuid::new_v4(), "alert", quiet)
        .await;
    let second = fixture
        .queue_event(first_rule, Uuid::new_v4(), "alert", later(quiet, 1))
        .await;
    let urgent = fixture
        .queue_event(urgent_rule, Uuid::new_v4(), "alert", quiet)
        .await;
    let test = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO app.notification_outbox (id, kind, priority, test_key, created_at, available_at, updated_at)
         VALUES ($1, 'test', 'normal', $1, $2, $2, $2)",
    )
    .bind(test)
    .bind(quiet)
    .execute(&fixture.pool)
    .await
    .unwrap();

    assert!(fixture.next(&mail, later(quiet, 1)).await);
    assert!(fixture.next(&mail, later(quiet, 1)).await);
    assert!(!fixture.next(&mail, later(morning, -1)).await);
    for normal in [first, second] {
        assert_eq!(
            fixture.deferrals(normal).await,
            [("quiet_hours".into(), morning)]
        );
    }
    assert!(fixture.next(&mail, morning).await);
    assert!(!fixture.next(&mail, morning).await);
    assert_eq!(
        *mail.sent.lock().unwrap(),
        [
            (
                urgent,
                Some(MailKind::Alert {
                    condition: Condition::SourceUnavailable
                })
            ),
            (test, None),
            (first, Some(MailKind::Digest { count: 2 })),
        ]
    );
    let summary: (String, Option<Uuid>) =
        sqlx::query_as("SELECT status, summary_id FROM app.notification_outbox WHERE id = $1")
            .bind(second)
            .fetch_one(&fixture.pool)
            .await
            .unwrap();
    assert_eq!(summary, ("summarized".into(), Some(first)));
}

#[tokio::test]
async fn sliding_hour_caps_both_priorities_and_aggregates_overflow() {
    let fixture = Fixture::create().await;
    let mail = FakeSmtp::default();
    let start = at("2026-11-27T01:00:00Z");
    let hour = later(start, 3600);
    for offset in 0..6 {
        for urgent in [false, true]
            .into_iter()
            .filter(|urgent| !urgent || offset < 2)
        {
            let rule = fixture.rule(urgent).await;
            let instant = later(start, offset);
            fixture
                .queue_event(rule, Uuid::new_v4(), "alert", instant)
                .await;
            assert!(fixture.next(&mail, instant).await);
        }
    }
    let overflow: Vec<Uuid> = {
        let mut overflow = Vec::new();
        for urgent in [false, false, true] {
            let rule = fixture.rule(urgent).await;
            overflow.push(
                fixture
                    .queue_event(rule, Uuid::new_v4(), "alert", later(start, 6))
                    .await,
            );
        }
        overflow
    };
    assert!(!fixture.next(&mail, later(start, 6)).await);
    assert!(!fixture.next(&mail, later(hour, -1)).await);
    for id in overflow {
        assert_eq!(fixture.deferrals(id).await, [("rate_limit".into(), hour)]);
    }
    assert!(fixture.next(&mail, hour).await);
    assert!(fixture.next(&mail, hour).await);
    assert!(!fixture.next(&mail, hour).await);
    let calls = mail.sent.lock().unwrap();
    assert_eq!(calls.len(), 10);
    assert_eq!(calls[9].1, Some(MailKind::Digest { count: 2 }));
}

#[tokio::test]
async fn cooldown_preserves_alert_before_recovery_for_the_same_incident() {
    let fixture = Fixture::create().await;
    let mail = FakeSmtp::default();
    let start = at("2026-11-27T01:00:00Z");
    let rule = fixture.rule(false).await;
    let incident = Uuid::new_v4();
    fixture
        .queue_event(rule, Uuid::new_v4(), "alert", start)
        .await;
    let delayed = fixture
        .queue_event(rule, incident, "alert", later(start, 1))
        .await;
    let restored = fixture
        .queue_event(rule, incident, "recovery", later(start, 2))
        .await;
    assert!(fixture.next(&mail, start).await);
    assert!(!fixture.next(&mail, later(start, 2)).await);
    assert_eq!(
        fixture.deferrals(delayed).await,
        [("cooldown".into(), later(start, 900))]
    );
    assert!(fixture.next(&mail, later(start, 900)).await);
    assert!(fixture.next(&mail, later(start, 900)).await);
    assert!(!fixture.next(&mail, later(start, 900)).await);
    let calls = mail.sent.lock().unwrap();
    assert_eq!(calls[1].0, delayed);
    assert_eq!(
        calls[2],
        (
            restored,
            Some(MailKind::Recovery {
                condition: Condition::SourceUnavailable
            })
        )
    );
}

#[derive(Default)]
struct DisconnectedSmtp(Notify);

impl NotificationChannel for DisconnectedSmtp {
    fn validate(&self) -> Result<(), ChannelConfigError> {
        Ok(())
    }

    async fn test(&self, _id: Uuid) -> DeliveryOutcome {
        unreachable!("fixture sends an alert")
    }

    async fn send(&self, _id: Uuid, _mail: MailKind) -> DeliveryOutcome {
        self.0.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn a_crashed_sender_leaves_one_unknown_attempt_before_retry() {
    let fixture = Fixture::create().await;
    let start = at("2026-11-27T01:00:00Z");
    let rule = fixture.rule(false).await;
    let id = fixture
        .queue_event(rule, Uuid::new_v4(), "alert", start)
        .await;
    let hanging = DisconnectedSmtp::default();
    let fake = FakeSmtp::default();
    let worker = fixture.next(&hanging, start);
    // Keep the future out of the final scope so cancellation closes its locked SQL session.
    {
        let mut worker = Box::pin(worker);
        tokio::select! {
            () = hanging.0.notified() => {},
            _ = &mut worker => panic!("the fake SMTP attempt should remain in flight"),
        }
        assert_eq!(fixture.attempts(id).await, [AttemptOutcome::Sending]);
        assert!(!fixture.next(&fake, start).await);
    }
    assert!(!fixture.next(&fake, later(start, 64)).await);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            assert!(!fixture.next(&fake, later(start, 65)).await);
            if fixture.attempts(id).await == [AttemptOutcome::Interrupted] {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(!fixture.next(&fake, later(start, 94)).await);
    assert!(fixture.next(&fake, later(start, 95)).await);
    assert_eq!(
        fixture.attempts(id).await,
        [AttemptOutcome::Interrupted, AttemptOutcome::Accepted]
    );
    assert_eq!(fake.sent.lock().unwrap()[0].0, id);
}
