use super::{ABANDONED_AFTER, NotificationHealth, NotificationPolicy};
use crate::{
    database::Database,
    integrations::store::finish_smtp_test,
    notifications::{
        channel::{NotificationChannel, PRIMARY_REQUIRED, PreparedSnapshot, SmtpProfile},
        model::{AttemptOutcome, Condition, DeliveryKind},
        store::lock_rule_updates,
    },
};
use chrono::{DateTime, Utc};
use sqlx::{Acquire, PgConnection, Postgres, pool::PoolConnection};
use uuid::Uuid;

// One transport, one database-wide sender. This session lock spans reservation,
// SMTP and completion, with no SQL transaction held over the network request.
const SENDER_LOCK: i64 = 6_758_026_211_706_146_534;

const SMTP_ROUTE: &str = "
    SELECT version.smtp_snapshot_id AS active_snapshot,
           CASE WHEN version.version IS NOT NULL THEN version.smtp_snapshot_id IS NOT NULL
                ELSE NOT EXISTS (
                    SELECT FROM app.integration_audit audit
                    WHERE audit.integration_id = config.id AND audit.action = 'disabled'
                ) END AS delivery_enabled
    FROM (VALUES (1)) fallback(row)
    LEFT JOIN app.integration_configs config ON config.connector = 'smtp'
    LEFT JOIN app.integration_versions version
      ON version.integration_id = config.id AND version.state = 'active'";

const RETIRABLE_SUMMARIES: &str = "
    summary.status = 'pending' AND summary.kind = 'alert' AND EXISTS (
        SELECT FROM app.notification_outbox member
        WHERE member.summary_id = summary.id AND (
            member.suppression_reason IS NOT NULL OR (
                summary.available_at <= $1 AND (
                    app.notification_silence_overlaps(summary.rule_id, summary.event_id, $1, $1)
                    OR app.notification_silence_overlaps(member.rule_id, member.event_id, $1, $1)
                )
            )
        )
    )";

const REQUEUEABLE_ALERTS: &str = "
    FROM app.notification_outbox member
    LEFT JOIN app.notification_outbox summary ON summary.id = member.summary_id
    JOIN app.alert_events event ON event.id = member.event_id
    JOIN app.notification_rules rule ON rule.id = member.rule_id
    WHERE (member.status = 'suppressed' OR (member.status = 'summarized' AND summary.status = 'suppressed'))
      AND member.suppression_reason IS NULL AND rule.enabled AND rule.email_enabled
      AND rule.condition = event.condition
      AND NOT EXISTS (SELECT FROM app.notification_outbox retry WHERE retry.replaces_id = member.id)";

pub(super) struct Job {
    pub(super) id: Uuid,
    pub(super) kind: DeliveryKind,
    pub(super) condition: Option<Condition>,
    pub(super) count: u64,
    pub(super) candidate_profile: Option<SmtpProfile>,
    pub(super) snapshot: Option<PreparedSnapshot>,
    pub(super) integration_test_id: Option<Uuid>,
    attempt: i32,
}

#[derive(sqlx::FromRow)]
struct Pending {
    id: Uuid,
    kind: DeliveryKind,
    condition: Option<Condition>,
    priority: String,
    attempts: i32,
    digest_count: Option<i64>,
    was_deferred: bool,
    candidate_profile: Option<SmtpProfile>,
    snapshot_id: Option<Uuid>,
    integration_test_id: Option<Uuid>,
}

#[derive(sqlx::FromRow)]
struct Sending {
    id: Uuid,
    kind: DeliveryKind,
    attempts: i32,
    send_started_at: DateTime<Utc>,
    integration_test_id: Option<Uuid>,
}

pub(super) async fn valid_time_zone(db: &Database, name: &str) -> Result<bool, sqlx::Error> {
    let mut db = db.connection().await?;
    sqlx::query_scalar("SELECT EXISTS (SELECT FROM pg_timezone_names WHERE name = $1)")
        .bind(name)
        .fetch_one(&mut *db)
        .await
}

