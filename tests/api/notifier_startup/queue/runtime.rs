use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::instrument::WithSubscriber;
use web::notifications::channel::{
    ChannelConfigError, DeliveryOutcome, MailKind, PreparedSnapshot,
};

struct SnapshotLoss<'a> {
    channel: &'a EmailChannels,
    pool: &'a PgPool,
    directory: &'a Path,
    saved: &'a Path,
    lose_once: AtomicBool,
}

impl NotificationChannel for SnapshotLoss<'_> {
    fn validate(&self) -> Result<(), ChannelConfigError> {
        self.channel.validate()
    }

    async fn test(&self, _: Uuid) -> DeliveryOutcome {
        panic!("candidate must use the bound relay")
    }

    async fn send(&self, _: Uuid, _: MailKind) -> DeliveryOutcome {
        panic!("fixture only contains a candidate test")
    }

    async fn prepare_snapshot(&self, snapshot: Uuid) -> Option<PreparedSnapshot> {
        self.channel.prepare_snapshot(snapshot).await
    }

    async fn capture_profile(&self, profile: SmtpProfile) -> Option<Uuid> {
        self.channel.capture_profile(profile).await
    }

    async fn test_snapshot(&self, snapshot: Uuid, id: Uuid) -> DeliveryOutcome {
        assert_eq!(id, CANDIDATE);
        if self.lose_once.swap(false, Ordering::Relaxed) {
            let bound: Uuid = sqlx::query_scalar(
                "SELECT smtp_snapshot_id FROM app.notification_outbox
                 WHERE id = $1 AND status = 'sending'",
            )
            .bind(id)
            .fetch_one(self.pool)
            .await
            .unwrap();
            assert_eq!(snapshot, bound);
            fs::rename(self.directory.join(format!("{bound}.smtp")), self.saved).unwrap();
        }
        self.channel.test_snapshot(snapshot, id).await
    }
}

