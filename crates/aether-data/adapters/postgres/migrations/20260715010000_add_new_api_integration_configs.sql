CREATE TABLE new_api_integration_configs (
    instance_id        TEXT PRIMARY KEY,
    route_profile      TEXT NOT NULL,
    execution_mode     TEXT NOT NULL,
    enabled            BOOLEAN NOT NULL,
    capability_version TEXT NOT NULL,
    revision           BIGINT NOT NULL CHECK (revision >= 0),
    updated_at_unix_ms BIGINT NOT NULL
);