pub(super) enum SmtpTransport {
    Available {
        delivery_enabled: bool,
        active_snapshot: Option<PreparedSnapshot>,
    },
    Unavailable(&'static str),
}

#[derive(PartialEq, sqlx::FromRow)]
struct SmtpRoute {
    active_snapshot: Option<Uuid>,
    delivery_enabled: bool,
}

impl SmtpRoute {
    async fn read(db: &mut PgConnection) -> Result<Self, sqlx::Error> {
        sqlx::query_as(SMTP_ROUTE).fetch_one(db).await
    }
}

pub(super) async fn smtp_transport(
    db: &mut PgConnection,
    channel: &impl NotificationChannel,
) -> Result<SmtpTransport, sqlx::Error> {
    let route = SmtpRoute::read(db).await?;
    Ok(resolve_smtp_transport(route.active_snapshot, route.delivery_enabled, channel).await)
}

// The schema permits one SMTP integration, one pending test and one outbox row per test.
pub(super) async fn pending_candidate_snapshot(
    db: &Database,
) -> Result<Option<(Uuid, Uuid)>, sqlx::Error> {
    let mut connection = db.connection().await?;
    sqlx::query_as(
        "SELECT job.id, job.smtp_snapshot_id
         FROM app.integration_configs config
         JOIN app.integration_tests test ON test.integration_id = config.id
         JOIN app.notification_outbox job ON job.integration_test_id = test.id
         WHERE config.connector = 'smtp' AND test.status = 'pending'
           AND job.status IN ('pending', 'sending') AND job.smtp_snapshot_id IS NOT NULL",
    )
    .fetch_optional(&mut *connection)
    .await
}

async fn resolve_smtp_transport(
    snapshot_id: Option<Uuid>,
    delivery_enabled: bool,
    channel: &impl NotificationChannel,
) -> SmtpTransport {
    let active_snapshot = if let Some(id) = snapshot_id {
        let Some(snapshot) = channel.prepare_snapshot(id).await else {
            return SmtpTransport::Unavailable("active SMTP snapshot is unavailable");
        };
        Some(snapshot)
    } else {
        if delivery_enabled && !channel.primary_available() {
            return SmtpTransport::Unavailable(PRIMARY_REQUIRED.reason());
        }
        None
    };
    SmtpTransport::Available {
        delivery_enabled,
        active_snapshot,
    }
}

pub(super) async fn claim(
    db: &Database,
    policy: &NotificationPolicy,
    channel: &impl NotificationChannel,
    paused_candidate: Option<Uuid>,
    clock: impl Fn() -> DateTime<Utc>,
) -> Result<Option<(PoolConnection<Postgres>, Job)>, sqlx::Error> {
    let has_work = {
        let mut read_conn = db.connection().await?;
        sqlx::query_scalar::<_, bool>(&format!(
            "SELECT EXISTS (SELECT FROM app.notification_outbox job
             WHERE (status = 'pending' AND available_at <= $1
                    AND NOT app.notification_silence_overlaps(job.rule_id, job.event_id, $1, $1))
                 OR status = 'sending')
             OR EXISTS (SELECT 1 FROM app.notification_outbox summary WHERE {RETIRABLE_SUMMARIES})
             OR EXISTS (SELECT 1 {REQUEUEABLE_ALERTS})",
        ))
        .bind(clock())
        .fetch_one(&mut *read_conn)
        .await?
    };
    if !has_work {
        return Ok(None);
    }
    let mut lock = db.write_connection().await?;
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(SENDER_LOCK)
        .fetch_one(&mut *lock)
        .await?;
    if !locked {
        return Ok(None);
    }
    let mut tx = lock.begin().await?;
    lock_rule_updates(&mut tx).await?;
    let route = SmtpRoute::read(&mut tx).await?;
    let SmtpTransport::Available {
        delivery_enabled,
        active_snapshot,
    } = resolve_smtp_transport(route.active_snapshot, route.delivery_enabled, channel).await
    else {
        tx.rollback().await?;
        unlock_sender(&mut lock).await?;
        return Ok(None);
    };
    let now = clock();
    let snapshot_storage_available = channel.snapshot_storage_available();
    if let Some(active) = sqlx::query_as::<_, Sending>(
        "SELECT id, kind, attempts, send_started_at, integration_test_id FROM app.notification_outbox
         WHERE status = 'sending' FOR UPDATE",
    )
    .fetch_optional(&mut *tx)
    .await?
    {
        // A versioned test can spend one extra DB deadline capturing and pinning
        // the snapshot between reservation and transport start.
        let abandonment = if active.integration_test_id.is_some() {
            ABANDONED_AFTER + chrono::TimeDelta::seconds(5)
        } else {
            ABANDONED_AFTER
        };
        if active.send_started_at + abandonment > now {
            tx.rollback().await?;
            unlock_sender(&mut lock).await?;
            return Ok(None);
        }
        let job = Job {
            id: active.id,
            kind: active.kind,
            condition: None,
            attempt: active.attempts,
            count: 1,
            candidate_profile: None,
            snapshot: None,
            integration_test_id: active.integration_test_id,
        };
        settle(&mut tx, &job, AttemptOutcome::Interrupted, now).await?;
    }
    if snapshot_storage_available {
        sqlx::query(
            "UPDATE app.notification_outbox job SET status = 'suppressed', updated_at = $1
             FROM app.integration_tests test
             WHERE job.integration_test_id = test.id AND job.status = 'pending' AND test.status <> 'pending'",
        )
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    if delivery_enabled {
        replace_suppressed_summaries(&mut tx, now).await?;
        apply_delivery_policy(&mut tx, policy, now).await?;
    }
    apply_hourly_limits(
        &mut tx,
        policy,
        now,
        delivery_enabled,
        snapshot_storage_available,
        paused_candidate,
    )
    .await?;
    // The statement that locks the job also reads the current SMTP route. A
    // disable or rotation committed during preflight invalidates the prepared transport.
    let Some(pending) = sqlx::query_as::<_, Pending>(&format!(
        "WITH smtp AS ({SMTP_ROUTE})
         SELECT job.id, job.kind, event.condition, job.priority, job.attempts,
                job.digest_count, job.integration_test_id,
                (job.defer_reason IS NOT NULL OR
                    app.notification_silence_overlaps(job.rule_id, job.event_id, job.created_at, $1)
                ) AS was_deferred,
                candidate.settings->>'profile' AS candidate_profile, job.smtp_snapshot_id AS snapshot_id
         FROM app.notification_outbox job
         CROSS JOIN smtp
         LEFT JOIN app.alert_events event ON event.id = job.event_id
         LEFT JOIN app.integration_tests test ON test.id = job.integration_test_id
         LEFT JOIN app.integration_versions candidate
           ON candidate.integration_id = test.integration_id AND candidate.version = test.config_version
         WHERE job.status = 'pending' AND job.available_at <= $1
           AND NOT app.notification_silence_overlaps(job.rule_id, job.event_id, $1, $1)
           AND smtp.active_snapshot IS NOT DISTINCT FROM $2 AND smtp.delivery_enabled = $3
           AND ((job.integration_test_id IS NULL AND smtp.delivery_enabled)
                OR (test.status = 'pending' AND $4 AND job.id IS DISTINCT FROM $5))
           AND (job.kind NOT IN ('reminder', 'recovery') OR NOT EXISTS (
               SELECT FROM app.notification_outbox prior
               JOIN app.alert_events previous ON previous.id = prior.event_id
               LEFT JOIN app.notification_outbox summary ON summary.id = prior.summary_id
               WHERE prior.rule_id = job.rule_id AND prior.kind = 'alert'
                 AND previous.incident_key = event.incident_key
                 AND previous.condition = event.condition AND previous.sequence < event.sequence
                 AND (coalesce(summary.status, prior.status) IN ('pending', 'sending') OR (
                     (prior.status = 'suppressed' OR (prior.status = 'summarized' AND summary.status = 'suppressed'))
                     AND prior.suppression_reason IS NULL AND NOT EXISTS (
                         SELECT FROM app.notification_outbox retry WHERE retry.replaces_id = prior.id
                     )
                 ))
           ))
         ORDER BY job.priority DESC, job.available_at, job.created_at, job.id
         LIMIT 1 FOR UPDATE OF job SKIP LOCKED"
    ))
    .bind(now)
    .bind(route.active_snapshot)
    .bind(delivery_enabled)
    .bind(snapshot_storage_available)
    .bind(paused_candidate)
    .fetch_optional(&mut *tx)
    .await?
    else {
        // An empty reservation may still have provisional recovery or policy
        // updates. Only publish those if the route they were computed for still applies.
        if SmtpRoute::read(&mut tx).await? == route {
            tx.commit().await?;
        } else {
            tx.rollback().await?;
        }
        unlock_sender(&mut lock).await?;
        return Ok(None);
    };
    let snapshot = if pending.integration_test_id.is_some() {
        pending.snapshot_id.map(PreparedSnapshot::ById)
    } else {
        active_snapshot
    };
    let count = match pending.digest_count {
        Some(count) => count,
        None => summarize(&mut tx, &pending, now).await?,
    };
    let attempt = pending.attempts + 1;
    sqlx::query(
        "UPDATE app.notification_outbox
         SET status = 'sending', attempts = $2, send_started_at = $3, updated_at = $3
         WHERE id = $1",
    )
    .bind(pending.id)
    .bind(attempt)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO app.notification_attempts (outbox_id, attempt, started_at, outcome)
         VALUES ($1, $2, $3, 'sending')",
    )
    .bind(pending.id)
    .bind(attempt)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    if pending.integration_test_id.is_none() {
        sqlx::query("UPDATE app.notification_health SET last_attempt_at = $1 WHERE singleton")
            .bind(now)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(Some((
        lock,
        Job {
            id: pending.id,
            kind: pending.kind,
            condition: pending.condition,
            count: count as u64,
            attempt,
            candidate_profile: pending.candidate_profile,
            snapshot,
            integration_test_id: pending.integration_test_id,
        },
    )))
}

async fn replace_suppressed_summaries(
    db: &mut PgConnection,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    // Retire the old summary if it can no longer send with its original membership.
    // Manually silenced alerts keep their replacements pending until eligible.
    sqlx::query(&format!(
        "UPDATE app.notification_outbox summary SET status = 'suppressed', updated_at = $1
         WHERE {RETIRABLE_SUMMARIES}"
    ))
    .bind(now)
    .execute(&mut *db)
    .await?;

    // Preserve the membership and attempts under the old Message-ID. Each
    // eligible alert can join a new summary using a fresh outbox entry.
    sqlx::query(&format!(
        "INSERT INTO app.notification_outbox (id, event_id, rule_id, kind, priority,
             created_at, available_at, updated_at, defer_reason, replaces_id)
         SELECT gen_random_uuid(), member.event_id, member.rule_id, member.kind, member.priority,
                $1, greatest(member.available_at, summary.available_at), $1, member.defer_reason, member.id
         {REQUEUEABLE_ALERTS}
         FOR SHARE OF rule SKIP LOCKED
         ON CONFLICT (replaces_id) DO NOTHING"
    ))
    .bind(now)
    .execute(db)
    .await?;
    Ok(())
}

