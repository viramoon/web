ALTER TABLE app.integration_tests ADD COLUMN evidence jsonb
    CHECK (evidence IS NULL OR jsonb_typeof(evidence) = 'object');

ALTER TABLE app.integration_versions ADD COLUMN activated_at timestamptz;
UPDATE app.integration_versions v SET activated_at = a.occurred_at
FROM app.integration_audit a
WHERE a.integration_id = v.integration_id AND a.config_version = v.version
    AND a.action = 'test_passed';

ALTER TABLE app.integration_audit DROP CONSTRAINT integration_audit_action_check;
ALTER TABLE app.integration_audit ADD CONSTRAINT integration_audit_action_check
    CHECK (action IN (
        'created', 'settings_changed', 'credential_staged', 'credential_revoked',
        'disabled', 'test_started', 'test_passed', 'test_failed', 'test_unavailable',
        'test_superseded', 'activated', 'rolled_back'
    ));
ALTER TABLE app.integration_audit ADD COLUMN restored_from_version bigint,
    ADD FOREIGN KEY (integration_id, restored_from_version)
        REFERENCES app.integration_versions (integration_id, version),
    ADD CHECK ((action = 'rolled_back') = (restored_from_version IS NOT NULL));

CREATE UNIQUE INDEX integration_one_smtp_config ON app.integration_configs (connector)
    WHERE connector = 'smtp';

ALTER TABLE app.notification_outbox ADD COLUMN integration_test_id uuid UNIQUE
    REFERENCES app.integration_tests (id);
ALTER TABLE app.notification_outbox
    ADD CHECK (integration_test_id IS NULL OR kind = 'test');
