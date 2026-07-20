use serde::{Deserialize, Serialize};
use sqlx::{MySqlPool, SqlitePool};

use crate::{DataBackends, DataLayerError};

const MYSQL_DUPLICATE_SENTINEL: u64 = 1;
const MYSQL_PRIMARY_KEY_COLLISION_SENTINEL: u64 = 2;
const MYSQL_APPEND_SQL: &str = "INSERT INTO relay_profit_ledger
    (id, instance_id, request_id, channel_id, model_id, prompt_tokens,
     completion_tokens, charged_quota, quota_per_unit, upstream_cost_usd,
     downstream_revenue_usd, payment_fee_usd, net_profit_usd, margin_percent,
     cost_confidence, occurred_at_unix_ms)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
    ON DUPLICATE KEY UPDATE
        prompt_tokens = prompt_tokens + IF(
            BINARY instance_id = BINARY VALUES(instance_id)
            AND BINARY request_id = BINARY VALUES(request_id),
            LAST_INSERT_ID(1) * 0,
            LAST_INSERT_ID(2) * 0
        )";

const POSTGRES_APPEND_SQL: &str = "INSERT INTO relay_profit_ledger
    (id, instance_id, request_id, channel_id, model_id, prompt_tokens,
     completion_tokens, charged_quota, quota_per_unit, upstream_cost_usd,
     downstream_revenue_usd, payment_fee_usd, net_profit_usd, margin_percent,
     cost_confidence, occurred_at_unix_ms)
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
    ON CONFLICT (instance_id, request_id) DO NOTHING";

const SQLITE_APPEND_SQL: &str = "INSERT INTO relay_profit_ledger
    (id, instance_id, request_id, channel_id, model_id, prompt_tokens,
     completion_tokens, charged_quota, quota_per_unit, upstream_cost_usd,
     downstream_revenue_usd, payment_fee_usd, net_profit_usd, margin_percent,
     cost_confidence, occurred_at_unix_ms)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
    ON CONFLICT (instance_id, request_id) DO NOTHING";

const MAX_ID_CHARS: usize = 64;
const MAX_INSTANCE_ID_CHARS: usize = 255;
const MAX_REQUEST_ID_CHARS: usize = 128;
const MAX_CHANNEL_ID_CHARS: usize = 64;
const MAX_MODEL_ID_CHARS: usize = 255;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RelayCostConfidence {
    Known,
    Unknown,
}