async fn summarize(
    db: &mut PgConnection,
    pending: &Pending,
    now: DateTime<Utc>,
) -> Result<i64, sqlx::Error> {
    if pending.kind != DeliveryKind::Alert {
        return Ok(1);
    }
    if pending.attempts == 0 && pending.was_deferred {
        sqlx::query(
            "UPDATE app.notification_outbox SET status = 'summarized', summary_id = $1, updated_at = $2
             WHERE id IN (
                 SELECT id FROM app.notification_outbox
                 WHERE id <> $1 AND status = 'pending' AND attempts = 0 AND available_at <= $2
                   AND priority = $3 AND kind = 'alert'
                   AND NOT app.notification_silence_overlaps(rule_id, event_id, $2, $2)
                   AND (defer_reason IS NOT NULL OR
                       app.notification_silence_overlaps(rule_id, event_id, created_at, $2))
                 ORDER BY available_at, created_at, id LIMIT 99 FOR UPDATE SKIP LOCKED
             )",
        )
        .bind(pending.id)
        .bind(now)
        .bind(&pending.priority)
        .execute(&mut *db)
        .await?;
    }
    // Once the first attempt is reserved, membership remains fixed through retries.
    sqlx::query_scalar("SELECT 1 + count(*) FROM app.notification_outbox WHERE summary_id = $1")
        .bind(pending.id)
        .fetch_one(db)
        .await
}

