use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum Condition {
    SourceUnavailable,
    DiskCritical,
    BackupStale,
    RecoveryVerificationFailed,
    MarketDataDelayed,
    BookOutOfSync,
    BackfillFailed,
    BacktestFailed,
    BacktestCompleted,
    BackupSucceeded,
    MarketAnomaly,
    SmtpUnavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum Severity {
    Informational,
    Important,
    Urgent,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum Phase {
    Triggered,
    Recovered,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum Disposition {
    Observed,
    Pending,
    InApp,
    Ignored,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema, sqlx::FromRow)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    /// `smtp_unavailable` is reserved for notifier-generated events.
    pub condition: Condition,
    pub severity: Severity,
    pub enabled: bool,
    pub email_enabled: bool,
    #[schema(minimum = 0, maximum = 604800)]
    pub cooldown_seconds: Option<i32>,
    #[schema(minimum = 60, maximum = 604800)]
    pub reminder_seconds: Option<i32>,
    #[serde(default = "one_occurrence")]
    #[schema(default = 1, minimum = 1, maximum = 100)]
    pub min_occurrences: i32,
}

fn one_occurrence() -> i32 {
    1
}

impl RuleConfig {
    pub(crate) fn is_valid(&self) -> bool {
        self.condition != Condition::SmtpUnavailable
            && self
                .cooldown_seconds
                .is_none_or(|seconds| (0..=604_800).contains(&seconds))
            && self.reminder_seconds.is_none_or(|seconds| {
                self.severity == Severity::Urgent && (60..=604_800).contains(&seconds)
            })
            && (1..=100).contains(&self.min_occurrences)
    }
}

#[derive(Clone, Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct Rule {
    #[schema(format = "uuid")]
    pub id: String,
    pub revision: i64,
    pub condition: Condition,
    pub severity: Severity,
    pub enabled: bool,
    pub email_enabled: bool,
    pub cooldown_seconds: Option<i32>,
    pub reminder_seconds: Option<i32>,
    pub min_occurrences: i32,
}

/// A producer assigns an opaque subject UUID and the start of its deduplication window.
/// The signal carries only operational metadata and can be replayed with the same key.
#[derive(Clone, Copy, Debug)]
pub struct AlertSignal {
    pub condition: Condition,
    pub incident_key: Uuid,
    pub window_start: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
    pub phase: Phase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordOutcome {
    pub id: Uuid,
    pub disposition: Disposition,
    pub duplicate: bool,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct Alert {
    #[schema(format = "uuid")]
    pub id: String,
    #[schema(value_type = String, pattern = "^[1-9][0-9]*$")]
    #[serde(serialize_with = "serialize_sequence")]
    pub sequence: i64,
    #[schema(format = "uuid")]
    pub rule_id: Option<String>,
    pub condition: Condition,
    pub severity: Severity,
    pub phase: Phase,
    pub observed_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub acknowledged_at: Option<DateTime<Utc>>,
    pub disposition: Disposition,
    pub duplicate_count: i64,
    pub last_duplicate_at: Option<DateTime<Utc>>,
    #[sqlx(skip)]
    pub deliveries: Vec<Delivery>,
}

fn serialize_sequence<S: serde::Serializer>(value: &i64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(value)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum DeliveryStatus {
    Pending,
    Sending,
    Accepted,
    Failed,
    Summarized,
    Suppressed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum DeliveryKind {
    Alert,
    Reminder,
    Recovery,
    Test,
    OutageDigest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum AttemptOutcome {
    Sending,
    Accepted,
    Timeout,
    Transient,
    Rejected,
    ConfigurationError,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum DeferReason {
    QuietHours,
    Cooldown,
    RateLimit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize, ToSchema, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum SuppressionReason {
    RuleDisabled,
    RuleChanged,
    IncidentRecovered,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct DeliveryAttempt {
    pub attempt: i32,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub outcome: AttemptOutcome,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct DeliveryDeferral {
    pub reason: DeferReason,
    pub deferred_at: DateTime<Utc>,
    pub available_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct Delivery {
    #[schema(format = "uuid")]
    pub id: String,
    pub kind: DeliveryKind,
    pub status: DeliveryStatus,
    pub created_at: DateTime<Utc>,
    pub available_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_outcome: Option<AttemptOutcome>,
    pub defer_reason: Option<DeferReason>,
    pub suppression_reason: Option<SuppressionReason>,
    #[schema(format = "uuid")]
    pub summary_id: Option<String>,
    pub summary_status: Option<DeliveryStatus>,
    pub attempts: Vec<DeliveryAttempt>,
    pub deferrals: Vec<DeliveryDeferral>,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct ChannelTest {
    #[schema(format = "uuid")]
    pub id: String,
    #[serde(flatten)]
    #[sqlx(flatten)]
    pub delivery: ChannelTestDelivery,
}

#[derive(Debug, Serialize, ToSchema, sqlx::FromRow)]
pub struct ChannelTestDelivery {
    pub status: DeliveryStatus,
    pub created_at: DateTime<Utc>,
    pub available_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub defer_reason: Option<DeferReason>,
    pub last_outcome: Option<AttemptOutcome>,
    #[sqlx(json)]
    pub attempts: Vec<DeliveryAttempt>,
    #[sqlx(json)]
    pub deferrals: Vec<DeliveryDeferral>,
}
