mod ingest;

pub use ingest::{RecordError, record_signal};

use super::model::{Alert, ChannelTest, Delivery, Rule, RuleConfig, SuppressionReason};
use crate::database::Database;
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

#[derive(Debug)]
pub enum StoreError {
    Invalid,
    Conflict,
    Database(sqlx::Error),
}

impl From<sqlx::Error> for StoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

const RULE_COLUMNS: &str = "id::text AS id, revision, condition, severity, enabled,
    email_enabled, cooldown_seconds, reminder_seconds, min_occurrences";

// Rule edits and the worker's reservation must see a consistent set of summary members.
// This lock ends with the transaction, before the worker contacts SMTP.
pub(super) async fn lock_rule_updates(tx: &mut PgConnection) -> Result<(), sqlx::Error> {
    const RULE_UPDATE_LOCK: i64 = 6_758_026_211_706_146_535;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(RULE_UPDATE_LOCK)
        .execute(tx)
        .await?;
    Ok(())
}

pub(crate) async fn list_rules(
    db: &Database,
    limit: i64,
    after: Option<Uuid>,
) -> Result<Vec<Rule>, sqlx::Error> {
    let mut connection = db.connection().await?;
    sqlx::query_as(&format!(
        "SELECT {RULE_COLUMNS} FROM app.notification_rules
         WHERE $1::uuid IS NULL OR id > $1 ORDER BY app.notification_rules.id LIMIT $2"
    ))
    .bind(after)
    .bind(limit)
    .fetch_all(&mut *connection)
    .await
}

pub(crate) async fn get_rule(db: &Database, id: Uuid) -> Result<Option<Rule>, sqlx::Error> {
    let mut connection = db.connection().await?;
    sqlx::query_as(&format!(
        "SELECT {RULE_COLUMNS} FROM app.notification_rules WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(&mut *connection)
    .await
}

pub(crate) async fn put_rule(
    db: &Database,
    id: Uuid,
    revision: i64,
    config: RuleConfig,
) -> Result<Rule, StoreError> {
    if !config.is_valid() {
        return Err(StoreError::Invalid);
    }
    let mut connection = db.write_connection().await?;
    let mut tx = connection.begin().await?;
    let rule = if revision == 0 {
        sqlx::query_as::<_, Rule>(&format!(
            "INSERT INTO app.notification_rules (id, condition, severity, enabled, email_enabled,
                 cooldown_seconds, reminder_seconds, min_occurrences)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT (id) DO NOTHING
             RETURNING {RULE_COLUMNS}"
        ))
        .bind(id)
        .bind(config.condition)
        .bind(config.severity)
        .bind(config.enabled)
        .bind(config.email_enabled)
        .bind(config.cooldown_seconds)
        .bind(config.reminder_seconds)
        .bind(config.min_occurrences)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::Conflict)?
    } else {
        lock_rule_updates(&mut tx).await?;
        let old = sqlx::query_as::<_, RuleConfig>(
            "SELECT * FROM app.notification_rules WHERE id = $1 AND revision = $2 FOR UPDATE",
        )
        .bind(id)
        .bind(revision)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::Conflict)?;
        let reason = if old.condition != config.condition {
            Some(SuppressionReason::RuleChanged)
        } else if (old.enabled && !config.enabled) || (old.email_enabled && !config.email_enabled) {
            Some(SuppressionReason::RuleDisabled)
        } else {
            None
        };
        if let Some(reason) = reason {
            sqlx::query(
                "UPDATE app.notification_outbox AS delivery
                 SET status = CASE WHEN delivery.status = 'pending' THEN 'suppressed' ELSE delivery.status END,
                     suppression_reason = $2, updated_at = now()
                 WHERE delivery.rule_id = $1 AND (
                   delivery.status IN ('pending', 'sending') OR (
                     delivery.suppression_reason IS NULL AND NOT EXISTS (
                       SELECT FROM app.notification_outbox retry WHERE retry.replaces_id = delivery.id
                     ) AND (
                       delivery.status = 'suppressed' OR (
                         delivery.status = 'summarized' AND EXISTS (
                           SELECT FROM app.notification_outbox summary WHERE summary.id = delivery.summary_id
                             AND summary.status IN ('pending', 'sending', 'suppressed')
                         )
                       )
                     )
                   )
                 )",
            )
            .bind(id)
            .bind(reason)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE app.notification_incidents SET active = false, email_open = false,
                     consecutive_count = 0, next_reminder_at = NULL WHERE rule_id = $1",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query_as::<_, Rule>(&format!(
            "UPDATE app.notification_rules SET revision = revision + 1, condition = $2,
                 severity = $3, enabled = $4, email_enabled = $5, cooldown_seconds = $6,
                 reminder_seconds = $7, min_occurrences = $8 WHERE id = $1
             RETURNING {RULE_COLUMNS}"
        ))
        .bind(id)
        .bind(config.condition)
        .bind(config.severity)
        .bind(config.enabled)
        .bind(config.email_enabled)
        .bind(config.cooldown_seconds)
        .bind(config.reminder_seconds)
        .bind(config.min_occurrences)
        .fetch_one(&mut *tx)
        .await?
    };
    tx.commit().await?;
    Ok(rule)
}

