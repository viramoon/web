ALTER TABLE app.notification_outbox
    ADD COLUMN replaces_id uuid UNIQUE REFERENCES app.notification_outbox (id);
