use super::{Settings, TestEvidence};
use crate::{database::Database, notifications::model::AttemptOutcome};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::{Connection, FromRow, PgConnection, types::Json};
use std::future::Future;
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Clone, Serialize, ToSchema)]
pub(crate) struct Integration {
    #[schema(format = "uuid")]
    pub id: String,
    pub connector: String,
    pub revision: i64,
    pub latest: VersionView,
    pub active: Option<VersionView>,
    pub rollback_versions: Vec<i64>,
    #[schema(format = "uuid")]
    pub last_test_id: Option<String>,
}

#[derive(Clone, Serialize, ToSchema)]
pub(crate) struct VersionView {
    pub version: i64,
    pub state: VersionState,
    pub settings: Settings,
    pub credential: Option<CredentialView>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub(crate) enum VersionState {
    Draft,
    Candidate,
    Active,
    Disabled,
}

#[derive(Clone, Serialize, ToSchema)]
pub(crate) struct CredentialView {
    pub version: i64,
    pub mask: String,
    pub rotated_at: DateTime<Utc>,
}

pub(crate) struct CredentialRef {
    pub reference: Uuid,
    pub mask: String,
    pub format: String,
}

pub(crate) enum Change {
    Settings(Value),
    RevokeCredential,
    Disable,
}

pub(crate) struct TestInput {
    pub test: TestView,
    pub connector: String,
    pub settings: Value,
    pub credential: Option<CredentialRef>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct TestView {
    #[schema(format = "uuid")]
    pub id: String,
    #[schema(format = "uuid")]
    pub integration_id: String,
    pub config_version: i64,
    pub status: TestStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub evidence: Option<TestEvidence>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub(crate) enum TestStatus {
    Pending,
    Passed,
    Failed,
    Unavailable,
    Superseded,
}

pub(crate) enum Outcome {
    Passed,
    Failed,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Error {
    Missing,
    Conflict,
    Unavailable,
    CredentialUnavailable,
    Internal,
}

impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        let kind = match error {
            sqlx::Error::Io(_)
            | sqlx::Error::Tls(_)
            | sqlx::Error::PoolClosed
            | sqlx::Error::PoolTimedOut => Self::Unavailable,
            sqlx::Error::Database(error) if error.code().as_deref() == Some("40001") => {
                Self::Conflict
            }
            sqlx::Error::Database(error)
                if error.code().is_some_and(|code| {
                    code.starts_with("08")
                        || matches!(code.as_ref(), "42P01" | "57P01" | "57P02" | "57P03")
                }) =>
            {
                Self::Unavailable
            }
            _ => Self::Internal,
        };
        tracing::warn!(?kind, "integration persistence error");
        kind
    }
}

#[derive(FromRow)]
struct VisibleVersion {
    id: Uuid,
    connector: String,
    revision: i64,
    latest_version: i64,
    version: i64,
    state: VersionState,
    settings: Value,
    credential_version: Option<i64>,
    credential_mask: Option<String>,
    credential_rotated_at: Option<DateTime<Utc>>,
    last_test_id: Option<Uuid>,
    rollback_versions: Vec<i64>,
}

async fn fetch_integrations(
    connection: &mut PgConnection,
    id: Option<Uuid>,
) -> Result<Vec<Integration>, Error> {
    let rows = sqlx::query_as::<_, VisibleVersion>(
        "SELECT i.id, i.connector, i.revision, i.latest_version,
                v.version, v.state, v.settings,
                c.version AS credential_version, c.mask AS credential_mask,
                c.rotated_at AS credential_rotated_at, a.test_id AS last_test_id,
                ARRAY(
                    SELECT history.version FROM app.integration_versions history
                    LEFT JOIN app.integration_credentials old ON old.id = history.credential_id
                    WHERE history.integration_id = i.id AND history.state = 'disabled'
                      AND history.activated_at IS NOT NULL AND old.revoked_at IS NULL
                      AND (i.connector <> 'smtp' OR history.smtp_snapshot_id IS NOT NULL)
                    ORDER BY history.version DESC
                ) AS rollback_versions
         FROM app.integration_configs i
         JOIN app.integration_versions v ON v.integration_id = i.id
             AND (v.version = i.latest_version OR v.state = 'active')
         LEFT JOIN app.integration_credentials c ON c.id = v.credential_id
             AND c.revoked_at IS NULL
         LEFT JOIN LATERAL (
             SELECT test_id FROM app.integration_audit
             WHERE integration_id = i.id AND action = 'test_started'
             ORDER BY sequence DESC LIMIT 1
         ) a ON true
         WHERE ($1::uuid IS NULL OR i.id = $1)
         ORDER BY i.id, v.version DESC",
    )
    .bind(id)
    .fetch_all(connection)
    .await?;
    let mut integrations: Vec<Integration> = Vec::new();
    for row in rows {
        let credential = row
            .credential_version
            .zip(row.credential_mask)
            .zip(row.credential_rotated_at)
            .map(|((version, mask), rotated_at)| CredentialView {
                version,
                mask,
                rotated_at,
            });
        let version = VersionView {
            version: row.version,
            state: row.state,
            settings: serde_json::from_value(row.settings).map_err(|_| Error::Internal)?,
            credential,
        };
        if row.version == row.latest_version {
            integrations.push(Integration {
                id: row.id.to_string(),
                connector: row.connector,
                revision: row.revision,
                active: (row.state == VersionState::Active).then(|| version.clone()),
                latest: version,
                last_test_id: row.last_test_id.map(|id| id.to_string()),
                rollback_versions: row.rollback_versions,
            });
        } else {
            let integration = integrations.last_mut().ok_or(Error::Internal)?;
            integration.active = Some(version);
        }
    }
    Ok(integrations)
}

async fn fetch_integration(connection: &mut PgConnection, id: Uuid) -> Result<Integration, Error> {
    fetch_integrations(connection, Some(id))
        .await?
        .pop()
        .ok_or(Error::Missing)
}

pub(crate) async fn list(db: &Database) -> Result<Vec<Integration>, Error> {
    let mut connection = db.connection().await?;
    fetch_integrations(&mut connection, None).await
}

pub(crate) async fn get(db: &Database, id: Uuid) -> Result<Integration, Error> {
    let mut connection = db.connection().await?;
    fetch_integration(&mut connection, id).await
}

pub(crate) async fn create(
    db: &Database,
    connector: &str,
    settings: Value,
) -> Result<Integration, Error> {
    let id = Uuid::new_v4();
    let mut connection = db.write_connection().await?;
    let mut tx = connection.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO app.integration_configs (id, connector) VALUES ($1, $2)
         ON CONFLICT (connector) WHERE connector = 'smtp' DO NOTHING",
    )
    .bind(id)
    .bind(connector)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        return Err(Error::Conflict);
    }
    sqlx::query(
        "INSERT INTO app.integration_versions (integration_id, version, state, settings)
         VALUES ($1, 1, 'draft', $2)",
    )
    .bind(id)
    .bind(settings)
    .execute(&mut *tx)
    .await?;
    audit(&mut tx, id, 1, "created", None).await?;
    let result = fetch_integration(&mut tx, id).await?;
    tx.commit().await?;
    Ok(result)
}

