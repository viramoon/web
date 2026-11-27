use super::*;
use crate::notifications::{
    channel::{ChannelConfigError, DeliveryOutcome, MailKind, NotificationChannel, SmtpProfile},
    store::enqueue_test,
    worker::{NotificationPolicy, dispatch_one},
};
use serde_json::json;
use std::{sync::Mutex, time::Duration};

#[path = "../../../tests/api/postgres.rs"]
mod postgres;

fn new_reference() -> CredentialRef {
    CredentialRef {
        reference: Uuid::new_v4(),
        mask: "********".into(),
        format: "jwe".into(),
    }
}

fn test_id(run: &TestInput) -> Uuid {
    Uuid::parse_str(&run.test.id).unwrap()
}

fn fixture(outcome: Outcome) -> (Outcome, TestEvidence) {
    let evidence = match outcome {
        Outcome::Passed => AttemptOutcome::Accepted,
        Outcome::Failed => AttemptOutcome::Rejected,
        Outcome::Unavailable => AttemptOutcome::ConfigurationError,
    };
    (outcome, TestEvidence::Smtp(evidence))
}

#[tokio::test]
async fn postgres_rotation_preserves_active_and_supersedes_stale_attempts() {
    let pg = postgres::Cluster::create();
    let db = Database::new(&pg.url(), Duration::from_secs(2), Duration::from_secs(2)).unwrap();
    db.migrate().await.unwrap();
    let settings = json!({"symbol": "SPX"});
    let created = create(&db, "licensed_options", settings.clone())
        .await
        .unwrap();
    let id = Uuid::parse_str(&created.id).unwrap();
    let first = set_credential(&db, id, created.revision, new_reference(), async { Ok(()) })
        .await
        .unwrap();
    let run = begin_test(&db, id, first.revision).await.unwrap();
    let pending = get(&db, id).await.unwrap();
    assert!(matches!(pending.latest.state, VersionState::Candidate));
    assert!(pending.active.is_none());
    assert_eq!(pending.last_test_id.as_deref(), Some(run.test.id.as_str()));
    let passed = finish_test(&db, id, test_id(&run), fixture(Outcome::Passed))
        .await
        .unwrap();
    assert!(matches!(passed.status, TestStatus::Passed));
    let tested = get(&db, id).await.unwrap();
    assert!(tested.active.is_none());
    let active = activate(&db, id, tested.revision, test_id(&run), false)
        .await
        .unwrap();
    let active_version = active.active.unwrap().version;
    assert_eq!(active_version, first.latest.version);

    let staged = set_credential(&db, id, active.revision, new_reference(), async { Ok(()) })
        .await
        .unwrap();
    assert!(matches!(staged.latest.state, VersionState::Draft));
    let run = begin_test(&db, id, staged.revision).await.unwrap();
    let failed = finish_test(&db, id, test_id(&run), fixture(Outcome::Failed))
        .await
        .unwrap();
    assert!(matches!(failed.status, TestStatus::Failed));
    let after_failure = get(&db, id).await.unwrap();
    let previous = after_failure.active.unwrap();
    assert_eq!(previous.version, active_version);
    assert_eq!(previous.credential.unwrap().version, first.latest.version);
    assert_eq!(
        after_failure.latest.credential.unwrap().version,
        staged.latest.version
    );

    let stale = begin_test(&db, id, after_failure.revision).await.unwrap();
    let pending = get(&db, id).await.unwrap();
    let retry = begin_test(&db, id, pending.revision).await.unwrap();
    let stale_result = finish_test(&db, id, test_id(&stale), fixture(Outcome::Passed))
        .await
        .unwrap();
    assert!(matches!(stale_result.status, TestStatus::Superseded));
    let before_retry = get(&db, id).await.unwrap();
    assert_eq!(before_retry.active.unwrap().version, active_version);
    assert_eq!(
        before_retry.last_test_id.as_deref(),
        Some(retry.test.id.as_str())
    );
    let retry_result = finish_test(&db, id, test_id(&retry), fixture(Outcome::Passed))
        .await
        .unwrap();
    assert!(matches!(retry_result.status, TestStatus::Passed));
    let tested = get(&db, id).await.unwrap();
    assert_eq!(tested.active.unwrap().version, active_version);
    let rotated = activate(&db, id, tested.revision, test_id(&retry), false)
        .await
        .unwrap();
    assert_eq!(rotated.active.unwrap().version, staged.latest.version);

    let (left, right) = tokio::join!(
        change(
            &db,
            id,
            rotated.revision,
            Change::Settings(settings.clone())
        ),
        change(&db, id, rotated.revision, Change::Settings(settings)),
    );
    let edited = match (left, right) {
        (Ok(value), Err(Error::Conflict)) | (Err(Error::Conflict), Ok(value)) => value,
        _ => panic!("expected one committed edit and one conflicting edit"),
    };
    let revoke = change(&db, id, edited.revision, Change::RevokeCredential)
        .await
        .unwrap();
    assert!(revoke.active.is_none());
    assert!(revoke.latest.credential.is_none());
    assert!(revoke.rollback_versions.is_empty());
    assert!(matches!(
        rollback(&db, id, revoke.revision, active_version, false).await,
        Err(Error::Conflict)
    ));
    let audit = postgres::checked(pg.client("psql").args([
        "-XAt",
        "-d",
        "web_test_db",
        "-c",
        "SELECT action FROM app.integration_audit ORDER BY sequence",
    ]));
    assert!(audit.lines().any(|action| action == "test_superseded"));
    assert_eq!(
        audit
            .lines()
            .filter(|action| *action == "test_passed")
            .count(),
        2
    );
    assert_eq!(audit.lines().last(), Some("credential_revoked"));
    db.close().await;
    pg.stop();
}