async fn check_runtime_loss(previous_attempts: i32, delay_seconds: i64) {
    let pg = Cluster::create();
    let pool = outbox_fixture(&pg).await;
    let root = tempfile::tempdir_in("/tmp").unwrap();
    let primary = relay();
    let secondary = relay();
    let mut command = notifier();
    let marker = smtp_fixture(&mut command, &primary, root.path());
    command.env("WEB_SMTP_TO", "receiver@example.invalid");
    configure_secondary_smtp(&mut command, secondary.local_addr().unwrap().port());
    command.env(
        "WEB_SMTP_SECONDARY_FROM",
        format!("{marker}@example.invalid"),
    );
    command.env(
        "WEB_SMTP_SECONDARY_CREDENTIAL_FILE",
        root.path().join("smtp-fixture.json"),
    );
    let directory = snapshot_fixture(&mut command, root.path());
    let email = EmailChannels::for_notifier(|key| command_setting(&command, key)).unwrap();
    let binding = if previous_attempts == 0 {
        None
    } else {
        Some(email.capture_profile(SmtpProfile::Secondary).await.unwrap())
    };
    bind_candidate(&pool, binding).await;
    sqlx::raw_sql(
        "DELETE FROM app.notification_attempts;
         DELETE FROM app.notification_outbox WHERE integration_test_id IS NULL;",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE app.notification_outbox SET status = 'pending', attempts = $2,
             send_started_at = NULL, last_outcome = CASE WHEN $2 > 0 THEN 'transient' END
         WHERE id = $1",
    )
    .bind(CANDIDATE)
    .bind(previous_attempts)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO app.notification_attempts (outbox_id, attempt, started_at, finished_at, outcome)
         SELECT $1, attempt, started_at, started_at, 'transient'
         FROM generate_series(1, $2) attempt
         CROSS JOIN (SELECT now() - interval '2 hours' AS started_at) t",
    )
    .bind(CANDIDATE)
    .bind(previous_attempts)
    .execute(&pool)
    .await
    .unwrap();
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let db = Database::new(&pg.url(), Duration::from_secs(2), Duration::from_secs(2)).unwrap();
    let policy = NotificationPolicy::default();
    let saved = root.path().join("saved-runtime-candidate");
    let channel = SnapshotLoss {
        channel: &email,
        pool: &pool,
        directory: &directory,
        saved: &saved,
        lose_once: AtomicBool::new(true),
    };
    let before = candidate_state(&pool).await;
    let health = serde_json::to_value(worker::health(&db).await).unwrap();
    let stderr = tempfile::NamedTempFile::new_in("/tmp").unwrap();
    let writer = stderr.reopen().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .with_writer(move || writer.try_clone().unwrap())
        .finish();
    assert!(
        worker::dispatch_one(&db, &policy, &channel, || now)
            .with_subscriber(subscriber)
            .await
            .unwrap()
    );
    assert_eq!(
        secondary.accept().unwrap_err().kind(),
        ErrorKind::WouldBlock
    );
    let deferred = candidate_state(&pool).await;
    let snapshot: Uuid = deferred["job"]["smtp_snapshot_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        before["job"]["smtp_snapshot_id"]
            .as_str()
            .map(|id| id.parse().unwrap()),
        binding
    );
    if let Some(binding) = binding {
        assert_eq!(snapshot, binding);
    }
    assert_eq!(deferred["job"]["status"], "pending");
    assert_eq!(deferred["job"]["attempts"], previous_attempts + 1);
    assert_eq!(deferred["job"]["last_outcome"], "configuration_error");
    let retry_at = now + TimeDelta::seconds(delay_seconds);
    assert_eq!(
        deferred["job"]["available_at"]
            .as_str()
            .unwrap()
            .parse::<DateTime<Utc>>(),
        Ok(retry_at)
    );
    assert_eq!(deferred["test"], before["test"]);
    assert_eq!(deferred["deferrals"], before["deferrals"]);
    let last = &deferred["attempts"][previous_attempts as usize];
    assert_eq!(last["outcome"], "configuration_error");
    assert_eq!(
        last["finished_at"]
            .as_str()
            .unwrap()
            .parse::<DateTime<Utc>>(),
        Ok(now)
    );
    assert_eq!(
        serde_json::to_value(worker::health(&db).await).unwrap(),
        health
    );

    let log = fs::read_to_string(stderr.path()).unwrap();
    let event: Value = serde_json::from_str(&log).unwrap();
    assert_eq!(event["fields"]["message"], "SMTP candidate test deferred");
    assert_eq!(event["fields"]["stage"], "smtp_configuration");
    assert_eq!(
        event["fields"]["reason"],
        "bound candidate SMTP snapshot is unavailable"
    );
    for record in [&log, &deferred.to_string()] {
        assert!(!record.contains(&marker));
        assert!(!record.contains(directory.to_str().unwrap()));
    }
    assert!(!log.contains(&snapshot.to_string()));

    let file = directory.join(format!("{snapshot}.smtp"));
    fs::rename(&saved, &file).unwrap();
    assert!(
        !worker::dispatch_one(&db, &policy, &channel, || retry_at - TimeDelta::seconds(1))
            .await
            .unwrap()
    );
    assert_eq!(candidate_state(&pool).await, deferred);
    assert_eq!(
        secondary.accept().unwrap_err().kind(),
        ErrorKind::WouldBlock
    );
    let (sent, ()) = tokio::join!(
        worker::dispatch_one(&db, &policy, &channel, || retry_at),
        observe_connection(&secondary)
    );
    assert!(sent.unwrap());
    let after = candidate_state(&pool).await;
    assert_eq!(after["job"]["smtp_snapshot_id"], snapshot.to_string());
    assert_eq!(after["job"]["attempts"], previous_attempts + 2);
    assert_eq!(after["attempts"][previous_attempts as usize], *last);
    assert!(matches!(
        after["job"]["last_outcome"].as_str(),
        Some("timeout" | "transient")
    ));
    assert_eq!(primary.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
}

#[tokio::test]
async fn runtime_loss_of_already_bound_candidate_retries_even_at_attempt_limit() {
    check_runtime_loss(4, 240).await;
}

#[tokio::test]
async fn runtime_loss_after_candidate_is_first_bound_retries_original_snapshot() {
    check_runtime_loss(0, 30).await;
}
