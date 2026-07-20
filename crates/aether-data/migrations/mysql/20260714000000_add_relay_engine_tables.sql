-- Relay Engine: 中转定价与动态路由引擎

CREATE TABLE relay_channels (
    id          VARCHAR(64) PRIMARY KEY,
    name        VARCHAR(255) NOT NULL,
    provider    VARCHAR(64) NOT NULL,
    endpoint    TEXT NOT NULL,
    weight      INT NOT NULL DEFAULT 1,
    enabled     BOOLEAN NOT NULL DEFAULT TRUE,
    price_weight_override  DOUBLE,
    health_weight_override DOUBLE,
    config_json TEXT,
    created_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE relay_channel_keys (
    id          VARCHAR(64) PRIMARY KEY,
    channel_id  VARCHAR(64) NOT NULL,
    api_key     TEXT NOT NULL,
    group_id    VARCHAR(128),
    group_ratio DOUBLE NOT NULL DEFAULT 1.0,
    label       VARCHAR(255),
    enabled     BOOLEAN NOT NULL DEFAULT TRUE,
    created_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_relay_channel_keys_channel FOREIGN KEY (channel_id) REFERENCES relay_channels(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_relay_channel_keys_channel ON relay_channel_keys(channel_id);

CREATE TABLE relay_pricing_cache (
    id                              VARCHAR(64) PRIMARY KEY,
    channel_id                      VARCHAR(64) NOT NULL,
    key_id                          VARCHAR(64) NOT NULL,
    model_id                        VARCHAR(255) NOT NULL,
    model_ratio                     DOUBLE NOT NULL,
    group_ratio                     DOUBLE NOT NULL,
    completion_ratio                DOUBLE NOT NULL DEFAULT 2.0,
    cost_per_prompt_token_quota     DOUBLE NOT NULL,
    cost_per_completion_token_quota DOUBLE NOT NULL,
    synced_at                       TIMESTAMP NOT NULL,
    UNIQUE KEY uk_relay_pricing_cache (channel_id, key_id, model_id),
    CONSTRAINT fk_relay_pricing_cache_channel FOREIGN KEY (channel_id) REFERENCES relay_channels(id) ON DELETE CASCADE,
    CONSTRAINT fk_relay_pricing_cache_key FOREIGN KEY (key_id) REFERENCES relay_channel_keys(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_relay_pricing_cache_model ON relay_pricing_cache(model_id);

CREATE TABLE relay_markup_rules (
    id              VARCHAR(64) PRIMARY KEY,
    scope_type      VARCHAR(32) NOT NULL,
    scope_value     VARCHAR(255),
    strategy_type   VARCHAR(32) NOT NULL,
    strategy_params TEXT NOT NULL,
    priority        INT NOT NULL DEFAULT 0,
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    created_at      TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at      TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE relay_profit_ledger (
    id                      VARCHAR(64) PRIMARY KEY,
    instance_id             VARCHAR(255) NOT NULL CHECK (instance_id <> ''),
    request_id              VARCHAR(128) NOT NULL,
    channel_id              VARCHAR(64) NOT NULL,
    model_id                VARCHAR(255) NOT NULL,
    prompt_tokens           BIGINT NOT NULL,
    completion_tokens       BIGINT NOT NULL,
    charged_quota           TEXT NOT NULL,
    quota_per_unit          TEXT,
    upstream_cost_usd       DOUBLE,
    downstream_revenue_usd  DOUBLE,
    payment_fee_usd         DOUBLE,
    net_profit_usd          DOUBLE,
    margin_percent          DOUBLE,
    cost_confidence         VARCHAR(16) NOT NULL CHECK (cost_confidence IN ('known', 'unknown')),
    occurred_at_unix_ms     BIGINT NOT NULL,
    created_at              TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (instance_id, request_id),
    CHECK (
        (cost_confidence = 'known'
            AND upstream_cost_usd IS NOT NULL)
        OR
        (cost_confidence = 'unknown'
            AND upstream_cost_usd IS NULL)
    ),
    CHECK (
        quota_per_unit IS NOT NULL
        OR (downstream_revenue_usd IS NULL AND payment_fee_usd IS NULL)
    ),
    CHECK (
        (downstream_revenue_usd IS NULL AND payment_fee_usd IS NULL)
        OR (downstream_revenue_usd IS NOT NULL AND payment_fee_usd IS NOT NULL)
    ),
    CHECK (
        (upstream_cost_usd IS NOT NULL
            AND downstream_revenue_usd IS NOT NULL
            AND net_profit_usd IS NOT NULL
            AND margin_percent IS NOT NULL)
        OR
        ((upstream_cost_usd IS NULL OR downstream_revenue_usd IS NULL)
            AND net_profit_usd IS NULL
            AND margin_percent IS NULL)
    )
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_relay_profit_ledger_time ON relay_profit_ledger(instance_id, occurred_at_unix_ms);
CREATE INDEX idx_relay_profit_ledger_channel ON relay_profit_ledger(instance_id, channel_id, occurred_at_unix_ms);
CREATE INDEX idx_relay_profit_ledger_model ON relay_profit_ledger(instance_id, model_id, occurred_at_unix_ms);

CREATE TABLE relay_downstream_instances (
    id          VARCHAR(64) PRIMARY KEY,
    name        VARCHAR(255) NOT NULL,
    endpoint    TEXT NOT NULL,
    api_key     TEXT NOT NULL,
    enabled     BOOLEAN NOT NULL DEFAULT TRUE,
    last_sync_at TIMESTAMP NULL,
    created_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE relay_settlements (
    id                      VARCHAR(64) PRIMARY KEY,
    downstream_id           VARCHAR(64) NOT NULL,
    period_start            TIMESTAMP NOT NULL,
    period_end              TIMESTAMP NOT NULL,
    downstream_revenue_usd  DOUBLE NOT NULL,
    upstream_cost_usd       DOUBLE NOT NULL,
    difference_usd          DOUBLE NOT NULL,
    difference_percent      DOUBLE NOT NULL,
    is_anomaly              BOOLEAN NOT NULL DEFAULT FALSE,
    raw_data_json           TEXT,
    created_at              TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT fk_relay_settlements_downstream FOREIGN KEY (downstream_id) REFERENCES relay_downstream_instances(id) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_relay_settlements_time ON relay_settlements(created_at);
CREATE INDEX idx_relay_settlements_downstream ON relay_settlements(downstream_id, period_start);

CREATE TABLE relay_health_snapshots (
    id          VARCHAR(64) PRIMARY KEY,
    channel_id  VARCHAR(64) NOT NULL,
    health_score DOUBLE NOT NULL,
    state       VARCHAR(32) NOT NULL,
    metrics_json TEXT NOT NULL,
    created_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_relay_health_snapshots_channel ON relay_health_snapshots(channel_id, created_at);

CREATE TABLE relay_event_inbox (
    instance_id       VARCHAR(255) NOT NULL,
    event_id          VARCHAR(255) NOT NULL,
    dedupe_key        VARCHAR(512),
    event_type        VARCHAR(128) NOT NULL,
    payload_json      TEXT NOT NULL,
    quota_per_unit    VARCHAR(128) NOT NULL,
    occurred_at       BIGINT NOT NULL,
    source_created_at BIGINT NOT NULL,
    persisted_at      TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (instance_id, event_id)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
CREATE INDEX idx_relay_event_inbox_type ON relay_event_inbox(instance_id, event_type, occurred_at);

CREATE TABLE relay_event_cursors (
    instance_id VARCHAR(255) PRIMARY KEY,
    cursor      TEXT NOT NULL,
    updated_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