#[tokio::test]
async fn postgres_smtp_slot_reuse_cannot_change_active_or_rollback_transport() {
    struct Mail<'a> {
        primary: Option<Uuid>,
        secondary: Option<Uuid>,
        rejected: Uuid,
        used: &'a Mutex<Vec<Uuid>>,
    }

    impl NotificationChannel for Mail<'_> {
        fn validate(&self) -> Result<(), ChannelConfigError> {
            Ok(())
        }

        async fn test(&self, _id: Uuid) -> DeliveryOutcome {
            panic!("an active configuration must use its persisted SMTP snapshot");
        }

        async fn send(&self, _id: Uuid, _mail: MailKind) -> DeliveryOutcome {
            panic!("an active configuration must use its persisted SMTP snapshot");
        }

        async fn capture_profile(&self, profile: SmtpProfile) -> Option<Uuid> {
            match profile {
                SmtpProfile::Primary => self.primary,
                SmtpProfile::Secondary => self.secondary,
            }
        }

        async fn test_snapshot(&self, snapshot: Uuid, _id: Uuid) -> DeliveryOutcome {
            self.used.lock().unwrap().push(snapshot);
            if snapshot == self.rejected {
                DeliveryOutcome::Rejected
            } else {
                DeliveryOutcome::Accepted
            }
        }
    }

    let pg = postgres::Cluster::create();
    let db = Database::new(&pg.url(), Duration::from_secs(2), Duration::from_secs(2)).unwrap();
    db.migrate().await.unwrap();
    let settings = json!({"profile":"primary"});
    let created = create(&db, "smtp", settings.clone()).await.unwrap();
    let id = created.id.parse::<Uuid>().unwrap();
    let first = Uuid::from_u128(11);
    let second = Uuid::from_u128(12);
    let replacement = Uuid::from_u128(13);
    let used = Mutex::new(Vec::new());
    let policy = NotificationPolicy::default();
    let start = Utc::now() + chrono::TimeDelta::minutes(2);
    let send = |mail, hours| {
        let db = &db;
        let policy = &policy;
        async move {
            assert!(
                dispatch_one(db, policy, mail, || start + chrono::TimeDelta::hours(hours))
                    .await
                    .unwrap()
            );
        }
    };
    let original = Mail {
        primary: Some(first),
        secondary: Some(second),
        rejected: replacement,
        used: &used,
    };
    let mut current = created;
    for (index, profile) in ["primary", "secondary"].into_iter().enumerate() {
        if index > 0 {
            current = change(
                &db,
                id,
                current.revision,
                Change::Settings(json!({"profile":profile})),
            )
            .await
            .unwrap();
        }
        let test = begin_test(&db, id, current.revision).await.unwrap();
        send(&original, index as i64).await;
        let candidate = get(&db, id).await.unwrap();
        current = activate(&db, id, candidate.revision, test_id(&test), false)
            .await
            .unwrap();
        let active = current.active.as_ref().unwrap();
        assert_eq!(
            serde_json::to_value(&active.settings).unwrap()["profile"],
            profile
        );
    }

    // A restarted notifier can serve the active version without either current slot.
    let restart = Mail {
        primary: None,
        secondary: None,
        ..original
    };
    enqueue_test(&db, Uuid::new_v4()).await.unwrap();
    send(&restart, 2).await;
    assert_eq!(used.lock().unwrap().last().copied(), Some(second));

    let reused = Mail {
        primary: Some(replacement),
        ..restart
    };
    let staged = change(&db, id, current.revision, Change::Settings(settings))
        .await
        .unwrap();
    let candidate_test = begin_test(&db, id, staged.revision).await.unwrap();
    send(&reused, 3).await;
    let status = get_test(&db, test_id(&candidate_test)).await.unwrap();
    assert_eq!(status.config_version, 3);
    assert!(matches!(status.status, TestStatus::Failed));
    let current = get(&db, id).await.unwrap();
    assert_eq!(current.active.unwrap().version, 2);
    assert_eq!(current.rollback_versions, [1]);

    let restored = rollback(&db, id, current.revision, 1, false).await.unwrap();
    assert_eq!(restored.active.unwrap().version, 4);
    enqueue_test(&db, Uuid::new_v4()).await.unwrap();
    send(&reused, 4).await;
    assert_eq!(
        *used.lock().unwrap(),
        [first, second, second, replacement, first]
    );
    db.close().await;
    pg.stop();
}

