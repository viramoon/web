mod backup;
mod publish;
mod quality;

pub(crate) use backup::{__path_backup_status, backup_status};
pub(crate) use quality::{__path_quality_gaps, quality_gaps};

use crate::{AppState, config::ConfigError};
use axum::{Router, routing::get};
use chrono::{DateTime, Utc};
use serde::{Deserialize, de::DeserializeOwned};
use std::{
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Component, Path, PathBuf},
};

pub(crate) const BACKUP_STATUS: &str = "/api/v1/backup/status";
pub(crate) const QUALITY_GAPS: &str = "/api/v1/quality/gaps";

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpsConfig {
    backup_status_file: Option<PathBuf>,
    backup_restore_file: Option<PathBuf>,
    quality_status_file: Option<PathBuf>,
}

impl OpsConfig {
    pub(crate) fn overlay(&mut self, get: &impl Fn(&str) -> Option<String>) {
        for (destination, key) in [
            (&mut self.backup_status_file, "WEB_BACKUP_STATUS_FILE"),
            (&mut self.backup_restore_file, "WEB_BACKUP_RESTORE_FILE"),
            (&mut self.quality_status_file, "WEB_QUALITY_STATUS_FILE"),
        ] {
            if let Some(path) = get(key) {
                *destination = Some(PathBuf::from(path));
            }
        }
    }

    pub(crate) fn validate(&self, static_root: &Path) -> Result<(), ConfigError> {
        if self.backup_restore_file.is_some() && self.backup_status_file.is_none() {
            return Err("backup restore status requires a backup status file".into());
        }
        for path in [
            self.backup_status_file.as_deref(),
            self.backup_restore_file.as_deref(),
            self.quality_status_file.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if !path.is_absolute()
                || path.file_name().is_none()
                || path.components().any(|part| part == Component::ParentDir)
                || path.starts_with(static_root)
                || fs::canonicalize(static_root)
                    .ok()
                    .zip(
                        path.parent()
                            .and_then(|parent| fs::canonicalize(parent).ok()),
                    )
                    .is_some_and(|(root, parent)| parent.starts_with(root))
            {
                return Err(
                    "operations status files require absolute paths outside the static root".into(),
                );
            }
        }
        Ok(())
    }
}

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route(BACKUP_STATUS, get(backup_status))
        .route(QUALITY_GAPS, get(quality_gaps))
}

pub(crate) fn is_private(path: &str) -> bool {
    matches!(path, BACKUP_STATUS | QUALITY_GAPS)
}

fn read_snapshot<T: DeserializeOwned>(path: &Path, limit: u64) -> Option<(T, DateTime<Utc>)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > limit {
        return None;
    }
    let mut contents = Vec::new();
    file.take(limit + 1).read_to_end(&mut contents).ok()?;
    if contents.len() as u64 > limit {
        return None;
    }
    Some((
        serde_json::from_slice(&contents).ok()?,
        metadata.modified().ok()?.into(),
    ))
}

#[cfg(test)]
mod tests;