const ALERT_COLUMNS: &str = "id::text AS id, sequence, rule_id::text AS rule_id, condition,
    severity, phase, observed_at, created_at, acknowledged_at, disposition, duplicate_count,
    last_duplicate_at";

const DELIVERY_AUDIT: &str = "COALESCE((
        SELECT jsonb_agg(to_jsonb(a) - 'outbox_id' ORDER BY a.attempt)
        FROM app.notification_attempts a WHERE a.outbox_id = d.id
    ), '[]'::jsonb) AS attempts,
    COALESCE((
        SELECT jsonb_agg(to_jsonb(f) - 'outbox_id' - 'sequence' ORDER BY f.sequence)
        FROM app.notification_deferrals f WHERE f.outbox_id = d.id
    ), '[]'::jsonb) AS deferrals";

async fn attach_deliveries(
    conn: &mut PgConnection,
    alerts: &mut [Alert],
) -> Result<(), sqlx::Error> {
    if alerts.is_empty() {
        return Ok(());
    }
    let ids = alerts
        .iter()
        .map(|alert| Uuid::parse_str(&alert.id).expect("PostgreSQL UUID text"))
        .collect::<Vec<_>>();
    let deliveries = sqlx::query_as::<_, (String, sqlx::types::Json<Delivery>)>(&format!(
        "SELECT delivery.event_id::text, to_jsonb(delivery) - 'event_id'
         FROM (
           SELECT d.event_id, d.id::text AS id, d.kind, d.status, d.created_at, d.available_at,
                  d.updated_at, d.last_outcome, d.defer_reason, d.suppression_reason,
                  d.summary_id::text AS summary_id, summary.status AS summary_status,
                  {DELIVERY_AUDIT}
           FROM app.notification_outbox d
           LEFT JOIN app.notification_outbox summary ON summary.id = d.summary_id
           WHERE d.event_id = ANY($1)
           ORDER BY d.created_at, d.id
         ) delivery"
    ))
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    for (id, delivery) in deliveries {
        if let Some(alert) = alerts.iter_mut().find(|alert| alert.id == id) {
            alert.deliveries.push(delivery.0);
        }
    }
    Ok(())
}

pub(crate) async fn list_alerts(
    db: &Database,
    limit: i64,
    before: Option<i64>,
    unacknowledged: bool,
) -> Result<Vec<Alert>, sqlx::Error> {
    let mut connection = db.connection().await?;
    let mut alerts = sqlx::query_as(&format!(
        "SELECT {ALERT_COLUMNS} FROM app.alert_events
         WHERE ($1::bigint IS NULL OR sequence < $1)
           AND (NOT $2 OR (acknowledged_at IS NULL AND disposition IN ('pending','in_app')))
         ORDER BY sequence DESC LIMIT $3"
    ))
    .bind(before)
    .bind(unacknowledged)
    .bind(limit)
    .fetch_all(&mut *connection)
    .await?;
    attach_deliveries(&mut connection, &mut alerts).await?;
    Ok(alerts)
}

pub(crate) async fn get_alert(db: &Database, id: Uuid) -> Result<Option<Alert>, sqlx::Error> {
    let mut connection = db.connection().await?;
    let mut alert = sqlx::query_as(&format!(
        "SELECT {ALERT_COLUMNS} FROM app.alert_events WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(&mut *connection)
    .await?;
    attach_deliveries(&mut connection, alert.as_mut_slice()).await?;
    Ok(alert)
}

pub(crate) async fn ack_alert(db: &Database, id: Uuid) -> Result<Option<Alert>, sqlx::Error> {
    let mut connection = db.connection().await?;
    let mut alert = sqlx::query_as(&format!(
        "UPDATE app.alert_events SET acknowledged_at = COALESCE(acknowledged_at, now())
         WHERE id = $1 RETURNING {ALERT_COLUMNS}"
    ))
    .bind(id)
    .fetch_optional(&mut *connection)
    .await?;
    attach_deliveries(&mut connection, alert.as_mut_slice()).await?;
    Ok(alert)
}

async fn fetch_test(conn: &mut PgConnection, id: Uuid) -> Result<Option<ChannelTest>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT d.test_key::text AS id, d.status, d.created_at, d.available_at, d.updated_at,
                d.last_outcome, d.defer_reason, {DELIVERY_AUDIT}
         FROM app.notification_outbox d WHERE d.test_key = $1"
    ))
    .bind(id)
    .fetch_optional(conn)
    .await
}

pub(crate) async fn enqueue_test(db: &Database, id: Uuid) -> Result<ChannelTest, sqlx::Error> {
    let mut connection = db.write_connection().await?;
    let mut tx = connection.begin().await?;
    sqlx::query(
        "INSERT INTO app.notification_outbox (id, kind, priority, test_key)
         VALUES ($1, 'test', 'normal', $2) ON CONFLICT (test_key) DO NOTHING",
    )
    .bind(Uuid::new_v4())
    .bind(id)
    .execute(&mut *tx)
    .await?;
    let view = fetch_test(&mut tx, id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    tx.commit().await?;
    Ok(view)
}

pub(crate) async fn get_test(db: &Database, id: Uuid) -> Result<Option<ChannelTest>, sqlx::Error> {
    let mut connection = db.connection().await?;
    fetch_test(&mut connection, id).await
}
