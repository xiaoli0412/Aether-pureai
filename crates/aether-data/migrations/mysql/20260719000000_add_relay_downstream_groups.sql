CREATE TABLE relay_downstream_groups (
    id                          VARCHAR(64) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin PRIMARY KEY,
    name                        VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
    description                 TEXT,
    parent_id                   VARCHAR(64) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin,
    global_ratio_multiplier     DOUBLE NOT NULL,
    model_whitelist_json        TEXT NOT NULL,
    model_blacklist_json        TEXT NOT NULL,
    model_ratio_overrides_json  TEXT NOT NULL,
    priority                    INT NOT NULL,
    requests_per_minute         BIGINT NOT NULL,
    requests_per_day            BIGINT NOT NULL,
    daily_quota_limit           DOUBLE NOT NULL,
    monthly_quota_limit         DOUBLE NOT NULL,
    time_rules_json             TEXT NOT NULL,
    enabled                     BOOLEAN NOT NULL,
    created_at                  VARCHAR(64) NOT NULL,
    updated_at                  VARCHAR(64) NOT NULL,
    CONSTRAINT uk_relay_downstream_groups_name UNIQUE (name),
    CONSTRAINT chk_relay_downstream_groups_rpm CHECK (requests_per_minute >= 0),
    CONSTRAINT chk_relay_downstream_groups_rpd CHECK (requests_per_day >= 0)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE INDEX idx_relay_downstream_groups_export
    ON relay_downstream_groups(enabled, priority, name);