impl RelayCostConfidence {
    fn as_str(self) -> &'static str {
        match self {
            Self::Known => "known",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedRelayProfitRecord {
    pub id: String,
    pub instance_id: String,
    pub request_id: String,
    pub channel_id: String,
    pub model_id: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub charged_quota: String,
    pub quota_per_unit: Option<String>,
    pub upstream_cost_usd: Option<f64>,
    pub downstream_revenue_usd: Option<f64>,
    pub payment_fee_usd: Option<f64>,
    pub net_profit_usd: Option<f64>,
    pub margin_percent: Option<f64>,
    pub cost_confidence: RelayCostConfidence,
    pub occurred_at_unix_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayProfitLedgerFilter {
    pub instance_id: String,
    pub start_unix_ms: i64,
    pub end_unix_ms: i64,
}

#[derive(sqlx::FromRow)]
struct RelayProfitLedgerRow {
    id: String,
    instance_id: String,
    request_id: String,
    channel_id: String,
    model_id: String,
    prompt_tokens: i64,
    completion_tokens: i64,
    charged_quota: String,
    quota_per_unit: Option<String>,
    upstream_cost_usd: Option<f64>,
    downstream_revenue_usd: Option<f64>,
    payment_fee_usd: Option<f64>,
    net_profit_usd: Option<f64>,
    margin_percent: Option<f64>,
    cost_confidence: String,
    occurred_at_unix_ms: i64,
}

impl TryFrom<RelayProfitLedgerRow> for PersistedRelayProfitRecord {
    type Error = DataLayerError;

    fn try_from(row: RelayProfitLedgerRow) -> Result<Self, Self::Error> {
        let cost_confidence = match row.cost_confidence.as_str() {
            "known" => RelayCostConfidence::Known,
            "unknown" => RelayCostConfidence::Unknown,
            value => {
                return Err(DataLayerError::UnexpectedValue(format!(
                    "unexpected relay profit cost_confidence {value}"
                )))
            }
        };
        Ok(Self {
            id: row.id,
            instance_id: row.instance_id,
            request_id: row.request_id,
            channel_id: row.channel_id,
            model_id: row.model_id,
            prompt_tokens: row.prompt_tokens,
            completion_tokens: row.completion_tokens,
            charged_quota: row.charged_quota,
            quota_per_unit: row.quota_per_unit,
            upstream_cost_usd: row.upstream_cost_usd,
            downstream_revenue_usd: row.downstream_revenue_usd,
            payment_fee_usd: row.payment_fee_usd,
            net_profit_usd: row.net_profit_usd,
            margin_percent: row.margin_percent,
            cost_confidence,
            occurred_at_unix_ms: row.occurred_at_unix_ms,
        })
    }
}

#[derive(Clone)]
enum RelayProfitLedgerBackend {
    Postgres(sqlx::PgPool),
    Mysql(MySqlPool),
    Sqlite(SqlitePool),
}

#[derive(Clone)]
pub struct RelayProfitLedgerStore {
    backend: RelayProfitLedgerBackend,
}

impl RelayProfitLedgerStore {
    pub fn from_backends(backends: &DataBackends) -> Option<Self> {
        backends
            .postgres()
            .map(|backend| Self {
                backend: RelayProfitLedgerBackend::Postgres(backend.pool_clone()),
            })
            .or_else(|| {
                backends.mysql().map(|backend| Self {
                    backend: RelayProfitLedgerBackend::Mysql(backend.pool_clone()),
                })
            })
            .or_else(|| {
                backends.sqlite().map(|backend| Self {
                    backend: RelayProfitLedgerBackend::Sqlite(backend.pool_clone()),
                })
            })
    }

    pub fn sqlite(pool: SqlitePool) -> Self {
        Self {
            backend: RelayProfitLedgerBackend::Sqlite(pool),
        }
    }

    pub async fn append(
        &self,
        record: &PersistedRelayProfitRecord,
    ) -> Result<bool, DataLayerError> {
        validate_record(record)?;

        let inserted = match &self.backend {
            RelayProfitLedgerBackend::Postgres(pool) => sqlx::query(POSTGRES_APPEND_SQL)
                .bind(&record.id)
                .bind(&record.instance_id)
                .bind(&record.request_id)
                .bind(&record.channel_id)
                .bind(&record.model_id)
                .bind(record.prompt_tokens)
                .bind(record.completion_tokens)
                .bind(&record.charged_quota)
                .bind(&record.quota_per_unit)
                .bind(record.upstream_cost_usd)
                .bind(record.downstream_revenue_usd)
                .bind(record.payment_fee_usd)
                .bind(record.net_profit_usd)
                .bind(record.margin_percent)
                .bind(record.cost_confidence.as_str())
                .bind(record.occurred_at_unix_ms)
                .execute(pool)
                .await
                .map(|result| result.rows_affected() > 0)
                .map_err(DataLayerError::sql),
            RelayProfitLedgerBackend::Mysql(pool) => {
                let result = sqlx::query(MYSQL_APPEND_SQL)
                    .bind(&record.id)
                    .bind(&record.instance_id)
                    .bind(&record.request_id)
                    .bind(&record.channel_id)
                    .bind(&record.model_id)
                    .bind(record.prompt_tokens)
                    .bind(record.completion_tokens)
                    .bind(&record.charged_quota)
                    .bind(&record.quota_per_unit)
                    .bind(record.upstream_cost_usd)
                    .bind(record.downstream_revenue_usd)
                    .bind(record.payment_fee_usd)
                    .bind(record.net_profit_usd)
                    .bind(record.margin_percent)
                    .bind(record.cost_confidence.as_str())
                    .bind(record.occurred_at_unix_ms)
                    .execute(pool)
                    .await
                    .map_err(DataLayerError::sql)?;
                mysql_append_was_inserted(result.last_insert_id())
            }
            RelayProfitLedgerBackend::Sqlite(pool) => sqlx::query(SQLITE_APPEND_SQL)
                .bind(&record.id)
                .bind(&record.instance_id)
                .bind(&record.request_id)
                .bind(&record.channel_id)
                .bind(&record.model_id)
                .bind(record.prompt_tokens)
                .bind(record.completion_tokens)
                .bind(&record.charged_quota)
                .bind(&record.quota_per_unit)
                .bind(record.upstream_cost_usd)
                .bind(record.downstream_revenue_usd)
                .bind(record.payment_fee_usd)
                .bind(record.net_profit_usd)
                .bind(record.margin_percent)
                .bind(record.cost_confidence.as_str())
                .bind(record.occurred_at_unix_ms)
                .execute(pool)
                .await
                .map(|result| result.rows_affected() > 0)
                .map_err(DataLayerError::sql),
        }?;

        Ok(inserted)
    }

    pub async fn list_for_filter(
        &self,
        filter: &RelayProfitLedgerFilter,
    ) -> Result<Vec<PersistedRelayProfitRecord>, DataLayerError> {
        validate_filter(filter)?;
        let rows: Vec<RelayProfitLedgerRow> = match &self.backend {
            RelayProfitLedgerBackend::Postgres(pool) => {
                sqlx::query_as(
                    "SELECT id, instance_id, request_id, channel_id, model_id, prompt_tokens,
                        completion_tokens, charged_quota, quota_per_unit, upstream_cost_usd,
                        downstream_revenue_usd, payment_fee_usd, net_profit_usd, margin_percent,
                        cost_confidence, occurred_at_unix_ms
                 FROM relay_profit_ledger
                 WHERE instance_id = $1
                   AND occurred_at_unix_ms >= $2
                   AND occurred_at_unix_ms < $3
                 ORDER BY occurred_at_unix_ms ASC, id ASC",
                )
                .bind(&filter.instance_id)
                .bind(filter.start_unix_ms)
                .bind(filter.end_unix_ms)
                .fetch_all(pool)
                .await
            }
            RelayProfitLedgerBackend::Mysql(pool) => {
                sqlx::query_as(
                    "SELECT id, instance_id, request_id, channel_id, model_id, prompt_tokens,
                        completion_tokens, charged_quota, quota_per_unit, upstream_cost_usd,
                        downstream_revenue_usd, payment_fee_usd, net_profit_usd, margin_percent,
                        cost_confidence, occurred_at_unix_ms
                 FROM relay_profit_ledger
                 WHERE instance_id = ?
                   AND occurred_at_unix_ms >= ?
                   AND occurred_at_unix_ms < ?
                 ORDER BY occurred_at_unix_ms ASC, id ASC",
                )
                .bind(&filter.instance_id)
                .bind(filter.start_unix_ms)
                .bind(filter.end_unix_ms)
                .fetch_all(pool)
                .await
            }
            RelayProfitLedgerBackend::Sqlite(pool) => {
                sqlx::query_as(
                    "SELECT id, instance_id, request_id, channel_id, model_id, prompt_tokens,
                        completion_tokens, charged_quota, quota_per_unit, upstream_cost_usd,
                        downstream_revenue_usd, payment_fee_usd, net_profit_usd, margin_percent,
                        cost_confidence, occurred_at_unix_ms
                 FROM relay_profit_ledger
                 WHERE instance_id = ?
                   AND occurred_at_unix_ms >= ?
                   AND occurred_at_unix_ms < ?
                 ORDER BY occurred_at_unix_ms ASC, id ASC",
                )
                .bind(&filter.instance_id)
                .bind(filter.start_unix_ms)
                .bind(filter.end_unix_ms)
                .fetch_all(pool)
                .await
            }
        }
        .map_err(DataLayerError::sql)?;

        rows.into_iter()
            .map(PersistedRelayProfitRecord::try_from)
            .collect()
    }
}

fn mysql_append_was_inserted(last_insert_id: u64) -> Result<bool, DataLayerError> {
    match last_insert_id {
        0 => Ok(true),
        MYSQL_DUPLICATE_SENTINEL => Ok(false),
        MYSQL_PRIMARY_KEY_COLLISION_SENTINEL => Err(DataLayerError::UnexpectedValue(
            "relay profit append encountered a primary key collision with a different business key"
                .to_string(),
        )),
        unexpected => Err(DataLayerError::UnexpectedValue(format!(
            "unexpected MySQL last_insert_id {unexpected} after relay profit append"
        ))),
    }
}

fn validate_record(record: &PersistedRelayProfitRecord) -> Result<(), DataLayerError> {
    validate_identifier("id", &record.id, MAX_ID_CHARS)?;
    validate_identifier("instance_id", &record.instance_id, MAX_INSTANCE_ID_CHARS)?;
    validate_identifier("request_id", &record.request_id, MAX_REQUEST_ID_CHARS)?;
    validate_identifier("channel_id", &record.channel_id, MAX_CHANNEL_ID_CHARS)?;
    validate_identifier("model_id", &record.model_id, MAX_MODEL_ID_CHARS)?;

    if record.prompt_tokens < 0 {
        return Err(DataLayerError::InvalidInput(
            "relay profit prompt_tokens must be non-negative".to_string(),
        ));
    }
    if record.completion_tokens < 0 {
        return Err(DataLayerError::InvalidInput(
            "relay profit completion_tokens must be non-negative".to_string(),
        ));
    }
    if record.occurred_at_unix_ms < 0 {
        return Err(DataLayerError::InvalidInput(
            "relay profit occurred_at_unix_ms must be non-negative".to_string(),
        ));
    }
    if !is_base10_decimal(&record.charged_quota) {
        return Err(DataLayerError::InvalidInput(
            "relay profit charged_quota must be a non-negative base-10 decimal".to_string(),
        ));
    }

    validate_finite("upstream_cost_usd", record.upstream_cost_usd)?;
    validate_finite("downstream_revenue_usd", record.downstream_revenue_usd)?;
    validate_finite("payment_fee_usd", record.payment_fee_usd)?;
    validate_finite("net_profit_usd", record.net_profit_usd)?;
    validate_finite("margin_percent", record.margin_percent)?;

    match record.quota_per_unit.as_deref() {
        Some(value) if !is_base10_decimal(value) => {
            return Err(DataLayerError::InvalidInput(
                "relay profit quota_per_unit must be a non-negative base-10 decimal".to_string(),
            ));
        }
        None if record.downstream_revenue_usd.is_some() || record.payment_fee_usd.is_some() => {
            return Err(DataLayerError::InvalidInput(
                "relay profit revenue must be unknown when quota_per_unit is missing".to_string(),
            ));
        }
        _ => {}
    }
    if record.downstream_revenue_usd.is_some() != record.payment_fee_usd.is_some() {
        return Err(DataLayerError::InvalidInput(
            "relay profit revenue and payment fee confidence must match".to_string(),
        ));
    }

    let upstream_cost_is_known = record.upstream_cost_usd.is_some();
    if upstream_cost_is_known != (record.cost_confidence == RelayCostConfidence::Known) {
        return Err(DataLayerError::InvalidInput(
            "relay profit cost confidence does not match upstream cost availability".to_string(),
        ));
    }

    let profit_is_known = upstream_cost_is_known && record.downstream_revenue_usd.is_some();
    if record.net_profit_usd.is_some() != profit_is_known
        || record.margin_percent.is_some() != profit_is_known
    {
        return Err(DataLayerError::InvalidInput(
            "relay profit and margin require known upstream cost and revenue".to_string(),
        ));
    }

    Ok(())
}

fn validate_filter(filter: &RelayProfitLedgerFilter) -> Result<(), DataLayerError> {
    validate_identifier("instance_id", &filter.instance_id, MAX_INSTANCE_ID_CHARS)?;
    if filter.start_unix_ms < 0 || filter.end_unix_ms < 0 {
        return Err(DataLayerError::InvalidInput(
            "relay profit filter timestamps must be non-negative".to_string(),
        ));
    }
    if filter.end_unix_ms < filter.start_unix_ms {
        return Err(DataLayerError::InvalidInput(
            "relay profit filter end must not precede start".to_string(),
        ));
    }
    Ok(())
}

fn validate_identifier(field: &str, value: &str, max_chars: usize) -> Result<(), DataLayerError> {
    if value.trim().is_empty() {
        return Err(DataLayerError::InvalidInput(format!(
            "relay profit {field} must not be blank"
        )));
    }
    if value.chars().count() > max_chars {
        return Err(DataLayerError::InvalidInput(format!(
            "relay profit {field} exceeds the maximum length of {max_chars} characters"
        )));
    }
    Ok(())
}

fn validate_finite(field: &str, value: Option<f64>) -> Result<(), DataLayerError> {
    if value.is_some_and(|value| !value.is_finite()) {
        return Err(DataLayerError::InvalidInput(format!(
            "relay profit {field} must be finite"
        )));
    }
    Ok(())
}

fn is_base10_decimal(value: &str) -> bool {
    if value.is_empty() || value.trim() != value {
        return false;
    }
    let mut parts = value.split('.');
    let Some(whole) = parts.next() else {
        return false;
    };
    let fraction = parts.next();
    let whole_is_valid = whole == "0"
        || whole.as_bytes().split_first().is_some_and(|(first, rest)| {
            matches!(first, b'1'..=b'9') && rest.iter().all(|byte| byte.is_ascii_digit())
        });
    if parts.next().is_some()
        || !whole_is_valid
        || fraction
            .is_some_and(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use sqlx::{mysql::MySqlPoolOptions, sqlite::SqlitePoolOptions};
    use uuid::Uuid;

    use super::{
        mysql_append_was_inserted, PersistedRelayProfitRecord, RelayCostConfidence,
        RelayProfitLedgerBackend, RelayProfitLedgerStore, MYSQL_APPEND_SQL,
        MYSQL_DUPLICATE_SENTINEL,
    };
    use crate::DataLayerError;

    async fn test_pool() -> sqlx::SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite test pool should connect");
        sqlx::query(
            "CREATE TABLE relay_profit_ledger (
                id TEXT PRIMARY KEY,
                instance_id TEXT NOT NULL CHECK (instance_id <> ''),
                request_id TEXT NOT NULL,
                channel_id TEXT NOT NULL,
                model_id TEXT NOT NULL,
                prompt_tokens INTEGER NOT NULL,
                completion_tokens INTEGER NOT NULL,
                charged_quota TEXT NOT NULL,
                quota_per_unit TEXT,
                upstream_cost_usd REAL,
                downstream_revenue_usd REAL,
                payment_fee_usd REAL,
                net_profit_usd REAL,
                margin_percent REAL,
                cost_confidence TEXT NOT NULL CHECK (cost_confidence IN ('known', 'unknown')),
                occurred_at_unix_ms INTEGER NOT NULL,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
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
            )",
        )
        .execute(&pool)
        .await
        .expect("relay profit ledger table should be created");
        pool
    }

    fn known_record() -> PersistedRelayProfitRecord {
        PersistedRelayProfitRecord {
            id: "profit-known".to_string(),
            instance_id: "instance-a".to_string(),
            request_id: "request-known".to_string(),
            channel_id: "channel-a".to_string(),
            model_id: "model-a".to_string(),
            prompt_tokens: 120,
            completion_tokens: 30,
            charged_quota: "900.5000".to_string(),
            quota_per_unit: Some("750000.000".to_string()),
            upstream_cost_usd: Some(0.0002),
            downstream_revenue_usd: Some(0.0012),
            payment_fee_usd: Some(0.000_007_2),
            net_profit_usd: Some(0.000_992_8),
            margin_percent: Some(82.733_333_333_3),
            cost_confidence: RelayCostConfidence::Known,
            occurred_at_unix_ms: 1_784_073_600_123,
        }
    }

    fn unknown_record() -> PersistedRelayProfitRecord {
        PersistedRelayProfitRecord {
            id: "profit-unknown".to_string(),
            instance_id: "instance-a".to_string(),
            request_id: "request-unknown".to_string(),
            channel_id: "channel-b".to_string(),
            model_id: "model-b".to_string(),
            prompt_tokens: 50,
            completion_tokens: 10,
            charged_quota: "300.2500".to_string(),
            quota_per_unit: None,
            upstream_cost_usd: None,
            downstream_revenue_usd: None,
            payment_fee_usd: None,
            net_profit_usd: None,
            margin_percent: None,
            cost_confidence: RelayCostConfidence::Unknown,
            occurred_at_unix_ms: 1_784_073_601_456,
        }
    }

    #[tokio::test]
    async fn append_is_idempotent_and_preserves_known_values() {
        let pool = test_pool().await;
        let store = RelayProfitLedgerStore::sqlite(pool.clone());
        let record = known_record();

        assert!(store
            .append(&record)
            .await
            .expect("known profit should persist"));
        assert!(!store
            .append(&record)
            .await
            .expect("duplicate profit should be ignored"));

        let row: (
            i64,
            String,
            Option<String>,
            String,
            Option<f64>,
            Option<f64>,
            i64,
        ) = sqlx::query_as(
            "SELECT COUNT(*), charged_quota, quota_per_unit, cost_confidence,
                        upstream_cost_usd, net_profit_usd, occurred_at_unix_ms
                 FROM relay_profit_ledger WHERE id = ?",
        )
        .bind(&record.id)
        .fetch_one(&pool)
        .await
        .expect("known profit row should load");
        assert_eq!(row.0, 1);
        assert_eq!(row.1, record.charged_quota);
        assert_eq!(row.2, record.quota_per_unit);
        assert_eq!(row.3, "known");
        assert_eq!(row.4, record.upstream_cost_usd);
        assert_eq!(row.5, record.net_profit_usd);
        assert_eq!(row.6, record.occurred_at_unix_ms);
    }

    #[tokio::test]
    async fn append_persists_unknown_cost_and_profit_as_sql_null() {
        let pool = test_pool().await;
        let store = RelayProfitLedgerStore::sqlite(pool.clone());
        let record = unknown_record();

        assert!(store
            .append(&record)
            .await
            .expect("unknown-cost profit should persist"));

        let row: (
            Option<f64>,
            Option<f64>,
            Option<f64>,
            Option<String>,
            Option<f64>,
            Option<f64>,
            String,
            String,
            i64,
        ) = sqlx::query_as(
            "SELECT upstream_cost_usd, net_profit_usd, margin_percent, quota_per_unit,
                        downstream_revenue_usd, payment_fee_usd, cost_confidence,
                        charged_quota, occurred_at_unix_ms
                 FROM relay_profit_ledger WHERE id = ?",
        )
        .bind(&record.id)
        .fetch_one(&pool)
        .await
        .expect("unknown-cost profit row should load");
        assert_eq!(row.0, None);
        assert_eq!(row.1, None);
        assert_eq!(row.2, None);
        assert_eq!(row.3, None);
        assert_eq!(row.4, None);
        assert_eq!(row.5, None);
        assert_eq!(row.6, "unknown");
        assert_eq!(row.7, record.charged_quota);
        assert_eq!(row.8, record.occurred_at_unix_ms);
    }

    #[tokio::test]
    async fn request_id_uniqueness_is_scoped_to_instance() {
        let pool = test_pool().await;
        let store = RelayProfitLedgerStore::sqlite(pool.clone());
        let first = known_record();
        assert!(store
            .append(&first)
            .await
            .expect("first instance profit should persist"));

        let mut same_instance = first.clone();
        same_instance.id = "profit-same-instance-duplicate".to_string();
        assert!(!store
            .append(&same_instance)
            .await
            .expect("same instance request should be idempotent"));

        let mut other_instance = first.clone();
        other_instance.id = "profit-other-instance".to_string();
        other_instance.instance_id = "instance-b".to_string();
        assert!(store
            .append(&other_instance)
            .await
            .expect("other instance may reuse the request id"));

        let row_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM relay_profit_ledger WHERE request_id = ?")
                .bind(&first.request_id)
                .fetch_one(&pool)
                .await
                .expect("scoped request rows should count");
        assert_eq!(row_count, 2);
    }

    #[tokio::test]
    async fn list_for_filter_is_instance_scoped_and_uses_a_half_open_time_window() {
        let pool = test_pool().await;
        let store = RelayProfitLedgerStore::sqlite(pool);

        let mut first = known_record();
        first.id = "profit-window-first".to_string();
        first.request_id = "request-window-first".to_string();
        first.occurred_at_unix_ms = 100;
        assert!(store
            .append(&first)
            .await
            .expect("first record should persist"));

        let mut last = known_record();
        last.id = "profit-window-last".to_string();
        last.request_id = "request-window-last".to_string();
        last.occurred_at_unix_ms = 200;
        assert!(store
            .append(&last)
            .await
            .expect("last record should persist"));

        let mut other_instance = known_record();
        other_instance.id = "profit-window-other-instance".to_string();
        other_instance.request_id = "request-window-other-instance".to_string();
        other_instance.instance_id = "instance-b".to_string();
        other_instance.occurred_at_unix_ms = 150;
        assert!(store
            .append(&other_instance)
            .await
            .expect("other instance record should persist"));

        let records = store
            .list_for_filter(&super::RelayProfitLedgerFilter {
                instance_id: "instance-a".to_string(),
                start_unix_ms: 100,
                end_unix_ms: 200,
            })
            .await
            .expect("filtered records should load");

        assert_eq!(records, vec![first]);
    }

    #[tokio::test]
    async fn primary_key_collisions_are_not_silently_treated_as_idempotent() {
        let pool = test_pool().await;
        let store = RelayProfitLedgerStore::sqlite(pool);
        let first = known_record();
        assert!(store
            .append(&first)
            .await
            .expect("first profit should persist"));

        let mut primary_key_collision = first.clone();
        primary_key_collision.request_id = "request-with-a-different-business-key".to_string();
        let error = store
            .append(&primary_key_collision)
            .await
            .expect_err("a reused primary key with a different business key must fail");

        assert!(matches!(error, DataLayerError::Sql(_)));
    }

    #[tokio::test]
    async fn zero_quota_per_unit_strings_are_valid_and_preserved_verbatim() {
        let pool = test_pool().await;
        let store = RelayProfitLedgerStore::sqlite(pool.clone());

        for (index, quota_per_unit) in ["0", "0.000"].into_iter().enumerate() {
            let mut record = known_record();
            record.id = format!("profit-zero-qpu-{index}");
            record.request_id = format!("request-zero-qpu-{index}");
            record.quota_per_unit = Some(quota_per_unit.to_string());

            assert!(store
                .append(&record)
                .await
                .expect("contract-valid zero quota_per_unit should persist"));
            let stored: Option<String> =
                sqlx::query_scalar("SELECT quota_per_unit FROM relay_profit_ledger WHERE id = ?")
                    .bind(&record.id)
                    .fetch_one(&pool)
                    .await
                    .expect("stored quota_per_unit should load");
            assert_eq!(stored.as_deref(), Some(quota_per_unit));
        }
    }

    #[tokio::test]
    async fn malformed_quota_per_unit_is_rejected_even_when_revenue_is_unknown() {
        let pool = test_pool().await;
        let store = RelayProfitLedgerStore::sqlite(pool.clone());

        for (index, quota_per_unit) in ["1e3", "-1", "01", " 1", "1 ", ""].into_iter().enumerate() {
            let mut record = unknown_record();
            record.id = format!("profit-invalid-qpu-{index}");
            record.request_id = format!("request-invalid-qpu-{index}");
            record.quota_per_unit = Some(quota_per_unit.to_string());

            let error = store
                .append(&record)
                .await
                .expect_err("malformed quota_per_unit must fail closed");
            assert!(error.to_string().contains("quota_per_unit"));
        }

        let row_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM relay_profit_ledger")
            .fetch_one(&pool)
            .await
            .expect("invalid quota rows should count");
        assert_eq!(row_count, 0);
    }

    #[tokio::test]
    async fn append_rejects_blank_required_identifiers() {
        let pool = test_pool().await;
        let store = RelayProfitLedgerStore::sqlite(pool);
        let invalid_records = [
            ("id", {
                let mut record = known_record();
                record.id.clear();
                record
            }),
            ("instance_id", {
                let mut record = known_record();
                record.instance_id.clear();
                record
            }),
            ("request_id", {
                let mut record = known_record();
                record.request_id.clear();
                record
            }),
            ("channel_id", {
                let mut record = known_record();
                record.channel_id.clear();
                record
            }),
            ("model_id", {
                let mut record = known_record();
                record.model_id.clear();
                record
            }),
            ("id", {
                let mut record = known_record();
                record.id = " \t".to_string();
                record
            }),
            ("instance_id", {
                let mut record = known_record();
                record.instance_id = " \t".to_string();
                record
            }),
            ("request_id", {
                let mut record = known_record();
                record.request_id = " \t".to_string();
                record
            }),
            ("channel_id", {
                let mut record = known_record();
                record.channel_id = " \t".to_string();
                record
            }),
            ("model_id", {
                let mut record = known_record();
                record.model_id = " \t".to_string();
                record
            }),
        ];

        for (field, record) in invalid_records {
            let error = store
                .append(&record)
                .await
                .expect_err("missing identifiers must be rejected before SQL");
            assert!(matches!(error, DataLayerError::InvalidInput(_)));
            assert!(error.to_string().contains(field));
        }
    }

    #[tokio::test]
    async fn append_enforces_identifier_lengths_in_characters() {
        let pool = test_pool().await;
        let store = RelayProfitLedgerStore::sqlite(pool);
        let mut record = known_record();
        record.id = format!("{}{}", "id".repeat(30), "\u{1F642}".repeat(5));

        let error = store
            .append(&record)
            .await
            .expect_err("identifiers over the MySQL character limit must be rejected");

        assert!(matches!(error, DataLayerError::InvalidInput(_)));
        assert!(error.to_string().contains("id"));
    }

    #[tokio::test]
    async fn append_rejects_identifiers_that_exceed_schema_limits() {
        let pool = test_pool().await;
        let store = RelayProfitLedgerStore::sqlite(pool);
        let invalid_records = [
            ("id", {
                let mut record = known_record();
                record.id = "i".repeat(65);
                record
            }),
            ("instance_id", {
                let mut record = known_record();
                record.instance_id = "i".repeat(256);
                record
            }),
            ("request_id", {
                let mut record = known_record();
                record.request_id = "r".repeat(129);
                record
            }),
            ("channel_id", {
                let mut record = known_record();
                record.channel_id = "c".repeat(65);
                record
            }),
            ("model_id", {
                let mut record = known_record();
                record.model_id = "m".repeat(256);
                record
            }),
        ];

        for (field, record) in invalid_records {
            let error = store
                .append(&record)
                .await
                .expect_err("identifiers exceeding schema limits must be rejected before SQL");
            assert!(matches!(error, DataLayerError::InvalidInput(_)));
            assert!(error.to_string().contains(field));
        }
    }

    #[tokio::test]
    async fn append_rejects_negative_token_counts_and_timestamps() {
        let pool = test_pool().await;
        let store = RelayProfitLedgerStore::sqlite(pool);
        let invalid_records = [
            ("prompt_tokens", {
                let mut record = known_record();
                record.prompt_tokens = -1;
                record
            }),
            ("completion_tokens", {
                let mut record = known_record();
                record.completion_tokens = -1;
                record
            }),
            ("occurred_at_unix_ms", {
                let mut record = known_record();
                record.occurred_at_unix_ms = -1;
                record
            }),
        ];

        for (field, record) in invalid_records {
            let error = store
                .append(&record)
                .await
                .expect_err("negative token counts and timestamps must be rejected before SQL");
            assert!(matches!(error, DataLayerError::InvalidInput(_)));
            assert!(error.to_string().contains(field));
        }
    }

    #[tokio::test]
    async fn append_rejects_non_finite_monetary_and_margin_values() {
        let pool = test_pool().await;
        let store = RelayProfitLedgerStore::sqlite(pool);
        let invalid_records = [
            ("upstream_cost_usd", {
                let mut record = known_record();
                record.upstream_cost_usd = Some(f64::NAN);
                record
            }),
            ("downstream_revenue_usd", {
                let mut record = known_record();
                record.downstream_revenue_usd = Some(f64::INFINITY);
                record
            }),
            ("payment_fee_usd", {
                let mut record = known_record();
                record.payment_fee_usd = Some(f64::NEG_INFINITY);
                record
            }),
            ("net_profit_usd", {
                let mut record = known_record();
                record.net_profit_usd = Some(f64::NAN);
                record
            }),
            ("margin_percent", {
                let mut record = known_record();
                record.margin_percent = Some(f64::INFINITY);
                record
            }),
        ];

        for (field, record) in invalid_records {
            let error = store
                .append(&record)
                .await
                .expect_err("non-finite numeric values must be rejected before SQL");
            assert!(matches!(error, DataLayerError::InvalidInput(_)));
            assert!(error.to_string().contains(field));
        }
    }

    #[test]
    fn mysql_append_sql_uses_an_atomic_no_op_duplicate_sentinel() {
        assert!(MYSQL_APPEND_SQL.contains("ON DUPLICATE KEY UPDATE"));
        assert!(MYSQL_APPEND_SQL.contains(&format!("LAST_INSERT_ID({MYSQL_DUPLICATE_SENTINEL})")));
        assert!(MYSQL_APPEND_SQL.contains("LAST_INSERT_ID(2)"));
        assert!(MYSQL_APPEND_SQL.contains("BINARY instance_id = BINARY VALUES(instance_id)"));
        assert!(MYSQL_APPEND_SQL.contains("BINARY request_id = BINARY VALUES(request_id)"));
        assert!(!MYSQL_APPEND_SQL.contains("INSERT IGNORE"));
        assert!(!MYSQL_APPEND_SQL.contains("VALUES(id)"));
    }

    #[test]
    fn mysql_append_result_distinguishes_insert_from_duplicate_sentinel() {
        assert!(mysql_append_was_inserted(0).expect("zero should identify a new insert"));
        assert!(!mysql_append_was_inserted(MYSQL_DUPLICATE_SENTINEL)
            .expect("the duplicate sentinel should identify an idempotent no-op"));
        let error = mysql_append_was_inserted(2)
            .expect_err("a primary key collision must not be treated as idempotent");
        assert!(matches!(error, DataLayerError::UnexpectedValue(_)));
        assert!(error.to_string().contains("primary key collision"));
    }

    #[tokio::test]
    async fn mysql_append_preserves_case_distinct_business_keys_and_rejects_primary_key_reuse_when_url_is_set(
    ) {
        let Some(database_url) = std::env::var("AETHER_TEST_MYSQL_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            eprintln!("skipping MySQL relay profit integration test because AETHER_TEST_MYSQL_URL is unset");
            return;
        };

        let pool = MySqlPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .expect("MySQL profit test pool should connect");
        crate::lifecycle::migrate::run_mysql_migrations(&pool)
            .await
            .expect("MySQL relay profit migrations should run");

        let collations: Vec<(String, String)> = sqlx::query_as(
            "SELECT column_name, collation_name
             FROM information_schema.columns
             WHERE table_schema = DATABASE()
               AND table_name = 'relay_profit_ledger'
               AND column_name IN ('id', 'instance_id', 'request_id', 'channel_id', 'model_id')
             ORDER BY column_name",
        )
        .fetch_all(&pool)
        .await
        .expect("relay profit identifier collations should load");
        assert_eq!(
            collations,
            vec![
                ("channel_id".to_string(), "utf8mb4_bin".to_string()),
                ("id".to_string(), "utf8mb4_bin".to_string()),
                ("instance_id".to_string(), "utf8mb4_bin".to_string()),
                ("model_id".to_string(), "utf8mb4_bin".to_string()),
                ("request_id".to_string(), "utf8mb4_bin".to_string()),
            ],
            "relay profit identifiers must be binary/case-sensitive in MySQL",
        );

        let prefix = format!("profit-ledger-{}", Uuid::new_v4());
        let mut record = known_record();
        record.id = format!("{prefix}-id");
        record.instance_id = format!("{prefix}-instance");
        record.request_id = format!("{prefix}-request");
        record.channel_id = format!("{prefix}-channel");
        record.model_id = format!("{prefix}-model");

        let mut business_key_duplicate = record.clone();
        business_key_duplicate.id = format!("{prefix}-business-duplicate");
        let mut case_distinct = record.clone();
        case_distinct.id = format!("{prefix}-case-distinct");
        case_distinct.instance_id = record.instance_id.to_ascii_uppercase();
        case_distinct.request_id = record.request_id.to_ascii_uppercase();
        let mut primary_key_collision = record.clone();
        primary_key_collision.request_id = format!("{prefix}-different-request");

        let store = RelayProfitLedgerStore {
            backend: RelayProfitLedgerBackend::Mysql(pool.clone()),
        };
        let test_result = async {
            assert!(store
                .append(&record)
                .await
                .expect("first MySQL profit should persist"));
            assert!(!store
                .append(&business_key_duplicate)
                .await
                .expect("the same MySQL business key should be idempotent"));
            assert!(store
                .append(&case_distinct)
                .await
                .expect("case-distinct MySQL business keys should both persist"));
            let error = store
                .append(&primary_key_collision)
                .await
                .expect_err("a reused MySQL primary key with a different business key must fail");
            assert!(matches!(error, DataLayerError::UnexpectedValue(_)));
        };
        test_result.await;

        for id in [&record.id, &business_key_duplicate.id, &case_distinct.id] {
            sqlx::query("DELETE FROM relay_profit_ledger WHERE id = ?")
                .bind(id)
                .execute(&pool)
                .await
                .expect("MySQL relay profit test rows should clean up");
        }
    }
}
