//! Starts a disposable PostgreSQL cluster. No existing instance is stopped.
use std::{
    fs,
    net::TcpListener,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;

fn checked(command: &mut Command) -> String {
    let output = command.output().expect("test executable must be installed");
    assert!(
        output.status.success(),
        "{command:?}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

struct Cluster {
    dir: TempDir,
}

impl Cluster {
    fn create() -> Self {
        let dir = tempfile::tempdir().unwrap();
        checked(
            Command::new("initdb")
                .args([
                    "--username=web_test",
                    "--auth-local=trust",
                    "--auth-host=reject",
                    "--encoding=UTF8",
                    "--no-locale",
                    "-D",
                ])
                .arg(dir.path().join("data")),
        );
        let pg = Self { dir };
        pg.start();
        checked(pg.client("createdb").arg("web_test_db"));
        pg
    }
    fn ctl(&self) -> Command {
        let mut c = Command::new("pg_ctl");
        c.arg("-D")
            .arg(self.dir.path().join("data"))
            .args(["-w", "-t", "15"]);
        c
    }
    fn start(&self) {
        checked(
            self.ctl()
                .arg("-l")
                .arg(self.dir.path().join("postgres.log"))
                .args(["start", "-o"])
                .arg(format!("-h '' -k '{}' -p 55453", self.dir.path().display())),
        );
    }
    fn stop(&self) {
        checked(self.ctl().args(["-m", "fast", "stop"]));
    }
    fn url(&self) -> String {
        format!(
            "postgresql://web_test@localhost/web_test_db?host={}&port=55453",
            self.dir.path().display()
        )
    }
    fn client(&self, bin: &str) -> Command {
        let mut command = Command::new(bin);
        command
            .env("PGHOST", self.dir.path())
            .env("PGPORT", "55453")
            .env("PGUSER", "web_test");
        command
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        let _ = self.ctl().args(["-m", "immediate", "stop"]).output();
    }
}
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn web(pg: &Cluster, static_root: &Path, port: u16) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_web"));
    command
        .env("DATABASE_URL", pg.url())
        .env("RUST_LOG", "warn")
        .env("WEB_STATIC_ROOT", static_root)
        .env("WEB_PORT", port.to_string())
        .env("WEB_BIND_HOST", "127.0.0.1")
        .env("WEB_AUTO_MIGRATE", "false")
        .env("WEB_REQUIRE_FINANCE", "false")
        .env("FERTILE_PROFILE", "cloudbox")
        .env(
            "WEB_CONFIG_DIR",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("config"),
        )
        .env("FINANCE_BASE_URL", "http://127.0.0.1:1");
    command
}

async fn expect_http(client: &reqwest::Client, url: &str, code: u16) -> serde_json::Value {
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

fn http_fixture() -> (TempDir, u16, String, reqwest::Client) {
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