#[derive(FromRow)]
struct LockedIntegration {
    connector: String,
    latest_version: i64,
    state: VersionState,
    settings: Value,
    credential_id: Option<Uuid>,
    credential_format: Option<String>,
    credential_mask: Option<String>,
}

async fn lock(
    connection: &mut PgConnection,
    id: Uuid,
    revision: Option<i64>,
) -> Result<LockedIntegration, Error> {
    // Lock the configuration before reading the version. PostgreSQL may wait for a
    // prior writer, and a separate statement then sees that writer's committed version.
    let current_revision = sqlx::query_scalar::<_, i64>(
        "SELECT revision FROM app.integration_configs WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(Error::Missing)?;
    if revision.is_some_and(|expected| expected != current_revision) {
        return Err(Error::Conflict);
    }
    Ok(sqlx::query_as::<_, LockedIntegration>(
        "SELECT i.connector, i.latest_version, v.state, v.settings,
                c.id AS credential_id, c.format AS credential_format, c.mask AS credential_mask
         FROM app.integration_configs i
         JOIN app.integration_versions v ON v.integration_id = i.id AND v.version = i.latest_version
         LEFT JOIN app.integration_credentials c ON c.id = v.credential_id AND c.revoked_at IS NULL
         WHERE i.id = $1",
    )
    .bind(id)
    .fetch_one(connection)
    .await?)
}

pub(crate) async fn change(
    db: &Database,
    id: Uuid,
    revision: i64,
    change: Change,
) -> Result<Integration, Error> {
    let mut connection = db.write_connection().await?;
    let mut tx = connection.begin().await?;
    let current = lock(&mut tx, id, Some(revision)).await?;
    let (settings, credential, state, action) = match change {
        Change::Settings(settings) => (
            settings,
            current.credential_id,
            VersionState::Draft,
            "settings_changed",
        ),
        Change::RevokeCredential => {
            sqlx::query(
                "UPDATE app.integration_credentials SET revoked_at = now()
                 WHERE integration_id = $1 AND revoked_at IS NULL",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            retire_active(&mut tx, id, true).await?;
            (
                current.settings,
                None,
                VersionState::Draft,
                "credential_revoked",
            )
        }
        Change::Disable => {
            retire_active(&mut tx, id, false).await?;
            (
                current.settings,
                current.credential_id,
                VersionState::Disabled,
                "disabled",
            )
        }
    };
    let version = current.latest_version + 1;
    stage_version(&mut tx, id, version, settings, credential, state, None).await?;
    audit(&mut tx, id, version, action, None).await?;
    let result = fetch_integration(&mut tx, id).await?;
    tx.commit().await?;
    Ok(result)
}

pub(crate) async fn set_credential(
    db: &Database,
    id: Uuid,
    revision: i64,
    credential: CredentialRef,
    seal: impl Future<Output = Result<(), Error>>,
) -> Result<Integration, Error> {
    let mut connection = db.write_connection().await?;
    let mut tx = connection.begin().await?;
    let current = lock(&mut tx, id, Some(revision)).await?;
    let version = current.latest_version + 1;
    // The file is published durably before it can be referenced by a committed row.
    // Cancellation or a failed DB commit may leave only an unreferenced ciphertext.
    seal.await?;
    sqlx::query(
        "INSERT INTO app.integration_credentials (id, integration_id, version, format, mask)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(credential.reference)
    .bind(id)
    .bind(version)
    .bind(credential.format)
    .bind(credential.mask)
    .execute(&mut *tx)
    .await?;
    stage_version(
        &mut tx,
        id,
        version,
        current.settings,
        Some(credential.reference),
        VersionState::Draft,
        None,
    )
    .await?;
    audit(&mut tx, id, version, "credential_staged", None).await?;
    let result = fetch_integration(&mut tx, id).await?;
    tx.commit().await?;
    Ok(result)
}

async fn retire_active(
    connection: &mut PgConnection,
    id: Uuid,
    credential_required: bool,
) -> Result<(), Error> {
    sqlx::query(
        "UPDATE app.integration_versions SET state = 'disabled'
         WHERE integration_id = $1 AND state = 'active' AND (NOT $2 OR credential_id IS NOT NULL)",
    )
    .bind(id)
    .bind(credential_required)
    .execute(connection)
    .await?;
    Ok(())
}

async fn stage_version(
    connection: &mut PgConnection,
    id: Uuid,
    version: i64,
    settings: Value,
    credential: Option<Uuid>,
    state: VersionState,
    smtp_snapshot: Option<Uuid>,
) -> Result<(), Error> {
    supersede_pending(connection, id).await?;
    sqlx::query(
        "UPDATE app.integration_versions SET state = 'disabled'
         WHERE integration_id = $1 AND version = $2 AND state IN ('draft', 'candidate')",
    )
    .bind(id)
    .bind(version - 1)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO app.integration_versions
            (integration_id, version, settings, credential_id, state, activated_at, smtp_snapshot_id)
         VALUES ($1, $2, $3, $4, $5, CASE WHEN $5 = 'active' THEN now() END, $6)",
    )
    .bind(id)
    .bind(version)
    .bind(settings)
    .bind(credential)
    .bind(state)
    .bind(smtp_snapshot)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "UPDATE app.integration_configs SET latest_version = $2, revision = revision + 1 WHERE id = $1",
    )
    .bind(id)
    .bind(version)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

#[derive(FromRow)]
struct TestRow {
    id: Uuid,
    integration_id: Uuid,
    config_version: i64,
    status: TestStatus,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    evidence: Option<Json<TestEvidence>>,
}

impl From<TestRow> for TestView {
    fn from(row: TestRow) -> Self {
        Self {
            id: row.id.to_string(),
            integration_id: row.integration_id.to_string(),
            config_version: row.config_version,
            status: row.status,
            started_at: row.started_at,
            finished_at: row.finished_at,
            evidence: row.evidence.map(|value| value.0),
        }
    }
}

pub(crate) async fn get_test(db: &Database, test_id: Uuid) -> Result<TestView, Error> {
    let mut connection = db.connection().await?;
    Ok(load_test(&mut connection, test_id).await?.into())
}

async fn load_test(connection: &mut PgConnection, test_id: Uuid) -> Result<TestRow, Error> {
    sqlx::query_as::<_, TestRow>(
        "SELECT id, integration_id, config_version, status, started_at, finished_at, evidence
         FROM app.integration_tests WHERE id = $1",
    )
    .bind(test_id)
    .fetch_optional(connection)
    .await?
    .ok_or(Error::Missing)
}

pub(crate) async fn begin_test(db: &Database, id: Uuid, revision: i64) -> Result<TestInput, Error> {
    let mut connection = db.write_connection().await?;
    let mut tx = connection.begin().await?;
    let current = lock(&mut tx, id, Some(revision)).await?;
    if !matches!(current.state, VersionState::Draft | VersionState::Candidate) {
        return Err(Error::Conflict);
    }
    supersede_pending(&mut tx, id).await?;
    sqlx::query(
        "UPDATE app.integration_versions SET state = 'candidate'
         WHERE integration_id = $1 AND version = $2",
    )
    .bind(id)
    .bind(current.latest_version)
    .execute(&mut *tx)
    .await?;
    let test_id = Uuid::new_v4();
    let test = sqlx::query_as::<_, TestRow>(
        "INSERT INTO app.integration_tests (id, integration_id, config_version, status)
         VALUES ($1, $2, $3, 'pending')
         RETURNING id, integration_id, config_version, status, started_at, finished_at, evidence",
    )
    .bind(test_id)
    .bind(id)
    .bind(current.latest_version)
    .fetch_one(&mut *tx)
    .await?;
    if current.connector == "smtp" {
        sqlx::query(
            "INSERT INTO app.notification_outbox (id, kind, priority, test_key, integration_test_id)
             VALUES ($1, 'test', 'normal', $2, $3)",
        )
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .bind(test_id)
        .execute(&mut *tx)
        .await?;
    }
    bump_revision(&mut tx, id).await?;
    audit(
        &mut tx,
        id,
        current.latest_version,
        "test_started",
        Some(test_id),
    )
    .await?;
    tx.commit().await?;
    Ok(TestInput {
        test: test.into(),
        connector: current.connector,
        settings: current.settings,
        credential: current
            .credential_id
            .zip(current.credential_mask)
            .zip(current.credential_format)
            .map(|((reference, mask), format)| CredentialRef {
                reference,
                mask,
                format,
            }),
    })
}

pub(crate) async fn finish_test(
    db: &Database,
    id: Uuid,
    test_id: Uuid,
    result: (Outcome, TestEvidence),
) -> Result<TestView, Error> {
    let mut connection = db.write_connection().await?;
    let mut tx = connection.begin().await?;
    let finished = settle_test(&mut tx, id, test_id, result).await?;
    tx.commit().await?;
    Ok(finished)
}

async fn settle_test(
    connection: &mut PgConnection,
    id: Uuid,
    test_id: Uuid,
    (outcome, evidence): (Outcome, TestEvidence),
) -> Result<TestView, Error> {
    let current = lock(connection, id, None).await?;
    let test = load_test(connection, test_id).await?;
    if test.integration_id != id {
        return Err(Error::Missing);
    }
    if test.status != TestStatus::Pending {
        return Ok(test.into());
    }
    if current.latest_version != test.config_version || current.state != VersionState::Candidate {
        return Err(Error::Conflict);
    }
    let (status, action) = match outcome {
        Outcome::Passed => (TestStatus::Passed, "test_passed"),
        Outcome::Failed => (TestStatus::Failed, "test_failed"),
        Outcome::Unavailable => (TestStatus::Unavailable, "test_unavailable"),
    };
    let finished = sqlx::query_as::<_, TestRow>(
        "UPDATE app.integration_tests SET status = $2, finished_at = now(), evidence = $3 WHERE id = $1
         RETURNING id, integration_id, config_version, status, started_at, finished_at, evidence",
    )
    .bind(test_id)
    .bind(status)
    .bind(Json(evidence))
    .fetch_one(&mut *connection)
    .await?;
    bump_revision(connection, id).await?;
    audit(connection, id, test.config_version, action, Some(test_id)).await?;
    Ok(finished.into())
}

pub(crate) async fn finish_smtp_test(
    connection: &mut PgConnection,
    id: Uuid,
    test_id: Uuid,
    attempt: AttemptOutcome,
) -> Result<(), sqlx::Error> {
    let outcome = match attempt {
        AttemptOutcome::Accepted => Outcome::Passed,
        AttemptOutcome::ConfigurationError => Outcome::Unavailable,
        _ => Outcome::Failed,
    };
    settle_test(
        connection,
        id,
        test_id,
        (outcome, TestEvidence::Smtp(attempt)),
    )
    .await
    .map_err(|_| sqlx::Error::Protocol("could not settle integration SMTP test".into()))?;
    Ok(())
}

fn can_apply(connector: &str, binance_enabled: bool) -> Result<(), Error> {
    if matches!(connector, "binance_spot" | "binance_usd_m") && !binance_enabled {
        return Err(Error::Conflict);
    }
    Ok(())
}

pub(crate) async fn activate(
    db: &Database,
    id: Uuid,
    revision: i64,
    test_id: Uuid,
    binance_enabled: bool,
) -> Result<Integration, Error> {
    let mut connection = db.write_connection().await?;
    let mut tx = connection.begin().await?;
    let current = lock(&mut tx, id, Some(revision)).await?;
    can_apply(&current.connector, binance_enabled)?;
    if current.state != VersionState::Candidate {
        return Err(Error::Conflict);
    }
    let (latest_id, status, snapshot): (Uuid, TestStatus, Option<Uuid>) = sqlx::query_as(
        "SELECT t.id, t.status, job.smtp_snapshot_id
         FROM app.integration_audit a JOIN app.integration_tests t ON t.id = a.test_id
         LEFT JOIN app.notification_outbox job ON job.integration_test_id = t.id AND job.status = 'accepted'
         WHERE a.integration_id = $1 AND a.config_version = $2 AND a.action = 'test_started'
         ORDER BY a.sequence DESC LIMIT 1",
    )
    .bind(id)
    .bind(current.latest_version)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Error::Conflict)?;
    if latest_id != test_id
        || status != TestStatus::Passed
        || (current.connector == "smtp" && snapshot.is_none())
    {
        return Err(Error::Conflict);
    }
    retire_active(&mut tx, id, false).await?;
    sqlx::query(
        "UPDATE app.integration_versions SET state = 'active', activated_at = now(), smtp_snapshot_id = $3
         WHERE integration_id = $1 AND version = $2",
    )
    .bind(id)
    .bind(current.latest_version)
    .bind(snapshot)
    .execute(&mut *tx)
    .await?;
    bump_revision(&mut tx, id).await?;
    audit(
        &mut tx,
        id,
        current.latest_version,
        "activated",
        Some(test_id),
    )
    .await?;
    let result = fetch_integration(&mut tx, id).await?;
    tx.commit().await?;
    Ok(result)
}

pub(crate) async fn rollback(
    db: &Database,
    id: Uuid,
    revision: i64,
    prior_version: i64,
    binance_enabled: bool,
) -> Result<Integration, Error> {
    let mut connection = db.write_connection().await?;
    let mut tx = connection.begin().await?;
    let current = lock(&mut tx, id, Some(revision)).await?;
    can_apply(&current.connector, binance_enabled)?;
    let (settings, credential, snapshot): (Value, Option<Uuid>, Option<Uuid>) = sqlx::query_as(
        "SELECT v.settings, v.credential_id, v.smtp_snapshot_id
         FROM app.integration_versions v LEFT JOIN app.integration_credentials c ON c.id = v.credential_id
         WHERE v.integration_id = $1 AND v.version = $2 AND v.state = 'disabled'
           AND v.activated_at IS NOT NULL AND c.revoked_at IS NULL",
    )
    .bind(id)
    .bind(prior_version)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Error::Conflict)?;
    if current.connector == "smtp" && snapshot.is_none() {
        return Err(Error::Conflict);
    }
    let version = current.latest_version + 1;
    retire_active(&mut tx, id, false).await?;
    stage_version(
        &mut tx,
        id,
        version,
        settings,
        credential,
        VersionState::Active,
        snapshot,
    )
    .await?;
    sqlx::query(
        "INSERT INTO app.integration_audit (integration_id, config_version, action, restored_from_version)
         VALUES ($1, $2, 'rolled_back', $3)",
    )
    .bind(id)
    .bind(version)
    .bind(prior_version)
    .execute(&mut *tx)
    .await?;
    let result = fetch_integration(&mut tx, id).await?;
    tx.commit().await?;
    Ok(result)
}

async fn supersede_pending(connection: &mut PgConnection, id: Uuid) -> Result<(), Error> {
    let pending = sqlx::query_as::<_, (Uuid, i64)>(
        "UPDATE app.integration_tests SET status = 'superseded', finished_at = now()
         WHERE integration_id = $1 AND status = 'pending' RETURNING id, config_version",
    )
    .bind(id)
    .fetch_optional(&mut *connection)
    .await?;
    if let Some((test, version)) = pending {
        audit(connection, id, version, "test_superseded", Some(test)).await?;
    }
    Ok(())
}

async fn bump_revision(connection: &mut PgConnection, id: Uuid) -> Result<(), Error> {
    sqlx::query("UPDATE app.integration_configs SET revision = revision + 1 WHERE id = $1")
        .bind(id)
        .execute(connection)
        .await?;
    Ok(())
}

async fn audit(
    connection: &mut PgConnection,
    id: Uuid,
    version: i64,
    action: &'static str,
    test: Option<Uuid>,
) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO app.integration_audit (integration_id, config_version, action, test_id)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(version)
    .bind(action)
    .bind(test)
    .execute(connection)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests;
