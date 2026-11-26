use super::super::model::{
    AlertSignal, DeliveryKind, Disposition, Phase, RecordOutcome, RuleConfig, Severity,
};
use crate::database::Database;
use chrono::{DateTime, TimeDelta, Utc};
use sqlx::{Connection, PgConnection};
use std::fmt;
use uuid::Uuid;

#[cfg(test)]
mod tests;

#[derive(Debug)]
pub enum RecordError {
    UnknownRule,
    ConditionMismatch,
    InvalidTime,
    Database(sqlx::Error),
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnknownRule => "notification rule was not found",
            Self::ConditionMismatch => "notification rule does not match the signal condition",
            Self::InvalidTime => "invalid notification signal time",
            Self::Database(_) => "notification persistence failed",
        })
    }
}

impl std::error::Error for RecordError {}

impl From<sqlx::Error> for RecordError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

#[derive(Default, sqlx::FromRow)]
struct Incident {
    last_observed_at: Option<DateTime<Utc>>,
    consecutive_count: i32,
    active: bool,
    email_open: bool,
    next_reminder_at: Option<DateTime<Utc>>,
}

impl Incident {
    fn apply(
        &mut self,
        rule: &RuleConfig,
        phase: Phase,
        observed_at: DateTime<Utc>,
    ) -> Result<(Disposition, Option<DeliveryKind>), RecordError> {
        if self
            .last_observed_at
            .is_some_and(|last| observed_at <= last)
        {
            return Ok((Disposition::Ignored, None));
        }
        self.last_observed_at = Some(observed_at);
        if !rule.enabled {
            return Ok((Disposition::Ignored, None));
        }
        if phase == Phase::Recovered {
            let outcome = match (self.active, self.email_open && rule.email_enabled) {
                (true, true) => (Disposition::Pending, Some(DeliveryKind::Recovery)),
                (true, false) => (Disposition::InApp, None),
                (false, _) => (Disposition::Observed, None),
            };
            self.active = false;
            self.email_open = false;
            self.consecutive_count = 0;
            self.next_reminder_at = None;
            return Ok(outcome);
        }

        if self.active {
            if self.email_open && rule.email_enabled && rule.reminder_seconds.is_some() {
                match self.next_reminder_at {
                    Some(next) if next <= observed_at => {
                        self.schedule_reminder(rule, observed_at)?;
                        return Ok((Disposition::Pending, Some(DeliveryKind::Reminder)));
                    }
                    None => self.schedule_reminder(rule, observed_at)?,
                    _ => {}
                }
            } else {
                self.next_reminder_at = None;
            }
            return Ok((Disposition::Observed, None));
        }

        self.consecutive_count = (self.consecutive_count + 1).min(100);
        if self.consecutive_count < rule.min_occurrences {
            return Ok((Disposition::Observed, None));
        }
        self.active = true;
        self.email_open = rule.email_enabled;
        if self.email_open {
            self.schedule_reminder(rule, observed_at)?;
            Ok((Disposition::Pending, Some(DeliveryKind::Alert)))
        } else {
            Ok((Disposition::InApp, None))
        }
    }

    fn schedule_reminder(
        &mut self,
        rule: &RuleConfig,
        observed_at: DateTime<Utc>,
    ) -> Result<(), RecordError> {
        self.next_reminder_at = rule
            .reminder_seconds
            .map(|seconds| {
                observed_at
                    .checked_add_signed(TimeDelta::seconds(seconds.into()))
                    .ok_or(RecordError::InvalidTime)
            })
            .transpose()?;
        Ok(())
    }
}

async fn mark_duplicate(
    tx: &mut PgConnection,
    rule_id: Uuid,
    signal: AlertSignal,
) -> Result<Option<RecordOutcome>, sqlx::Error> {
    let row = sqlx::query_as::<_, (Uuid, Disposition)>(
        "UPDATE app.alert_events SET duplicate_count = duplicate_count + 1,
                last_duplicate_at = now()
         WHERE rule_id = $1 AND condition = $2 AND incident_key = $3
               AND business_window = $4 AND phase = $5
         RETURNING id, disposition",
    )
    .bind(rule_id)
    .bind(signal.condition)
    .bind(signal.incident_key)
    .bind(signal.window_start)
    .bind(signal.phase)
    .fetch_optional(tx)
    .await?;
    Ok(row.map(|(id, disposition)| RecordOutcome {
        id,
        disposition,
        duplicate: true,
    }))
}

