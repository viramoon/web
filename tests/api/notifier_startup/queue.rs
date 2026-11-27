use super::*;
use chrono::{DateTime, TimeDelta, Utc};
use std::io::Read;

mod runtime;

// outbox_fixture creates independent test jobs 1 (due) and 2 (sending).
const CANDIDATE: Uuid = Uuid::from_u128(2);

async fn bind_candidate(pool: &PgPool, snapshot: impl Into<Option<Uuid>>) {
    let integration = Uuid::new_v4();
    let test = Uuid::new_v4();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO app.integration_configs (id, connector) VALUES ($1, 'smtp')")
        .bind(integration)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO app.integration_versions (integration_id, version, state, settings, created_at)
         SELECT $1, 1, 'candidate', '{\"profile\":\"secondary\"}', created_at FROM app.notification_outbox WHERE id = $2",
    )
    .bind(integration)
    .bind(CANDIDATE)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO app.integration_tests (id, integration_id, config_version, status, started_at)
         SELECT $1, $2, 1, 'pending', created_at FROM app.notification_outbox WHERE id = $3",
    )
    .bind(test)
    .bind(integration)
    .bind(CANDIDATE)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE app.notification_outbox SET integration_test_id = $2, smtp_snapshot_id = $3
         WHERE id = $1",
    )
    .bind(CANDIDATE)
    .bind(test)
    .bind(snapshot.into())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

async fn candidate_state(pool: &PgPool) -> Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object('job', to_jsonb(job), 'test', to_jsonb(test),
            'attempts', (SELECT jsonb_agg(a ORDER BY attempt) FROM app.notification_attempts a WHERE a.outbox_id = job.id),
            'deferrals', (SELECT jsonb_agg(d ORDER BY sequence) FROM app.notification_deferrals d WHERE d.outbox_id = job.id))
         FROM app.notification_outbox job JOIN app.integration_tests test ON test.id = job.integration_test_id
         WHERE job.id = $1",
    ).bind(CANDIDATE).fetch_one(pool).await.unwrap()
}

fn relay() -> TcpListener {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}

// Accept and close a real loopback connection without transmitting SMTP data.
async fn observe_connection(relay: &TcpListener) {
    let listener = tokio::net::TcpListener::from_std(relay.try_clone().unwrap()).unwrap();
    timeout(Duration::from_secs(10), listener.accept())
        .await
        .expect("expected a connection at the configured loopback relay")
        .unwrap();
}

fn start(command: &mut Command, stderr: &tempfile::NamedTempFile) -> Server {
    Server(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(stderr.reopen().unwrap())
            .spawn()
            .unwrap(),
    )
}

