use serde::{Deserialize, Serialize};
use sqlx::{MySqlPool, SqlitePool};

use crate::{DataBackends, DataLayerError};

const MAX_ID_CHARS: usize = 64;
const MAX_NAME_CHARS: usize = 255;
const MAX_TIMESTAMP_CHARS: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedRelayDownstreamGroup {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub parent_id: Option<String>,
    pub global_ratio_multiplier: f64,
    pub model_whitelist_json: String,
    pub model_blacklist_json: String,
    pub model_ratio_overrides_json: String,
    pub priority: i32,
    pub requests_per_minute: i64,
    pub requests_per_day: i64,
    pub daily_quota_limit: f64,
    pub monthly_quota_limit: f64,
    pub time_rules_json: String,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(sqlx::FromRow)]
struct RelayDownstreamGroupRow {
    id: String,
    name: String,
    description: Option<String>,
    parent_id: Option<String>,
    global_ratio_multiplier: f64,
    model_whitelist_json: String,
    model_blacklist_json: String,
    model_ratio_overrides_json: String,
    priority: i32,
    requests_per_minute: i64,
    requests_per_day: i64,
    daily_quota_limit: f64,
    monthly_quota_limit: f64,
    time_rules_json: String,
    enabled: bool,
    created_at: String,
    updated_at: String,
}

impl From<RelayDownstreamGroupRow> for PersistedRelayDownstreamGroup {
    fn from(row: RelayDownstreamGroupRow) -> Self {
        Self {
            id: row.id,
            name: row.name,
            description: row.description,
            parent_id: row.parent_id,
            global_ratio_multiplier: row.global_ratio_multiplier,
            model_whitelist_json: row.model_whitelist_json,
            model_blacklist_json: row.model_blacklist_json,
            model_ratio_overrides_json: row.model_ratio_overrides_json,
            priority: row.priority,
            requests_per_minute: row.requests_per_minute,
            requests_per_day: row.requests_per_day,
            daily_quota_limit: row.daily_quota_limit,
            monthly_quota_limit: row.monthly_quota_limit,
            time_rules_json: row.time_rules_json,
            enabled: row.enabled,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

#[derive(Clone)]
enum RelayDownstreamGroupBackend {
    Postgres(sqlx::PgPool),
    Mysql(MySqlPool),
    Sqlite(SqlitePool),
}

#[derive(Clone)]
pub struct RelayDownstreamGroupStore {
    backend: RelayDownstreamGroupBackend,
}

impl RelayDownstreamGroupStore {
    pub fn from_backends(backends: &DataBackends) -> Option<Self> {
        backends
            .postgres()
            .map(|backend| Self {
                backend: RelayDownstreamGroupBackend::Postgres(backend.pool_clone()),
            })
            .or_else(|| {
                backends.mysql().map(|backend| Self {
                    backend: RelayDownstreamGroupBackend::Mysql(backend.pool_clone()),
                })
            })
            .or_else(|| {
                backends.sqlite().map(|backend| Self {
                    backend: RelayDownstreamGroupBackend::Sqlite(backend.pool_clone()),
                })
            })
    }

    pub fn sqlite(pool: SqlitePool) -> Self {
        Self {
            backend: RelayDownstreamGroupBackend::Sqlite(pool),
        }
    }

    pub async fn insert(
        &self,
        group: &PersistedRelayDownstreamGroup,
    ) -> Result<(), DataLayerError> {
        validate_group(group)?;

        match &self.backend {
            RelayDownstreamGroupBackend::Postgres(pool) => sqlx::query(
                "INSERT INTO relay_downstream_groups (
                    id, name, description, parent_id, global_ratio_multiplier,
                    model_whitelist_json, model_blacklist_json, model_ratio_overrides_json,
                    priority, requests_per_minute, requests_per_day, daily_quota_limit,
                    monthly_quota_limit, time_rules_json, enabled, created_at, updated_at
                 ) VALUES (
                    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17
                 )",
            )
            .bind(&group.id)
            .bind(&group.name)
            .bind(&group.description)
            .bind(&group.parent_id)
            .bind(group.global_ratio_multiplier)
            .bind(&group.model_whitelist_json)
            .bind(&group.model_blacklist_json)
            .bind(&group.model_ratio_overrides_json)
            .bind(group.priority)
            .bind(group.requests_per_minute)
            .bind(group.requests_per_day)
            .bind(group.daily_quota_limit)
            .bind(group.monthly_quota_limit)
            .bind(&group.time_rules_json)
            .bind(group.enabled)
            .bind(&group.created_at)
            .bind(&group.updated_at)
            .execute(pool)
            .await
            .map(|_| ()),
            RelayDownstreamGroupBackend::Mysql(pool) => sqlx::query(
                "INSERT INTO relay_downstream_groups (
                    id, name, description, parent_id, global_ratio_multiplier,
                    model_whitelist_json, model_blacklist_json, model_ratio_overrides_json,
                    priority, requests_per_minute, requests_per_day, daily_quota_limit,
                    monthly_quota_limit, time_rules_json, enabled, created_at, updated_at
                 ) VALUES (
                    ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
                 )",
            )
            .bind(&group.id)
            .bind(&group.name)
            .bind(&group.description)
            .bind(&group.parent_id)
            .bind(group.global_ratio_multiplier)
            .bind(&group.model_whitelist_json)
            .bind(&group.model_blacklist_json)
            .bind(&group.model_ratio_overrides_json)
            .bind(group.priority)
            .bind(group.requests_per_minute)
            .bind(group.requests_per_day)
            .bind(group.daily_quota_limit)
            .bind(group.monthly_quota_limit)
            .bind(&group.time_rules_json)
            .bind(group.enabled)
            .bind(&group.created_at)
            .bind(&group.updated_at)
            .execute(pool)
            .await
            .map(|_| ()),
            RelayDownstreamGroupBackend::Sqlite(pool) => sqlx::query(
                "INSERT INTO relay_downstream_groups (
                    id, name, description, parent_id, global_ratio_multiplier,
                    model_whitelist_json, model_blacklist_json, model_ratio_overrides_json,
                    priority, requests_per_minute, requests_per_day, daily_quota_limit,
                    monthly_quota_limit, time_rules_json, enabled, created_at, updated_at
                 ) VALUES (
                    ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
                 )",
            )
            .bind(&group.id)
            .bind(&group.name)
            .bind(&group.description)
            .bind(&group.parent_id)
            .bind(group.global_ratio_multiplier)
            .bind(&group.model_whitelist_json)
            .bind(&group.model_blacklist_json)
            .bind(&group.model_ratio_overrides_json)
            .bind(group.priority)
            .bind(group.requests_per_minute)
            .bind(group.requests_per_day)
            .bind(group.daily_quota_limit)
            .bind(group.monthly_quota_limit)
            .bind(&group.time_rules_json)
            .bind(group.enabled)
            .bind(&group.created_at)
            .bind(&group.updated_at)
            .execute(pool)
            .await
            .map(|_| ()),
        }
        .map_err(DataLayerError::sql)?;

        Ok(())
    }

