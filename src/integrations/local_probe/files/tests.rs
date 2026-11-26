use super::*;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};

// These inputs are authored synthetic fixtures. No generated payload is sampled
// from a market source. Commands check the actual argument and byte boundary.
const STRATEGY: &str = "metadata: {id: example.synthetic, version: '1.0.0'}\n";

fn synthetic_csv() -> String {
    let mut csv = String::new();
    for row in [
        ["date", "open", "high", "low", "close", "volume"],
        ["2024-01-02", "20.01", "20.05", "20.00", "20.04", "123"],
        ["2024-01-03", "20.04", "20.06", "20.02", "20.05", "456"],
    ] {
        csv.push_str(&row.join(","));
        csv.push('\n');
    }
    csv
}

fn executable(dir: &Path, script: &str) -> std::path::PathBuf {
    let path = dir.join("synthetic-tool");
    fs::write(&path, format!("#!/bin/sh\nset -eu\n{script}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn dataset(source: &str, bytes: &str) -> serde_json::Value {
    serde_json::json!({
        "instrument": "SYNTH.XNYS",
        "asset_type": "stock",
        "source": source,
        "source_sha256": sha256(bytes.as_bytes()),
        "data_kind": "fixture",
        "calendar": "XNYS",
        "first_session": "2024-01-02",
        "last_session": "2024-01-03",
        "observations": 2,
        "price_basis": "raw_unadjusted",
        "corporate_actions": "cash_dividends_excluded_split_free_reviewed"
    })
}

#[tokio::test]
async fn strategy_preview_checks_the_source_digest_and_redacts_compiler_payload() {
    let fixture = tempfile::tempdir_in("/tmp").unwrap();
    let file = fixture.path().join("strategy.yaml");
    fs::write(&file, STRATEGY).unwrap();
    let digest = sha256(STRATEGY.as_bytes());
    let response = serde_json::json!({
        "plan_version": "fertile.finance/strategy-plan/v1",
        "execution": "execution.next_close_cash_equity@1.0.0",
        "source_sha256": digest,
        "content_sha256": "f".repeat(64),
        "bundle": {"metadata": {"title": "SYNTHETIC_PRIVATE_PLAN_TEXT"}},
        "factors": [{"local_id": "SYNTHETIC_LOCAL_FACTOR"}],
        "modules": [],
        "sleeves": [{}]
    });
    let script = format!(
        r#"test "$*" = 'compile --format yaml -'
          actual=$(/usr/bin/sha256sum)
          test "$actual" = '{digest}  -'
          printf '%s\n' '{response}'"#
    );
    let tool = executable(fixture.path(), &script);
    let preview = strategy(&tool, &file).await.unwrap();
    assert_eq!(preview.source_sha256, digest);
    assert_eq!(preview.factors, 1);
    assert_eq!(preview.modules, 0);
    assert_eq!(preview.sleeves, 1);
    let json = serde_json::to_string(&preview).unwrap();
    assert!(!json.contains("SYNTHETIC_PRIVATE_PLAN_TEXT"));
    assert!(!json.contains("SYNTHETIC_LOCAL_FACTOR"));
    assert!(!json.contains(file.to_str().unwrap()));

    let mut wrong_version_response = response;
    wrong_version_response["source_sha256"] = serde_json::Value::String("0".repeat(64));
    executable(
        fixture.path(),
        &format!("printf '%s' '{wrong_version_response}'"),
    );
    assert_eq!(
        strategy(&tool, &file).await.unwrap_err(),
        LocalProbeResult::InvalidProducerResponse
    );
}

#[tokio::test]
async fn strategy_rejects_symlink_and_compiler_diagnostics_keep_fixed_status() {
    let fixture = tempfile::tempdir_in("/tmp").unwrap();
    let file = fixture.path().join("strategy.yaml");
    fs::write(&file, STRATEGY).unwrap();
    let link = fixture.path().join("symlink.yaml");
    symlink(&file, &link).unwrap();
    let tool = executable(
        fixture.path(),
        "printf '%s' 'SYNTHETIC_PRIVATE_DIAGNOSTIC' >&2; exit 2",
    );
    assert_eq!(
        strategy(&tool, &link).await.unwrap_err(),
        LocalProbeResult::InvalidFile
    );
    assert_eq!(
        strategy(&tool, &file).await.unwrap_err(),
        LocalProbeResult::ValidationFailed
    );
}

#[tokio::test]
async fn research_hashes_the_exact_staged_bytes_and_checks_the_importer_manifest() {
    let fixture = tempfile::tempdir_in("/tmp").unwrap();
    let manifest = fixture.path().join("manifest.json");
    let csv = fixture.path().join(CSV_NAME);
    let contents = synthetic_csv();
    fs::write(&csv, &contents).unwrap();
    let source = "SYNTHETIC_PRIVATE_SOURCE";
    let request = serde_json::json!({
        "license": "synthetic",
        "dataset": dataset(source, &contents)
    });
    fs::write(&manifest, request.to_string()).unwrap();
    let marker = fixture.path().join("scratch-used");
    let response = dataset("W4 synthetic preflight", &contents);
    let script = format!(
        r#"test "$#" = 14
          test "$1 $2 $3 $4" = '-I -B -m fertile_finance.research_catalog'
          case "$5" in /tmp/*/bars.csv) ;; *) exit 7 ;; esac
          test "$FERTILE_RESEARCH_ROOT" = "${{5%/*}}/catalog"
          test "$PWD/catalog" = "$FERTILE_RESEARCH_ROOT"
          test "$6 $7 $8 $9 ${{10}} ${{11}} ${{12}} ${{13}} ${{14}}" = '--instrument SYNTH.XNYS --source W4 synthetic preflight --kind fixture --asset-type stock --raw-split-free-reviewed'
          actual=$(/usr/bin/sha256sum < "$5")
          test "$actual" = '{}  -'
          /bin/mkdir -p "$FERTILE_RESEARCH_ROOT/SYNTH.XNYS"
          printf '%s' "$FERTILE_RESEARCH_ROOT" > '{}'
          printf '%s\n' '{response}' > "$FERTILE_RESEARCH_ROOT/SYNTH.XNYS/dataset.json"
          printf '%s\n' '{response}'"#,
        sha256(contents.as_bytes()),
        marker.display(),
    );
    let tool = executable(fixture.path(), &script);
    let preview = research(&tool, &manifest).await.unwrap();
    let output = serde_json::to_string(&preview).unwrap();
    assert_eq!(preview.instrument, "SYNTH.XNYS");
    assert_eq!(preview.calendar, Calendar::Xnys);
    assert_eq!(preview.observations, 2);
    assert_eq!(preview.license, ResearchLicense::Synthetic);
    assert!(!output.contains(source));
    assert!(!output.contains(csv.to_str().unwrap()));
    assert!(!Path::new(&fs::read_to_string(marker).unwrap()).exists());
    assert_eq!(fs::read_to_string(&csv).unwrap(), contents);

    let mut wrong_coverage = response;
    wrong_coverage["observations"] = 3.into();
    executable(fixture.path(), &format!("printf '%s' '{wrong_coverage}'"));
    assert_eq!(
        research(&tool, &manifest).await.unwrap_err(),
        LocalProbeResult::InvalidProducerResponse
    );

    let extra_row = ["2024-01-04", "1.00", "1.00", "1.00", "1.00", "0"].join(",");
    fs::write(csv, format!("{contents}{extra_row}\n")).unwrap();
    assert_eq!(
        research(Path::new("/nonexistent/tool"), &manifest)
            .await
            .unwrap_err(),
        LocalProbeResult::HashMismatch
    );
}

#[tokio::test]
async fn research_validates_permissions_calendar_and_instrument_before_invoking_importer() {
    let fixture = tempfile::tempdir_in("/tmp").unwrap();
    let manifest = fixture.path().join("manifest.json");
    let contents = synthetic_csv();
    for (field, wrong) in [
        ("data_kind", "historical"),
        ("calendar", "24X7"),
        ("instrument", "../../SYNTH.XNYS"),
    ] {
        let mut malformed = dataset("Synthetic preflight", &contents);
        malformed[field] = wrong.into();
        fs::write(
            &manifest,
            serde_json::json!({"license": "synthetic", "dataset": malformed}).to_string(),
        )
        .unwrap();
        assert_eq!(
            research(Path::new("/nonexistent/tool"), &manifest)
                .await
                .unwrap_err(),
            LocalProbeResult::InvalidFile
        );
    }
}
