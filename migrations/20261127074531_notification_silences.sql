CREATE TABLE app.notification_silences (
    sequence bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    rule_id uuid REFERENCES app.notification_rules (id),
    alert_id uuid REFERENCES app.alert_events (id),
    reason text NOT NULL CHECK (char_length(reason) BETWEEN 1 AND 256),
    started_by text NOT NULL,
    started_at timestamptz NOT NULL,
    until_at timestamptz NOT NULL CHECK (until_at > started_at),
    ended_by text,
    ended_at timestamptz,
    end_action text CHECK (end_action IN ('released', 'replaced')),
    CHECK ((rule_id IS NULL) <> (alert_id IS NULL)),
    CHECK ((ended_at IS NULL) = (ended_by IS NULL)),
    CHECK ((ended_at IS NULL) = (end_action IS NULL)),
    CHECK (ended_at >= started_at AND ended_at < until_at)
);

CREATE INDEX notification_silences_rule ON app.notification_silences (rule_id, sequence DESC)
    WHERE rule_id IS NOT NULL;
CREATE INDEX notification_silences_alert ON app.notification_silences (alert_id, sequence DESC)
    WHERE alert_id IS NOT NULL;

-- With identical window bounds, this is the active-silence check at that instant.
CREATE FUNCTION app.notification_silence_overlaps(
    target_rule uuid, target_alert uuid, window_start timestamptz, window_end timestamptz
) RETURNS boolean
LANGUAGE sql STABLE PARALLEL SAFE
RETURN EXISTS (
    SELECT FROM app.notification_silences silence
    WHERE (silence.rule_id = target_rule OR silence.alert_id = target_alert)
      AND silence.started_at <= window_end
      AND least(silence.until_at, silence.ended_at) > greatest(silence.started_at, window_start)
);
