//! Poll existing authenticated read sources and expose only operational fields.
use super::{EventPayload, EventResetReason};
use crate::{
    AppState,
    database::Database,
    notifications::model::{Condition, Disposition, Phase, Severity},
    ops::{BackupUpdate, backup_status},
};
use axum::{extract::State, http::StatusCode};
use chrono::{DateTime, Utc};
use finance_client::{
    apis::jobs_api,
    models::{Job, JobStatus},
};
use futures_util::future::join_all;
use serde::Serialize;
use std::{collections::VecDeque, time::Duration};
use utoipa::ToSchema;

const ALERT_WINDOW: i64 = 32;
const JOB_PAGE: usize = 32;
const ACTIVE_JOBS: usize = 64;
const JOB_CHECKS: usize = 8;
const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub(super) struct JobUpdate {
    #[serde(serialize_with = "text_id")]
    #[schema(value_type = String, pattern = "^[1-9][0-9]*$")]
    pub(super) id: i64,
    /// Values are queued, running, succeeded, failed or timed_out.
    #[schema(value_type = String)]
    pub(super) status: JobStatus,
    pub(super) attempt: u32,
}

fn text_id<S: serde::Serializer>(id: &i64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(id)
}

impl From<Job> for JobUpdate {
    fn from(job: Job) -> Self {
        Self {
            id: job.id,
            status: job.status,
            attempt: job.attempt,
        }
    }
}

impl JobUpdate {
    fn is_active(self) -> bool {
        matches!(self.status, JobStatus::Queued | JobStatus::Running)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, ToSchema, sqlx::FromRow)]
pub(super) struct AlertUpdate {
    #[schema(format = "uuid")]
    id: String,
    #[serde(skip)]
    sequence: i64,
    condition: Condition,
    phase: Phase,
    severity: Severity,
    disposition: Disposition,
    observed_at: DateTime<Utc>,
    acknowledged_at: Option<DateTime<Utc>>,
    duplicate_count: i64,
    last_duplicate_at: Option<DateTime<Utc>>,
}

#[derive(Default)]
pub(super) struct Sources {
    jobs: Jobs,
    alerts: Alerts,
    backup: Option<BackupUpdate>,
}

impl Sources {
    pub(super) async fn refresh(&mut self, state: &AppState) -> Vec<EventPayload> {
        let timeout = OBSERVATION_TIMEOUT.min(Duration::from_millis(
            state.config.server.request_timeout_ms,
        ));
        let (jobs, alerts, backup) = tokio::join!(
            self.jobs.refresh(state, timeout),
            tokio::time::timeout(timeout, read_alerts(&state.db)),
            tokio::time::timeout(timeout, backup_status(State(state.clone()))),
        );
        let mut events = jobs;
        if let Ok(Ok(rows)) = alerts {
            self.alerts.record(rows, &mut events);
        }
        if let Ok(backup) = backup {
            let observed = BackupUpdate::from(backup.0);
            if self.backup.as_ref() != Some(&observed) {
                self.backup = Some(observed.clone());
                events.push(EventPayload::Backup { backup: observed });
            }
        }
        events
    }
}

#[derive(Default)]
struct Jobs {
    cursor: Option<i64>,
    active: VecDeque<JobUpdate>,
}

enum JobObservation {
    Found(JobUpdate),
    Removed,
    Unavailable,
}

impl Jobs {
    async fn refresh(&mut self, state: &AppState, timeout: Duration) -> Vec<EventPayload> {
        let previous: Vec<_> = self.active.iter().take(JOB_CHECKS).copied().collect();
        let cursor = self.cursor;
        let discovery_limit = (ACTIVE_JOBS - self.active.len()).min(JOB_PAGE) as u32;
        let (details, page) = tokio::join!(
            join_all(previous.into_iter().map(|job| async move {
                let observed = tokio::time::timeout(
                    timeout,
                    state
                        .finance
                        .read(|cfg| async move { jobs_api::detail(&cfg, job.id).await }),
                )
                .await;
                match observed {
                    Ok(Ok(job)) => JobObservation::Found(job.into()),
                    Ok(Err(fault)) if fault.status == StatusCode::NOT_FOUND => {
                        JobObservation::Removed
                    }
                    _ => JobObservation::Unavailable,
                }
            })),
            async {
                if discovery_limit == 0 {
                    return None;
                }
                tokio::time::timeout(
                    timeout,
                    state.finance.read(|cfg| async move {
                        jobs_api::list(&cfg, Some(discovery_limit), cursor).await
                    }),
                )
                .await
                .ok()?
                .ok()
            },
        );
        let discovered = page
            .into_iter()
            .flat_map(|page| page.items)
            .take(discovery_limit as usize)
            .map(JobUpdate::from);
        self.record(details, discovered)
    }

    fn record(
        &mut self,
        details: Vec<JobObservation>,
        discovered: impl IntoIterator<Item = JobUpdate>,
    ) -> Vec<EventPayload> {
        let mut events = Vec::new();
        for detail in details {
            let Some(previous) = self.active.pop_front() else {
                break;
            };
            match detail {
                JobObservation::Found(observed) if observed.id == previous.id => {
                    if previous != observed {
                        events.push(EventPayload::Job { job: observed });
                    }
                    if observed.is_active() {
                        self.active.push_back(observed);
                    }
                }
                JobObservation::Removed => {}
                _ => self.active.push_back(previous),
            }
        }
        for observed in discovered {
            if observed.id <= self.cursor.unwrap_or(0) {
                continue;
            }
            self.cursor = Some(observed.id);
            events.push(EventPayload::Job { job: observed });
            if observed.is_active() {
                self.active.push_back(observed);
            }
        }
        events
    }
}

async fn read_alerts(db: &Database) -> Result<Vec<AlertUpdate>, sqlx::Error> {
    let mut connection = db.connection().await?;
    sqlx::query_as(
        "SELECT id::text AS id, sequence, condition, phase, severity, disposition, observed_at,
                acknowledged_at, duplicate_count, last_duplicate_at
         FROM app.alert_events ORDER BY sequence DESC LIMIT $1",
    )
    .bind(ALERT_WINDOW + 1)
    .fetch_all(&mut *connection)
    .await
}

#[derive(Default)]
struct Alerts(Option<Vec<AlertUpdate>>);

impl Alerts {
    fn record(&mut self, mut current: Vec<AlertUpdate>, events: &mut Vec<EventPayload>) {
        if let Some(previous) = &self.0
            && let Some(last_seen) = previous.first()
            && let Some(lookback) = current.get(ALERT_WINDOW as usize)
            && lookback.sequence > last_seen.sequence
        {
            events.push(EventPayload::Reset {
                reason: EventResetReason::AlertWindowOverrun,
            });
        }
        current.truncate(ALERT_WINDOW as usize);
        for alert in current.iter().rev() {
            if self
                .0
                .as_ref()
                .is_none_or(|previous| !previous.contains(alert))
            {
                events.push(EventPayload::Alert {
                    alert: alert.clone(),
                });
            }
        }
        self.0 = Some(current);
    }
}

#[cfg(test)]
mod tests;
