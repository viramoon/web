use super::*;
use crate::{auth::WriteAccess, config::Settings, database::Database, finance::Finance, router};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use serde_json::{Value, json};
use std::{
    fs::FileTimes,
    io::Write,
    os::unix::fs::symlink,
    sync::Arc,
    time::{Duration, SystemTime},
};
use tower::ServiceExt;

const FULL: &str = "20261127T030000.000002Z";
const HOURLY: &str = "20261127T040000.123456Z";

fn utc(value: &str) -> DateTime<Utc> {
    value.parse().unwrap()
}

fn write(path: &Path, data: Value) {
    fs::write(path, serde_json::to_vec(&data).unwrap()).unwrap();
}

fn backup_fixture(degraded: bool) -> Value {
    // Synthetic producer IDs use repeated hexadecimal digits. Times represent invented UTC points.
    json!({
        "remote_id": "a".repeat(64), "full": FULL, "hourly": HOURLY, "last_point": HOURLY,
        "degraded": degraded, "unrecognized": "synthetic-extra-value"
    })
}

#[test]
fn t2b_cache_retains_verified_head_and_separates_observation_and_restore_times() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let state = directory.path().join("backup-remote-status.json");
    let restore = directory.path().join("backup-remote-restore.json");
    let config = OpsConfig {
        backup_status_file: Some(state.clone()),
        backup_restore_file: Some(restore.clone()),
        ..Default::default()
    };
    let now = utc("2026-11-27T04:35:00Z");
    write(&state, backup_fixture(false));
    write(
        &restore,
        json!({
            "remote_id": "a".repeat(64), "point": {"id": FULL, "sha256": "b".repeat(64)},
            "restored_at": "20261127T041500.000000Z"
        }),
    );

    let read = |observed: &str| {
        let mtime = SystemTime::from(utc(observed));
        fs::File::open(&state)
            .unwrap()
            .set_times(FileTimes::new().set_modified(mtime))
            .unwrap();
        serde_json::to_value(backup::BackupStatus::read(&config, now)).unwrap()
    };
    let healthy = read("2026-11-27T04:01:00Z");
    assert_eq!(healthy["state"], "healthy");
    assert_eq!(healthy["alert"], false);
    assert_eq!(healthy["age_seconds"], 2099);
    assert_eq!(healthy["last_recoverable_point"], HOURLY);
    assert_eq!(
        healthy["last_recoverable_at"],
        "2026-11-27T04:00:00.123456Z"
    );
    assert_eq!(healthy["last_full_at"], "2026-11-27T03:00:00.000002Z");
    assert_eq!(healthy["last_hourly_at"], "2026-11-27T04:00:00.123456Z");
    assert_eq!(healthy["observed_at"], "2026-11-27T04:01:00Z");
    assert_eq!(
        healthy["last_isolated_restore"],
        json!({
            "point": FULL, "recoverable_at": "2026-11-27T03:00:00.000002Z",
            "restored_at": "2026-11-27T04:15:00Z"
        })
    );
    let serialized = healthy.to_string();
    assert!(!serialized.contains("synthetic-extra-value"));
    assert!(!serialized.contains(&"a".repeat(64)));
    assert!(!serialized.contains(&"b".repeat(64)));

    write(&state, backup_fixture(true));
    let degraded = read("2026-11-27T04:30:00Z");
    assert_eq!(degraded["state"], "degraded");
    assert_eq!(degraded["alert"], true);
    assert_eq!(degraded["degraded"], true);
    assert_eq!(degraded["last_recoverable_point"], HOURLY);
    assert_eq!(degraded["observed_at"], "2026-11-27T04:30:00Z");

    write(
        &restore,
        json!({
            "remote_id": "c".repeat(64), "point": {"id": FULL, "sha256": "b".repeat(64)},
            "restored_at": "20261127T041500.000000Z"
        }),
    );
    assert!(read("2026-11-27T04:30:00Z")["last_isolated_restore"].is_null());
}