async fn stop(child: &mut Server) {
    kill_process(Pid::from_child(&child.0), Signal::TERM).unwrap();
    let status = timeout(Duration::from_secs(5), async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("notifier did not drain the loopback attempt");
    assert_eq!(status.code(), Some(0));
    let mut stdout = Vec::new();
    child
        .0
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    assert!(stdout.is_empty());
}

#[tokio::test]
async fn missing_candidate_at_startup_preserves_retry_while_w3_progresses() {
    let pg = Cluster::create();
    let pool = outbox_fixture(&pg).await;
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let primary = relay();
    let secondary = relay();
    let mut command = notifier();
    let marker = smtp_fixture(&mut command, &primary, dir.path());
    command
        .env("WEB_SMTP_TO", "receiver@example.invalid")
        .env("DATABASE_URL", pg.url())
        .env("WEB_NOTIFIER_NORMAL_HOURLY_LIMIT", "2");
    configure_secondary_smtp(&mut command, secondary.local_addr().unwrap().port());
    let snapshots = snapshot_fixture(&mut command, dir.path());
    let channel = EmailChannels::for_notifier(|key| command_setting(&command, key)).unwrap();
    let snapshot = channel
        .capture_profile(SmtpProfile::Secondary)
        .await
        .unwrap();
    bind_candidate(&pool, snapshot).await;
    // Job 2 is the earliest due retry. Job 1 can send. Job 3 will witness a
    // second policy pass after job 1 consumes the second hourly reservation.
    let witness = Uuid::from_u128(3);
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE app.notification_attempts SET outcome = 'transient', finished_at = started_at
         WHERE outbox_id = $1",
    )
    .bind(CANDIDATE)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE app.notification_outbox SET status = 'pending', last_outcome = 'transient',
             available_at = send_started_at + interval '30 seconds' WHERE id = $1",
    )
    .bind(CANDIDATE)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("UPDATE app.notification_outbox SET available_at = now() - interval '2 minutes' WHERE id = $1")
        .bind(Uuid::from_u128(1))
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO app.notification_outbox (id, kind, priority, test_key, available_at)
        VALUES ($1, 'test', 'normal', $1, now() - interval '1 minute')",
    )
    .bind(witness)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let before = candidate_state(&pool).await;
    let file = snapshots.join(format!("{snapshot}.smtp"));
    let saved = dir.path().join("saved-candidate");
    fs::rename(&file, &saved).unwrap();

    let stderr = tempfile::NamedTempFile::new_in("/tmp").unwrap();
    let mut child = start(&mut command, &stderr);
    observe_connection(&primary).await;
    timeout(Duration::from_secs(10), async {
        loop {
            let deferred: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT FROM app.notification_deferrals
                 WHERE outbox_id = $1 AND reason = 'rate_limit')",
            )
            .bind(witness)
            .fetch_one(&pool)
            .await
            .unwrap();
            if deferred {
                break;
            }
            assert!(child.0.try_wait().unwrap().is_none());
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("expected the independent W3 quota deferral");
    stop(&mut child).await;
    assert_eq!(candidate_state(&pool).await, before);
    assert_eq!(
        secondary.accept().unwrap_err().kind(),
        ErrorKind::WouldBlock
    );
    let log = fs::read_to_string(stderr.path()).unwrap();
    let events: Vec<Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let paused: Vec<_> = events
        .iter()
        .filter(|e| e["fields"]["message"] == "SMTP candidate test paused")
        .collect();
    assert_eq!(paused.len(), 1);
    assert_eq!(paused[0]["fields"]["stage"], "smtp_configuration");
    assert_eq!(
        paused[0]["fields"]["reason"],
        "bound candidate SMTP snapshot is unavailable"
    );
    assert!(!log.contains(&marker));
    assert!(!log.contains(&snapshot.to_string()));

    fs::rename(saved, file).unwrap();
    command.env_remove("WEB_NOTIFIER_NORMAL_HOURLY_LIMIT");
    let restored_log = tempfile::NamedTempFile::new_in("/tmp").unwrap();
    let mut restarted = start(&mut command, &restored_log);
    observe_connection(&secondary).await;
    stop(&mut restarted).await;
    let after = candidate_state(&pool).await;
    assert_eq!(after["job"]["smtp_snapshot_id"], snapshot.to_string());
    assert_eq!(after["job"]["attempts"], 2);
    assert_eq!(after["attempts"][0], before["attempts"][0]);
    assert!(matches!(
        after["attempts"][1]["outcome"].as_str(),
        Some("transient" | "timeout")
    ));
    assert_eq!(after["test"]["status"], "pending");
}

#[tokio::test]
async fn abandoned_candidate_without_snapshot_directory_releases_the_w3_sender() {
    let pg = Cluster::create();
    let pool = outbox_fixture(&pg).await;
    bind_candidate(&pool, Uuid::new_v4()).await;
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let primary = relay();
    let mut command = notifier();
    smtp_fixture(&mut command, &primary, dir.path());
    command.env("WEB_SMTP_TO", "receiver@example.invalid");
    let channel = EmailChannels::for_notifier(|key| command_setting(&command, key)).unwrap();
    let db = Database::new(&pg.url(), Duration::from_secs(2), Duration::from_secs(2)).unwrap();
    let policy = NotificationPolicy::default();
    let started: DateTime<Utc> =
        sqlx::query_scalar("SELECT send_started_at FROM app.notification_outbox WHERE id = $1")
            .bind(CANDIDATE)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(worker::smtp_idle_reason(&db, &channel).await.unwrap(), None);
    let before = delivery_state(&pool).await;
    let grace_end = started + TimeDelta::seconds(70);
    assert!(
        !worker::dispatch_one(&db, &policy, &channel, || grace_end - TimeDelta::seconds(1))
            .await
            .unwrap()
    );
    assert_eq!(delivery_state(&pool).await, before);
    assert_eq!(primary.accept().unwrap_err().kind(), ErrorKind::WouldBlock);

    let (sent, ()) = tokio::join!(
        worker::dispatch_one(&db, &policy, &channel, || grace_end),
        observe_connection(&primary)
    );
    assert!(sent.unwrap());
    let candidate = candidate_state(&pool).await;
    assert_eq!(candidate["job"]["status"], "pending");
    assert_eq!(candidate["job"]["attempts"], 1);
    assert_eq!(candidate["attempts"][0]["outcome"], "interrupted");
    assert_eq!(candidate["test"]["status"], "pending");
    let w3: String = sqlx::query_scalar(
        "SELECT outcome FROM app.notification_attempts WHERE outbox_id = $1 AND attempt = 1",
    )
    .bind(Uuid::from_u128(1))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(matches!(w3.as_str(), "transient" | "timeout"));

    let limited = NotificationPolicy::from_environment(|key| {
        (key == "WEB_NOTIFIER_NORMAL_HOURLY_LIMIT").then(|| "1".into())
    })
    .unwrap();
    assert!(
        !worker::dispatch_one(&db, &limited, &channel, || grace_end
            + TimeDelta::seconds(30))
        .await
        .unwrap()
    );
    assert_eq!(candidate_state(&pool).await, candidate);
    assert_eq!(primary.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
}
