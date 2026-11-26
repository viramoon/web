use super::{OpsConfig, read_snapshot};
use crate::{AppState, ErrorBody};
use axum::{Json, extract::State};
use chrono::{DateTime, NaiveDateTime, TimeDelta, Timelike, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

const REMOTE_STATUS_MAX_BYTES: u64 = 16 * 1024;
const POINT_FORMAT: &str = "%Y%m%dT%H%M%S%.6fZ";
const RECOVERY_POINT_OBJECTIVE: TimeDelta = TimeDelta::hours(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BackupState {
    Disabled,
    Unavailable,
    NoVerifiedPoint,
    Healthy,
    Degraded,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct BackupStatus {
    state: BackupState,
    /// Modification time of the last valid T2b state file observation.
    observed_at: Option<DateTime<Utc>>,
    last_recoverable_point: Option<String>,
    last_recoverable_at: Option<DateTime<Utc>>,
    last_full_at: Option<DateTime<Utc>>,
    last_hourly_at: Option<DateTime<Utc>>,
    age_seconds: Option<i64>,
    /// Whether an enabled backup has an unavailable, degraded or overdue recovery point.
    alert: bool,
    /// The latest T2b producer observation reported an error or invalid recovery chain.
    degraded: bool,
    last_isolated_restore: Option<IsolatedRestore>,
}

/// Stable fields from backup status for operational change notifications.
#[derive(Clone, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct BackupUpdate {
    state: BackupState,
    observed_at: Option<DateTime<Utc>>,
    last_recoverable_at: Option<DateTime<Utc>>,
    last_full_at: Option<DateTime<Utc>>,
    last_hourly_at: Option<DateTime<Utc>>,
    last_isolated_restore_at: Option<DateTime<Utc>>,
    alert: bool,
    degraded: bool,
}

impl From<BackupStatus> for BackupUpdate {
    fn from(status: BackupStatus) -> Self {
        Self {
            state: status.state,
            observed_at: status.observed_at,
            last_recoverable_at: status.last_recoverable_at,
            last_full_at: status.last_full_at,
            last_hourly_at: status.last_hourly_at,
            last_isolated_restore_at: status
                .last_isolated_restore
                .map(|restore| restore.restored_at),
            alert: status.alert,
            degraded: status.degraded,
        }
    }
}

#[derive(Serialize, ToSchema)]
struct IsolatedRestore {
    point: String,
    recoverable_at: DateTime<Utc>,
    restored_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct RemoteStatusFile {
    remote_id: String,
    last_point: Option<PointId>,
    full: Option<PointId>,
    hourly: Option<PointId>,
    #[serde(default)]
    degraded: bool,
}

#[derive(Deserialize)]
struct RestoreFile {
    remote_id: String,
    point: PointReference,
    restored_at: PointId,
}

#[derive(Deserialize)]
struct PointReference {
    id: PointId,
    sha256: String,
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(try_from = "String")]
struct PointId {
    time: DateTime<Utc>,
    id: String,
}

impl TryFrom<String> for PointId {
    type Error = &'static str;

    fn try_from(id: String) -> Result<Self, Self::Error> {
        let time = NaiveDateTime::parse_from_str(&id, POINT_FORMAT)
            .ok()
            .filter(|time| time.nanosecond() < 1_000_000_000)
            .filter(|time| time.format(POINT_FORMAT).to_string() == id)
            .ok_or("invalid recovery point timestamp")?
            .and_utc();
        Ok(Self { time, id })
    }
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

impl BackupStatus {
    fn without_observation(state: BackupState) -> Self {
        Self {
            alert: state != BackupState::Disabled,
            state,
            observed_at: None,
            last_recoverable_point: None,
            last_recoverable_at: None,
            last_full_at: None,
            last_hourly_at: None,
            age_seconds: None,
            degraded: false,
            last_isolated_restore: None,
        }
    }

    pub(super) fn read(config: &OpsConfig, now: DateTime<Utc>) -> Self {
        let Some(path) = config.backup_status_file.as_deref() else {
            return Self::without_observation(BackupState::Disabled);
        };
        let Some((native, observed_at)) =
            read_snapshot::<RemoteStatusFile>(path, REMOTE_STATUS_MAX_BYTES)
        else {
            return Self::without_observation(BackupState::Unavailable);
        };
        if !digest(&native.remote_id)
            || native.last_point.as_ref() != native.full.as_ref().max(native.hourly.as_ref())
        {
            return Self::without_observation(BackupState::Unavailable);
        }

        let last_isolated_restore = config
            .backup_restore_file
            .as_deref()
            .and_then(|path| read_snapshot::<RestoreFile>(path, REMOTE_STATUS_MAX_BYTES))
            .map(|(receipt, _)| receipt)
            .filter(|receipt| {
                receipt.remote_id == native.remote_id
                    && digest(&receipt.point.sha256)
                    && receipt.point.id.time <= receipt.restored_at.time
                    && receipt.restored_at.time <= now
            })
            .map(|receipt| IsolatedRestore {
                point: receipt.point.id.id,
                recoverable_at: receipt.point.id.time,
                restored_at: receipt.restored_at.time,
            });
        let last_recoverable_at = native.last_point.as_ref().map(|point| point.time);
        let age = last_recoverable_at.map(|time| now - time);
        let alert = native.degraded
            || age.is_none_or(|age| age < TimeDelta::zero() || age > RECOVERY_POINT_OBJECTIVE);
        let state = match (age, alert) {
            (None, _) => BackupState::NoVerifiedPoint,
            (Some(_), true) => BackupState::Degraded,
            (Some(_), false) => BackupState::Healthy,
        };
        Self {
            state,
            observed_at: Some(observed_at),
            last_recoverable_point: native.last_point.map(|point| point.id),
            last_recoverable_at,
            last_full_at: native.full.map(|point| point.time),
            last_hourly_at: native.hourly.map(|point| point.time),
            age_seconds: age.map(|age| age.num_seconds()),
            alert,
            degraded: native.degraded,
            last_isolated_restore,
        }
    }
}

#[utoipa::path(get, path = "/api/v1/backup/status",
    security(("adminBearer" = []), ("adminSession" = [])),
    responses((status = 200, body = BackupStatus), (status = 401, body = ErrorBody),
        (status = 403, body = ErrorBody),
        (status = 504, description = "Request deadline exceeded (request_timeout)", body = ErrorBody)))]
pub(crate) async fn backup_status(State(state): State<AppState>) -> Json<BackupStatus> {
    let status =
        tokio::task::spawn_blocking(move || BackupStatus::read(&state.config.ops, Utc::now()))
            .await
            .unwrap_or_else(|_| BackupStatus::without_observation(BackupState::Unavailable));
    Json(status)
}