#[test]
fn backup_disabled_missing_overdue_and_invalid_are_distinguishable() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let state = directory.path().join("backup-remote-status.json");
    let config = OpsConfig {
        backup_status_file: Some(state.clone()),
        ..Default::default()
    };
    let now = utc("2026-11-27T05:00:00.123457Z");
    let value =
        |config: &OpsConfig| serde_json::to_value(backup::BackupStatus::read(config, now)).unwrap();
    let disabled = value(&OpsConfig::default());
    assert_eq!(disabled["state"], "disabled");
    assert_eq!(disabled["alert"], false);
    let missing = value(&config);
    assert_eq!(missing["state"], "unavailable");
    assert_eq!(missing["alert"], true);
    assert!(missing["observed_at"].is_null());

    write(&state, json!({ "remote_id": "a".repeat(64) }));
    assert_eq!(value(&config)["state"], "no_verified_point");
    assert_eq!(value(&config)["alert"], true);

    write(&state, backup_fixture(false));
    let overdue = value(&config);
    assert_eq!(overdue["state"], "degraded");
    assert_eq!(overdue["alert"], true);
    assert_eq!(overdue["degraded"], false);
    assert_eq!(overdue["age_seconds"], 3600);
    let exactly_at_threshold = serde_json::to_value(backup::BackupStatus::read(
        &config,
        utc("2026-11-27T05:00:00.123456Z"),
    ))
    .unwrap();
    assert_eq!(exactly_at_threshold["alert"], false);

    let mut invalid = backup_fixture(false);
    invalid["last_point"] = json!(FULL);
    write(&state, invalid);
    assert_eq!(value(&config)["state"], "unavailable");
    fs::rename(&state, state.with_extension("moved")).unwrap();
    symlink(state.with_extension("moved"), &state).unwrap();
    assert_eq!(value(&config)["state"], "unavailable");
}

fn quality_fixture() -> Value {
    // Take a fixed UTC-nanosecond boundary and offset it by one tick. Every source, symbol,
    // recovery category and position below is generated for this test, without market payloads.
    let tick = 1_790_000_000_000_000_000_u64;
    json!({
        "version": 1,
        "observed_at": "2026-11-27T04:30:00Z",
        "streams": [
            {
                "source": "fixture", "product": "trades", "license": "synthetic",
                "watermarks": {
                    "source_latest": {
                        "channel": "trades", "instrument_id": "SYNTH.OTHER", "epoch": "1",
                        "seq": "9007199254740995", "exchange_ts": null, "receive_ts": tick.to_string()
                    },
                    "local_durable": "9007199254740995", "projected": "9007199254740993", "remote_verified": null
                },
                "reports": [
                    {
                        "channel": "trades", "instrument_id": "SYNTH.OTHER",
                        "from_ts": tick.to_string(), "to_ts": tick.to_string(),
                        "cause": "disconnected", "outcome": "tick_backfilled", "count": "9007199254740997"
                    },
                    {
                        "channel": "trades", "instrument_id": "SYNTH.OTHER",
                        "from_ts": (tick + 1).to_string(), "to_ts": (tick + 1).to_string(),
                        "cause": "rotation", "outcome": null, "count": null
                    }
                ]
            },
            {
                "source": "fixture", "product": "book", "license": "synthetic",
                "watermarks": null,
                "reports": [{
                    "channel": "depth", "instrument_id": "SYNTH.OTHER",
                    "from_ts": u64::MAX.to_string(), "to_ts": u64::MAX.to_string(),
                    "cause": "source_gap", "outcome": "history_unrecoverable", "count": null
                }]
            }
        ]
    })
}

#[test]
fn quality_adapts_f3_categories_watermarks_paging_and_exact_boundaries() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let quality = directory.path().join("quality.json");
    let config = OpsConfig {
        quality_status_file: Some(quality.clone()),
        ..Default::default()
    };
    write(&quality, quality_fixture());
    let page = |query| {
        serde_json::to_value(quality::QualityGapsPage::read(
            &config,
            serde_json::from_value(query).unwrap(),
        ))
        .unwrap()
    };
    let latest = page(json!({"source": "fixture", "product": "trades", "limit": 1}));
    assert_eq!(latest["state"], "available");
    assert_eq!(latest["as_of"], "2026-11-27T04:30:00Z");
    assert_eq!(latest["next_offset"], 1);
    assert_eq!(latest["coverage"].as_array().unwrap().len(), 1);
    assert_eq!(
        latest["coverage"][0]["watermarks"]["projected"],
        "9007199254740993"
    );
    assert!(latest["coverage"][0]["watermarks"]["remote_verified"].is_null());
    let newer = &latest["items"][0];
    assert_eq!(newer["cause"], "rotation");
    assert!(newer["outcome"].is_null());

    let earlier = page(json!({"product": "trades", "limit": 1, "offset": 1}));
    let earlier_gap = &earlier["items"][0];
    assert_eq!(earlier_gap["outcome"], "tick_backfilled");
    assert_eq!(earlier_gap["count"], "9007199254740997");
    assert!(earlier["next_offset"].is_null());
    assert_eq!(earlier_gap["to"], newer["from"]);
    let from = earlier_gap["from"].as_str().unwrap();
    let to = earlier_gap["to"].as_str().unwrap();
    assert_eq!(utc(to) - utc(from), chrono::TimeDelta::nanoseconds(1));
    let filtered = page(json!({"from": from, "to": to}));
    assert_eq!(filtered["items"], json!([earlier_gap]));

    let extreme = &page(json!({"product": "book"}))["items"][0];
    assert_eq!(extreme["outcome"], "history_unrecoverable");
    assert_eq!(
        utc(extreme["to"].as_str().unwrap()) - utc(extreme["from"].as_str().unwrap()),
        chrono::TimeDelta::nanoseconds(1)
    );
}

