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

fn real_rclone() -> PathBuf {
    let binary = std::env::var_os("WEB_TEST_RCLONE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/bin/rclone"));
    assert!(
        binary.is_absolute() && binary.is_file(),
        "install rclone or set WEB_TEST_RCLONE to an absolute rclone binary"
    );
    binary
}

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
async fn rclone_local_crypt_round_trip_uses_real_ciphertext_and_cleans_up() {
    let binary = real_rclone();
    let fixture = private_tempdir("test-drive-", Path::new("/tmp")).unwrap();
    let preview = probe(&binary, fixture.path()).await.unwrap();
    assert!(preview.encrypted_round_trip && preview.cleaned_up);
    assert!(preview.available_bytes.is_some());
    assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 0);
}

// This process observes the real rclone CLI contract without printing or recording
// its credentials. It can inject a failure after the actual encrypted write.
const AUDIT_RCLONE: &str = r#"
import configparser
import os
import pathlib
import sys

args = sys.argv[1:]
config_path = args[args.index("--config") + 1]
op = args[args.index("--config") + 2]

if op == "obscure":
    assert config_path == "/dev/null"
    assert args[-1] == "-"
else:
    assert op in ("rcat", "cat", "deletefile")
    assert config_path.startswith("/proc/self/fd/")
    assert os.readlink(config_path).startswith("/memfd:web-integration-probe")
    config = configparser.ConfigParser(interpolation=None)
    with open(config_path, encoding="ascii") as file:
        config.read_file(file)
    assert config.sections() == ["web_probe_local", "web_probe_crypt"]
    assert config["web_probe_local"]["type"] == "local"
    remote = config["web_probe_crypt"]
    assert remote["type"] == "crypt"
    assert remote["remote"] == f"web_probe_local:{pathlib.Path.cwd() / 'storage'}"
    assert remote.getboolean("directory_name_encryption")
    assert not remote.getboolean("no_data_encryption")
    assert remote["filename_encryption"] == "standard"
    password, salt = remote["password"], remote["password2"]
    assert len(password) > 32 and len(salt) > 32 and password != salt
    assert all(secret not in arg for arg in args for secret in (password, salt))
    assert all(secret not in value for value in os.environ.values() for secret in (password, salt))
    assert args[-1].startswith("web_probe_crypt:web-probe-")
    # Diagnostic text may contain both temporary credentials. It is discarded in Web.
    assert os.readlink("/proc/self/fd/2") == "/dev/null"
    print(password, salt, "SYNTHETIC_CRYPT_DIAGNOSTIC", file=sys.stderr, flush=True)

    if op == "cat":
        entry, = pathlib.Path("storage").iterdir()
        with entry.open("rb") as ciphertext:
            assert ciphertext.read(8) == b"RCLONE\x00\x00"

    with open(audit_path, "a", encoding="ascii") as audit:
        audit.write(op + "\n")
    if fail_read and op == "cat":
        print("SYNTHETIC_READ_FAILURE")
        sys.exit(1)

os.execv(rclone_path, [rclone_path, *args])
"#;

#[tokio::test]
async fn drive_crypt_config_round_trip_redacts_diagnostics_and_cleans_up_after_failed_read() {
    let binary = real_rclone();
    let fixture = tempfile::tempdir_in("/tmp").unwrap();
    let root = private_tempdir("test-drive-", fixture.path()).unwrap();
    let tool = fixture.path().join("audit-rclone");
    let audit = fixture.path().join("operations");
    let literal = |path: &Path| serde_json::to_string(path.to_str().unwrap()).unwrap();

    for fail_read in [false, true] {
        fs::write(&audit, []).unwrap();
        fs::write(
            &tool,
            format!(
                "#!/usr/bin/python3 -I\nrclone_path = {}\naudit_path = {}\nfail_read = {}\n{AUDIT_RCLONE}",
                literal(&binary),
                literal(&audit),
                if fail_read { "True" } else { "False" }
            ),
        )
        .unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o700)).unwrap();
        let (outcome, evidence) = super::super::probe(Target::Drive(&tool, root.path())).await;
        let wire = serde_json::to_value(&evidence).unwrap();
        let expected = if fail_read {
            assert!(matches!(outcome, Outcome::Failed));
            assert_eq!(fs::read_to_string(&audit).unwrap(), "rcat\ncat\n");
            serde_json::json!({
                "result": "remote_failed", "strategy": null, "research": null, "drive": null
            })
        } else {
            assert!(matches!(outcome, Outcome::Passed));
            assert_eq!(
                fs::read_to_string(&audit).unwrap(),
                "rcat\ncat\ndeletefile\n"
            );
            let available = wire["drive"]["available_bytes"].as_str().unwrap();
            assert!(available.parse::<u128>().is_ok());
            serde_json::json!({
                "result": "verified", "strategy": null, "research": null,
                "drive": {
                    "mode": "local_test", "encrypted_round_trip": true,
                    "cleaned_up": true, "available_bytes": available
                }
            })
        };
        assert_eq!(wire, expected);
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }
}
