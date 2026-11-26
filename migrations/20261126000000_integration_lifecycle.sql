CREATE TABLE app.integration_configs (
    id uuid PRIMARY KEY,
    connector text NOT NULL,
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    latest_version bigint NOT NULL DEFAULT 1 CHECK (latest_version > 0)
);

CREATE TABLE app.integration_credentials (
    id uuid PRIMARY KEY,
    integration_id uuid NOT NULL REFERENCES app.integration_configs (id),
    version bigint NOT NULL CHECK (version > 0),
    format text NOT NULL CHECK (format = 'jwe'),
    mask text NOT NULL CHECK (mask = '********'),
    rotated_at timestamptz NOT NULL DEFAULT now(),
    revoked_at timestamptz,
    UNIQUE (integration_id, id),
    UNIQUE (integration_id, version)
);

CREATE TABLE app.integration_versions (
    integration_id uuid NOT NULL REFERENCES app.integration_configs (id),
    version bigint NOT NULL CHECK (version > 0),
    state text NOT NULL CHECK (state IN ('draft', 'candidate', 'active', 'disabled')),
    settings jsonb NOT NULL CHECK (jsonb_typeof(settings) = 'object'),
    credential_id uuid,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (integration_id, version),
    FOREIGN KEY (integration_id, credential_id)
        REFERENCES app.integration_credentials (integration_id, id)
);

ALTER TABLE app.integration_configs
    ADD FOREIGN KEY (id, latest_version)
        REFERENCES app.integration_versions (integration_id, version)
        DEFERRABLE INITIALLY DEFERRED;

ALTER TABLE app.integration_credentials
    ADD FOREIGN KEY (integration_id, version)
        REFERENCES app.integration_versions (integration_id, version)
        DEFERRABLE INITIALLY DEFERRED;

CREATE UNIQUE INDEX integration_one_active_version
    ON app.integration_versions (integration_id) WHERE state = 'active';

CREATE TABLE app.integration_tests (
    id uuid PRIMARY KEY,
    integration_id uuid NOT NULL,
    config_version bigint NOT NULL,
    status text NOT NULL CHECK (status IN ('pending', 'passed', 'failed', 'unavailable', 'superseded')),
    started_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz,
    CHECK ((status = 'pending') = (finished_at IS NULL)),
    FOREIGN KEY (integration_id, config_version)
        REFERENCES app.integration_versions (integration_id, version)
);

CREATE UNIQUE INDEX integration_one_pending_test
    ON app.integration_tests (integration_id) WHERE status = 'pending';

CREATE TABLE app.integration_audit (
    sequence bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    integration_id uuid NOT NULL,
    config_version bigint NOT NULL,
    actor text NOT NULL DEFAULT 'admin' CHECK (actor = 'admin'),
    action text NOT NULL CHECK (action IN (
        'created', 'settings_changed', 'credential_staged', 'credential_revoked',
        'disabled', 'test_started', 'test_passed', 'test_failed', 'test_unavailable', 'test_superseded'
    )),
    test_id uuid REFERENCES app.integration_tests (id),
    occurred_at timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (integration_id, config_version)
        REFERENCES app.integration_versions (integration_id, version)
);

CREATE INDEX integration_audit_history ON app.integration_audit (integration_id, sequence);
