//! Runs the real notifier with synthetic settings and an isolated outbox.
use super::{database::Server, postgres::Cluster};
use rustix::process::{Pid, Signal, kill_process};
use serde_json::Value;
use sqlx::PgPool;
use std::{
    fs,
    io::ErrorKind,
    net::TcpListener,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};
use tokio::time::{sleep, timeout};
use uuid::Uuid;
use web::database::Database;

const SMTP_REQUIRED: &str =
    "a usable SMTP primary profile or a secondary profile with snapshot storage is required";
const DISABLED: &str = "notification delivery is disabled";

fn notifier() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_web-notifier"));
    command.env_clear().env("RUST_LOG", "trace");
    command
}

async fn assert_idle(
    command: &mut Command,
    reason: &str,
    signal: Option<Signal>,
    markers: &[&str],
) {
    let stdout = tempfile::tempfile_in("/tmp").unwrap();
    let stderr = tempfile::NamedTempFile::new_in("/tmp").unwrap();
    let mut child = Server(
        command
            .stdin(Stdio::null())
            .stdout(stdout.try_clone().unwrap())
            .stderr(stderr.reopen().unwrap())
            .spawn()
            .unwrap(),
    );
    let read_log = || fs::read_to_string(stderr.path()).unwrap();
    let events = |log: &str| {
        log.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect::<Vec<_>>()
    };
    let is_idle = |event: &Value| event["fields"]["message"] == "notifier idle";
    timeout(Duration::from_secs(5), async {
        loop {
            if events(&read_log()).iter().any(is_idle) {
                break;
            }
            assert!(child.0.try_wait().unwrap().is_none(), "{}", read_log());
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("notifier never announced idle startup");

    // Stay alive beyond one worker poll and the supervisor startup window.
    sleep(Duration::from_millis(2200)).await;
    assert!(child.0.try_wait().unwrap().is_none());
    if let Some(signal) = signal {
        kill_process(Pid::from_child(&child.0), signal).unwrap();
        let status = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    break status;
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("idle notifier did not stop on signal");
        assert_eq!(status.code(), Some(0));
    }
    assert_eq!(stdout.metadata().unwrap().len(), 0);
    let log = read_log();
    let entries: Vec<Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(entries.len() <= 2);
    assert_eq!(entries.iter().filter(|event| is_idle(event)).count(), 1);
    for entry in entries {
        if is_idle(&entry) {
            assert_eq!(entry["fields"]["stage"], "smtp_configuration");
            assert_eq!(entry["fields"]["reason"], reason);
        } else {
            assert_eq!(entry["fields"]["message"], "SMTP profile is unavailable");
            assert_eq!(entry["fields"]["profile"], "primary");
        }
    }
    assert!(markers.iter().all(|marker| !log.contains(marker)));
}

async fn outbox_fixture(pg: &Cluster) -> PgPool {
    let url = pg.url();
    let db = Database::new(&url, Duration::from_secs(2), Duration::from_secs(2)).unwrap();
    db.migrate().await.unwrap();
    let pool = PgPool::connect(&url).await.unwrap();
    // Two fixed test UUIDs represent a due message and an abandoned send.
    sqlx::raw_sql(
        "INSERT INTO app.notification_outbox
             (id, kind, priority, test_key, status, attempts, created_at, available_at, send_started_at)
         VALUES
             ('00000000-0000-0000-0000-000000000001', 'test', 'normal',
              '00000000-0000-0000-0000-000000000001', 'pending', 0,
              now() - interval '5 minutes', now() - interval '5 minutes', NULL),
             ('00000000-0000-0000-0000-000000000002', 'test', 'normal',
              '00000000-0000-0000-0000-000000000002', 'sending', 1,
              now() - interval '5 minutes', now() - interval '5 minutes', now() - interval '5 minutes');
         INSERT INTO app.notification_attempts (outbox_id, attempt, started_at, outcome)
         SELECT id, attempts, send_started_at, 'sending'
         FROM app.notification_outbox WHERE status = 'sending';",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool
}

async fn delivery_state(pool: &PgPool) -> Value {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
            'outbox', (SELECT jsonb_agg(job ORDER BY id) FROM app.notification_outbox job),
            'attempts', (SELECT jsonb_agg(a ORDER BY outbox_id, attempt) FROM app.notification_attempts a),
            'deferrals', (SELECT jsonb_agg(d ORDER BY sequence) FROM app.notification_deferrals d),
            'health', (SELECT to_jsonb(h) FROM app.notification_health h)
         )",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

fn smtp_fixture(command: &mut Command, listener: &TcpListener, dir: &Path) -> String {
    let marker = Uuid::new_v4().to_string();
    let file = dir.join("smtp-fixture.json");
    let writer = fs::OpenOptions::new()
        .mode(0o600)
        .create_new(true)
        .write(true)
        .open(&file)
        .unwrap();
    serde_json::to_writer(
        writer,
        &serde_json::json!({"username": marker, "password": marker}),
    )
    .unwrap();
    command
        .env("WEB_SMTP_HOST", "127.0.0.1")
        .env(
            "WEB_SMTP_PORT",
            listener.local_addr().unwrap().port().to_string(),
        )
        .env("WEB_SMTP_FROM", format!("{marker}@example.invalid"))
        .env("WEB_SMTP_CREDENTIAL_FILE", file)
        .env("WEB_SMTP_TIMEOUT_SECONDS", "1");
    marker
}

#[tokio::test]
async fn notifier_without_smtp_keeps_the_outbox_intact() {
    let pg = Cluster::create();
    let pool = outbox_fixture(&pg).await;
    let before = delivery_state(&pool).await;
    let mut command = notifier();
    command.env("DATABASE_URL", pg.url());
    assert_idle(&mut command, SMTP_REQUIRED, Some(Signal::TERM), &[]).await;
    assert_eq!(delivery_state(&pool).await, before);
}

#[tokio::test]
async fn notifier_with_incomplete_smtp_and_snapshot_storage_keeps_the_outbox_intact() {
    let pg = Cluster::create();
    let pool = outbox_fixture(&pg).await;
    let before = delivery_state(&pool).await;
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let snapshots = dir.path().join("snapshots");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&snapshots)
        .unwrap();
    let public = dir.path().join("static");
    fs::create_dir(&public).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut command = notifier();
    let marker = smtp_fixture(&mut command, &listener, dir.path());
    command
        .env("DATABASE_URL", pg.url())
        .env("WEB_STATIC_ROOT", &public)
        .env("WEB_SMTP_SNAPSHOT_DIR", &snapshots);
    assert_idle(&mut command, SMTP_REQUIRED, Some(Signal::TERM), &[&marker]).await;
    assert_eq!(fs::read_dir(&snapshots).unwrap().count(), 0);
    assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
    assert_eq!(delivery_state(&pool).await, before);
}

#[tokio::test]
async fn notifier_disabled_by_fertile_keeps_the_outbox_intact_with_complete_smtp() {
    let pg = Cluster::create();
    let pool = outbox_fixture(&pg).await;
    let before = delivery_state(&pool).await;
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut command = notifier();
    let marker = smtp_fixture(&mut command, &listener, dir.path());
    command
        .env("DATABASE_URL", pg.url())
        .env("FERTILE_ENABLE_WEB_NOTIFIER", "0")
        .env("WEB_SMTP_TO", "receiver@example.invalid");
    assert_idle(&mut command, DISABLED, Some(Signal::INT), &[&marker]).await;
    assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
    assert_eq!(delivery_state(&pool).await, before);
}

#[tokio::test]
async fn notifier_idles_before_reading_database_or_worker_policy() {
    let mut absent = notifier();
    absent.env("WEB_NOTIFIER_QUIET_START", "invalid-synthetic-time");
    assert_idle(&mut absent, SMTP_REQUIRED, None, &[]).await;

    let marker = Uuid::new_v4().to_string();
    let mut incomplete = notifier();
    incomplete
        .env("WEB_SMTP_HOST", "127.0.0.1")
        .env("WEB_SMTP_FROM", format!("{marker}@example.invalid"))
        .env("WEB_SMTP_CREDENTIAL_FILE", &marker)
        .env("DATABASE_URL", &marker);
    assert_idle(&mut incomplete, SMTP_REQUIRED, None, &[&marker]).await;

    let mut disabled = notifier();
    disabled
        .env("FERTILE_ENABLE_WEB_NOTIFIER", "0")
        .env("WEB_SMTP_HOST", &marker)
        .env("WEB_SMTP_CREDENTIAL_FILE", &marker)
        .env("DATABASE_URL", &marker);
    assert_idle(&mut disabled, DISABLED, None, &[&marker]).await;
}
