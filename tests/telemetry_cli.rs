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

#[test]
fn notifier_startup_filters_dependency_diagnostics_without_losing_the_error() {
    let marker = "synthetic-private-notifier-settings-wg23";
    let database_url = format!(
        "postgresql://web_test@localhost/postgres?password=&guard_probe={marker}&sslmode=invalid"
    );
    let output = Command::new(env!("CARGO_BIN_EXE_web-notifier"))
        .env_clear()
        .env("DATABASE_URL", database_url)
        .env("WEB_SMTP_HOST", "127.0.0.1")
        .env("WEB_SMTP_FROM", "sender@example.invalid")
        .env("WEB_SMTP_TO", "receiver@example.invalid")
        .env("RUST_LOG", "trace")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("invalid DATABASE_URL"));
    assert!(!stderr.contains(marker));
    let log: serde_json::Value = serde_json::from_str(&stderr).unwrap();
    assert_eq!(log["level"], "ERROR");
    assert_eq!(log["fields"]["message"], "notifier command failed");
    assert_eq!(log["fields"]["stage"], "database_configuration");
    assert_eq!(log["fields"]["reason"], "invalid DATABASE_URL");
}

#[test]
fn notifier_rejects_bad_arguments_with_a_json_event() {
    let output = Command::new(env!("CARGO_BIN_EXE_web-notifier"))
        .env_clear()
        .arg("unexpected")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let log: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(log["fields"]["stage"], "command_line");
    assert_eq!(log["fields"]["reason"], "usage: web-notifier");
}
