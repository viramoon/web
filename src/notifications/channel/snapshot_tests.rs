use super::*;
use std::fs;

#[tokio::test]
async fn missing_or_unreadable_bound_snapshot_has_a_distinct_test_outcome() {
    let root = tempfile::tempdir_in("/tmp").unwrap();
    let public = root.path().join("static");
    fs::create_dir(&public).unwrap();
    let snapshots = root.path().join("snapshots");
    fs::create_dir(&snapshots).unwrap();
    fs::set_permissions(&snapshots, fs::Permissions::from_mode(0o700)).unwrap();
    let channels = EmailChannels::from_environment(|key| match key {
        "WEB_SMTP_SNAPSHOT_DIR" => Some(snapshots.to_str().unwrap().into()),
        "WEB_STATIC_ROOT" => Some(public.to_str().unwrap().into()),
        "WEB_SMTP_HOST" => Some("127.0.0.1".into()),
        "WEB_SMTP_FROM" => Some("sender@example.invalid".into()),
        "WEB_SMTP_TO" => Some("receiver@example.invalid".into()),
        _ => None,
    })
    .unwrap();
    let snapshot = channels
        .capture_profile(SmtpProfile::Primary)
        .await
        .unwrap();
    let file = snapshots.join(format!("{snapshot}.smtp"));
    let saved = fs::read(&file).unwrap();
    let id = Uuid::from_u128(1);

    fs::write(&file, b"synthetic unreadable snapshot").unwrap();
    assert_eq!(
        channels.test_snapshot(snapshot, id).await,
        DeliveryOutcome::SnapshotUnavailable
    );
    fs::remove_file(&file).unwrap();
    assert_eq!(
        channels.test_snapshot(snapshot, id).await,
        DeliveryOutcome::SnapshotUnavailable
    );

    let mut restored = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(file)
        .unwrap();
    restored.write_all(&saved).unwrap();
    assert!(matches!(
        channels.prepare_snapshot(snapshot).await,
        Some(PreparedSnapshot::Loaded(_))
    ));
}