async fn apply_delivery_policy(
    db: &mut PgConnection,
    policy: &NotificationPolicy,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    if let Some(until) = quiet_until(db, policy, now).await? {
        sqlx::query(
            "WITH deferred AS (
                 UPDATE app.notification_outbox
                 SET available_at = $2, updated_at = $1, defer_reason = 'quiet_hours'
                 WHERE status = 'pending' AND available_at <= $1 AND kind <> 'test'
                   AND (priority = 'normal' OR NOT $3)
                 RETURNING id
             )
             INSERT INTO app.notification_deferrals (outbox_id, reason, deferred_at, available_at)
             SELECT id, 'quiet_hours', $1, $2 FROM deferred",
        )
        .bind(now)
        .bind(until)
        .bind(policy.urgent_bypasses_quiet)
        .execute(&mut *db)
        .await?;
    }
    sqlx::query(
        "WITH deadlines AS (
             SELECT job.id, history.started_at +
                 coalesce(rule.cooldown_seconds, $2) * interval '1 second' AS until
             FROM app.notification_outbox job
             JOIN app.notification_rules rule ON rule.id = job.rule_id
             CROSS JOIN LATERAL (
                 SELECT max(coalesce(summary.send_started_at, earlier.send_started_at)) AS started_at
                 FROM app.notification_outbox earlier
                 LEFT JOIN app.notification_outbox summary ON summary.id = earlier.summary_id
                 WHERE earlier.rule_id = rule.id
             ) history
             WHERE job.status = 'pending' AND job.attempts = 0 AND job.available_at <= $1
               AND job.kind IN ('alert', 'reminder')
         ), deferred AS (
             UPDATE app.notification_outbox job
             SET available_at = deadlines.until, updated_at = $1, defer_reason = 'cooldown'
             FROM deadlines WHERE job.id = deadlines.id AND deadlines.until > $1
               AND job.status = 'pending'
             RETURNING job.id, job.available_at
         )
         INSERT INTO app.notification_deferrals (outbox_id, reason, deferred_at, available_at)
         SELECT id, 'cooldown', $1, available_at FROM deferred",
    )
    .bind(now)
    .bind(policy.cooldown_seconds)
    .execute(&mut *db)
    .await?;

    Ok(())
}

