use std::{fs, process::Command};

#[test]
fn startup_error_logs_fixed_stage_without_invalid_config_value() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let marker = "synthetic-private-settings-wg23";
    fs::write(
        temp.path().join("base.yaml"),
        format!("server: {{port: {marker}}}\n"),
    )
    .unwrap();
    fs::write(temp.path().join("cloudbox.yaml"), "{}\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_web"))
        .env_clear()
        .env("WEB_CONFIG_DIR", temp.path())
        .env("RUST_LOG", "trace")
        .arg("serve")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains(marker));
    let log: serde_json::Value = serde_json::from_str(&stderr).unwrap();
    assert_eq!(log["fields"]["stage"], "configuration");
}
