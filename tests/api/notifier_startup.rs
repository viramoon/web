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
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
use tokio::time::{sleep, timeout};
use uuid::Uuid;
use web::{
    database::Database,
    notifications::{
        channel::{EmailChannels, NotificationChannel, SmtpProfile},
        worker::{self, NotificationPolicy},
    },
};

const SMTP_REQUIRED: &str = "a usable SMTP primary profile is required";
const SNAPSHOT_REQUIRED: &str = "active SMTP snapshot is unavailable";
const DISABLED: &str = "notification delivery is disabled";

fn notifier() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_web-notifier"));
    command.env_clear().env("RUST_LOG", "trace");
    command
}

enum IdleStartup {
    LocalConfiguration,
    DatabasePreflight,
}

async fn assert_idle(
    command: &mut Command,
    reason: &str,
    signal: Option<Signal>,
    markers: &[&str],
) {
    assert_idle_startup(
        command,
        reason,
        signal,
        markers,
        IdleStartup::LocalConfiguration,
    )
    .await;
}

async fn assert_idle_startup(
    command: &mut Command,
    reason: &str,
    signal: Option<Signal>,
    markers: &[&str],
    startup: IdleStartup,
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
    // PostgreSQL preflight has its own five-second budget, beginning after
    // the subprocess has started and finished validating local configuration.
    let launch_budget = Duration::from_secs(5);
    let startup_budget = match startup {
        IdleStartup::LocalConfiguration => launch_budget,
        IdleStartup::DatabasePreflight => launch_budget + Duration::from_secs(5),
    };
    timeout(startup_budget, async {
        loop {
            if events(&read_log()).iter().any(is_idle) {
                break;
            }
            assert!(child.0.try_wait().unwrap().is_none(), "{}", read_log());
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "notifier did not announce idle startup ({reason}) within {startup_budget:?}: {error}\n{}",
            read_log()
        )
    });

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

fn snapshot_fixture(command: &mut Command, dir: &Path) -> PathBuf {
    let snapshots = dir.join("snapshots");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&snapshots)
        .unwrap();
    let public = dir.join("static");
    fs::create_dir(&public).unwrap();
    command
        .env("WEB_STATIC_ROOT", public)
        .env("WEB_SMTP_SNAPSHOT_DIR", &snapshots);
    snapshots
}

fn configure_secondary_smtp(command: &mut Command, port: u16) {
    command
        .env("WEB_SMTP_SECONDARY_HOST", "127.0.0.1")
        .env("WEB_SMTP_SECONDARY_PORT", port.to_string())
        .env("WEB_SMTP_SECONDARY_FROM", "sender@example.invalid")
        .env("WEB_SMTP_SECONDARY_TO", "receiver@example.invalid")
        .env("WEB_SMTP_SECONDARY_TIMEOUT_SECONDS", "1");
}

fn command_setting(command: &Command, key: &str) -> Option<String> {
    command
        .get_envs()
        .find(|(name, _)| *name == key)
        .and_then(|(_, value)| value)
        .map(|value| value.to_str().unwrap().to_owned())
}

async fn activate_snapshot(pool: &PgPool, command: &Command) -> Uuid {
    let channel = EmailChannels::from_environment(|key| command_setting(command, key)).unwrap();
    let snapshot = channel
        .capture_profile(SmtpProfile::Secondary)
        .await
        .unwrap();
    let integration = Uuid::new_v4();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO app.integration_configs (id, connector) VALUES ($1, 'smtp')")
        .bind(integration)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO app.integration_versions (integration_id, version, state, settings, smtp_snapshot_id)
         VALUES ($1, 1, 'active', '{\"profile\":\"secondary\"}', $2)",
    )
    .bind(integration)
    .bind(snapshot)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    snapshot
}

struct ActiveSnapshotFixture {
    command: Command,
    marker: String,
    snapshot_path: PathBuf,
    listener: TcpListener,
    pool: PgPool,
    _dir: tempfile::TempDir,
    pg: Cluster,
}

