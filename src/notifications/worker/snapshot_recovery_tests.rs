use super::{NotificationPolicy, run_with_clock};
use crate::{
    database::Database,
    integrations::{TestEvidence, store as integrations},
    notifications::{
        channel::{
            ChannelConfigError, DeliveryOutcome, EmailChannels, MailKind, NotificationChannel,
            PreparedSnapshot, SmtpProfile,
        },
        model::{AttemptOutcome, DeliveryStatus},
    },
    test_postgres as postgres,
};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::json;
use sqlx::PgPool;
use std::{
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::{net::TcpListener, sync::watch, time};
use uuid::Uuid;

mod relay;

struct SnapshotLoss<'a> {
    channel: &'a EmailChannels,
    pool: &'a PgPool,
    snapshots: &'a Path,
    saved: PathBuf,
    lose_once: AtomicBool,
}

impl NotificationChannel for SnapshotLoss<'_> {
    fn validate(&self) -> Result<(), ChannelConfigError> {
        self.channel.validate()
    }

    async fn test(&self, _: Uuid) -> DeliveryOutcome {
        panic!("a candidate must use its bound snapshot")
    }

    async fn send(&self, _: Uuid, _: MailKind) -> DeliveryOutcome {
        panic!("the fixture contains only a candidate test")
    }

    async fn prepare_snapshot(&self, id: Uuid) -> Option<PreparedSnapshot> {
        self.channel.prepare_snapshot(id).await
    }

    async fn capture_profile(&self, profile: SmtpProfile) -> Option<Uuid> {
        self.channel.capture_profile(profile).await
    }

    async fn test_snapshot(&self, snapshot: Uuid, outbox_id: Uuid) -> DeliveryOutcome {
        if self.lose_once.swap(false, Ordering::Relaxed) {
            let bound: Uuid = sqlx::query_scalar(
                "SELECT smtp_snapshot_id FROM app.notification_outbox
                 WHERE id = $1 AND status = 'sending'",
            )
            .bind(outbox_id)
            .fetch_one(self.pool)
            .await
            .unwrap();
            assert_eq!(bound, snapshot);
            fs::rename(self.snapshots.join(format!("{bound}.smtp")), &self.saved).unwrap();
        }
        self.channel.test_snapshot(snapshot, outbox_id).await
    }
}

#[derive(sqlx::FromRow)]
struct Delivery {
    status: DeliveryStatus,
    attempts: i32,
    available_at: DateTime<Utc>,
    smtp_snapshot_id: Option<Uuid>,
    last_outcome: Option<AttemptOutcome>,
}

