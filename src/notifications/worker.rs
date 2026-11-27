//! Schedules and delivers durable email outbox entries.

mod store;

use crate::{config::ConfigError, database::Database};
use chrono::{DateTime, NaiveTime, TimeDelta, Utc};
use serde::Serialize;
use std::time::Duration;
use tokio::{sync::watch, time};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{
    channel::{DeliveryOutcome, MailKind, NotificationChannel, PreparedSnapshot},
    model::{AttemptOutcome, DeliveryKind},
};
use store::Job;

const DB_DEADLINE: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_ATTEMPTS: i32 = 5;
const MAX_HOURLY_LIMIT: i64 = 1_000;
// A replacement worker must allow any SMTP subprocess from a lost database
// session to reach its transport deadline before treating the attempt as unknown.
const ABANDONED_AFTER: TimeDelta = TimeDelta::seconds(65);

#[derive(Clone, Debug)]
pub struct NotificationPolicy {
    time_zone: String,
    quiet_start: NaiveTime,
    quiet_end: NaiveTime,
    cooldown_seconds: i32,
    normal_hourly_limit: i64,
    urgent_hourly_limit: i64,
    urgent_bypasses_quiet: bool,
}

impl Default for NotificationPolicy {
    fn default() -> Self {
        Self {
            time_zone: "Asia/Shanghai".into(),
            quiet_start: "23:00".parse().expect("valid default time"),
            quiet_end: "08:00".parse().expect("valid default time"),
            cooldown_seconds: 15 * 60,
            normal_hourly_limit: 6,
            urgent_hourly_limit: 2,
            urgent_bypasses_quiet: true,
        }
    }
}