impl ActiveSnapshotFixture {
    async fn create() -> Self {
        let pg = Cluster::create();
        let pool = outbox_fixture(&pg).await;
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut command = notifier();
        let marker = smtp_fixture(&mut command, &listener, dir.path());
        configure_secondary_smtp(&mut command, listener.local_addr().unwrap().port());
        let snapshots = snapshot_fixture(&mut command, dir.path());
        command
            .env("DATABASE_URL", pg.url())
            .env("WEB_SMTP_TO", "receiver@example.invalid");
        let snapshot = activate_snapshot(&pool, &command).await;
        Self {
            command,
            marker,
            snapshot_path: snapshots.join(format!("{snapshot}.smtp")),
            listener,
            pool,
            _dir: dir,
            pg,
        }
    }

    fn database(&self) -> Database {
        Database::new(
            &self.pg.url(),
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .unwrap()
    }
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
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut command = notifier();
    let marker = smtp_fixture(&mut command, &listener, dir.path());
    let snapshots = snapshot_fixture(&mut command, dir.path());
    command.env("DATABASE_URL", pg.url());
    assert_idle(&mut command, SMTP_REQUIRED, Some(Signal::TERM), &[&marker]).await;
    assert_eq!(fs::read_dir(&snapshots).unwrap().count(), 0);
    assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
    assert_eq!(delivery_state(&pool).await, before);
}

#[tokio::test]
async fn notifier_with_only_secondary_smtp_keeps_unbound_outbox_intact() {
    let pg = Cluster::create();
    // No active W4 version: the due standalone test would use the incomplete primary slot.
    let pool = outbox_fixture(&pg).await;
    let before = delivery_state(&pool).await;
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut command = notifier();
    let marker = smtp_fixture(&mut command, &listener, dir.path());
    configure_secondary_smtp(&mut command, listener.local_addr().unwrap().port());
    let snapshots = snapshot_fixture(&mut command, dir.path());
    command.env("DATABASE_URL", pg.url());
    assert_idle(&mut command, SMTP_REQUIRED, Some(Signal::TERM), &[&marker]).await;
    assert_eq!(fs::read_dir(&snapshots).unwrap().count(), 0);
    assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
    assert_eq!(delivery_state(&pool).await, before);
}

#[tokio::test]
async fn notifier_with_only_secondary_smtp_idles_before_opening_snapshot_storage() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let marker = Uuid::new_v4().to_string();
    let mut command = notifier();
    command
        .env("WEB_SMTP_HOST", "127.0.0.1")
        .env("WEB_SMTP_FROM", format!("{marker}@example.invalid"))
        .env("DATABASE_URL", &marker);
    configure_secondary_smtp(&mut command, 1);
    let snapshots = snapshot_fixture(&mut command, dir.path());
    assert_idle(&mut command, SMTP_REQUIRED, None, &[&marker]).await;
    assert_eq!(fs::read_dir(snapshots).unwrap().count(), 0);
}

#[tokio::test]
async fn notifier_with_active_w4_and_unavailable_snapshot_keeps_outbox_intact() {
    let mut fixture = ActiveSnapshotFixture::create().await;
    let before = delivery_state(&fixture.pool).await;
    fixture.command.env_remove("WEB_SMTP_SNAPSHOT_DIR");
    let channel =
        EmailChannels::for_notifier(|key| command_setting(&fixture.command, key)).unwrap();
    assert_eq!(
        worker::smtp_idle_reason(&fixture.database(), &channel)
            .await
            .unwrap(),
        Some(SNAPSHOT_REQUIRED)
    );
    assert_idle_startup(
        &mut fixture.command,
        SNAPSHOT_REQUIRED,
        Some(Signal::TERM),
        &[&fixture.marker],
        IdleStartup::DatabasePreflight,
    )
    .await;
    assert_eq!(
        fixture.listener.accept().unwrap_err().kind(),
        ErrorKind::WouldBlock
    );
    assert_eq!(delivery_state(&fixture.pool).await, before);
}