#[test]
fn quality_rejects_unknown_fields_excess_size_and_impossible_counts() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let quality = directory.path().join("quality.json");
    let config = OpsConfig {
        quality_status_file: Some(quality.clone()),
        ..Default::default()
    };
    let read = |config: &OpsConfig| {
        serde_json::to_value(quality::QualityGapsPage::read(
            config,
            serde_json::from_value(json!({})).unwrap(),
        ))
        .unwrap()
    };
    let unavailable = json!({"state": "unavailable", "as_of":null, "coverage":[], "items":[], "next_offset":null});
    assert_eq!(read(&OpsConfig::default()), unavailable);
    assert_eq!(read(&config), unavailable);

    let mut invalid = quality_fixture();
    invalid["streams"][0]["reports"][0]["count"] = json!("18446744073709551616");
    write(&quality, invalid);
    assert_eq!(read(&config), unavailable);
    let mut invalid = quality_fixture();
    invalid["streams"][0]["reports"][0]["detail"] = json!("synthetic-redaction-marker");
    write(&quality, invalid);
    assert_eq!(read(&config), unavailable);
    fs::write(&quality, vec![b' '; 1024 * 1024 + 1]).unwrap();
    assert_eq!(read(&config), unavailable);
}

#[test]
fn ops_paths_are_absolute_private_and_require_a_status_for_restore() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let public = directory.path().join("public");
    fs::create_dir(&public).unwrap();
    let config = OpsConfig {
        backup_restore_file: Some(directory.path().join("restore.json")),
        ..Default::default()
    };
    assert!(config.validate(&public).is_err());
    for path in [
        PathBuf::from("relative.json"),
        public.join("status.json"),
        directory.path().join("../leak.json"),
    ] {
        let config = OpsConfig {
            quality_status_file: Some(path),
            ..Default::default()
        };
        assert!(config.validate(&public).is_err());
    }
    let alias = directory.path().join("alias");
    symlink(&public, &alias).unwrap();
    let config = OpsConfig {
        quality_status_file: Some(alias.join("status.json")),
        ..Default::default()
    };
    assert!(config.validate(&public).is_err());
}

#[tokio::test]
async fn ops_routes_require_admin_and_forbid_caching_and_invalid_queries() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let bearer = uuid::Uuid::new_v4().to_string();
    let token_file = directory.path().join("admin.env");
    writeln!(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&token_file)
            .unwrap(),
        "WEB_ADMIN_TOKEN={bearer}"
    )
    .unwrap();
    let quality = directory.path().join("quality.json");
    write(&quality, quality_fixture());
    let config = Settings::load(Path::new("config"), |name| match name {
        "WEB_QUALITY_STATUS_FILE" => Some(quality.to_str().unwrap().into()),
        _ => None,
    })
    .unwrap();
    let timeout = Duration::from_secs(1);
    let app = router(AppState {
        finance: Finance::new(&config.finance).unwrap(),
        db: Database::new(&config.database.url, timeout, timeout).unwrap(),
        write_access: WriteAccess::load(&token_file, 10, timeout).unwrap(),
        credential_store: None,
        config: Arc::new(config),
        started_at: Utc::now(),
    });
    for route in [BACKUP_STATUS, QUALITY_GAPS] {
        let response = app
            .clone()
            .oneshot(Request::get(route).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
    for (route, expected, value) in [
        (BACKUP_STATUS.to_string(), StatusCode::OK, "disabled"),
        (
            format!("{QUALITY_GAPS}?product=trades&limit=1"),
            StatusCode::OK,
            "available",
        ),
        (
            format!("{QUALITY_GAPS}?limit=0"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
        ),
        (
            format!("{QUALITY_GAPS}?status_path=synthetic-redaction-marker"),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::get(route)
                    .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let bytes = to_bytes(response.into_body(), 16 * 1024).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(!body.to_string().contains("synthetic-redaction-marker"));
        assert_eq!(
            if expected == StatusCode::OK {
                &body["state"]
            } else {
                &body["error"]["code"]
            },
            value
        );
    }
}