async fn apply_hourly_limits(
    db: &mut PgConnection,
    policy: &NotificationPolicy,
    now: DateTime<Utc>,
    delivery_enabled: bool,
    snapshot_storage_available: bool,
    paused_candidate: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    for (priority, limit) in [
        ("normal", policy.normal_hourly_limit),
        ("urgent", policy.urgent_hourly_limit),
    ] {
        // Counting reservations covers crashes, unverifiable SMTP timeouts and
        // explicit test messages. The lock serializes reservations across replicas.
        sqlx::query(
            "WITH deadline AS (
                 SELECT attempt.started_at + interval '1 hour' AS until
                 FROM app.notification_attempts attempt
                 JOIN app.notification_outbox job ON job.id = attempt.outbox_id
                 WHERE job.priority = $2 AND attempt.started_at > $1::timestamptz - interval '1 hour'
                 ORDER BY attempt.started_at DESC OFFSET $3 LIMIT 1
             ), deferred AS (
                 UPDATE app.notification_outbox job
                 SET available_at = deadline.until, updated_at = $1, defer_reason = 'rate_limit'
                 FROM deadline WHERE job.status = 'pending' AND job.available_at <= $1 AND job.priority = $2
                   AND CASE WHEN job.integration_test_id IS NULL THEN $4
                            ELSE $5 AND job.id IS DISTINCT FROM $6 END
                 RETURNING job.id, job.available_at
             )
             INSERT INTO app.notification_deferrals (outbox_id, reason, deferred_at, available_at)
             SELECT id, 'rate_limit', $1, available_at FROM deferred",
        )
        .bind(now)
        .bind(priority)
        .bind(limit - 1)
        .bind(delivery_enabled)
        .bind(snapshot_storage_available)
        .bind(paused_candidate)
        .execute(&mut *db)
        .await?;
    }
    Ok(())
}

