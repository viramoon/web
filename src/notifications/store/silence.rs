use super::{StoreError, lock_rule_updates};
use crate::database::Database;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Connection, PgConnection};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Clone, Copy)]
pub(crate) enum Target {
    Rule(Uuid),
    Alert(Uuid),
}

impl Target {
    fn predicates(self) -> (Option<Uuid>, Option<Uuid>) {
        match self {
            Self::Rule(id) => (Some(id), None),
            Self::Alert(id) => (None, Some(id)),
        }
    }

    async fn exists(self, conn: &mut PgConnection) -> Result<bool, sqlx::Error> {
        let (table, id) = match self {
            Self::Rule(id) => ("app.notification_rules", id),
            Self::Alert(id) => ("app.alert_events", id),
        };
        sqlx::query_scalar(&format!(
            "SELECT EXISTS (SELECT FROM {table} WHERE id = $1)"
        ))
        .bind(id)
        .fetch_one(conn)
        .await
    }
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SilenceRequest {
    /// Future deadline in RFC 3339 format.
    until: DateTime<Utc>,
    /// Short, single-line operational reason.
    #[schema(min_length = 1, max_length = 256)]
    reason: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SilenceStatus {
    /// Whether the latest silence applies at the time of the response.
    active: bool,
    /// Latest silence for this exact target, including released and expired records.
    silence: Option<Silence>,
}

#[derive(Serialize, ToSchema, sqlx::FromRow)]
struct Silence {
    #[serde(skip_serializing)]
    #[schema(ignore)]
    sequence: i64,
    reason: String,
    started_by: String,
    started_at: DateTime<Utc>,
    until: DateTime<Utc>,
    ended_by: Option<String>,
    ended_at: Option<DateTime<Utc>>,
    end_action: Option<SilenceEndAction>,
}

#[derive(Clone, Copy, Serialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
enum SilenceEndAction {
    Released,
    Replaced,
}

const COLUMNS: &str =
    "sequence, reason, started_by, started_at, until_at AS until, ended_by, ended_at, end_action";
// Bearer and browser sessions both authenticate the same single principal.
const ACTOR: &str = "admin";

async fn latest(
    conn: &mut PgConnection,
    target: Target,
    now: DateTime<Utc>,
) -> Result<SilenceStatus, sqlx::Error> {
    let (rule, alert) = target.predicates();
    let silence = sqlx::query_as::<_, Silence>(&format!(
        "SELECT {COLUMNS} FROM app.notification_silences
         WHERE rule_id = $1 OR alert_id = $2 ORDER BY sequence DESC LIMIT 1"
    ))
    .bind(rule)
    .bind(alert)
    .fetch_optional(conn)
    .await?;
    let active = silence
        .as_ref()
        .is_some_and(|silence| silence.until > now && silence.ended_at.is_none());
    Ok(SilenceStatus { active, silence })
}

async fn db_now(conn: &mut PgConnection) -> Result<DateTime<Utc>, sqlx::Error> {
    sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(conn)
        .await
}

pub(crate) async fn get(
    db: &Database,
    target: Target,
) -> Result<Option<SilenceStatus>, sqlx::Error> {
    let mut conn = db.connection().await?;
    if !target.exists(&mut conn).await? {
        return Ok(None);
    }
    let now = db_now(&mut conn).await?;
    Ok(Some(latest(&mut conn, target, now).await?))
}

pub(crate) async fn change(
    db: &Database,
    target: Target,
    request: Option<SilenceRequest>,
) -> Result<Option<SilenceStatus>, StoreError> {
    let mut conn = db.write_connection().await?;
    let mut tx = conn.begin().await?;
    // The same transaction lock guards rule edits and all SMTP reservations.
    lock_rule_updates(&mut tx).await?;
    if !target.exists(&mut tx).await? {
        return Ok(None);
    }
    let now = db_now(&mut tx).await?;
    let current = latest(&mut tx, target, now).await?;
    let request = request
        .map(|req| {
            let reason = req.reason.trim();
            let until = DateTime::from_timestamp_micros(req.until.timestamp_micros())
                .ok_or(StoreError::Invalid)?;
            if until <= now
                || !(1..=256).contains(&reason.chars().count())
                || req
                    .reason
                    .chars()
                    .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}'))
            {
                return Err(StoreError::Invalid);
            }
            Ok((until, reason.to_owned()))
        })
        .transpose()?;
    if let Some(active) = current.silence.as_ref().filter(|_| current.active) {
        if request
            .as_ref()
            .is_some_and(|(until, reason)| active.until == *until && active.reason == *reason)
        {
            tx.commit().await?;
            return Ok(Some(current));
        }
        sqlx::query(
            "UPDATE app.notification_silences SET ended_by = $2, ended_at = $3, end_action = $4
             WHERE sequence = $1",
        )
        .bind(active.sequence)
        .bind(ACTOR)
        .bind(now)
        .bind(if request.is_some() {
            SilenceEndAction::Replaced
        } else {
            SilenceEndAction::Released
        })
        .execute(&mut *tx)
        .await?;
    }
    if let Some((until, reason)) = request {
        let (rule, alert) = target.predicates();
        sqlx::query(
            "INSERT INTO app.notification_silences (rule_id, alert_id, reason, started_by, started_at, until_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(rule)
        .bind(alert)
        .bind(reason)
        .bind(ACTOR)
        .bind(now)
        .bind(until)
        .execute(&mut *tx)
        .await?;
    }
    let result = latest(&mut tx, target, now).await?;
    tx.commit().await?;
    Ok(Some(result))
}