async fn delivery(pool: &PgPool, id: Uuid) -> Delivery {
    sqlx::query_as(
        "SELECT status, attempts, available_at, smtp_snapshot_id, last_outcome
         FROM app.notification_outbox WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn wait_for_outcome(pool: &PgPool, id: Uuid, outcome: AttemptOutcome) -> Delivery {
    time::timeout(Duration::from_secs(10), async {
        loop {
            let state = delivery(pool, id).await;
            if state.last_outcome == Some(outcome) && state.status != DeliveryStatus::Sending {
                return state;
            }
            time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("candidate never recorded {outcome:?}"))
}

async fn check_automatic_recovery(previous_attempts: i32, delay: TimeDelta) {
    let pg = postgres::Cluster::create();
    let url = pg.url();
    let db = Database::new(&url, Duration::from_secs(2), Duration::from_secs(2)).unwrap();
    db.migrate().await.unwrap();
    let pool = PgPool::connect(&url).await.unwrap();

    let root = tempfile::tempdir_in("/tmp").unwrap();
    let snapshots = root.path().join("snapshots");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&snapshots)
        .unwrap();
    let static_root = root.path().join("static");
    fs::create_dir(&static_root).unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let certificate = root.path().join("relay.pem");
    let tls = relay::acceptor(&certificate);
    let mut email = EmailChannels::for_notifier(|name| match name {
        "WEB_STATIC_ROOT" => Some(static_root.to_str().unwrap().into()),
        "WEB_SMTP_SNAPSHOT_DIR" => Some(snapshots.to_str().unwrap().into()),
        "WEB_SMTP_HOST" | "WEB_SMTP_SECONDARY_HOST" => Some("127.0.0.1".into()),
        "WEB_SMTP_PORT" | "WEB_SMTP_SECONDARY_PORT" => Some(port.to_string()),
        "WEB_SMTP_FROM" => Some("primary@example.invalid".into()),
        "WEB_SMTP_SECONDARY_FROM" => Some(relay::SENDER.into()),
        "WEB_SMTP_TO" | "WEB_SMTP_SECONDARY_TO" => Some(relay::RECIPIENT.into()),
        _ => None,
    })
    .unwrap();
    email.trust_test_ca(&certificate);
    let channel = SnapshotLoss {
        channel: &email,
        pool: &pool,
        snapshots: &snapshots,
        saved: root.path().join("saved-candidate"),
        lose_once: AtomicBool::new(true),
    };

    let integration = integrations::create(&db, "smtp", json!({"profile": "secondary"}))
        .await
        .unwrap();
    let integration_id = integration.id.parse().unwrap();
    let test = integrations::begin_test(&db, integration_id, integration.revision)
        .await
        .unwrap();
    let test_id: Uuid = test.test.id.parse().unwrap();
    let id: Uuid =
        sqlx::query_scalar("SELECT id FROM app.notification_outbox WHERE integration_test_id = $1")
            .bind(test_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let original_binding = if previous_attempts == 0 {
        None
    } else {
        let snapshot = email.capture_profile(SmtpProfile::Secondary).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(
            "UPDATE app.notification_outbox SET smtp_snapshot_id = $2, attempts = $3,
                 last_outcome = 'transient' WHERE id = $1",
        )
        .bind(id)
        .bind(snapshot)
        .bind(previous_attempts)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO app.notification_attempts (outbox_id, attempt, started_at, finished_at, outcome)
             SELECT $1, attempt, now() - interval '2 hours', now() - interval '2 hours', 'transient'
             FROM generate_series(1, $2) attempt",
        )
        .bind(id)
        .bind(previous_attempts)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        Some(snapshot)
    };
    assert_eq!(delivery(&pool, id).await.smtp_snapshot_id, original_binding);

    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let (advance, clock) = watch::channel(now);
    let (sampled, mut polls) = watch::channel(now);
    let (shutdown, stop) = watch::channel(false);
    let policy = NotificationPolicy::default();
    let checks = async {
        let waiting = wait_for_outcome(&pool, id, AttemptOutcome::ConfigurationError).await;
        assert_eq!(waiting.status, DeliveryStatus::Pending);
        assert_eq!(waiting.attempts, previous_attempts + 1);
        let binding = waiting.smtp_snapshot_id.unwrap();
        if let Some(original) = original_binding {
            assert_eq!(binding, original);
        }
        let due = waiting.available_at;
        assert_eq!(due, now + delay);
        let pending = integrations::get_test(&db, test_id).await.unwrap();
        assert!(matches!(pending.status, integrations::TestStatus::Pending));
        assert!(pending.evidence.is_none());

        fs::rename(&channel.saved, snapshots.join(format!("{binding}.smtp"))).unwrap();
        let early = due - TimeDelta::seconds(1);
        advance.send_replace(early);
        polls.wait_for(|sample| *sample == early).await.unwrap();
        assert!(
            time::timeout(super::POLL_INTERVAL, listener.accept())
                .await
                .is_err()
        );
        assert_eq!(delivery(&pool, id).await.attempts, previous_attempts + 1);

        advance.send_replace(due);
        let (stream, _) = listener.accept().await.unwrap();
        let message = relay::accept_mail(tls, stream).await;
        assert!(
            message
                .lines()
                .any(|line| line == "Subject: Fertile notification test")
        );
        assert!(
            message
                .lines()
                .any(|line| line == format!("Message-ID: <{id}@web-notifier.invalid>"))
        );
        assert!(message.contains("This test message checks your Fertile email channel."));

        let accepted = wait_for_outcome(&pool, id, AttemptOutcome::Accepted).await;
        assert_eq!(accepted.status, DeliveryStatus::Accepted);
        assert_eq!(accepted.smtp_snapshot_id, Some(binding));
        assert_eq!(accepted.attempts, previous_attempts + 2);
        let attempts: Vec<(DateTime<Utc>, AttemptOutcome)> = sqlx::query_as(
            "SELECT started_at, outcome FROM app.notification_attempts
             WHERE outbox_id = $1 AND attempt > $2 ORDER BY attempt",
        )
        .bind(id)
        .bind(previous_attempts)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            attempts,
            [
                (now, AttemptOutcome::ConfigurationError),
                (due, AttemptOutcome::Accepted)
            ]
        );
        let passed = integrations::get_test(&db, test_id).await.unwrap();
        assert!(matches!(passed.status, integrations::TestStatus::Passed));
        assert!(passed.finished_at.is_some());
        assert!(matches!(
            passed.evidence,
            Some(TestEvidence::Smtp(AttemptOutcome::Accepted))
        ));
        let actions: Vec<String> = sqlx::query_scalar(
            "SELECT action FROM app.integration_audit WHERE test_id = $1 ORDER BY sequence",
        )
        .bind(test_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(actions, ["test_started", "test_passed"]);
        shutdown.send(true).unwrap();
    };
    let worker = run_with_clock(&db, &policy, &channel, stop, || {
        let instant = *clock.borrow();
        sampled.send_replace(instant);
        instant
    });
    let ((), result) = time::timeout(Duration::from_secs(30), async {
        tokio::join!(checks, worker)
    })
    .await
    .expect("running notifier did not recover the candidate");
    result.unwrap();
    pg.stop();
}

#[tokio::test]
async fn running_notifier_accepts_restored_prebound_candidate() {
    check_automatic_recovery(4, TimeDelta::seconds(240)).await;
}

#[tokio::test]
async fn running_notifier_accepts_restored_first_bound_candidate() {
    check_automatic_recovery(0, TimeDelta::seconds(30)).await;
}
