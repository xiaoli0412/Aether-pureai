CREATE TABLE new_api_integration_configs (
    instance_id        TEXT PRIMARY KEY,
    route_profile      TEXT NOT NULL,
    execution_mode     TEXT NOT NULL,
    enabled            INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    capability_version TEXT NOT NULL,
    revision           INTEGER NOT NULL CHECK (revision >= 0),
    updated_at_unix_ms INTEGER NOT NULL
);