async fn quiet_until(
    db: &mut PgConnection,
    policy: &NotificationPolicy,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    if policy.quiet_start == policy.quiet_end {
        return Ok(None);
    }
    // Compare instants, not only the displayed wall-clock hour, so a repeated
    // time during a fall-back cannot open and close the quiet interval twice.
    sqlx::query_scalar(
        "WITH zone AS (SELECT timezone($1, $2::timestamptz)::date AS day),
         intervals AS (
             SELECT timezone($1, start_day + $3::time) AS quiet_start,
                    timezone($1, (start_day + CASE WHEN $3 > $4 THEN 1 ELSE 0 END) + $4::time) AS quiet_end
             FROM zone CROSS JOIN LATERAL (VALUES (day - 1), (day)) AS starts(start_day)
         )
         SELECT quiet_end FROM intervals WHERE quiet_start <= $2 AND $2 < quiet_end
         ORDER BY quiet_end DESC LIMIT 1",
    )
    .bind(&policy.time_zone)
    .bind(now)
    .bind(policy.quiet_start)
    .bind(policy.quiet_end)
    .fetch_optional(db)
    .await
}

/// Persist the capture before sending. The caller holds the sender advisory lock.
pub(super) async fn pin_snapshot(
    db: &mut PgConnection,
    job_id: Uuid,
    snapshot_id: Uuid,
) -> Result<(), sqlx::Error> {
    let count = sqlx::query(
        "UPDATE app.notification_outbox SET smtp_snapshot_id = $2
         WHERE id = $1 AND status = 'sending' AND integration_test_id IS NOT NULL
           AND smtp_snapshot_id IS NULL",
    )
    .bind(job_id)
    .bind(snapshot_id)
    .execute(db)
    .await?
    .rows_affected();
    if count == 1 {
        Ok(())
    } else {
        Err(sqlx::Error::Protocol(
            "SMTP snapshot could not be pinned".into(),
        ))
    }
}

pub(super) async fn finish(
    db: &mut PgConnection,
    job: &Job,
    outcome: AttemptOutcome,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    let mut tx = db.begin().await?;
    settle(&mut tx, job, outcome, now).await?;
    tx.commit().await?;
    unlock_sender(db).await
}

async fn unlock_sender(db: &mut PgConnection) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(SENDER_LOCK)
        .execute(db)
        .await?;
    Ok(())
}

async fn settle(
    db: &mut PgConnection,
    job: &Job,
    outcome: AttemptOutcome,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    let next_attempt = outcome.next_attempt(job.attempt).map(|delay| now + delay);
    let status = match outcome {
        AttemptOutcome::Accepted => "accepted",
        _ if next_attempt.is_some() => "pending",
        _ => "failed",
    };
    sqlx::query(
        "UPDATE app.notification_attempts SET outcome = $3, finished_at = greatest(started_at, $4)
         WHERE outbox_id = $1 AND attempt = $2 AND outcome = 'sending'",
    )
    .bind(job.id)
    .bind(job.attempt)
    .bind(outcome)
    .bind(now)
    .execute(&mut *db)
    .await?;
    sqlx::query(
        "UPDATE app.notification_outbox SET status = CASE
             WHEN $2 = 'pending' AND suppression_reason IS NOT NULL THEN 'suppressed' ELSE $2 END,
             last_outcome = $3,
             updated_at = $4, available_at = coalesce($5, available_at)
         WHERE id = $1 AND status = 'sending'",
    )
    .bind(job.id)
    .bind(status)
    .bind(outcome)
    .bind(now)
    .bind(next_attempt)
    .execute(&mut *db)
    .await?;
    if let Some(test_id) = job.integration_test_id {
        if next_attempt.is_none() {
            let id = sqlx::query_scalar(
                "SELECT integration_id FROM app.integration_tests WHERE id = $1",
            )
            .bind(test_id)
            .fetch_one(&mut *db)
            .await?;
            finish_smtp_test(db, id, test_id, outcome).await?;
        }
        Ok(())
    } else {
        track_health(db, outcome, now).await
    }
}

