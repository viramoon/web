use super::*;
use crate::integrations::{
    local_probe::{LocalProbeConfig, Target},
    store::Outcome,
};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
};

#[tokio::test]
async fn drive_probe_redacts_transport_output_and_cleans_up_on_failure() {
    let fixture = tempfile::tempdir_in("/tmp").unwrap();
    let root = private_tempdir("test-drive-", fixture.path()).unwrap();
    let tool = fixture.path().join("fake-rclone");
    fs::write(
        &tool,
        "#!/bin/sh\nprintf '%s' SYNTHETIC_PASSWORD_FROM_STDOUT\nprintf '%s' SYNTHETIC_SALT_FROM_STDERR >&2\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o700)).unwrap();
    let (outcome, evidence) = super::super::probe(Target::Drive(&tool, root.path())).await;
    assert!(matches!(outcome, Outcome::Failed));
    assert_eq!(evidence.result, LocalProbeResult::RemoteFailed);
    let wire = serde_json::to_string(&evidence).unwrap();
    assert!(!wire.contains("SYNTHETIC_PASSWORD_FROM_STDOUT"));
    assert!(!wire.contains("SYNTHETIC_SALT_FROM_STDERR"));
    assert!(!wire.contains(root.path().to_str().unwrap()));
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn drive_probe_rejects_symlinks_unsafe_paths_and_nonprivate_roots() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let link = temp.path().join("link");
    symlink(temp.path(), &link).unwrap();
    let syntax = temp.path().join("unsafe\n[remote]");
    fs::DirBuilder::new().create(&syntax).unwrap();
    for path in [&link, &syntax, &PathBuf::from("named_drive:untrusted")] {
        assert_eq!(
            probe(Path::new("/nonexistent/rclone"), path)
                .await
                .unwrap_err(),
            LocalProbeResult::InvalidFile
        );
    }
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        probe(Path::new("/nonexistent/rclone"), temp.path())
            .await
            .unwrap_err(),
        LocalProbeResult::InvalidFile
    );
}

#[test]
fn deployment_sources_cannot_overlap_the_static_root() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let root = temp.path().join("static");
    fs::create_dir(&root).unwrap();
    let path = root.join("strategy.yaml");
    fs::write(&path, "synthetic").unwrap();
    let aliased_root = temp.path().join("alias");
    symlink(&root, &aliased_root).unwrap();
    let aliased_path = aliased_root.join("strategy.yaml");
    for bad_path in [path, aliased_path] {
        let mut config = LocalProbeConfig::default();
        config.overlay(&|key| {
            (key == "WEB_INTEGRATION_PRIMARY_STRATEGY_FILE")
                .then(|| bad_path.to_str().unwrap().into())
        });
        assert!(config.validate(&root).is_err());
    }
    let mut config = LocalProbeConfig::default();
    config.overlay(&|key| {
        (key == "WEB_INTEGRATION_SECONDARY_DRIVE_TEST_ROOT")
            .then(|| temp.path().to_str().unwrap().into())
    });
    assert!(config.validate(&root).is_err());
}

#[tokio::test]
#[ignore = "requires a local rclone; set WEB_TEST_RCLONE to its absolute path"]
async fn rclone_local_crypt_round_trip_uses_real_ciphertext_and_cleans_up() {
    let binary = std::env::var_os("WEB_TEST_RCLONE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/bin/rclone"));
    assert!(binary.is_absolute());
    let fixture = private_tempdir("test-drive-", Path::new("/tmp")).unwrap();
    let preview = probe(&binary, fixture.path()).await.unwrap();
    assert!(preview.encrypted_round_trip && preview.cleaned_up);
    assert!(preview.available_bytes.is_some());
    assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 0);
}
