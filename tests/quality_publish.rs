use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use chrono::Utc;
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions, Permissions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::Arc,
    time::Duration,
};
use tower::ServiceExt;
use web::{
    AppState, auth::WriteAccess, config::Settings, database::Database, finance::Finance, router,
};

const WAL_TOOL: &str = r#"#!/bin/sh
set -eu
test "$1" = --data-root
test -d "$2"
test -z "${MUST_NOT_REACH_F3:-}"
if test "$3" = products; then
    test "$#" = 3
    if test -f "$2/revoked"; then
        printf '%s\n' '[]'
    else
        printf '%s\n' '[{"source":"fixture","product":"trades"},{"source":"not-approved","product":"book"}]'
    fi
    exit 0
fi
test "$#" = 7
test "$4" = --source
test "$5" = fixture
test "$6" = --product
test "$7" = trades
case "$3" in
    quality)
        printf '%s\n' '[{"channel":"trades","instrument_id":"SYNTH.OTHER","from_ts":"1790000000000000000","to_ts":"1790000000000000000","cause":"rotation","outcome":null,"count":"9007199254740997"}]' ;;
    watermarks)
        if test -f "$2/fail"; then
            printf '%s\n' 'synthetic-untrusted-output-marker' >&2
            printf '%s\n' 'synthetic-untrusted-output-marker'
            exit 1
        fi
        printf '%s\n' '{"source_latest":null,"local_durable":"9007199254740995","projected":"9007199254740993","remote_verified":null}' ;;
    *) exit 81 ;;
esac
"#;

struct PublisherFixture {
    directory: tempfile::TempDir,
    data: PathBuf,
    inputs: PathBuf,
    status: PathBuf,
    static_root: PathBuf,
}

impl PublisherFixture {
    fn new() -> Self {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let binary = directory.path().join("finance-market-wal");
        let data = directory.path().join("wal-data");
        let static_root = directory.path().join("static");
        let inputs = directory.path().join("quality-inputs.json");
        let status = directory.path().join("quality-status.json");
        fs::create_dir(&data).unwrap();
        fs::create_dir(&static_root).unwrap();
        fs::write(&binary, WAL_TOOL).unwrap();
        fs::set_permissions(&binary, Permissions::from_mode(0o700)).unwrap();
        fs::write(
            &inputs,
            json!({
                "wal_bin": binary,
                "data_root": data,
                "streams": [
                    {"source": "fixture", "product": "trades", "license": "synthetic"},
                    {"source": "fixture", "product": "unavailable", "license": "synthetic"}
                ]
            })
            .to_string(),
        )
        .unwrap();
        Self {
            directory,
            data,
            inputs,
            status,
            static_root,
        }
    }

    fn publish_to(&self, destination: &Path) -> Output {
        Command::new(env!("CARGO_BIN_EXE_web"))
            .args(["publish-quality", self.inputs.to_str().unwrap()])
            .env_clear()
            .env(
                "WEB_CONFIG_DIR",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("config"),
            )
            .env("WEB_STATIC_ROOT", &self.static_root)
            .env("WEB_QUALITY_STATUS_FILE", destination)
            .env("MUST_NOT_REACH_F3", "synthetic-parent-env-marker")
            .output()
            .unwrap()
    }

    fn publish(&self) -> Output {
        self.publish_to(&self.status)
    }

    async fn gaps(&self) -> Value {
        // Create the admin fixture inside /tmp. The router never needs a real database connection.
        let token_file = self.directory.path().join("test-admin.env");
        let token = uuid::Uuid::new_v4().to_string();
        writeln!(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&token_file)
                .unwrap(),
            "WEB_ADMIN_TOKEN={token}"
        )
        .unwrap();
        let config = Settings::load(Path::new("config"), |name| match name {
            "WEB_QUALITY_STATUS_FILE" => self.status.to_str().map(String::from),
            "WEB_STATIC_ROOT" => self.static_root.to_str().map(String::from),
            _ => None,
        })
        .unwrap();
        let timeout = Duration::from_secs(1);
        let response = router(AppState {
            finance: Finance::new(&config.finance).unwrap(),
            db: Database::new(&config.database.url, timeout, timeout).unwrap(),
            write_access: WriteAccess::load(&token_file, 10, timeout).unwrap(),
            credential_store: None,
            config: Arc::new(config),
            started_at: Utc::now(),
        })
        .oneshot(
            Request::get("/api/v1/quality/gaps")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let bytes = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }
}

#[tokio::test]
async fn quality_publish_command_serves_only_approved_f3_products() {
    let fixture = PublisherFixture::new();
    let result = fixture.publish();
    assert!(
        result.status.success(),
        "publisher failed: {:?}",
        result.status
    );
    assert!(result.stdout.is_empty());
    assert!(result.stderr.is_empty());
    assert_eq!(
        fs::metadata(&fixture.status).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let page = fixture.gaps().await;
    assert_eq!(page["state"], "available");
    assert_eq!(page["coverage"].as_array().unwrap().len(), 1);
    assert_eq!(page["coverage"][0]["source"], "fixture");
    assert_eq!(page["coverage"][0]["license"], "synthetic");
    assert_eq!(
        page["coverage"][0]["watermarks"]["projected"],
        "9007199254740993"
    );
    assert_eq!(page["items"][0]["count"], "9007199254740997");
    assert_eq!(page["items"][0]["instrument_id"], "SYNTH.OTHER");
}

#[test]
fn quality_publisher_retains_last_snapshot_on_failure_and_removes_revoked_streams() {
    let fixture = PublisherFixture::new();
    assert!(fixture.publish().status.success());
    let before = fs::read(&fixture.status).unwrap();
    fs::File::create(fixture.data.join("fail")).unwrap();
    let failed = fixture.publish();
    assert!(!failed.status.success());
    assert!(failed.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("the F3 quality tool returned a failure")
    );
    assert!(!String::from_utf8_lossy(&failed.stderr).contains("synthetic-untrusted-output-marker"));
    assert_eq!(fs::read(&fixture.status).unwrap(), before);

    let public = fixture.static_root.join("quality.json");
    assert!(!fixture.publish_to(&public).status.success());
    assert!(!public.exists());
    fs::File::create(fixture.data.join("revoked")).unwrap();
    assert!(fixture.publish().status.success());
    let snapshot: Value = serde_json::from_slice(&fs::read(&fixture.status).unwrap()).unwrap();
    assert_eq!(snapshot["streams"], json!([]));
}
