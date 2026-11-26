CREATE TABLE app.notification_rules (
    id uuid PRIMARY KEY,
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    condition text NOT NULL CHECK (condition IN (
        'source_unavailable', 'disk_critical', 'backup_stale', 'recovery_verification_failed',
        'market_data_delayed', 'book_out_of_sync', 'backfill_failed', 'backtest_failed',
        'backtest_completed', 'backup_succeeded', 'market_anomaly'
    )),
    severity text NOT NULL CHECK (severity IN ('informational', 'important', 'urgent')),
    enabled boolean NOT NULL,
    email_enabled boolean NOT NULL,
    cooldown_seconds integer CHECK (cooldown_seconds BETWEEN 0 AND 604800),
    reminder_seconds integer CHECK (reminder_seconds BETWEEN 60 AND 604800),
    min_occurrences integer NOT NULL CHECK (min_occurrences BETWEEN 1 AND 100),
    CHECK (reminder_seconds IS NULL OR severity = 'urgent')
);

CREATE TABLE app.notification_incidents (
    rule_id uuid NOT NULL REFERENCES app.notification_rules (id),
    condition text NOT NULL,
    incident_key uuid NOT NULL,
    last_observed_at timestamptz,
    consecutive_count integer NOT NULL DEFAULT 0 CHECK (consecutive_count BETWEEN 0 AND 100),
    active boolean NOT NULL DEFAULT false,
    email_open boolean NOT NULL DEFAULT false,
    next_reminder_at timestamptz,
    CHECK (NOT email_open OR active),
    PRIMARY KEY (rule_id, condition, incident_key)
);

CREATE TABLE app.alert_events (
    id uuid PRIMARY KEY,
    sequence bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
    rule_id uuid REFERENCES app.notification_rules (id),
    condition text NOT NULL,
    severity text NOT NULL CHECK (severity IN ('informational', 'important', 'urgent')),
    phase text NOT NULL CHECK (phase IN ('triggered', 'recovered')),
    incident_key uuid,
    business_window timestamptz,
    observed_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    acknowledged_at timestamptz,
    disposition text NOT NULL CHECK (disposition IN ('observed', 'pending', 'in_app', 'ignored')),
    duplicate_count bigint NOT NULL DEFAULT 0 CHECK (duplicate_count >= 0),
    last_duplicate_at timestamptz,
    CHECK ((rule_id IS NULL) = (condition = 'smtp_unavailable')),
    CHECK ((rule_id IS NULL) = (incident_key IS NULL)),
    CHECK ((rule_id IS NULL) = (business_window IS NULL)),
    CHECK (business_window <= observed_at),
    UNIQUE (rule_id, condition, incident_key, business_window, phase)
);

CREATE INDEX alert_events_unacknowledged ON app.alert_events (sequence)
    WHERE acknowledged_at IS NULL AND disposition IN ('pending', 'in_app');

CREATE TABLE app.notification_outbox (
    id uuid PRIMARY KEY,
    event_id uuid REFERENCES app.alert_events (id),
    rule_id uuid REFERENCES app.notification_rules (id),
    kind text NOT NULL CHECK (kind IN ('alert', 'reminder', 'recovery', 'test', 'outage_digest')),
    priority text NOT NULL CHECK (priority IN ('normal', 'urgent')),
    status text NOT NULL DEFAULT 'pending' CHECK (status IN (
        'pending', 'sending', 'accepted', 'failed', 'summarized', 'suppressed'
    )),
    created_at timestamptz NOT NULL DEFAULT now(),
    available_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    last_outcome text CHECK (last_outcome IN (
        'accepted', 'timeout', 'transient', 'rejected', 'configuration_error', 'interrupted'
    )),
    send_started_at timestamptz,
    summary_id uuid REFERENCES app.notification_outbox (id),
    digest_count bigint CHECK (digest_count > 0),
    defer_reason text CHECK (defer_reason IN ('quiet_hours', 'cooldown', 'rate_limit')),
    suppression_reason text CHECK (suppression_reason IN ('rule_disabled', 'rule_changed', 'incident_recovered')),
    test_key uuid UNIQUE,
    CHECK ((kind = 'test') = (test_key IS NOT NULL)),
    CHECK ((kind = 'outage_digest') = (digest_count IS NOT NULL)),
    CHECK (kind NOT IN ('alert', 'reminder', 'recovery') OR (event_id IS NOT NULL AND rule_id IS NOT NULL)),
    CHECK (summary_id <> id)
);

CREATE INDEX notification_outbox_due ON app.notification_outbox (available_at, created_at, id)
    WHERE status = 'pending';
CREATE UNIQUE INDEX notification_outbox_single_sender ON app.notification_outbox ((true))
    WHERE status = 'sending';
CREATE INDEX notification_outbox_event ON app.notification_outbox (event_id);
CREATE INDEX notification_outbox_summary ON app.notification_outbox (summary_id)
    WHERE summary_id IS NOT NULL;
CREATE INDEX notification_outbox_rule_attempt ON app.notification_outbox (rule_id, send_started_at DESC)
    WHERE send_started_at IS NOT NULL;

CREATE TABLE app.notification_attempts (
    outbox_id uuid NOT NULL REFERENCES app.notification_outbox (id),
    attempt integer NOT NULL CHECK (attempt > 0),
    started_at timestamptz NOT NULL,
    finished_at timestamptz,
    outcome text NOT NULL CHECK (outcome IN (
        'sending', 'accepted', 'timeout', 'transient', 'rejected', 'configuration_error', 'interrupted'
    )),
    CHECK ((outcome = 'sending') = (finished_at IS NULL)),
    PRIMARY KEY (outbox_id, attempt)
);

CREATE INDEX notification_attempts_window ON app.notification_attempts (started_at);

CREATE TABLE app.notification_deferrals (
    sequence bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    outbox_id uuid NOT NULL REFERENCES app.notification_outbox (id),
    reason text NOT NULL CHECK (reason IN ('quiet_hours', 'cooldown', 'rate_limit')),
    deferred_at timestamptz NOT NULL,
    available_at timestamptz NOT NULL
);

CREATE INDEX notification_deferrals_outbox ON app.notification_deferrals (outbox_id, sequence);

CREATE TABLE app.notification_health (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    state text NOT NULL DEFAULT 'idle' CHECK (state IN ('idle', 'healthy', 'degraded')),
    failure_since timestamptz,
    last_outcome text CHECK (last_outcome IN (
        'accepted', 'timeout', 'transient', 'rejected', 'configuration_error', 'interrupted'
    )),
    last_attempt_at timestamptz,
    last_accepted_at timestamptz,
    failure_count bigint NOT NULL DEFAULT 0 CHECK (failure_count >= 0)
);

INSERT INTO app.notification_health DEFAULT VALUES;