/// Atomically persist a new business event, its incident transition and any notification.
/// At-least-once producers replay with the same rule, condition, subject, window and phase.
pub async fn record_signal(
    db: &Database,
    rule_id: Uuid,
    mut signal: AlertSignal,
) -> Result<RecordOutcome, RecordError> {
    if signal.window_start > signal.observed_at {
        return Err(RecordError::InvalidTime);
    }
    // PostgreSQL timestamps store microseconds. Compare state transitions at that precision too.
    signal.observed_at = DateTime::from_timestamp_micros(signal.observed_at.timestamp_micros())
        .ok_or(RecordError::InvalidTime)?;

    let mut connection = db.write_connection().await?;
    let mut tx = connection.begin().await?;
    let rule = sqlx::query_as::<_, RuleConfig>(
        "SELECT * FROM app.notification_rules WHERE id = $1 FOR SHARE",
    )
    .bind(rule_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(RecordError::UnknownRule)?;

    if rule.condition != signal.condition {
        let duplicate = mark_duplicate(&mut tx, rule_id, signal)
            .await?
            .ok_or(RecordError::ConditionMismatch)?;
        tx.commit().await?;
        return Ok(duplicate);
    }

    sqlx::query(
        "INSERT INTO app.notification_incidents (rule_id, condition, incident_key)
         VALUES ($1,$2,$3) ON CONFLICT DO NOTHING",
    )
    .bind(rule_id)
    .bind(signal.condition)
    .bind(signal.incident_key)
    .execute(&mut *tx)
    .await?;

    let mut incident = sqlx::query_as::<_, Incident>(
        "SELECT * FROM app.notification_incidents
         WHERE rule_id = $1 AND condition = $2 AND incident_key = $3 FOR UPDATE",
    )
    .bind(rule_id)
    .bind(signal.condition)
    .bind(signal.incident_key)
    .fetch_one(&mut *tx)
    .await?;

    if let Some(duplicate) = mark_duplicate(&mut tx, rule_id, signal).await? {
        tx.commit().await?;
        return Ok(duplicate);
    }

    let was_active = incident.active;
    let (disposition, kind) = incident.apply(&rule, signal.phase, signal.observed_at)?;
    sqlx::query(
        "UPDATE app.notification_incidents SET last_observed_at = $4, consecutive_count = $5,
             active = $6, email_open = $7, next_reminder_at = $8
         WHERE rule_id = $1 AND condition = $2 AND incident_key = $3",
    )
    .bind(rule_id)
    .bind(signal.condition)
    .bind(signal.incident_key)
    .bind(incident.last_observed_at)
    .bind(incident.consecutive_count)
    .bind(incident.active)
    .bind(incident.email_open)
    .bind(incident.next_reminder_at)
    .execute(&mut *tx)
    .await?;

    if was_active && !incident.active {
        sqlx::query(
            "UPDATE app.notification_outbox d
             SET status = CASE WHEN d.status = 'pending' THEN 'suppressed' ELSE d.status END,
                 suppression_reason = 'incident_recovered', updated_at = now()
             FROM app.alert_events a WHERE d.event_id = a.id AND a.rule_id = $1
               AND a.condition = $2 AND a.incident_key = $3
               AND d.status IN ('pending', 'sending') AND d.kind = 'reminder'",
        )
        .bind(rule_id)
        .bind(signal.condition)
        .bind(signal.incident_key)
        .execute(&mut *tx)
        .await?;
    }

    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO app.alert_events (id, rule_id, condition, severity, phase, incident_key,
             business_window, observed_at, disposition) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
    )
    .bind(id)
    .bind(rule_id)
    .bind(signal.condition)
    .bind(rule.severity)
    .bind(signal.phase)
    .bind(signal.incident_key)
    .bind(signal.window_start)
    .bind(signal.observed_at)
    .bind(disposition)
    .execute(&mut *tx)
    .await?;

    if let Some(kind) = kind {
        sqlx::query(
            "INSERT INTO app.notification_outbox (id, event_id, rule_id, kind, priority)
             VALUES ($1,$2,$3,$4,$5)",
        )
        .bind(Uuid::new_v4())
        .bind(id)
        .bind(rule_id)
        .bind(kind)
        .bind(if rule.severity == Severity::Urgent {
            "urgent"
        } else {
            "normal"
        })
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(RecordOutcome {
        id,
        disposition,
        duplicate: false,
    })
}
