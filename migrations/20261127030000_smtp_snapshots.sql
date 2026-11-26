ALTER TABLE app.notification_outbox ADD COLUMN smtp_snapshot_id uuid,
    ADD CHECK (smtp_snapshot_id IS NULL OR integration_test_id IS NOT NULL);

ALTER TABLE app.integration_versions ADD COLUMN smtp_snapshot_id uuid;
