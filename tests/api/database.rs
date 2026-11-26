//! Starts a disposable PostgreSQL cluster. No existing instance is stopped.
use std::{
    fs,
    net::TcpListener,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;

pub(super) use super::postgres::{Cluster, checked};

pub(super) struct Server(pub(super) Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(super) fn web(pg: &Cluster, static_root: &Path, port: u16) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_web"));
    command
        .env_clear()
        .env("DATABASE_URL", pg.url())
        .env("RUST_LOG", "warn")
        .env("WEB_STATIC_ROOT", static_root)
        .env(
            "WEB_ADMIN_TOKEN_FILE",
            static_root.join("unused-admin-token"),
        )
        .env("WEB_PORT", port.to_string())
        .env("WEB_BIND_HOST", "127.0.0.1")
        .env("WEB_AUTO_MIGRATE", "false")
        .env("WEB_REQUIRE_FINANCE", "false")
        .env("FERTILE_PROFILE", "cloudbox")
        .env(
            "WEB_CONFIG_DIR",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("config"),
        )
        .env("FINANCE_BASE_URL", "http://127.0.0.1:1")
        .env("NO_PROXY", "*");
    command
}

pub(super) async fn expect_http(
    client: &reqwest::Client,
    url: &str,
    code: u16,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(r) = client.get(url).send().await
            && r.status().as_u16() == code
        {
            let body = r.json().await.unwrap();
            eprintln!("GET {url} -> {code} {body}");
            return body;
        }
        assert!(Instant::now() < deadline, "{url} did not reach {code}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(super) fn http_fixture() -> (TempDir, u16, String, reqwest::Client) {
    let public = tempfile::tempdir().unwrap();
    fs::write(public.path().join("index.html"), "<!doctype html>").unwrap();
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    (public, port, base, client)
}

#[tokio::test]
async fn postgres_outage_and_recovery_same_web_process() {
    let pg = Cluster::create();
    let (public, port, base, client) = http_fixture();
    let mut server = Server(
        web(&pg, public.path(), port)
            .arg("serve")
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let pid = server.0.id();
    // PostgreSQL is running, but the required application schema is absent.
    expect_http(&client, &format!("{base}/healthz/live"), 200).await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    expect_http(&client, &format!("{base}/healthz/ready"), 503).await;
    checked(web(&pg, public.path(), port).arg("migrate"));
    checked(web(&pg, public.path(), port).arg("migrate"));
    let schemas = checked(pg.client("psql").args([
        "-XAt",
        "-d",
        "web_test_db",
        "-c",
        "SELECT string_agg(nspname, ',' ORDER BY nspname)
         FROM pg_namespace WHERE nspname IN ('app','blog')",
    ]));
    assert_eq!(schemas.trim(), "app,blog");
    eprintln!("web migrate -> schemas {schemas}");
    expect_http(&client, &format!("{base}/healthz/ready"), 200).await;
    let before = expect_http(&client, &format!("{base}/api/v1/system/status"), 200).await;
    let started = &before["web"]["started_at"];
    assert!(started.is_string());
    pg.stop();
    expect_http(&client, &format!("{base}/healthz/live"), 200).await;
    expect_http(&client, &format!("{base}/healthz/ready"), 503).await;
    let down = expect_http(&client, &format!("{base}/api/v1/system/status"), 200).await;
    assert_eq!(down["database"]["ok"], false);
    pg.start();
    expect_http(&client, &format!("{base}/healthz/ready"), 200).await;
    let up = expect_http(&client, &format!("{base}/api/v1/system/status"), 200).await;
    assert_eq!(&up["web"]["started_at"], started);
    assert_eq!(up["database"]["ok"], true);
    assert!(up["database"]["latency_ms"].is_number());
    assert!(server.0.try_wait().unwrap().is_none());
    eprintln!("same web pid {pid}, postgres outage and recovery verified");
}

#[tokio::test]
async fn postgres_unavailable_on_startup_is_migrated_on_reconnect() {
    let pg = Cluster::create();
    pg.stop();
    let (public, port, base, client) = http_fixture();
    let mut server = Server(
        web(&pg, public.path(), port)
            .env("WEB_AUTO_MIGRATE", "true")
            .arg("serve")
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    expect_http(&client, &format!("{base}/healthz/live"), 200).await;
    expect_http(&client, &format!("{base}/healthz/ready"), 503).await;
    pg.start();
    expect_http(&client, &format!("{base}/healthz/ready"), 200).await;
    let version = checked(pg.client("psql").args([
        "-XAt",
        "-d",
        "web_test_db",
        "-c",
        "SELECT version FROM app.schema_info WHERE component='web'",
    ]));
    assert_eq!(version.trim(), "1");
    assert!(server.0.try_wait().unwrap().is_none());
    eprintln!(
        "same web pid {}, startup outage recovered with automatic migration to schema version 1",
        server.0.id()
    );
}

#[tokio::test]
async fn postgres_integration_migration_upgrades_existing_schema() {
    let pg = Cluster::create();
    let baseline = tempfile::tempdir().unwrap();
    fs::write(
        baseline.path().join("20261122000000_initial_schemas.sql"),
        include_str!("../../migrations/20261122000000_initial_schemas.sql"),
    )
    .unwrap();
    let pool = sqlx::PgPool::connect(&pg.url()).await.unwrap();
    sqlx::migrate::Migrator::new(baseline.path())
        .await
        .unwrap()
        .run(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO app.schema_info (component, version) VALUES ('fixture', 9)")
        .execute(&pool)
        .await
        .unwrap();
    let baseline_versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(baseline_versions, vec![20261122000000]);

    let public = tempfile::tempdir().unwrap();
    checked(web(&pg, public.path(), 8090).arg("migrate"));
    checked(web(&pg, public.path(), 8090).arg("migrate"));

    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        versions,
        vec![
            20261122000000,
            20261126000000,
            20261127000000,
            20261127010000,
            20261127020000,
            20261127030000
        ]
    );
    let components: Vec<(String, i32)> =
        sqlx::query_as("SELECT component, version FROM app.schema_info ORDER BY component")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(components, vec![("fixture".into(), 9), ("web".into(), 1)]);
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables
         WHERE schemaname = 'app' AND starts_with(tablename, 'integration_') ORDER BY tablename",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        tables,
        [
            "integration_audit",
            "integration_configs",
            "integration_credentials",
            "integration_tests",
            "integration_versions"
        ]
    );
    let notification_tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables WHERE schemaname = 'app'
         AND (tablename = 'alert_events' OR starts_with(tablename, 'notification_'))
         ORDER BY tablename",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        notification_tables,
        [
            "alert_events",
            "notification_attempts",
            "notification_deferrals",
            "notification_health",
            "notification_incidents",
            "notification_outbox",
            "notification_rules",
        ]
    );
    let replaces_id_type: String = sqlx::query_scalar(
        "SELECT data_type FROM information_schema.columns
         WHERE table_schema = 'app' AND table_name = 'notification_outbox' AND column_name = 'replaces_id'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(replaces_id_type, "uuid");
    pool.close().await;
    pg.stop();
}
