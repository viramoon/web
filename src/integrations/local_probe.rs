mod drive;
mod files;

use super::store::{Outcome, TestInput};
use crate::config::ConfigError;
use rustix::fs::{MemfdFlags, memfd_create};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions, Permissions},
    io::{self, Seek, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command, time::timeout};
use utoipa::ToSchema;

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalProbeConfig {
    strategy_compiler: Option<PathBuf>,
    research_python: Option<PathBuf>,
    rclone: Option<PathBuf>,
    #[serde(default)]
    primary: Slot,
    #[serde(default)]
    secondary: Slot,
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Slot {
    strategy_file: Option<PathBuf>,
    research_manifest: Option<PathBuf>,
    drive_test_root: Option<PathBuf>,
}

impl LocalProbeConfig {
    pub(crate) fn overlay(&mut self, get: &impl Fn(&str) -> Option<String>) {
        for (key, path) in [
            ("WEB_STRATEGY_COMPILER", &mut self.strategy_compiler),
            ("WEB_RESEARCH_PYTHON", &mut self.research_python),
            ("WEB_TEST_RCLONE", &mut self.rclone),
        ] {
            if let Some(value) = get(key) {
                *path = Some(value.into());
            }
        }
        for (prefix, slot) in [
            ("WEB_INTEGRATION_PRIMARY", &mut self.primary),
            ("WEB_INTEGRATION_SECONDARY", &mut self.secondary),
        ] {
            for (suffix, path) in [
                ("STRATEGY_FILE", &mut slot.strategy_file),
                ("RESEARCH_MANIFEST", &mut slot.research_manifest),
                ("DRIVE_TEST_ROOT", &mut slot.drive_test_root),
            ] {
                if let Some(value) = get(&format!("{prefix}_{suffix}")) {
                    *path = Some(value.into());
                }
            }
        }
    }

    pub(crate) fn validate(&self, static_root: &Path) -> Result<(), ConfigError> {
        for path in [
            &self.strategy_compiler,
            &self.research_python,
            &self.rclone,
            &self.primary.strategy_file,
            &self.primary.research_manifest,
            &self.primary.drive_test_root,
            &self.secondary.strategy_file,
            &self.secondary.research_manifest,
            &self.secondary.drive_test_root,
        ]
        .into_iter()
        .flatten()
        {
            if !path.is_absolute()
                || path == Path::new("/")
                || path.components().any(|part| part == Component::ParentDir)
            {
                return Err("integration probe paths must be absolute and confined to the configured targets".into());
            }
        }
        for slot in [&self.primary, &self.secondary] {
            let paths = [
                (slot.strategy_file.as_deref(), false),
                (slot.research_manifest.as_deref(), false),
                (slot.drive_test_root.as_deref(), true),
            ];
            for (path, directory) in paths {
                if let Some(path) = path {
                    let overlaps = |root: &Path, path: &Path| {
                        path.starts_with(root) || (directory && root.starts_with(path))
                    };
                    if overlaps(static_root, path)
                        || static_root
                            .canonicalize()
                            .ok()
                            .zip(path.canonicalize().ok())
                            .is_some_and(|(root, path)| overlaps(&root, &path))
                    {
                        return Err("integration probe files and test remote must be outside the static root".into());
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn available(&self, connector: &str) -> bool {
        [&self.primary, &self.secondary]
            .into_iter()
            .any(|slot| self.target(connector, slot).is_some())
    }

    fn target<'a>(&'a self, connector: &str, slot: &'a Slot) -> Option<Target<'a>> {
        match connector {
            "strategy_file" => Some(Target::Strategy(
                self.strategy_compiler.as_deref()?,
                slot.strategy_file.as_deref()?,
            )),
            "research_file" => Some(Target::Research(
                self.research_python.as_deref()?,
                slot.research_manifest.as_deref()?,
            )),
            "google_drive" => Some(Target::Drive(
                self.rclone
                    .as_deref()
                    .unwrap_or_else(|| Path::new("/usr/bin/rclone")),
                slot.drive_test_root.as_deref()?,
            )),
            _ => None,
        }
    }
}

enum Target<'a> {
    Strategy(&'a Path, &'a Path),
    Research(&'a Path, &'a Path),
    Drive(&'a Path, &'a Path),
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalEvidence {
    result: LocalProbeResult,
    strategy: Option<files::StrategyPreview>,
    research: Option<files::ResearchPreview>,
    drive: Option<drive::LocalDrivePreview>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum LocalProbeResult {
    Verified,
    NotConfigured,
    InvalidFile,
    HashMismatch,
    InvalidProducerResponse,
    ToolUnavailable,
    ValidationFailed,
    RemoteFailed,
    RoundTripMismatch,
    CleanupFailed,
    DeadlineExceeded,
}

impl LocalEvidence {
    fn from_result(result: LocalProbeResult) -> Self {
        Self {
            result,
            strategy: None,
            research: None,
            drive: None,
        }
    }
}

pub(super) async fn test(config: &LocalProbeConfig, input: &TestInput) -> (Outcome, LocalEvidence) {
    let slot = match input
        .settings
        .get("profile")
        .and_then(|value| value.as_str())
    {
        Some("primary") => &config.primary,
        Some("secondary") => &config.secondary,
        _ => {
            return (
                Outcome::Failed,
                LocalEvidence::from_result(LocalProbeResult::InvalidFile),
            );
        }
    };
    let Some(target) = config.target(&input.connector, slot) else {
        return (
            Outcome::Unavailable,
            LocalEvidence::from_result(LocalProbeResult::NotConfigured),
        );
    };
    probe(target).await
}

async fn probe(target: Target<'_>) -> (Outcome, LocalEvidence) {
    let verify = async {
        let mut evidence = LocalEvidence::from_result(LocalProbeResult::Verified);
        match target {
            Target::Strategy(binary, path) => {
                evidence.strategy = Some(files::strategy(binary, path).await?);
            }
            Target::Research(binary, manifest) => {
                evidence.research = Some(files::research(binary, manifest).await?);
            }
            Target::Drive(binary, root) => {
                evidence.drive = Some(drive::probe(binary, root).await?);
            }
        }
        Ok(evidence)
    };
    let result = timeout(Duration::from_secs(12), verify)
        .await
        .unwrap_or(Err(LocalProbeResult::DeadlineExceeded));
    match result {
        Ok(evidence) => (Outcome::Passed, evidence),
        Err(result @ (LocalProbeResult::DeadlineExceeded | LocalProbeResult::ToolUnavailable)) => {
            (Outcome::Unavailable, LocalEvidence::from_result(result))
        }
        Err(result) => (Outcome::Failed, LocalEvidence::from_result(result)),
    }
}

fn command(binary: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env_clear()
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    command
}

async fn capture(
    command: &mut Command,
    limit: usize,
) -> Result<(ExitStatus, Vec<u8>), LocalProbeResult> {
    let mut child = command
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|_| LocalProbeResult::ToolUnavailable)?;
    let stdout = child.stdout.take().expect("subprocess stdout is piped");
    let mut bytes = Vec::new();
    stdout
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| LocalProbeResult::InvalidProducerResponse)?;
    if bytes.len() > limit {
        return Err(LocalProbeResult::InvalidProducerResponse);
    }
    let status = child
        .wait()
        .await
        .map_err(|_| LocalProbeResult::ToolUnavailable)?;
    Ok((status, bytes))
}

fn memory_file(contents: &[u8]) -> io::Result<File> {
    let mut file = File::from(memfd_create("web-integration-probe", MemfdFlags::CLOEXEC)?);
    file.write_all(contents)?;
    file.rewind()?;
    Ok(file)
}

fn private_tempdir(prefix: &str, root: &Path) -> io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix(prefix)
        .permissions(Permissions::from_mode(0o700))
        .tempdir_in(root)
}

fn regular_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("probe input must be a regular file"));
    }
    Ok(file)
}

async fn read_file(path: &Path, max_bytes: usize) -> Result<Vec<u8>, LocalProbeResult> {
    let mut bytes = Vec::new();
    tokio::fs::File::from_std(regular_file(path).map_err(|_| LocalProbeResult::InvalidFile)?)
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| LocalProbeResult::InvalidFile)?;
    if bytes.is_empty() || bytes.len() > max_bytes {
        return Err(LocalProbeResult::InvalidFile);
    }
    Ok(bytes)
}