#[tokio::test]
async fn notifier_with_deleted_active_snapshot_idles_and_keeps_outbox_intact() {
    let mut fixture = ActiveSnapshotFixture::create().await;
    let before = delivery_state(&fixture.pool).await;
    let channel =
        EmailChannels::for_notifier(|key| command_setting(&fixture.command, key)).unwrap();
    let db = fixture.database();
    assert_eq!(worker::smtp_idle_reason(&db, &channel).await.unwrap(), None);
    // Check both a running worker's next reservation and a newly started process.
    fs::remove_file(&fixture.snapshot_path).unwrap();
    assert_eq!(
        worker::smtp_idle_reason(&db, &channel).await.unwrap(),
        Some(SNAPSHOT_REQUIRED)
    );
    assert!(
        !worker::dispatch_one(
            &db,
            &NotificationPolicy::default(),
            &channel,
            chrono::Utc::now
        )
        .await
        .unwrap()
    );
    assert_idle_startup(
        &mut fixture.command,
        SNAPSHOT_REQUIRED,
        Some(Signal::TERM),
        &[&fixture.marker],
        IdleStartup::DatabasePreflight,
    )
    .await;
    assert_eq!(
        fixture.listener.accept().unwrap_err().kind(),
        ErrorKind::WouldBlock
    );
    assert_eq!(delivery_state(&fixture.pool).await, before);
}

#[tokio::test]
async fn notifier_restarts_without_primary_and_uses_bound_w4_snapshot() {
    let pg = Cluster::create();
    let pool = outbox_fixture(&pg).await;
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let bound_relay = TcpListener::bind("127.0.0.1:0").unwrap();
    let replacement_relay = TcpListener::bind("127.0.0.1:0").unwrap();
    bound_relay.set_nonblocking(true).unwrap();
    replacement_relay.set_nonblocking(true).unwrap();
    let mut command = notifier();
    let snapshots = snapshot_fixture(&mut command, dir.path());
    configure_secondary_smtp(&mut command, bound_relay.local_addr().unwrap().port());
    let snapshot = activate_snapshot(&pool, &command).await;
    let snapshot_path = snapshots.join(format!("{snapshot}.smtp"));
    let snapshot_before = fs::read(&snapshot_path).unwrap();

    configure_secondary_smtp(&mut command, replacement_relay.local_addr().unwrap().port());
    let stderr = tempfile::NamedTempFile::new_in("/tmp").unwrap();
    let mut child = Server(
        command
            .env("DATABASE_URL", pg.url())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr.reopen().unwrap())
            .spawn()
            .unwrap(),
    );
    let connection = timeout(Duration::from_secs(8), async {
        loop {
            match bound_relay.accept() {
                Ok((connection, _)) => break connection,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    assert!(child.0.try_wait().unwrap().is_none());
                    sleep(Duration::from_millis(20)).await;
                }
                Err(error) => panic!("loopback listener failed: {error}"),
            }
        }
    })
    .await
    .expect("notifier did not connect to the bound relay");
    // The loopback fixture observes the attempted SMTP connection only.
    drop(connection);
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
    .expect("notifier did not stop after the snapshot attempt");
    assert_eq!(status.code(), Some(0));
    let outcomes: Vec<String> = sqlx::query_scalar(
        "SELECT outcome FROM app.notification_attempts
         WHERE outbox_id = '00000000-0000-0000-0000-000000000001' ORDER BY attempt",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(outcomes[0].as_str(), "transient" | "timeout"));
    assert_eq!(
        replacement_relay.accept().unwrap_err().kind(),
        ErrorKind::WouldBlock
    );
    assert_eq!(fs::read(snapshot_path).unwrap(), snapshot_before);
    assert!(
        !fs::read_to_string(stderr.path())
            .unwrap()
            .contains("notifier idle")
    );
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