impl NotificationPolicy {
    pub fn from_environment(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let defaults = Self::default();
        let quiet_time = |key, default| -> Result<_, ConfigError> {
            let Some(value) = get(key) else {
                return Ok(default);
            };
            if value.len() != 5 {
                return Err(format!("{key} must use HH:MM").into());
            }
            NaiveTime::parse_from_str(&value, "%H:%M")
                .map_err(|_| format!("{key} must use HH:MM").into())
        };
        let number = |key, default, min, max| -> Result<i64, ConfigError> {
            let Some(value) = get(key) else {
                return Ok(default);
            };
            value
                .parse()
                .ok()
                .filter(|value| (min..=max).contains(value))
                .ok_or_else(|| format!("{key} must be between {min} and {max}").into())
        };
        let time_zone = get("WEB_NOTIFIER_QUIET_TIME_ZONE").unwrap_or(defaults.time_zone);
        if time_zone.is_empty()
            || time_zone.len() > 64
            || !time_zone
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'/' | b'_' | b'-' | b'+'))
        {
            return Err("WEB_NOTIFIER_QUIET_TIME_ZONE must name an IANA time zone".into());
        }
        let urgent_bypasses_quiet = get("WEB_NOTIFIER_URGENT_BYPASS_QUIET")
            .map(|value| value.parse())
            .transpose()
            .map_err(|_| "WEB_NOTIFIER_URGENT_BYPASS_QUIET must be true or false")?
            .unwrap_or(defaults.urgent_bypasses_quiet);
        Ok(Self {
            time_zone,
            quiet_start: quiet_time("WEB_NOTIFIER_QUIET_START", defaults.quiet_start)?,
            quiet_end: quiet_time("WEB_NOTIFIER_QUIET_END", defaults.quiet_end)?,
            cooldown_seconds: number(
                "WEB_NOTIFIER_COOLDOWN_SECONDS",
                i64::from(defaults.cooldown_seconds),
                0,
                604_800,
            )? as i32,
            normal_hourly_limit: number(
                "WEB_NOTIFIER_NORMAL_HOURLY_LIMIT",
                defaults.normal_hourly_limit,
                1,
                MAX_HOURLY_LIMIT,
            )?,
            urgent_hourly_limit: number(
                "WEB_NOTIFIER_URGENT_HOURLY_LIMIT",
                defaults.urgent_hourly_limit,
                1,
                MAX_HOURLY_LIMIT,
            )?,
            urgent_bypasses_quiet,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum NotificationState {
    Idle,
    Healthy,
    Degraded,
    #[default]
    Unavailable,
}

impl AttemptOutcome {
    fn next_attempt(self, attempt: i32) -> Option<TimeDelta> {
        if (1..MAX_ATTEMPTS).contains(&attempt)
            && matches!(self, Self::Timeout | Self::Transient | Self::Interrupted)
        {
            return Some(TimeDelta::seconds(30 * 2_i64.pow((attempt - 1) as u32)));
        }
        None
    }
}

impl From<DeliveryOutcome> for AttemptOutcome {
    fn from(outcome: DeliveryOutcome) -> Self {
        match outcome {
            DeliveryOutcome::Accepted => Self::Accepted,
            DeliveryOutcome::Timeout => Self::Timeout,
            DeliveryOutcome::Transient => Self::Transient,
            DeliveryOutcome::Rejected => Self::Rejected,
            DeliveryOutcome::ConfigurationError => Self::ConfigurationError,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, ToSchema, sqlx::FromRow)]
pub struct NotificationHealth {
    pub state: NotificationState,
    pub failure_since: Option<DateTime<Utc>>,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub last_accepted_at: Option<DateTime<Utc>>,
    pub last_outcome: Option<AttemptOutcome>,
}

pub async fn health(db: &Database) -> NotificationHealth {
    time::timeout(Duration::from_secs(2), store::health(db))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default()
}

pub async fn smtp_idle_reason(
    db: &Database,
    channel: &impl NotificationChannel,
) -> Result<Option<&'static str>, sqlx::Error> {
    time::timeout(DB_DEADLINE, async {
        let mut connection = db.connection().await?;
        Ok(
            match store::smtp_transport(&mut connection, channel).await? {
                store::SmtpTransport::Available { .. } => None,
                store::SmtpTransport::Unavailable(reason) => Some(reason),
            },
        )
    })
    .await
    .map_err(|_| sqlx::Error::PoolTimedOut)?
}

pub async fn run(
    db: &Database,
    policy: &NotificationPolicy,
    channel: &impl NotificationChannel,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ConfigError> {
    channel.validate()?;
    let valid_zone = time::timeout(DB_DEADLINE, store::valid_time_zone(db, &policy.time_zone))
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or("notifier database unavailable while validating the time zone")?;
    if !valid_zone {
        return Err("WEB_NOTIFIER_QUIET_TIME_ZONE must name an IANA time zone".into());
    }
    let paused_candidate = paused_candidate_at_startup(db, channel)
        .await
        .map_err(|_| "notifier could not read the pending SMTP candidate")?;
    if paused_candidate.is_some() {
        tracing::warn!(
            stage = "smtp_configuration",
            reason = "bound candidate SMTP snapshot is unavailable",
            "SMTP candidate test paused"
        );
    }
    let mut database_failed = false;
    while !*shutdown.borrow() {
        let result = dispatch(db, policy, channel, paused_candidate, Utc::now).await;
        if result.is_err() != database_failed {
            database_failed = result.is_err();
            if database_failed {
                tracing::warn!("notifier database operation failed");
            } else {
                tracing::info!("notifier database operation recovered");
            }
        }
        if matches!(result, Ok(true)) {
            continue;
        }
        tokio::select! {
            _ = time::sleep(POLL_INTERVAL) => {},
            _ = shutdown.wait_for(|stop| *stop) => break,
        }
    }
    Ok(())
}

async fn paused_candidate_at_startup(
    db: &Database,
    channel: &impl NotificationChannel,
) -> Result<Option<Uuid>, sqlx::Error> {
    if !channel.snapshot_storage_available() {
        return Ok(None); // Reservations already exclude all candidates without storage.
    }
    time::timeout(DB_DEADLINE, async {
        if let Some((job_id, snapshot)) = store::pending_candidate_snapshot(db).await?
            && channel.prepare_snapshot(snapshot).await.is_none()
        {
            return Ok(Some(job_id));
        }
        Ok(None)
    })
    .await
    .map_err(|_| sqlx::Error::PoolTimedOut)?
}

/// Processes at most one message. The clock is sampled at reservation and at
/// completion, so tests can exercise policy boundaries without wall-clock waits.
pub async fn dispatch_one(
    db: &Database,
    policy: &NotificationPolicy,
    channel: &impl NotificationChannel,
    clock: impl Fn() -> DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let paused_candidate = paused_candidate_at_startup(db, channel).await?;
    dispatch(db, policy, channel, paused_candidate, clock).await
}

async fn dispatch(
    db: &Database,
    policy: &NotificationPolicy,
    channel: &impl NotificationChannel,
    paused_candidate: Option<Uuid>,
    clock: impl Fn() -> DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let Some((mut lock, mut job)) = time::timeout(
        DB_DEADLINE,
        store::claim(db, policy, channel, paused_candidate, &clock),
    )
    .await
    .map_err(|_| sqlx::Error::PoolTimedOut)??
    else {
        return Ok(false);
    };
    if let (Some(_), None, Some(profile)) = (
        job.integration_test_id,
        job.snapshot.as_ref(),
        job.candidate_profile,
    ) {
        job.snapshot = time::timeout(DB_DEADLINE, async {
            let snapshot = channel.capture_profile(profile).await;
            if let Some(snapshot) = snapshot {
                store::pin_snapshot(&mut lock, job.id, snapshot).await?;
            }
            Ok::<_, sqlx::Error>(snapshot.map(PreparedSnapshot::ById))
        })
        .await
        .map_err(|_| sqlx::Error::PoolTimedOut)??;
    }
    let outcome = send(channel, &job).await;
    time::timeout(
        DB_DEADLINE,
        store::finish(&mut lock, &job, outcome, clock()),
    )
    .await
    .map_err(|_| sqlx::Error::PoolTimedOut)??;
    Ok(true)
}

async fn send(channel: &impl NotificationChannel, job: &Job) -> AttemptOutcome {
    let kind = match (job.kind, job.condition) {
        (DeliveryKind::Test, _) => {
            return match (&job.snapshot, job.integration_test_id) {
                (Some(snapshot), _) => snapshot.test(channel, job.id).await.into(),
                (None, None) => channel.test(job.id).await.into(),
                (None, Some(_)) => AttemptOutcome::ConfigurationError,
            };
        }
        (DeliveryKind::OutageDigest, _) => MailKind::TransportRecovered { count: job.count },
        (DeliveryKind::Alert, _) if job.count > 1 => MailKind::Digest { count: job.count },
        (DeliveryKind::Alert, Some(condition)) => MailKind::Alert { condition },
        (DeliveryKind::Reminder, Some(condition)) => MailKind::Reminder { condition },
        (DeliveryKind::Recovery, Some(condition)) => MailKind::Recovery { condition },
        _ => return AttemptOutcome::ConfigurationError,
    };
    match &job.snapshot {
        Some(snapshot) => snapshot.send(channel, job.id, kind).await.into(),
        None => channel.send(job.id, kind).await.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_notification_policy_and_overrides() {
        let defaults = NotificationPolicy::from_environment(|_| None).unwrap();
        assert_eq!(defaults.time_zone, "Asia/Shanghai");
        assert_eq!(defaults.quiet_start.format("%H:%M").to_string(), "23:00");
        assert_eq!(defaults.quiet_end.format("%H:%M").to_string(), "08:00");
        assert_eq!(defaults.cooldown_seconds, 900);
        assert_eq!(defaults.normal_hourly_limit, 6);
        assert_eq!(defaults.urgent_hourly_limit, 2);
        assert!(defaults.urgent_bypasses_quiet);

        let overrides = NotificationPolicy::from_environment(|key| {
            match key {
                "WEB_NOTIFIER_QUIET_TIME_ZONE" => "America/New_York",
                "WEB_NOTIFIER_QUIET_START" | "WEB_NOTIFIER_QUIET_END" => "00:00",
                "WEB_NOTIFIER_COOLDOWN_SECONDS" => "0",
                "WEB_NOTIFIER_NORMAL_HOURLY_LIMIT" => "3",
                "WEB_NOTIFIER_URGENT_HOURLY_LIMIT" => "1",
                "WEB_NOTIFIER_URGENT_BYPASS_QUIET" => "false",
                _ => return None,
            }
            .to_owned()
            .into()
        })
        .unwrap();
        assert_eq!(overrides.time_zone, "America/New_York");
        assert_eq!(overrides.quiet_start, overrides.quiet_end);
        assert_eq!(overrides.cooldown_seconds, 0);
        assert_eq!(overrides.normal_hourly_limit, 3);
        assert_eq!(overrides.urgent_hourly_limit, 1);
        assert!(!overrides.urgent_bypasses_quiet);
        for (setting, invalid) in [
            ("WEB_NOTIFIER_QUIET_TIME_ZONE", "invalid zone"),
            ("WEB_NOTIFIER_QUIET_START", "24:00"),
            ("WEB_NOTIFIER_COOLDOWN_SECONDS", "-1"),
            ("WEB_NOTIFIER_NORMAL_HOURLY_LIMIT", "0"),
            ("WEB_NOTIFIER_URGENT_HOURLY_LIMIT", "1001"),
            ("WEB_NOTIFIER_URGENT_BYPASS_QUIET", "maybe"),
        ] {
            assert!(
                NotificationPolicy::from_environment(|key| {
                    (key == setting).then(|| invalid.to_string())
                })
                .is_err()
            );
        }
    }

    #[test]
    fn smtp_outcome_retry_backoff_is_bounded() {
        use AttemptOutcome::*;
        for outcome in [Timeout, Transient, Interrupted] {
            for (attempt, seconds) in [(1, 30), (2, 60), (3, 120), (4, 240)] {
                assert_eq!(
                    outcome.next_attempt(attempt),
                    Some(TimeDelta::seconds(seconds))
                );
            }
            assert_eq!(outcome.next_attempt(5), None);
        }
        for terminal in [Accepted, Rejected, ConfigurationError] {
            assert_eq!(terminal.next_attempt(1), None);
        }
    }
}