async fn track_health(
    db: &mut PgConnection,
    outcome: AttemptOutcome,
    now: DateTime<Utc>,
) -> Result<(), sqlx::Error> {
    let failure_count: i64 = sqlx::query_scalar(
        "SELECT failure_count FROM app.notification_health WHERE singleton FOR UPDATE",
    )
    .fetch_one(&mut *db)
    .await?;
    if outcome != AttemptOutcome::Accepted {
        sqlx::query(
            "UPDATE app.notification_health SET state = 'degraded', last_outcome = $1,
                 failure_since = coalesce(failure_since, $2), failure_count = failure_count + 1
             WHERE singleton",
        )
        .bind(outcome)
        .bind(now)
        .execute(&mut *db)
        .await?;
        if failure_count == 0 {
            health_event(db, "triggered", now).await?;
        }
        return Ok(());
    }
    sqlx::query(
        "UPDATE app.notification_health SET state = 'healthy', last_outcome = 'accepted',
             failure_since = NULL, failure_count = 0, last_accepted_at = $1 WHERE singleton",
    )
    .bind(now)
    .execute(&mut *db)
    .await?;
    if failure_count > 0 {
        // An older digest can confirm recovery from a new failure streak. Its
        // payload stays unchanged, so this streak always gets its own digest.
        let event_id = health_event(db, "recovered", now).await?;
        sqlx::query(
            "INSERT INTO app.notification_outbox
                 (id, event_id, kind, priority, created_at, available_at, updated_at, digest_count)
             VALUES ($1, $2, 'outage_digest', 'normal', $3, $3, $3, $4)",
        )
        .bind(Uuid::new_v4())
        .bind(event_id)
        .bind(now)
        .bind(failure_count)
        .execute(&mut *db)
        .await?;
    }
    Ok(())
}

async fn health_event(
    db: &mut PgConnection,
    phase: &str,
    now: DateTime<Utc>,
) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO app.alert_events
             (id, condition, severity, phase, observed_at, created_at, disposition)
         VALUES ($1, 'smtp_unavailable', 'important', $2, $3, $3, 'in_app')",
    )
    .bind(id)
    .bind(phase)
    .bind(now)
    .execute(db)
    .await?;
    Ok(id)
}

pub(super) async fn health(db: &Database) -> Result<NotificationHealth, sqlx::Error> {
    let mut db = db.connection().await?;
    sqlx::query_as(
        "SELECT state, failure_since, last_attempt_at, last_accepted_at, last_outcome
         FROM app.notification_health WHERE singleton",
    )
    .fetch_one(&mut *db)
    .await
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use crate::notifications::channel::EmailChannels;
    use std::{fs, os::unix::fs::DirBuilderExt};

    #[tokio::test]
    async fn active_smtp_route_requires_and_loads_the_bound_snapshot() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let public = root.path().join("static");
        fs::create_dir(&public).unwrap();
        let snapshots = root.path().join("snapshots");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&snapshots)
            .unwrap();
        let primary = |key: &str| match key {
            "WEB_SMTP_HOST" => Some("127.0.0.1".into()),
            "WEB_SMTP_FROM" => Some("sender@example.invalid".into()),
            "WEB_SMTP_TO" => Some("receiver@example.invalid".into()),
            _ => None,
        };
        let storage = |key: &str| match key {
            "WEB_STATIC_ROOT" => Some(public.to_str().unwrap().into()),
            "WEB_SMTP_SNAPSHOT_DIR" => Some(snapshots.to_str().unwrap().into()),
            _ => None,
        };
        let original =
            EmailChannels::for_notifier(|key| primary(key).or_else(|| storage(key))).unwrap();
        let snapshot_id = original
            .capture_profile(SmtpProfile::Primary)
            .await
            .unwrap();

        let without_storage = EmailChannels::for_notifier(primary).unwrap();
        assert!(matches!(
            resolve_smtp_transport(Some(snapshot_id), true, &without_storage).await,
            SmtpTransport::Unavailable("active SMTP snapshot is unavailable")
        ));
        let restarted = EmailChannels::for_notifier(storage).unwrap();
        assert!(matches!(
            resolve_smtp_transport(None, true, &restarted).await,
            SmtpTransport::Unavailable("a usable SMTP primary profile is required")
        ));
        assert!(matches!(
            resolve_smtp_transport(Some(snapshot_id), true, &restarted).await,
            SmtpTransport::Available {
                active_snapshot: Some(PreparedSnapshot::Loaded(_)),
                delivery_enabled: true,
            }
        ));
        fs::write(
            snapshots.join(format!("{snapshot_id}.smtp")),
            b"synthetic invalid capture",
        )
        .unwrap();
        assert!(matches!(
            resolve_smtp_transport(Some(snapshot_id), true, &original).await,
            SmtpTransport::Unavailable("active SMTP snapshot is unavailable")
        ));
    }
}