    pub async fn replace(
        &self,
        group: &PersistedRelayDownstreamGroup,
    ) -> Result<bool, DataLayerError> {
        validate_group(group)?;

        let result = match &self.backend {
            RelayDownstreamGroupBackend::Postgres(pool) => sqlx::query(
                "UPDATE relay_downstream_groups
                 SET name = $1, description = $2, parent_id = $3,
                     global_ratio_multiplier = $4, model_whitelist_json = $5,
                     model_blacklist_json = $6, model_ratio_overrides_json = $7,
                     priority = $8, requests_per_minute = $9, requests_per_day = $10,
                     daily_quota_limit = $11, monthly_quota_limit = $12,
                     time_rules_json = $13, enabled = $14, created_at = $15, updated_at = $16
                 WHERE id = $17",
            )
            .bind(&group.name)
            .bind(&group.description)
            .bind(&group.parent_id)
            .bind(group.global_ratio_multiplier)
            .bind(&group.model_whitelist_json)
            .bind(&group.model_blacklist_json)
            .bind(&group.model_ratio_overrides_json)
            .bind(group.priority)
            .bind(group.requests_per_minute)
            .bind(group.requests_per_day)
            .bind(group.daily_quota_limit)
            .bind(group.monthly_quota_limit)
            .bind(&group.time_rules_json)
            .bind(group.enabled)
            .bind(&group.created_at)
            .bind(&group.updated_at)
            .bind(&group.id)
            .execute(pool)
            .await
            .map(|result| result.rows_affected()),
            RelayDownstreamGroupBackend::Mysql(pool) => sqlx::query(
                "UPDATE relay_downstream_groups
                 SET name = ?, description = ?, parent_id = ?,
                     global_ratio_multiplier = ?, model_whitelist_json = ?,
                     model_blacklist_json = ?, model_ratio_overrides_json = ?,
                     priority = ?, requests_per_minute = ?, requests_per_day = ?,
                     daily_quota_limit = ?, monthly_quota_limit = ?,
                     time_rules_json = ?, enabled = ?, created_at = ?, updated_at = ?
                 WHERE id = ?",
            )
            .bind(&group.name)
            .bind(&group.description)
            .bind(&group.parent_id)
            .bind(group.global_ratio_multiplier)
            .bind(&group.model_whitelist_json)
            .bind(&group.model_blacklist_json)
            .bind(&group.model_ratio_overrides_json)
            .bind(group.priority)
            .bind(group.requests_per_minute)
            .bind(group.requests_per_day)
            .bind(group.daily_quota_limit)
            .bind(group.monthly_quota_limit)
            .bind(&group.time_rules_json)
            .bind(group.enabled)
            .bind(&group.created_at)
            .bind(&group.updated_at)
            .bind(&group.id)
            .execute(pool)
            .await
            .map(|result| result.rows_affected()),
            RelayDownstreamGroupBackend::Sqlite(pool) => sqlx::query(
                "UPDATE relay_downstream_groups
                 SET name = ?, description = ?, parent_id = ?,
                     global_ratio_multiplier = ?, model_whitelist_json = ?,
                     model_blacklist_json = ?, model_ratio_overrides_json = ?,
                     priority = ?, requests_per_minute = ?, requests_per_day = ?,
                     daily_quota_limit = ?, monthly_quota_limit = ?,
                     time_rules_json = ?, enabled = ?, created_at = ?, updated_at = ?
                 WHERE id = ?",
            )
            .bind(&group.name)
            .bind(&group.description)
            .bind(&group.parent_id)
            .bind(group.global_ratio_multiplier)
            .bind(&group.model_whitelist_json)
            .bind(&group.model_blacklist_json)
            .bind(&group.model_ratio_overrides_json)
            .bind(group.priority)
            .bind(group.requests_per_minute)
            .bind(group.requests_per_day)
            .bind(group.daily_quota_limit)
            .bind(group.monthly_quota_limit)
            .bind(&group.time_rules_json)
            .bind(group.enabled)
            .bind(&group.created_at)
            .bind(&group.updated_at)
            .bind(&group.id)
            .execute(pool)
            .await
            .map(|result| result.rows_affected()),
        }
        .map_err(DataLayerError::sql)?;

        Ok(result > 0)
    }

