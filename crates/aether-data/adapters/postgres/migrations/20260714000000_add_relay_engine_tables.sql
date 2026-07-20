-- Relay Engine: 中转定价与动态路由引擎
-- Creates 8 tables for upstream channel management, pricing, routing, profit tracking, and reconciliation.

-- 上游通道配置
CREATE TABLE relay_channels (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    provider    TEXT NOT NULL,
    endpoint    TEXT NOT NULL,
    weight      INTEGER NOT NULL DEFAULT 1,
    enabled     BOOLEAN NOT NULL DEFAULT TRUE,
    price_weight_override  DOUBLE PRECISION,
    health_weight_override DOUBLE PRECISION,
    config_json TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- 通道 API 密钥
CREATE TABLE relay_channel_keys (
    id          TEXT PRIMARY KEY,
    channel_id  TEXT NOT NULL REFERENCES relay_channels(id) ON DELETE CASCADE,
    api_key     TEXT NOT NULL,
    group_id    TEXT,
    group_ratio DOUBLE PRECISION NOT NULL DEFAULT 1.0,
    label       TEXT,
    enabled     BOOLEAN NOT NULL DEFAULT TRUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_relay_channel_keys_channel ON relay_channel_keys(channel_id);

-- 定价缓存
CREATE TABLE relay_pricing_cache (
    id                              TEXT PRIMARY KEY,
    channel_id                      TEXT NOT NULL REFERENCES relay_channels(id) ON DELETE CASCADE,
    key_id                          TEXT NOT NULL REFERENCES relay_channel_keys(id) ON DELETE CASCADE,
    model_id                        TEXT NOT NULL,
    model_ratio                     DOUBLE PRECISION NOT NULL,
    group_ratio                     DOUBLE PRECISION NOT NULL,
    completion_ratio                DOUBLE PRECISION NOT NULL DEFAULT 2.0,
    cost_per_prompt_token_quota     DOUBLE PRECISION NOT NULL,
    cost_per_completion_token_quota DOUBLE PRECISION NOT NULL,
    synced_at                       TIMESTAMPTZ NOT NULL,
    UNIQUE(channel_id, key_id, model_id)
);
CREATE INDEX idx_relay_pricing_cache_model ON relay_pricing_cache(model_id);

-- 加价规则
CREATE TABLE relay_markup_rules (
    id              TEXT PRIMARY KEY,
    scope_type      TEXT NOT NULL,
    scope_value     TEXT,
    strategy_type   TEXT NOT NULL,
    strategy_params TEXT NOT NULL,
    priority        INTEGER NOT NULL DEFAULT 0,
    enabled         BOOLEAN NOT NULL DEFAULT TRUE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- 利润流水
CREATE TABLE relay_profit_ledger (
    id                      TEXT PRIMARY KEY,
    instance_id             TEXT NOT NULL CHECK (instance_id <> ''),
    request_id              TEXT NOT NULL,
    channel_id              TEXT NOT NULL,
    model_id                TEXT NOT NULL,
    prompt_tokens           BIGINT NOT NULL,
    completion_tokens       BIGINT NOT NULL,
    charged_quota           TEXT NOT NULL,
    quota_per_unit          TEXT,
    upstream_cost_usd       DOUBLE PRECISION,
    downstream_revenue_usd  DOUBLE PRECISION,
    payment_fee_usd         DOUBLE PRECISION,
    net_profit_usd          DOUBLE PRECISION,
    margin_percent          DOUBLE PRECISION,
    cost_confidence         TEXT NOT NULL CHECK (cost_confidence IN ('known', 'unknown')),
    occurred_at_unix_ms     BIGINT NOT NULL,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
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
);
CREATE INDEX idx_relay_profit_ledger_time ON relay_profit_ledger(instance_id, occurred_at_unix_ms);
CREATE INDEX idx_relay_profit_ledger_channel ON relay_profit_ledger(instance_id, channel_id, occurred_at_unix_ms);
CREATE INDEX idx_relay_profit_ledger_model ON relay_profit_ledger(instance_id, model_id, occurred_at_unix_ms);

-- 下游实例
CREATE TABLE relay_downstream_instances (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    endpoint    TEXT NOT NULL,
    api_key     TEXT NOT NULL,
    enabled     BOOLEAN NOT NULL DEFAULT TRUE,
    last_sync_at TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- 对账记录
CREATE TABLE relay_settlements (
    id                      TEXT PRIMARY KEY,
    downstream_id           TEXT NOT NULL REFERENCES relay_downstream_instances(id) ON DELETE CASCADE,
    period_start            TIMESTAMPTZ NOT NULL,
    period_end              TIMESTAMPTZ NOT NULL,
    downstream_revenue_usd  DOUBLE PRECISION NOT NULL,
    upstream_cost_usd       DOUBLE PRECISION NOT NULL,
    difference_usd          DOUBLE PRECISION NOT NULL,
    difference_percent      DOUBLE PRECISION NOT NULL,
    is_anomaly              BOOLEAN NOT NULL DEFAULT FALSE,
    raw_data_json           TEXT,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_relay_settlements_time ON relay_settlements(created_at);
CREATE INDEX idx_relay_settlements_downstream ON relay_settlements(downstream_id, period_start);

-- 健康快照
CREATE TABLE relay_health_snapshots (
    id          TEXT PRIMARY KEY,
    channel_id  TEXT NOT NULL,
    health_score DOUBLE PRECISION NOT NULL,
    state       TEXT NOT NULL,
    metrics_json TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_relay_health_snapshots_channel ON relay_health_snapshots(channel_id, created_at);

CREATE TABLE relay_event_inbox (
    instance_id       TEXT NOT NULL,
    event_id          TEXT NOT NULL,
    dedupe_key        TEXT,
    event_type        TEXT NOT NULL,
    payload_json      TEXT NOT NULL,
    quota_per_unit    TEXT NOT NULL,
    occurred_at       BIGINT NOT NULL,
    source_created_at BIGINT NOT NULL,
    persisted_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (instance_id, event_id)
);
CREATE INDEX idx_relay_event_inbox_type ON relay_event_inbox(instance_id, event_type, occurred_at);

CREATE TABLE relay_event_cursors (
    instance_id TEXT PRIMARY KEY,
    cursor      TEXT NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
