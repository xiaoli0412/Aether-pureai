use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{MySqlPool, SqlitePool};

use crate::{DataBackends, DataLayerError};

const MAX_ID_CHARS: usize = 64;
const MAX_NAME_CHARS: usize = 255;
const MAX_ENDPOINT_CHARS: usize = 8_192;
const MAX_API_KEY_CHARS: usize = 65_535;
const MAX_RAW_DATA_BYTES: usize = 1_048_576;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedRelayDownstreamInstance {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    pub api_key: String,
    pub enabled: bool,
    pub last_sync_at_unix_ms: Option<i64>,
    pub created_at_unix_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedRelaySettlement {
    pub id: String,
    pub downstream_id: String,
    pub period_start_unix_ms: i64,
    pub period_end_unix_ms: i64,
    pub downstream_revenue_usd: f64,
    pub upstream_cost_usd: f64,
    pub difference_usd: f64,
    pub difference_percent: f64,
    pub is_anomaly: bool,
    pub raw_data_json: Option<String>,
    pub created_at_unix_ms: i64,
}

#[derive(sqlx::FromRow)]
struct SqliteDownstreamInstanceRow {
    id: String,
    name: String,
    endpoint: String,
    api_key: String,
    enabled: bool,
    last_sync_at: Option<String>,
    created_at: String,
}

#[derive(Clone)]
enum RelayReconciliationBackend {
    Postgres(sqlx::PgPool),
    Mysql(MySqlPool),
    Sqlite(SqlitePool),
}

/// Durable source of truth for relay downstream instances and reconciliation
/// checkpoints. RuntimeState is deliberately not used as a fallback here:
/// losing a cache must never turn a durable reconciliation history into an
/// empty configuration.
#[derive(Clone)]
pub struct RelayReconciliationStore {
    backend: RelayReconciliationBackend,
}

impl RelayReconciliationStore {
    pub fn from_backends(backends: &DataBackends) -> Option<Self> {
        backends
            .postgres()
            .map(|backend| Self {
                backend: RelayReconciliationBackend::Postgres(backend.pool_clone()),
            })
            .or_else(|| {
                backends.mysql().map(|backend| Self {
                    backend: RelayReconciliationBackend::Mysql(backend.pool_clone()),
                })
            })
            .or_else(|| {
                backends.sqlite().map(|backend| Self {
                    backend: RelayReconciliationBackend::Sqlite(backend.pool_clone()),
                })
            })
    }

    pub fn sqlite(pool: SqlitePool) -> Self {
        Self {
            backend: RelayReconciliationBackend::Sqlite(pool),
        }
    }

    pub async fn insert_downstream_instance(
        &self,
        instance: &PersistedRelayDownstreamInstance,
    ) -> Result<(), DataLayerError> {
        validate_downstream_instance(instance)?;

        match &self.backend {
            RelayReconciliationBackend::Postgres(pool) => sqlx::query(
                "INSERT INTO relay_downstream_instances
                 (id, name, endpoint, api_key, enabled, last_sync_at, created_at)
                 VALUES ($1, $2, $3, $4, $5, to_timestamp($6), to_timestamp($7))",
            )
            .bind(&instance.id)
            .bind(&instance.name)
            .bind(&instance.endpoint)
            .bind(&instance.api_key)
            .bind(instance.enabled)
            .bind(instance.last_sync_at_unix_ms.map(unix_ms_to_seconds))
            .bind(unix_ms_to_seconds(instance.created_at_unix_ms))
            .execute(pool)
            .await
            .map(|_| ()),
            RelayReconciliationBackend::Mysql(pool) => sqlx::query(
                "INSERT INTO relay_downstream_instances
                 (id, name, endpoint, api_key, enabled, last_sync_at, created_at)
                 VALUES (?, ?, ?, ?, ?, FROM_UNIXTIME(?), FROM_UNIXTIME(?))",
            )
            .bind(&instance.id)
            .bind(&instance.name)
            .bind(&instance.endpoint)
            .bind(&instance.api_key)
            .bind(instance.enabled)
            .bind(instance.last_sync_at_unix_ms.map(unix_ms_to_seconds))
            .bind(unix_ms_to_seconds(instance.created_at_unix_ms))
            .execute(pool)
            .await
            .map(|_| ()),
            RelayReconciliationBackend::Sqlite(pool) => sqlx::query(
                "INSERT INTO relay_downstream_instances
                 (id, name, endpoint, api_key, enabled, last_sync_at, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&instance.id)
            .bind(&instance.name)
            .bind(&instance.endpoint)
            .bind(&instance.api_key)
            .bind(instance.enabled)
            .bind(
                instance
                    .last_sync_at_unix_ms
                    .map(format_reconciliation_timestamp)
                    .transpose()?,
            )
            .bind(format_reconciliation_timestamp(
                instance.created_at_unix_ms,
            )?)
            .execute(pool)
            .await
            .map(|_| ()),
        }
        .map_err(DataLayerError::sql)?;

        Ok(())
    }

    pub async fn list_enabled_downstream_instances(
        &self,
    ) -> Result<Vec<PersistedRelayDownstreamInstance>, DataLayerError> {
        match &self.backend {
            RelayReconciliationBackend::Postgres(pool) => {
                let rows: Vec<(String, String, String, String, bool, Option<i64>, i64)> =
                    sqlx::query_as(
                        "SELECT id, name, endpoint, api_key, enabled,
                                CAST(EXTRACT(EPOCH FROM last_sync_at) * 1000 AS BIGINT),
                                CAST(EXTRACT(EPOCH FROM created_at) * 1000 AS BIGINT)
                         FROM relay_downstream_instances
                         WHERE enabled = TRUE
                         ORDER BY created_at ASC, id ASC",
                    )
                    .fetch_all(pool)
                    .await
                    .map_err(DataLayerError::sql)?;
                Ok(rows
                    .into_iter()
                    .map(
                        |(
                            id,
                            name,
                            endpoint,
                            api_key,
                            enabled,
                            last_sync_at_unix_ms,
                            created_at_unix_ms,
                        )| PersistedRelayDownstreamInstance {
                            id,
                            name,
                            endpoint,
                            api_key,
                            enabled,
                            last_sync_at_unix_ms,
                            created_at_unix_ms,
                        },
                    )
                    .collect())
            }
            RelayReconciliationBackend::Mysql(pool) => {
                let rows: Vec<(String, String, String, String, bool, Option<i64>, i64)> =
                    sqlx::query_as(
                        "SELECT id, name, endpoint, api_key, enabled,
                                CAST(UNIX_TIMESTAMP(last_sync_at) * 1000 AS SIGNED),
                                CAST(UNIX_TIMESTAMP(created_at) * 1000 AS SIGNED)
                         FROM relay_downstream_instances
                         WHERE enabled = TRUE
                         ORDER BY created_at ASC, id ASC",
                    )
                    .fetch_all(pool)
                    .await
                    .map_err(DataLayerError::sql)?;
                Ok(rows
                    .into_iter()
                    .map(
                        |(
                            id,
                            name,
                            endpoint,
                            api_key,
                            enabled,
                            last_sync_at_unix_ms,
                            created_at_unix_ms,
                        )| PersistedRelayDownstreamInstance {
                            id,
                            name,
                            endpoint,
                            api_key,
                            enabled,
                            last_sync_at_unix_ms,
                            created_at_unix_ms,
                        },
                    )
                    .collect())
            }
            RelayReconciliationBackend::Sqlite(pool) => {
                let rows: Vec<SqliteDownstreamInstanceRow> = sqlx::query_as(
                    "SELECT id, name, endpoint, api_key, enabled, last_sync_at, created_at
                     FROM relay_downstream_instances
                     WHERE enabled = 1
                     ORDER BY created_at ASC, id ASC",
                )
                .fetch_all(pool)
                .await
                .map_err(DataLayerError::sql)?;
                rows.into_iter().map(sqlite_row_to_instance).collect()
            }
        }
    }

    pub async fn delete_downstream_instance(&self, id: &str) -> Result<bool, DataLayerError> {
        validate_identifier("id", id, MAX_ID_CHARS)?;

        let rows_affected = match &self.backend {
            RelayReconciliationBackend::Postgres(pool) => {
                sqlx::query("DELETE FROM relay_downstream_instances WHERE id = $1")
                    .bind(id)
                    .execute(pool)
                    .await
                    .map(|result| result.rows_affected())
            }
            RelayReconciliationBackend::Mysql(pool) => {
                sqlx::query("DELETE FROM relay_downstream_instances WHERE id = ?")
                    .bind(id)
                    .execute(pool)
                    .await
                    .map(|result| result.rows_affected())
            }
            RelayReconciliationBackend::Sqlite(pool) => {
                sqlx::query("DELETE FROM relay_downstream_instances WHERE id = ?")
                    .bind(id)
                    .execute(pool)
                    .await
                    .map(|result| result.rows_affected())
            }
        }
        .map_err(DataLayerError::sql)?;

        Ok(rows_affected > 0)
    }

    /// Commits the settlement and checkpoint together. A failed settlement
    /// write must not advance the cursor and cause an unrecoverable gap.
    pub async fn record_settlement_and_advance_sync(
        &self,
        settlement: &PersistedRelaySettlement,
    ) -> Result<(), DataLayerError> {
        validate_settlement(settlement)?;

        match &self.backend {
            RelayReconciliationBackend::Postgres(pool) => {
                let mut transaction = pool.begin().await.map_err(DataLayerError::sql)?;
                let updated = sqlx::query(
                    "UPDATE relay_downstream_instances
                     SET last_sync_at = to_timestamp($1)
                     WHERE id = $2",
                )
                .bind(unix_ms_to_seconds(settlement.period_end_unix_ms))
                .bind(&settlement.downstream_id)
                .execute(&mut *transaction)
                .await
                .map_err(DataLayerError::sql)?;
                ensure_downstream_exists(updated.rows_affected(), &settlement.downstream_id)?;
                sqlx::query(
                    "INSERT INTO relay_settlements
                     (id, downstream_id, period_start, period_end, downstream_revenue_usd,
                      upstream_cost_usd, difference_usd, difference_percent, is_anomaly,
                      raw_data_json, created_at)
                     VALUES ($1, $2, to_timestamp($3), to_timestamp($4), $5, $6, $7, $8, $9, $10,
                             to_timestamp($11))",
                )
                .bind(&settlement.id)
                .bind(&settlement.downstream_id)
                .bind(unix_ms_to_seconds(settlement.period_start_unix_ms))
                .bind(unix_ms_to_seconds(settlement.period_end_unix_ms))
                .bind(settlement.downstream_revenue_usd)
                .bind(settlement.upstream_cost_usd)
                .bind(settlement.difference_usd)
                .bind(settlement.difference_percent)
                .bind(settlement.is_anomaly)
                .bind(&settlement.raw_data_json)
                .bind(unix_ms_to_seconds(settlement.created_at_unix_ms))
                .execute(&mut *transaction)
                .await
                .map_err(DataLayerError::sql)?;
                transaction.commit().await.map_err(DataLayerError::sql)?;
            }
            RelayReconciliationBackend::Mysql(pool) => {
                let mut transaction = pool.begin().await.map_err(DataLayerError::sql)?;
                let updated = sqlx::query(
                    "UPDATE relay_downstream_instances
                     SET last_sync_at = FROM_UNIXTIME(?)
                     WHERE id = ?",
                )
                .bind(unix_ms_to_seconds(settlement.period_end_unix_ms))
                .bind(&settlement.downstream_id)
                .execute(&mut *transaction)
                .await
                .map_err(DataLayerError::sql)?;
                ensure_downstream_exists(updated.rows_affected(), &settlement.downstream_id)?;
                sqlx::query(
                    "INSERT INTO relay_settlements
                     (id, downstream_id, period_start, period_end, downstream_revenue_usd,
                      upstream_cost_usd, difference_usd, difference_percent, is_anomaly,
                      raw_data_json, created_at)
                     VALUES (?, ?, FROM_UNIXTIME(?), FROM_UNIXTIME(?), ?, ?, ?, ?, ?, ?,
                             FROM_UNIXTIME(?))",
                )
                .bind(&settlement.id)
                .bind(&settlement.downstream_id)
                .bind(unix_ms_to_seconds(settlement.period_start_unix_ms))
                .bind(unix_ms_to_seconds(settlement.period_end_unix_ms))
                .bind(settlement.downstream_revenue_usd)
                .bind(settlement.upstream_cost_usd)
                .bind(settlement.difference_usd)
                .bind(settlement.difference_percent)
                .bind(settlement.is_anomaly)
                .bind(&settlement.raw_data_json)
                .bind(unix_ms_to_seconds(settlement.created_at_unix_ms))
                .execute(&mut *transaction)
                .await
                .map_err(DataLayerError::sql)?;
                transaction.commit().await.map_err(DataLayerError::sql)?;
            }
            RelayReconciliationBackend::Sqlite(pool) => {
                let mut transaction = pool.begin().await.map_err(DataLayerError::sql)?;
                let updated = sqlx::query(
                    "UPDATE relay_downstream_instances
                     SET last_sync_at = ?
                     WHERE id = ?",
                )
                .bind(format_reconciliation_timestamp(
                    settlement.period_end_unix_ms,
                )?)
                .bind(&settlement.downstream_id)
                .execute(&mut *transaction)
                .await
                .map_err(DataLayerError::sql)?;
                ensure_downstream_exists(updated.rows_affected(), &settlement.downstream_id)?;
                sqlx::query(
                    "INSERT INTO relay_settlements
                     (id, downstream_id, period_start, period_end, downstream_revenue_usd,
                      upstream_cost_usd, difference_usd, difference_percent, is_anomaly,
                      raw_data_json, created_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&settlement.id)
                .bind(&settlement.downstream_id)
                .bind(format_reconciliation_timestamp(
                    settlement.period_start_unix_ms,
                )?)
                .bind(format_reconciliation_timestamp(
                    settlement.period_end_unix_ms,
                )?)
                .bind(settlement.downstream_revenue_usd)
                .bind(settlement.upstream_cost_usd)
                .bind(settlement.difference_usd)
                .bind(settlement.difference_percent)
                .bind(settlement.is_anomaly)
                .bind(&settlement.raw_data_json)
                .bind(format_reconciliation_timestamp(
                    settlement.created_at_unix_ms,
                )?)
                .execute(&mut *transaction)
                .await
                .map_err(DataLayerError::sql)?;
                transaction.commit().await.map_err(DataLayerError::sql)?;
            }
        }

        Ok(())
    }
}

fn sqlite_row_to_instance(
    row: SqliteDownstreamInstanceRow,
) -> Result<PersistedRelayDownstreamInstance, DataLayerError> {
    let instance = PersistedRelayDownstreamInstance {
        id: row.id,
        name: row.name,
        endpoint: row.endpoint,
        api_key: row.api_key,
        enabled: row.enabled,
        last_sync_at_unix_ms: row
            .last_sync_at
            .as_deref()
            .map(|value| parse_reconciliation_timestamp(value, "last_sync_at"))
            .transpose()?,
        created_at_unix_ms: parse_reconciliation_timestamp(&row.created_at, "created_at")?,
    };
    validate_downstream_instance(&instance)?;
    Ok(instance)
}

fn validate_downstream_instance(
    instance: &PersistedRelayDownstreamInstance,
) -> Result<(), DataLayerError> {
    validate_identifier("id", &instance.id, MAX_ID_CHARS)?;
    validate_identifier("name", &instance.name, MAX_NAME_CHARS)?;
    validate_identifier("endpoint", &instance.endpoint, MAX_ENDPOINT_CHARS)?;
    validate_identifier("api_key", &instance.api_key, MAX_API_KEY_CHARS)?;
    validate_unix_ms("created_at_unix_ms", instance.created_at_unix_ms)?;
    if let Some(last_sync_at_unix_ms) = instance.last_sync_at_unix_ms {
        validate_unix_ms("last_sync_at_unix_ms", last_sync_at_unix_ms)?;
    }
    Ok(())
}

fn validate_settlement(settlement: &PersistedRelaySettlement) -> Result<(), DataLayerError> {
    validate_identifier("id", &settlement.id, MAX_ID_CHARS)?;
    validate_identifier("downstream_id", &settlement.downstream_id, MAX_ID_CHARS)?;
    validate_unix_ms("period_start_unix_ms", settlement.period_start_unix_ms)?;
    validate_unix_ms("period_end_unix_ms", settlement.period_end_unix_ms)?;
    validate_unix_ms("created_at_unix_ms", settlement.created_at_unix_ms)?;
    if settlement.period_start_unix_ms >= settlement.period_end_unix_ms {
        return Err(DataLayerError::InvalidInput(
            "relay settlement period must have a positive duration".to_string(),
        ));
    }
    validate_non_negative_finite("downstream_revenue_usd", settlement.downstream_revenue_usd)?;
    validate_non_negative_finite("upstream_cost_usd", settlement.upstream_cost_usd)?;
    validate_finite("difference_usd", settlement.difference_usd)?;
    validate_finite("difference_percent", settlement.difference_percent)?;
    if let Some(raw_data_json) = &settlement.raw_data_json {
        if raw_data_json.len() > MAX_RAW_DATA_BYTES {
            return Err(DataLayerError::InvalidInput(
                "relay settlement raw_data_json is too large".to_string(),
            ));
        }
        serde_json::from_str::<serde_json::Value>(raw_data_json).map_err(|_| {
            DataLayerError::InvalidInput(
                "relay settlement raw_data_json must be valid JSON".to_string(),
            )
        })?;
    }
    Ok(())
}

fn validate_identifier(field: &str, value: &str, max_chars: usize) -> Result<(), DataLayerError> {
    if value.trim().is_empty() {
        return Err(DataLayerError::InvalidInput(format!(
            "relay reconciliation {field} must not be blank"
        )));
    }
    if value.contains('\0') {
        return Err(DataLayerError::InvalidInput(format!(
            "relay reconciliation {field} must not contain NUL bytes"
        )));
    }
    if value.chars().count() > max_chars {
        return Err(DataLayerError::InvalidInput(format!(
            "relay reconciliation {field} exceeds the maximum length of {max_chars} characters"
        )));
    }
    Ok(())
}

fn validate_unix_ms(field: &str, value: i64) -> Result<(), DataLayerError> {
    if value < 0 || DateTime::<Utc>::from_timestamp_millis(value).is_none() {
        return Err(DataLayerError::InvalidInput(format!(
            "relay reconciliation {field} must be a supported non-negative Unix timestamp"
        )));
    }
    Ok(())
}

fn validate_non_negative_finite(field: &str, value: f64) -> Result<(), DataLayerError> {
    validate_finite(field, value)?;
    if value < 0.0 {
        return Err(DataLayerError::InvalidInput(format!(
            "relay reconciliation {field} must be non-negative"
        )));
    }
    Ok(())
}

fn validate_finite(field: &str, value: f64) -> Result<(), DataLayerError> {
    if !value.is_finite() {
        return Err(DataLayerError::InvalidInput(format!(
            "relay reconciliation {field} must be finite"
        )));
    }
    Ok(())
}

fn ensure_downstream_exists(rows_affected: u64, downstream_id: &str) -> Result<(), DataLayerError> {
    if rows_affected == 1 {
        return Ok(());
    }
    Err(DataLayerError::UnexpectedValue(format!(
        "relay reconciliation downstream instance {downstream_id} does not exist"
    )))
}

fn unix_ms_to_seconds(value: i64) -> f64 {
    value as f64 / 1_000.0
}

fn format_reconciliation_timestamp(value: i64) -> Result<String, DataLayerError> {
    DateTime::<Utc>::from_timestamp_millis(value)
        .ok_or_else(|| {
            DataLayerError::InvalidInput(
                "relay reconciliation timestamp is outside the supported range".to_string(),
            )
        })
        .map(|timestamp| timestamp.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn parse_reconciliation_timestamp(value: &str, field: &str) -> Result<i64, DataLayerError> {
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.timestamp_millis())
        .map_err(|_| {
            DataLayerError::UnexpectedValue(format!(
                "relay reconciliation {field} is not an RFC3339 timestamp"
            ))
        })
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqlitePoolOptions;

    use super::{
        PersistedRelayDownstreamInstance, PersistedRelaySettlement, RelayReconciliationStore,
    };

    async fn test_store() -> RelayReconciliationStore {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite test pool should connect");
        sqlx::query(
            "CREATE TABLE relay_downstream_instances (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                endpoint TEXT NOT NULL,
                api_key TEXT NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                last_sync_at TEXT,
                created_at TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .expect("downstream instance table should be created");
        sqlx::query(
            "CREATE TABLE relay_settlements (
                id TEXT PRIMARY KEY,
                downstream_id TEXT NOT NULL REFERENCES relay_downstream_instances(id) ON DELETE CASCADE,
                period_start TEXT NOT NULL,
                period_end TEXT NOT NULL,
                downstream_revenue_usd REAL NOT NULL,
                upstream_cost_usd REAL NOT NULL,
                difference_usd REAL NOT NULL,
                difference_percent REAL NOT NULL,
                is_anomaly INTEGER NOT NULL DEFAULT 0,
                raw_data_json TEXT,
                created_at TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .expect("settlement table should be created");
        RelayReconciliationStore::sqlite(pool)
    }

    fn downstream_instance() -> PersistedRelayDownstreamInstance {
        PersistedRelayDownstreamInstance {
            id: "downstream-a".to_string(),
            name: "Downstream A".to_string(),
            endpoint: "https://new-api.example.test".to_string(),
            api_key: "test-secret".to_string(),
            enabled: true,
            last_sync_at_unix_ms: None,
            created_at_unix_ms: 1_000,
        }
    }

    fn settlement(id: &str, period_end_unix_ms: i64) -> PersistedRelaySettlement {
        PersistedRelaySettlement {
            id: id.to_string(),
            downstream_id: "downstream-a".to_string(),
            period_start_unix_ms: 1_000,
            period_end_unix_ms,
            downstream_revenue_usd: 1.25,
            upstream_cost_usd: 0.75,
            difference_usd: 0.5,
            difference_percent: 66.666_666_666_7,
            is_anomaly: false,
            raw_data_json: Some("{\"requests\":1}".to_string()),
            created_at_unix_ms: period_end_unix_ms,
        }
    }

    #[tokio::test]
    async fn downstream_instances_survive_a_fresh_store_without_runtime_state() {
        let store = test_store().await;
        store
            .insert_downstream_instance(&downstream_instance())
            .await
            .expect("downstream instance should persist");

        let loaded = store
            .list_enabled_downstream_instances()
            .await
            .expect("durable downstream instance should load");

        assert_eq!(loaded, vec![downstream_instance()]);
    }

    #[tokio::test]
    async fn settlement_and_checkpoint_commit_or_roll_back_together() {
        let store = test_store().await;
        store
            .insert_downstream_instance(&downstream_instance())
            .await
            .expect("downstream instance should persist");
        store
            .record_settlement_and_advance_sync(&settlement("settlement-a", 2_000))
            .await
            .expect("settlement and checkpoint should commit");

        let after_first_write = store
            .list_enabled_downstream_instances()
            .await
            .expect("checkpoint should load");
        assert_eq!(after_first_write[0].last_sync_at_unix_ms, Some(2_000));

        let error = store
            .record_settlement_and_advance_sync(&settlement("settlement-a", 3_000))
            .await
            .expect_err("duplicate settlement must fail after the tentative checkpoint update");
        assert!(error.to_string().contains("UNIQUE"));

        let after_failed_write = store
            .list_enabled_downstream_instances()
            .await
            .expect("rolled-back checkpoint should load");
        assert_eq!(after_failed_write[0].last_sync_at_unix_ms, Some(2_000));
    }

    #[tokio::test]
    async fn deleting_downstream_instance_removes_the_durable_source_row() {
        let store = test_store().await;
        store
            .insert_downstream_instance(&downstream_instance())
            .await
            .expect("downstream instance should persist");

        assert!(store
            .delete_downstream_instance("downstream-a")
            .await
            .expect("delete should succeed"));
        assert!(store
            .list_enabled_downstream_instances()
            .await
            .expect("list should succeed")
            .is_empty());
    }
}