    pub async fn get(
        &self,
        group_id: &str,
    ) -> Result<Option<PersistedRelayDownstreamGroup>, DataLayerError> {
        validate_identifier("id", group_id, MAX_ID_CHARS)?;

        let row: Option<RelayDownstreamGroupRow> = match &self.backend {
            RelayDownstreamGroupBackend::Postgres(pool) => {
                sqlx::query_as(
                    "SELECT id, name, description, parent_id, global_ratio_multiplier,
                        model_whitelist_json, model_blacklist_json, model_ratio_overrides_json,
                        priority, requests_per_minute, requests_per_day, daily_quota_limit,
                        monthly_quota_limit, time_rules_json, enabled, created_at, updated_at
                 FROM relay_downstream_groups
                 WHERE id = $1",
                )
                .bind(group_id)
                .fetch_optional(pool)
                .await
            }
            RelayDownstreamGroupBackend::Mysql(pool) => {
                sqlx::query_as(
                    "SELECT id, name, description, parent_id, global_ratio_multiplier,
                        model_whitelist_json, model_blacklist_json, model_ratio_overrides_json,
                        priority, requests_per_minute, requests_per_day, daily_quota_limit,
                        monthly_quota_limit, time_rules_json, enabled, created_at, updated_at
                 FROM relay_downstream_groups
                 WHERE id = ?",
                )
                .bind(group_id)
                .fetch_optional(pool)
                .await
            }
            RelayDownstreamGroupBackend::Sqlite(pool) => {
                sqlx::query_as(
                    "SELECT id, name, description, parent_id, global_ratio_multiplier,
                        model_whitelist_json, model_blacklist_json, model_ratio_overrides_json,
                        priority, requests_per_minute, requests_per_day, daily_quota_limit,
                        monthly_quota_limit, time_rules_json, enabled, created_at, updated_at
                 FROM relay_downstream_groups
                 WHERE id = ?",
                )
                .bind(group_id)
                .fetch_optional(pool)
                .await
            }
        }
        .map_err(DataLayerError::sql)?;

        Ok(row.map(Into::into))
    }