#[tokio::test]
async fn postgres_legacy_smtp_accepted_test_and_historical_version_require_recapture() {
    let pg = postgres::Cluster::create();
    let db = Database::new(&pg.url(), Duration::from_secs(2), Duration::from_secs(2)).unwrap();
    db.migrate().await.unwrap();
    let settings = json!({"profile":"primary"});
    let created = create(&db, "smtp", settings.clone()).await.unwrap();
    let id = created.id.parse::<Uuid>().unwrap();
    let legacy = begin_test(&db, id, created.revision).await.unwrap();

    // Reproduce a successful W4 test that predated SMTP snapshot storage.
    finish_test(&db, id, test_id(&legacy), fixture(Outcome::Passed))
        .await
        .unwrap();
    let candidate = get(&db, id).await.unwrap();
    let mut conn = db.connection().await.unwrap();
    sqlx::query(
        "UPDATE app.notification_outbox SET status = 'accepted' WHERE integration_test_id = $1",
    )
    .bind(test_id(&legacy))
    .execute(&mut *conn)
    .await
    .unwrap();
    assert!(matches!(
        activate(&db, id, candidate.revision, test_id(&legacy), false).await,
        Err(Error::Conflict)
    ));

    sqlx::query(
        "UPDATE app.integration_versions SET state = 'active', activated_at = now()
         WHERE integration_id = $1 AND version = 1",
    )
    .bind(id)
    .execute(&mut *conn)
    .await
    .unwrap();
    let disabled = change(&db, id, candidate.revision, Change::Disable)
        .await
        .unwrap();
    assert!(disabled.rollback_versions.is_empty());
    assert!(matches!(
        rollback(&db, id, disabled.revision, 1, false).await,
        Err(Error::Conflict)
    ));
    drop(conn);
    db.close().await;
    pg.stop();
}

#[tokio::test]
async fn smtp_disable_committed_before_reservation_preserves_the_ordinary_job() {
    struct NoDelivery;
    impl NotificationChannel for NoDelivery {
        fn validate(&self) -> Result<(), ChannelConfigError> {
            Ok(())
        }
        async fn test(&self, _: Uuid) -> DeliveryOutcome {
            panic!("reserved a disabled primary transport");
        }
        async fn send(&self, _: Uuid, _: MailKind) -> DeliveryOutcome {
            panic!("reserved a disabled primary transport");
        }
        async fn test_snapshot(&self, _: Uuid, _: Uuid) -> DeliveryOutcome {
            panic!("reserved a disabled snapshot");
        }
    }

    let pg = postgres::Cluster::create();
    let db = Database::new(&pg.url(), Duration::from_secs(2), Duration::from_secs(2)).unwrap();
    db.migrate().await.unwrap();
    let smtp = create(&db, "smtp", json!({"profile":"primary"}))
        .await
        .unwrap();
    let id = smtp.id.parse::<Uuid>().unwrap();
    let mut observer = db.connection().await.unwrap();
    sqlx::query(
        "UPDATE app.integration_versions SET state = 'active', smtp_snapshot_id = $2
         WHERE integration_id = $1",
    )
    .bind(id)
    .bind(Uuid::new_v4())
    .execute(&mut *observer)
    .await
    .unwrap();
    let key = Uuid::new_v4();
    let before = enqueue_test(&db, key).await.unwrap();

    // Hold the quota query before the worker can select an outbox row. Wait
    // for the actual PostgreSQL lock conflict before committing the admin change.
    let mut blocker = db.write_connection().await.unwrap();
    let mut gate = blocker.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *gate)
        .await
        .unwrap();
    sqlx::query("LOCK TABLE app.notification_attempts IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *gate)
        .await
        .unwrap();
    let waiting = async {
        loop {
            let blocked: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT FROM pg_stat_activity
                 WHERE datname = current_database() AND $1 = ANY(pg_blocking_pids(pid)))",
            )
            .bind(pid)
            .fetch_one(&mut *observer)
            .await
            .unwrap();
            if blocked {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let policy = NotificationPolicy::default();
    let worker = dispatch_one(&db, &policy, &NoDelivery, Utc::now);
    tokio::pin!(worker);
    tokio::select! {
        _ = waiting => {},
        result = &mut worker => panic!("worker stopped before the reservation barrier: {result:?}"),
    }
    let disabled = change(&db, id, smtp.revision, Change::Disable)
        .await
        .unwrap();
    assert!(disabled.active.is_none());
    gate.commit().await.unwrap();
    assert!(!worker.await.unwrap());
    assert!(
        !dispatch_one(&db, &policy, &NoDelivery, Utc::now)
            .await
            .unwrap()
    );
    let after = crate::notifications::store::get_test(&db, key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(after).unwrap()
    );
    assert_eq!(
        crate::notifications::worker::health(&db).await.state,
        crate::notifications::worker::NotificationState::Idle
    );
}
