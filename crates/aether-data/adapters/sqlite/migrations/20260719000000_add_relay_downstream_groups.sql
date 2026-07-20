CREATE TABLE relay_downstream_groups (
    id                          TEXT PRIMARY KEY CHECK (id <> ''),
    name                        TEXT NOT NULL UNIQUE CHECK (name <> ''),
    description                 TEXT,
    parent_id                   TEXT,
    global_ratio_multiplier     REAL NOT NULL,
    model_whitelist_json        TEXT NOT NULL,
    model_blacklist_json        TEXT NOT NULL,
    model_ratio_overrides_json  TEXT NOT NULL,
    priority                    INTEGER NOT NULL,
    requests_per_minute         INTEGER NOT NULL CHECK (requests_per_minute >= 0),
    requests_per_day            INTEGER NOT NULL CHECK (requests_per_day >= 0),
    daily_quota_limit           REAL NOT NULL,
    monthly_quota_limit         REAL NOT NULL,
    time_rules_json             TEXT NOT NULL,
    enabled                     INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    created_at                  TEXT NOT NULL,
    updated_at                  TEXT NOT NULL
);

CREATE INDEX idx_relay_downstream_groups_export
    ON relay_downstream_groups(enabled, priority, name);