    pub async fn list(&self) -> Result<Vec<PersistedRelayDownstreamGroup>, DataLayerError> {
        let rows: Vec<RelayDownstreamGroupRow> = match &self.backend {
            RelayDownstreamGroupBackend::Postgres(pool) => {
                sqlx::query_as(
                    "SELECT id, name, description, parent_id, global_ratio_multiplier,
                        model_whitelist_json, model_blacklist_json, model_ratio_overrides_json,
                        priority, requests_per_minute, requests_per_day, daily_quota_limit,
                        monthly_quota_limit, time_rules_json, enabled, created_at, updated_at
                 FROM relay_downstream_groups
                 ORDER BY priority DESC, name ASC, id ASC",
                )
                .fetch_all(pool)
                .await
            }
            RelayDownstreamGroupBackend::Mysql(pool) => {
                sqlx::query_as(
                    "SELECT id, name, description, parent_id, global_ratio_multiplier,
                        model_whitelist_json, model_blacklist_json, model_ratio_overrides_json,
                        priority, requests_per_minute, requests_per_day, daily_quota_limit,
                        monthly_quota_limit, time_rules_json, enabled, created_at, updated_at
                 FROM relay_downstream_groups
                 ORDER BY priority DESC, name ASC, id ASC",
                )
                .fetch_all(pool)
                .await
            }
            RelayDownstreamGroupBackend::Sqlite(pool) => {
                sqlx::query_as(
                    "SELECT id, name, description, parent_id, global_ratio_multiplier,
                        model_whitelist_json, model_blacklist_json, model_ratio_overrides_json,
                        priority, requests_per_minute, requests_per_day, daily_quota_limit,
                        monthly_quota_limit, time_rules_json, enabled, created_at, updated_at
                 FROM relay_downstream_groups
                 ORDER BY priority DESC, name ASC, id ASC",
                )
                .fetch_all(pool)
                .await
            }
        }
        .map_err(DataLayerError::sql)?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    pub async fn delete(&self, group_id: &str) -> Result<bool, DataLayerError> {
        validate_identifier("id", group_id, MAX_ID_CHARS)?;

        let result = match &self.backend {
            RelayDownstreamGroupBackend::Postgres(pool) => {
                sqlx::query("DELETE FROM relay_downstream_groups WHERE id = $1")
                    .bind(group_id)
                    .execute(pool)
                    .await
                    .map(|result| result.rows_affected())
            }
            RelayDownstreamGroupBackend::Mysql(pool) => {
                sqlx::query("DELETE FROM relay_downstream_groups WHERE id = ?")
                    .bind(group_id)
                    .execute(pool)
                    .await
                    .map(|result| result.rows_affected())
            }
            RelayDownstreamGroupBackend::Sqlite(pool) => {
                sqlx::query("DELETE FROM relay_downstream_groups WHERE id = ?")
                    .bind(group_id)
                    .execute(pool)
                    .await
                    .map(|result| result.rows_affected())
            }
        }
        .map_err(DataLayerError::sql)?;

        Ok(result > 0)
    }
}

fn validate_group(group: &PersistedRelayDownstreamGroup) -> Result<(), DataLayerError> {
    validate_identifier("id", &group.id, MAX_ID_CHARS)?;
    validate_identifier("name", &group.name, MAX_NAME_CHARS)?;
    validate_identifier("created_at", &group.created_at, MAX_TIMESTAMP_CHARS)?;
    validate_identifier("updated_at", &group.updated_at, MAX_TIMESTAMP_CHARS)?;
    if !group.global_ratio_multiplier.is_finite()
        || !group.daily_quota_limit.is_finite()
        || !group.monthly_quota_limit.is_finite()
    {
        return Err(DataLayerError::InvalidInput(
            "relay downstream group numeric fields must be finite".to_string(),
        ));
    }
    if group.requests_per_minute < 0 || group.requests_per_day < 0 {
        return Err(DataLayerError::InvalidInput(
            "relay downstream group request limits must be non-negative".to_string(),
        ));
    }
    validate_json_array("model_whitelist_json", &group.model_whitelist_json)?;
    validate_json_array("model_blacklist_json", &group.model_blacklist_json)?;
    validate_json_object(
        "model_ratio_overrides_json",
        &group.model_ratio_overrides_json,
    )?;
    validate_json_array("time_rules_json", &group.time_rules_json)?;
    Ok(())
}

fn validate_identifier(field: &str, value: &str, max_chars: usize) -> Result<(), DataLayerError> {
    if value.trim().is_empty() {
        return Err(DataLayerError::InvalidInput(format!(
            "relay downstream group {field} must not be blank"
        )));
    }
    if value.chars().count() > max_chars {
        return Err(DataLayerError::InvalidInput(format!(
            "relay downstream group {field} exceeds the maximum length of {max_chars} characters"
        )));
    }
    Ok(())
}

fn validate_json_array(field: &str, value: &str) -> Result<(), DataLayerError> {
    let parsed: serde_json::Value = serde_json::from_str(value).map_err(|_| {
        DataLayerError::InvalidInput(format!("relay downstream group {field} must be valid JSON"))
    })?;
    if !parsed.is_array() {
        return Err(DataLayerError::InvalidInput(format!(
            "relay downstream group {field} must be a JSON array"
        )));
    }
    Ok(())
}

fn validate_json_object(field: &str, value: &str) -> Result<(), DataLayerError> {
    let parsed: serde_json::Value = serde_json::from_str(value).map_err(|_| {
        DataLayerError::InvalidInput(format!("relay downstream group {field} must be valid JSON"))
    })?;
    if !parsed.is_object() {
        return Err(DataLayerError::InvalidInput(format!(
            "relay downstream group {field} must be a JSON object"
        )));
    }
    Ok(())
}
