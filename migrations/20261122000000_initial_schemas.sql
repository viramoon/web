CREATE SCHEMA app;
CREATE SCHEMA blog;

CREATE TABLE app.schema_info (
    component text PRIMARY KEY,
    version integer NOT NULL CHECK (version > 0)
);

INSERT INTO app.schema_info (component, version) VALUES ('web', 1);
