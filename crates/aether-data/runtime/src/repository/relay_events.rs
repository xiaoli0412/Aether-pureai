use serde::{Deserialize, Serialize};
use sqlx::{MySql, MySqlPool, Postgres, Sqlite, SqlitePool, Transaction};

use crate::{DataBackends, DataLayerError};

const USAGE_SETTLED_EVENT_TYPE: &str = "usage_settled";
const PROFIT_STATUS_PENDING: &str = "pending";
const PROFIT_STATUS_RECORDED: &str = "recorded";
const PROFIT_REPLAY_STATE_ELIGIBLE: &str = "eligible";
const PROFIT_REPLAY_STATE_INVALID: &str = "invalid";
const MAX_PENDING_USAGE_EVENTS: usize = 1_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedRelayEvent {
    pub id: String,
    pub dedupe_key: Option<String>,
    pub event_type: String,
    pub payload_json: String,
    pub quota_per_unit: String,
    pub occurred_at: i64,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedRelayOutboxEvent {
    pub id: String,
    pub event_type: String,
    pub payload_json: String,
    pub created_at_unix_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayEventOutboxPage {
    pub events: Vec<PersistedRelayOutboxEvent>,
    pub next_cursor: String,
    pub has_more: bool,
}

#[derive(Clone)]
enum RelayEventInboxBackend {
    Postgres(sqlx::PgPool),
    Mysql(MySqlPool),
    Sqlite(SqlitePool),
}

#[derive(Clone)]
pub struct RelayEventInboxStore {
    backend: RelayEventInboxBackend,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayEventPersistOutcome {
    Applied { inserted: u64 },
    CursorConflict { current_cursor: String },
}

#[derive(Clone)]
enum RelayEventOutboxBackend {
    Postgres(sqlx::PgPool),
    Mysql(MySqlPool),
    Sqlite(SqlitePool),
}

#[derive(Clone)]
pub struct RelayEventOutboxStore {
    backend: RelayEventOutboxBackend,
}

impl RelayEventOutboxStore {
    pub fn from_backends(backends: &DataBackends) -> Option<Self> {
        backends
            .postgres()
            .map(|backend| Self {
                backend: RelayEventOutboxBackend::Postgres(backend.pool_clone()),
            })
            .or_else(|| {
                backends.mysql().map(|backend| Self {
                    backend: RelayEventOutboxBackend::Mysql(backend.pool_clone()),
                })
            })
            .or_else(|| {
                backends.sqlite().map(|backend| Self {
                    backend: RelayEventOutboxBackend::Sqlite(backend.pool_clone()),
                })
            })
    }

    pub fn sqlite(pool: SqlitePool) -> Self {
        Self {
            backend: RelayEventOutboxBackend::Sqlite(pool),
        }
    }

    pub async fn append(
        &self,
        instance_id: &str,
        event: &PersistedRelayOutboxEvent,
    ) -> Result<bool, DataLayerError> {
        let rows_affected = match &self.backend {
            RelayEventOutboxBackend::Postgres(pool) => sqlx::query(
                "INSERT INTO relay_event_outbox
                 (instance_id, event_id, event_type, payload_json, created_at_unix_ms)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (instance_id, event_id) DO NOTHING",
            )
            .bind(instance_id)
            .bind(&event.id)
            .bind(&event.event_type)
            .bind(&event.payload_json)
            .bind(event.created_at_unix_ms)
            .execute(pool)
            .await
            .map(|result| result.rows_affected()),
            RelayEventOutboxBackend::Mysql(pool) => sqlx::query(
                "INSERT INTO relay_event_outbox
                 (instance_id, event_id, event_type, payload_json, created_at_unix_ms)
                 VALUES (?, ?, ?, ?, ?)
                 ON DUPLICATE KEY UPDATE event_id = VALUES(event_id)",
            )
            .bind(instance_id)
            .bind(&event.id)
            .bind(&event.event_type)
            .bind(&event.payload_json)
            .bind(event.created_at_unix_ms)
            .execute(pool)
            .await
            .map(|result| result.rows_affected()),
            RelayEventOutboxBackend::Sqlite(pool) => sqlx::query(
                "INSERT INTO relay_event_outbox
                 (instance_id, event_id, event_type, payload_json, created_at_unix_ms)
                 VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(instance_id, event_id) DO NOTHING",
            )
            .bind(instance_id)
            .bind(&event.id)
            .bind(&event.event_type)
            .bind(&event.payload_json)
            .bind(event.created_at_unix_ms)
            .execute(pool)
            .await
            .map(|result| result.rows_affected()),
        }
        .map_err(DataLayerError::sql)?;

        Ok(rows_affected > 0)
    }

    pub async fn page(
        &self,
        instance_id: &str,
        after_cursor: &str,
        limit: usize,
    ) -> Result<RelayEventOutboxPage, DataLayerError> {
        if limit == 0 || limit > 1000 {
            return Err(DataLayerError::InvalidInput(
                "relay event outbox limit must be between 1 and 1000".to_string(),
            ));
        }
        let after_sequence = if after_cursor.is_empty() {
            0
        } else {
            after_cursor.parse::<i64>().map_err(|_| {
                DataLayerError::InvalidInput("invalid relay event outbox cursor".to_string())
            })?
        };
        if after_sequence < 0 {
            return Err(DataLayerError::InvalidInput(
                "invalid relay event outbox cursor".to_string(),
            ));
        }
        let fetch_limit = i64::try_from(limit + 1).map_err(|_| {
            DataLayerError::InvalidInput("relay event outbox limit is too large".to_string())
        })?;

        let mut rows: Vec<(i64, String, String, String, i64)> = match &self.backend {
            RelayEventOutboxBackend::Postgres(pool) => {
                sqlx::query_as(
                    "SELECT sequence, event_id, event_type, payload_json, created_at_unix_ms
                 FROM relay_event_outbox
                 WHERE instance_id = $1 AND sequence > $2
                 ORDER BY sequence ASC
                 LIMIT $3",
                )
                .bind(instance_id)
                .bind(after_sequence)
                .bind(fetch_limit)
                .fetch_all(pool)
                .await
            }
            RelayEventOutboxBackend::Mysql(pool) => {
                sqlx::query_as(
                    "SELECT sequence, event_id, event_type, payload_json, created_at_unix_ms
                 FROM relay_event_outbox
                 WHERE BINARY instance_id = ? AND sequence > ?
                 ORDER BY sequence ASC
                 LIMIT ?",
                )
                .bind(instance_id)
                .bind(after_sequence)
                .bind(fetch_limit)
                .fetch_all(pool)
                .await
            }
            RelayEventOutboxBackend::Sqlite(pool) => {
                sqlx::query_as(
                    "SELECT sequence, event_id, event_type, payload_json, created_at_unix_ms
                 FROM relay_event_outbox
                 WHERE instance_id = ? AND sequence > ?
                 ORDER BY sequence ASC
                 LIMIT ?",
                )
                .bind(instance_id)
                .bind(after_sequence)
                .bind(fetch_limit)
                .fetch_all(pool)
                .await
            }
        }
        .map_err(DataLayerError::sql)?;

        let has_more = rows.len() > limit;
        if has_more {
            rows.truncate(limit);
        }
        let next_cursor = rows
            .last()
            .map(|row| row.0.to_string())
            .unwrap_or_else(|| after_cursor.to_string());
        let events = rows
            .into_iter()
            .map(|(_, id, event_type, payload_json, created_at_unix_ms)| {
                PersistedRelayOutboxEvent {
                    id,
                    event_type,
                    payload_json,
                    created_at_unix_ms,
                }
            })
            .collect();

        Ok(RelayEventOutboxPage {
            events,
            next_cursor,
            has_more,
        })
    }
}

impl RelayEventInboxStore {
    pub fn from_backends(backends: &DataBackends) -> Option<Self> {
        backends
            .postgres()
            .map(|backend| Self {
                backend: RelayEventInboxBackend::Postgres(backend.pool_clone()),
            })
            .or_else(|| {
                backends.mysql().map(|backend| Self {
                    backend: RelayEventInboxBackend::Mysql(backend.pool_clone()),
                })
            })
            .or_else(|| {
                backends.sqlite().map(|backend| Self {
                    backend: RelayEventInboxBackend::Sqlite(backend.pool_clone()),
                })
            })
    }

    pub fn sqlite(pool: SqlitePool) -> Self {
        Self {
            backend: RelayEventInboxBackend::Sqlite(pool),
        }
    }

    pub async fn cursor(&self, instance_id: &str) -> Result<String, DataLayerError> {
        let cursor = match &self.backend {
            RelayEventInboxBackend::Postgres(pool) => {
                sqlx::query_scalar::<_, String>(
                    "SELECT cursor FROM relay_event_cursors WHERE instance_id = $1",
                )
                .bind(instance_id)
                .fetch_optional(pool)
                .await
            }
            RelayEventInboxBackend::Mysql(pool) => {
                sqlx::query_scalar::<_, String>(
                    "SELECT cursor FROM relay_event_cursors WHERE BINARY instance_id = ?",
                )
                .bind(instance_id)
                .fetch_optional(pool)
                .await
            }
            RelayEventInboxBackend::Sqlite(pool) => {
                sqlx::query_scalar::<_, String>(
                    "SELECT cursor FROM relay_event_cursors WHERE instance_id = ?",
                )
                .bind(instance_id)
                .fetch_optional(pool)
                .await
            }
        }
        .map_err(DataLayerError::sql)?;

        Ok(cursor.unwrap_or_default())
    }

    pub async fn persist_batch(
        &self,
        instance_id: &str,
        expected_cursor: &str,
        events: &[PersistedRelayEvent],
        next_cursor: &str,
    ) -> Result<RelayEventPersistOutcome, DataLayerError> {
        let inserted = match &self.backend {
            RelayEventInboxBackend::Postgres(pool) => {
                let mut tx = pool.begin().await.map_err(DataLayerError::sql)?;
                let claimed =
                    claim_postgres_cursor(&mut tx, instance_id, expected_cursor, next_cursor)
                        .await?;
                if !claimed {
                    let current_cursor =
                        postgres_cursor_in_transaction(&mut tx, instance_id).await?;
                    if current_cursor != expected_cursor || expected_cursor != next_cursor {
                        tx.rollback().await.map_err(DataLayerError::sql)?;
                        return Ok(RelayEventPersistOutcome::CursorConflict { current_cursor });
                    }
                }
                let inserted = persist_postgres_events(&mut tx, instance_id, events).await?;
                tx.commit().await.map_err(DataLayerError::sql)?;
                inserted
            }
            RelayEventInboxBackend::Mysql(pool) => {
                let mut tx = pool.begin().await.map_err(DataLayerError::sql)?;
                let claimed =
                    claim_mysql_cursor(&mut tx, instance_id, expected_cursor, next_cursor).await?;
                if !claimed {
                    let current_cursor = mysql_cursor_in_transaction(&mut tx, instance_id).await?;
                    tx.rollback().await.map_err(DataLayerError::sql)?;
                    return Ok(RelayEventPersistOutcome::CursorConflict { current_cursor });
                }
                let inserted = persist_mysql_events(&mut tx, instance_id, events).await?;
                tx.commit().await.map_err(DataLayerError::sql)?;
                inserted
            }
            RelayEventInboxBackend::Sqlite(pool) => {
                let mut tx = pool.begin().await.map_err(DataLayerError::sql)?;
                let claimed =
                    claim_sqlite_cursor(&mut tx, instance_id, expected_cursor, next_cursor).await?;
                if !claimed {
                    let current_cursor = sqlite_cursor_in_transaction(&mut tx, instance_id).await?;
                    if current_cursor != expected_cursor || expected_cursor != next_cursor {
                        tx.rollback().await.map_err(DataLayerError::sql)?;
                        return Ok(RelayEventPersistOutcome::CursorConflict { current_cursor });
                    }
                }
                let inserted = persist_sqlite_events(&mut tx, instance_id, events).await?;
                tx.commit().await.map_err(DataLayerError::sql)?;
                inserted
            }
        };

        Ok(RelayEventPersistOutcome::Applied { inserted })
    }

    /// Loads usage settlements whose durable inbox marker still needs a profit append.
    ///
    /// The event fetch cursor is intentionally independent from this query: a process can
    /// crash after committing the inbox/cursor transaction and safely replay these rows later.
    pub async fn pending_usage_events(
        &self,
        instance_id: &str,
        limit: usize,
    ) -> Result<Vec<PersistedRelayEvent>, DataLayerError> {
        if limit == 0 || limit > MAX_PENDING_USAGE_EVENTS {
            return Err(DataLayerError::InvalidInput(format!(
                "pending relay usage event limit must be between 1 and {MAX_PENDING_USAGE_EVENTS}"
            )));
        }
        let limit = i64::try_from(limit).map_err(|_| {
            DataLayerError::InvalidInput("pending relay usage event limit is too large".to_string())
        })?;
        let rows: Vec<(String, Option<String>, String, String, String, i64, i64)> =
            match &self.backend {
                RelayEventInboxBackend::Postgres(pool) => {
                    sqlx::query_as(
                        "SELECT event_id, dedupe_key, event_type, payload_json, quota_per_unit,
                            occurred_at, source_created_at
                     FROM relay_event_inbox
                      WHERE instance_id = $1
                        AND event_type = $2
                        AND profit_status = $3
                        AND profit_replay_state = $4
                      ORDER BY occurred_at ASC, event_id ASC
                      LIMIT $5",
                    )
                    .bind(instance_id)
                    .bind(USAGE_SETTLED_EVENT_TYPE)
                    .bind(PROFIT_STATUS_PENDING)
                    .bind(PROFIT_REPLAY_STATE_ELIGIBLE)
                    .bind(limit)
                    .fetch_all(pool)
                    .await
                }
                RelayEventInboxBackend::Mysql(pool) => {
                    sqlx::query_as(
                        "SELECT event_id, dedupe_key, event_type, payload_json, quota_per_unit,
                            occurred_at, source_created_at
                     FROM relay_event_inbox
                     WHERE BINARY instance_id = ?
                       AND event_type = ?
                       AND profit_status = ?
                       AND profit_replay_state = ?
                     ORDER BY occurred_at ASC, event_id ASC
                     LIMIT ?",
                    )
                    .bind(instance_id)
                    .bind(USAGE_SETTLED_EVENT_TYPE)
                    .bind(PROFIT_STATUS_PENDING)
                    .bind(PROFIT_REPLAY_STATE_ELIGIBLE)
                    .bind(limit)
                    .fetch_all(pool)
                    .await
                }
                RelayEventInboxBackend::Sqlite(pool) => {
                    sqlx::query_as(
                        "SELECT event_id, dedupe_key, event_type, payload_json, quota_per_unit,
                            occurred_at, source_created_at
                     FROM relay_event_inbox
                     WHERE instance_id = ?
                       AND event_type = ?
                       AND profit_status = ?
                       AND profit_replay_state = ?
                     ORDER BY occurred_at ASC, event_id ASC
                     LIMIT ?",
                    )
                    .bind(instance_id)
                    .bind(USAGE_SETTLED_EVENT_TYPE)
                    .bind(PROFIT_STATUS_PENDING)
                    .bind(PROFIT_REPLAY_STATE_ELIGIBLE)
                    .bind(limit)
                    .fetch_all(pool)
                    .await
                }
            }
            .map_err(DataLayerError::sql)?;

        Ok(rows
            .into_iter()
            .map(
                |(
                    id,
                    dedupe_key,
                    event_type,
                    payload_json,
                    quota_per_unit,
                    occurred_at,
                    created_at,
                )| PersistedRelayEvent {
                    id,
                    dedupe_key,
                    event_type,
                    payload_json,
                    quota_per_unit,
                    occurred_at,
                    created_at,
                },
            )
            .collect())
    }

    /// Marks one usage settlement recorded only after its idempotent ledger append succeeds.
    ///
    /// Repeating this after a crash is harmless: the ledger's business-key uniqueness makes the
    /// append idempotent, and a second marker update simply reports that another worker finished.
    pub async fn mark_profit_recorded(
        &self,
        instance_id: &str,
        event_id: &str,
    ) -> Result<bool, DataLayerError> {
        let Some(request_id) = self.pending_usage_request_id(instance_id, event_id).await? else {
            return Ok(false);
        };
        let rows_affected = match &self.backend {
            RelayEventInboxBackend::Postgres(pool) => sqlx::query(
                "UPDATE relay_event_inbox
                 SET profit_status = $1
                 WHERE instance_id = $2
                   AND event_id = $3
                   AND event_type = $4
                   AND profit_status = $5
                   AND profit_replay_state = $6
                   AND EXISTS (
                       SELECT 1
                       FROM relay_profit_ledger
                       WHERE instance_id = $7 AND request_id = $8
                   )",
            )
            .bind(PROFIT_STATUS_RECORDED)
            .bind(instance_id)
            .bind(event_id)
            .bind(USAGE_SETTLED_EVENT_TYPE)
            .bind(PROFIT_STATUS_PENDING)
            .bind(PROFIT_REPLAY_STATE_ELIGIBLE)
            .bind(instance_id)
            .bind(&request_id)
            .execute(pool)
            .await
            .map(|result| result.rows_affected()),
            RelayEventInboxBackend::Mysql(pool) => sqlx::query(
                "UPDATE relay_event_inbox
                 SET profit_status = ?
                 WHERE BINARY instance_id = ?
                   AND BINARY event_id = ?
                   AND event_type = ?
                   AND profit_status = ?
                   AND profit_replay_state = ?
                   AND EXISTS (
                       SELECT 1
                       FROM relay_profit_ledger
                       WHERE BINARY instance_id = ? AND BINARY request_id = ?
                   )",
            )
            .bind(PROFIT_STATUS_RECORDED)
            .bind(instance_id)
            .bind(event_id)
            .bind(USAGE_SETTLED_EVENT_TYPE)
            .bind(PROFIT_STATUS_PENDING)
            .bind(PROFIT_REPLAY_STATE_ELIGIBLE)
            .bind(instance_id)
            .bind(&request_id)
            .execute(pool)
            .await
            .map(|result| result.rows_affected()),
            RelayEventInboxBackend::Sqlite(pool) => sqlx::query(
                "UPDATE relay_event_inbox
                 SET profit_status = ?
                 WHERE instance_id = ?
                   AND event_id = ?
                   AND event_type = ?
                   AND profit_status = ?
                   AND profit_replay_state = ?
                   AND EXISTS (
                       SELECT 1
                       FROM relay_profit_ledger
                       WHERE instance_id = ? AND request_id = ?
                   )",
            )
            .bind(PROFIT_STATUS_RECORDED)
            .bind(instance_id)
            .bind(event_id)
            .bind(USAGE_SETTLED_EVENT_TYPE)
            .bind(PROFIT_STATUS_PENDING)
            .bind(PROFIT_REPLAY_STATE_ELIGIBLE)
            .bind(instance_id)
            .bind(&request_id)
            .execute(pool)
            .await
            .map(|result| result.rows_affected()),
        }
        .map_err(DataLayerError::sql)?;

        Ok(rows_affected > 0)
    }

    /// Keeps an invalid usage event durable and observable without allowing it to block replay.
    pub async fn mark_profit_invalid(
        &self,
        instance_id: &str,
        event_id: &str,
        error: &str,
    ) -> Result<bool, DataLayerError> {
        let error = error.trim();
        if error.is_empty() {
            return Err(DataLayerError::InvalidInput(
                "relay profit replay error must not be blank".to_string(),
            ));
        }

        let rows_affected = match &self.backend {
            RelayEventInboxBackend::Postgres(pool) => sqlx::query(
                "UPDATE relay_event_inbox
                 SET profit_replay_state = $1, profit_replay_error = $2
                 WHERE instance_id = $3
                   AND event_id = $4
                   AND event_type = $5
                   AND profit_status = $6
                   AND profit_replay_state = $7",
            )
            .bind(PROFIT_REPLAY_STATE_INVALID)
            .bind(error)
            .bind(instance_id)
            .bind(event_id)
            .bind(USAGE_SETTLED_EVENT_TYPE)
            .bind(PROFIT_STATUS_PENDING)
            .bind(PROFIT_REPLAY_STATE_ELIGIBLE)
            .execute(pool)
            .await
            .map(|result| result.rows_affected()),
            RelayEventInboxBackend::Mysql(pool) => sqlx::query(
                "UPDATE relay_event_inbox
                 SET profit_replay_state = ?, profit_replay_error = ?
                 WHERE BINARY instance_id = ?
                   AND BINARY event_id = ?
                   AND event_type = ?
                   AND profit_status = ?
                   AND profit_replay_state = ?",
            )
            .bind(PROFIT_REPLAY_STATE_INVALID)
            .bind(error)
            .bind(instance_id)
            .bind(event_id)
            .bind(USAGE_SETTLED_EVENT_TYPE)
            .bind(PROFIT_STATUS_PENDING)
            .bind(PROFIT_REPLAY_STATE_ELIGIBLE)
            .execute(pool)
            .await
            .map(|result| result.rows_affected()),
            RelayEventInboxBackend::Sqlite(pool) => sqlx::query(
                "UPDATE relay_event_inbox
                 SET profit_replay_state = ?, profit_replay_error = ?
                 WHERE instance_id = ?
                   AND event_id = ?
                   AND event_type = ?
                   AND profit_status = ?
                   AND profit_replay_state = ?",
            )
            .bind(PROFIT_REPLAY_STATE_INVALID)
            .bind(error)
            .bind(instance_id)
            .bind(event_id)
            .bind(USAGE_SETTLED_EVENT_TYPE)
            .bind(PROFIT_STATUS_PENDING)
            .bind(PROFIT_REPLAY_STATE_ELIGIBLE)
            .execute(pool)
            .await
            .map(|result| result.rows_affected()),
        }
        .map_err(DataLayerError::sql)?;

        Ok(rows_affected > 0)
    }

    async fn pending_usage_request_id(
        &self,
        instance_id: &str,
        event_id: &str,
    ) -> Result<Option<String>, DataLayerError> {
        let payload_json = match &self.backend {
            RelayEventInboxBackend::Postgres(pool) => {
                sqlx::query_scalar::<_, String>(
                    "SELECT payload_json
                 FROM relay_event_inbox
                  WHERE instance_id = $1
                    AND event_id = $2
                    AND event_type = $3
                    AND profit_status = $4
                    AND profit_replay_state = $5",
                )
                .bind(instance_id)
                .bind(event_id)
                .bind(USAGE_SETTLED_EVENT_TYPE)
                .bind(PROFIT_STATUS_PENDING)
                .bind(PROFIT_REPLAY_STATE_ELIGIBLE)
                .fetch_optional(pool)
                .await
            }
            RelayEventInboxBackend::Mysql(pool) => {
                sqlx::query_scalar::<_, String>(
                    "SELECT payload_json
                 FROM relay_event_inbox
                 WHERE BINARY instance_id = ?
                   AND BINARY event_id = ?
                   AND event_type = ?
                   AND profit_status = ?
                   AND profit_replay_state = ?",
                )
                .bind(instance_id)
                .bind(event_id)
                .bind(USAGE_SETTLED_EVENT_TYPE)
                .bind(PROFIT_STATUS_PENDING)
                .bind(PROFIT_REPLAY_STATE_ELIGIBLE)
                .fetch_optional(pool)
                .await
            }
            RelayEventInboxBackend::Sqlite(pool) => {
                sqlx::query_scalar::<_, String>(
                    "SELECT payload_json
                 FROM relay_event_inbox
                  WHERE instance_id = ?
                    AND event_id = ?
                    AND event_type = ?
                    AND profit_status = ?
                    AND profit_replay_state = ?",
                )
                .bind(instance_id)
                .bind(event_id)
                .bind(USAGE_SETTLED_EVENT_TYPE)
                .bind(PROFIT_STATUS_PENDING)
                .bind(PROFIT_REPLAY_STATE_ELIGIBLE)
                .fetch_optional(pool)
                .await
            }
        }
        .map_err(DataLayerError::sql)?;

        payload_json
            .map(|payload_json| request_id_from_usage_payload(&payload_json))
            .transpose()
    }

    #[cfg(test)]
    async fn inbox_count(&self, instance_id: &str) -> Result<i64, DataLayerError> {
        let RelayEventInboxBackend::Sqlite(pool) = &self.backend else {
            return Err(DataLayerError::InvalidInput(
                "inbox_count is only available for sqlite tests".to_string(),
            ));
        };
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM relay_event_inbox WHERE instance_id = ?")
            .bind(instance_id)
            .fetch_one(pool)
            .await
            .map_err(DataLayerError::sql)
    }

    #[cfg(test)]
    async fn profit_status(
        &self,
        instance_id: &str,
        event_id: &str,
    ) -> Result<String, DataLayerError> {
        let RelayEventInboxBackend::Sqlite(pool) = &self.backend else {
            return Err(DataLayerError::InvalidInput(
                "profit_status is only available for sqlite tests".to_string(),
            ));
        };
        sqlx::query_scalar::<_, String>(
            "SELECT profit_status FROM relay_event_inbox WHERE instance_id = ? AND event_id = ?",
        )
        .bind(instance_id)
        .bind(event_id)
        .fetch_one(pool)
        .await
        .map_err(DataLayerError::sql)
    }
}

async fn claim_postgres_cursor(
    tx: &mut Transaction<'_, Postgres>,
    instance_id: &str,
    expected_cursor: &str,
    next_cursor: &str,
) -> Result<bool, DataLayerError> {
    sqlx::query(
        "INSERT INTO relay_event_cursors (instance_id, cursor, updated_at)
         VALUES ($1, $2, CURRENT_TIMESTAMP)
         ON CONFLICT (instance_id) DO UPDATE
         SET cursor = EXCLUDED.cursor, updated_at = CURRENT_TIMESTAMP
         WHERE relay_event_cursors.cursor = $3",
    )
    .bind(instance_id)
    .bind(next_cursor)
    .bind(expected_cursor)
    .execute(&mut **tx)
    .await
    .map(|result| result.rows_affected() > 0)
    .map_err(DataLayerError::sql)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MysqlCursorClaimDecision {
    Insert,
    Update,
    Conflict,
}

fn mysql_cursor_claim_decision(
    current_cursor: Option<&str>,
    expected_cursor: &str,
) -> MysqlCursorClaimDecision {
    match current_cursor {
        Some(current_cursor) if current_cursor == expected_cursor => {
            MysqlCursorClaimDecision::Update
        }
        Some(_) => MysqlCursorClaimDecision::Conflict,
        None if expected_cursor.is_empty() => MysqlCursorClaimDecision::Insert,
        None => MysqlCursorClaimDecision::Conflict,
    }
}

async fn claim_mysql_cursor(
    tx: &mut Transaction<'_, MySql>,
    instance_id: &str,
    expected_cursor: &str,
    next_cursor: &str,
) -> Result<bool, DataLayerError> {
    let current_cursor = mysql_cursor_for_update(tx, instance_id).await?;
    match mysql_cursor_claim_decision(current_cursor.as_deref(), expected_cursor) {
        MysqlCursorClaimDecision::Conflict => Ok(false),
        MysqlCursorClaimDecision::Update => {
            update_mysql_cursor(tx, instance_id, next_cursor).await?;
            Ok(true)
        }
        MysqlCursorClaimDecision::Insert => {
            // A missing key cannot be held by a row lock at every MySQL isolation level.
            // Reserve it, then verify the cursor under the resulting row lock before updating.
            reserve_mysql_cursor(tx, instance_id).await?;
            let current_cursor = mysql_cursor_for_update(tx, instance_id).await?;
            if mysql_cursor_claim_decision(current_cursor.as_deref(), expected_cursor)
                != MysqlCursorClaimDecision::Update
            {
                return Ok(false);
            }
            update_mysql_cursor(tx, instance_id, next_cursor).await?;
            Ok(true)
        }
    }
}

async fn mysql_cursor_for_update(
    tx: &mut Transaction<'_, MySql>,
    instance_id: &str,
) -> Result<Option<String>, DataLayerError> {
    sqlx::query_scalar::<_, String>(
        "SELECT cursor FROM relay_event_cursors WHERE BINARY instance_id = ? FOR UPDATE",
    )
    .bind(instance_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(DataLayerError::sql)
}

async fn reserve_mysql_cursor(
    tx: &mut Transaction<'_, MySql>,
    instance_id: &str,
) -> Result<(), DataLayerError> {
    sqlx::query(
        "INSERT INTO relay_event_cursors (instance_id, cursor, updated_at)
         VALUES (?, '', CURRENT_TIMESTAMP)
         ON DUPLICATE KEY UPDATE cursor = cursor",
    )
    .bind(instance_id)
    .execute(&mut **tx)
    .await
    .map(|_| ())
    .map_err(DataLayerError::sql)
}

async fn update_mysql_cursor(
    tx: &mut Transaction<'_, MySql>,
    instance_id: &str,
    next_cursor: &str,
) -> Result<(), DataLayerError> {
    sqlx::query(
        "UPDATE relay_event_cursors
         SET cursor = ?, updated_at = CURRENT_TIMESTAMP
         WHERE BINARY instance_id = ?",
    )
    .bind(next_cursor)
    .bind(instance_id)
    .execute(&mut **tx)
    .await
    .map(|_| ())
    .map_err(DataLayerError::sql)
}

async fn claim_sqlite_cursor(
    tx: &mut Transaction<'_, Sqlite>,
    instance_id: &str,
    expected_cursor: &str,
    next_cursor: &str,
) -> Result<bool, DataLayerError> {
    sqlx::query(
        "INSERT INTO relay_event_cursors (instance_id, cursor, updated_at)
         VALUES (?, ?, CURRENT_TIMESTAMP)
         ON CONFLICT(instance_id) DO UPDATE
         SET cursor = excluded.cursor, updated_at = CURRENT_TIMESTAMP
         WHERE relay_event_cursors.cursor = ?",
    )
    .bind(instance_id)
    .bind(next_cursor)
    .bind(expected_cursor)
    .execute(&mut **tx)
    .await
    .map(|result| result.rows_affected() > 0)
    .map_err(DataLayerError::sql)
}

async fn postgres_cursor_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    instance_id: &str,
) -> Result<String, DataLayerError> {
    sqlx::query_scalar::<_, String>("SELECT cursor FROM relay_event_cursors WHERE instance_id = $1")
        .bind(instance_id)
        .fetch_optional(&mut **tx)
        .await
        .map(|cursor| cursor.unwrap_or_default())
        .map_err(DataLayerError::sql)
}

async fn mysql_cursor_in_transaction(
    tx: &mut Transaction<'_, MySql>,
    instance_id: &str,
) -> Result<String, DataLayerError> {
    mysql_cursor_for_update(tx, instance_id)
        .await
        .map(|cursor| cursor.unwrap_or_default())
}

async fn sqlite_cursor_in_transaction(
    tx: &mut Transaction<'_, Sqlite>,
    instance_id: &str,
) -> Result<String, DataLayerError> {
    sqlx::query_scalar::<_, String>("SELECT cursor FROM relay_event_cursors WHERE instance_id = ?")
        .bind(instance_id)
        .fetch_optional(&mut **tx)
        .await
        .map(|cursor| cursor.unwrap_or_default())
        .map_err(DataLayerError::sql)
}

fn initial_profit_status(event_type: &str) -> &'static str {
    if event_type == USAGE_SETTLED_EVENT_TYPE {
        PROFIT_STATUS_PENDING
    } else {
        PROFIT_STATUS_RECORDED
    }
}

#[derive(Deserialize)]
struct UsageSettlementPayload {
    request_id: String,
}

fn request_id_from_usage_payload(payload_json: &str) -> Result<String, DataLayerError> {
    let payload: UsageSettlementPayload = serde_json::from_str(payload_json).map_err(|error| {
        DataLayerError::InvalidInput(format!(
            "persisted usage_settled payload cannot provide a profit request_id: {error}"
        ))
    })?;
    let request_id = payload.request_id.trim();
    if request_id.is_empty() {
        return Err(DataLayerError::InvalidInput(
            "persisted usage_settled payload request_id must not be blank".to_string(),
        ));
    }
    Ok(request_id.to_string())
}

async fn persist_postgres_events(
    tx: &mut Transaction<'_, Postgres>,
    instance_id: &str,
    events: &[PersistedRelayEvent],
) -> Result<u64, DataLayerError> {
    let mut inserted = 0;
    for event in events {
        inserted += sqlx::query(
            "INSERT INTO relay_event_inbox
             (instance_id, event_id, dedupe_key, event_type, payload_json, quota_per_unit,
              occurred_at, source_created_at, profit_status)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (instance_id, event_id) DO NOTHING",
        )
        .bind(instance_id)
        .bind(&event.id)
        .bind(&event.dedupe_key)
        .bind(&event.event_type)
        .bind(&event.payload_json)
        .bind(&event.quota_per_unit)
        .bind(event.occurred_at)
        .bind(event.created_at)
        .bind(initial_profit_status(&event.event_type))
        .execute(&mut **tx)
        .await
        .map_err(DataLayerError::sql)?
        .rows_affected();
    }
    Ok(inserted)
}

async fn persist_mysql_events(
    tx: &mut Transaction<'_, MySql>,
    instance_id: &str,
    events: &[PersistedRelayEvent],
) -> Result<u64, DataLayerError> {
    let mut inserted = 0;
    for event in events {
        inserted += sqlx::query(
            "INSERT INTO relay_event_inbox
             (instance_id, event_id, dedupe_key, event_type, payload_json, quota_per_unit,
              occurred_at, source_created_at, profit_status)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON DUPLICATE KEY UPDATE event_id = VALUES(event_id)",
        )
        .bind(instance_id)
        .bind(&event.id)
        .bind(&event.dedupe_key)
        .bind(&event.event_type)
        .bind(&event.payload_json)
        .bind(&event.quota_per_unit)
        .bind(event.occurred_at)
        .bind(event.created_at)
        .bind(initial_profit_status(&event.event_type))
        .execute(&mut **tx)
        .await
        .map_err(DataLayerError::sql)?
        .rows_affected();
    }
    Ok(inserted)
}

async fn persist_sqlite_events(
    tx: &mut Transaction<'_, Sqlite>,
    instance_id: &str,
    events: &[PersistedRelayEvent],
) -> Result<u64, DataLayerError> {
    let mut inserted = 0;
    for event in events {
        inserted += sqlx::query(
            "INSERT INTO relay_event_inbox
             (instance_id, event_id, dedupe_key, event_type, payload_json, quota_per_unit,
              occurred_at, source_created_at, profit_status)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(instance_id, event_id) DO NOTHING",
        )
        .bind(instance_id)
        .bind(&event.id)
        .bind(&event.dedupe_key)
        .bind(&event.event_type)
        .bind(&event.payload_json)
        .bind(&event.quota_per_unit)
        .bind(event.occurred_at)
        .bind(event.created_at)
        .bind(initial_profit_status(&event.event_type))
        .execute(&mut **tx)
        .await
        .map_err(DataLayerError::sql)?
        .rows_affected();
    }
    Ok(inserted)
}

#[cfg(test)]
mod tests {
    use super::{
        PersistedRelayEvent, PersistedRelayOutboxEvent, RelayEventInboxBackend,
        RelayEventInboxStore, RelayEventOutboxStore, RelayEventPersistOutcome,
    };
    use crate::repository::relay_profit::{
        PersistedRelayProfitRecord, RelayCostConfidence, RelayProfitLedgerFilter,
        RelayProfitLedgerStore,
    };
    use sqlx::{
        mysql::MySqlPoolOptions,
        sqlite::{SqliteConnectOptions, SqlitePoolOptions},
        SqlitePool,
    };
    use std::{path::PathBuf, str::FromStr};
    use uuid::Uuid;

    async fn create_test_tables(pool: &SqlitePool) {
        sqlx::query(
            "CREATE TABLE relay_event_inbox (
                instance_id TEXT NOT NULL,
                event_id TEXT NOT NULL,
                dedupe_key TEXT,
                event_type TEXT NOT NULL CHECK (event_type <> 'reject'),
                payload_json TEXT NOT NULL,
                quota_per_unit TEXT NOT NULL,
                occurred_at INTEGER NOT NULL,
                source_created_at INTEGER NOT NULL,
                profit_status TEXT NOT NULL DEFAULT 'pending'
                    CHECK (profit_status IN ('pending', 'recorded')),
                profit_replay_state TEXT NOT NULL DEFAULT 'eligible'
                    CHECK (profit_replay_state IN ('eligible', 'invalid')),
                profit_replay_error TEXT,
                persisted_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (instance_id, event_id)
            )",
        )
        .execute(pool)
        .await
        .expect("event inbox table should be created");
        sqlx::query(
            "CREATE TABLE relay_event_cursors (
                instance_id TEXT PRIMARY KEY,
                cursor TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .execute(pool)
        .await
        .expect("event cursor table should be created");
        sqlx::query(
            "CREATE TABLE relay_profit_ledger (
                id TEXT PRIMARY KEY,
                instance_id TEXT NOT NULL,
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
                cost_confidence TEXT NOT NULL,
                occurred_at_unix_ms INTEGER NOT NULL,
                UNIQUE (instance_id, request_id)
            )",
        )
        .execute(pool)
        .await
        .expect("profit ledger table should be created");
    }

    async fn test_stores() -> (RelayEventInboxStore, RelayProfitLedgerStore) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite test pool should connect");
        create_test_tables(&pool).await;
        (
            RelayEventInboxStore::sqlite(pool.clone()),
            RelayProfitLedgerStore::sqlite(pool),
        )
    }

    async fn shared_file_test_stores() -> (
        RelayEventInboxStore,
        RelayEventInboxStore,
        RelayProfitLedgerStore,
        RelayProfitLedgerStore,
        PathBuf,
    ) {
        let database_path = std::env::temp_dir().join(format!(
            "aether-relay-events-cursor-cas-{}.sqlite",
            Uuid::new_v4()
        ));
        let database_url = format!("sqlite://{}", database_path.display());
        let options = SqliteConnectOptions::from_str(&database_url)
            .expect("temporary sqlite URL should parse")
            .create_if_missing(true);
        let first_pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .expect("first sqlite process pool should connect");
        create_test_tables(&first_pool).await;
        let second_pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("second sqlite process pool should connect");
        (
            RelayEventInboxStore::sqlite(first_pool.clone()),
            RelayEventInboxStore::sqlite(second_pool.clone()),
            RelayProfitLedgerStore::sqlite(first_pool),
            RelayProfitLedgerStore::sqlite(second_pool),
            database_path,
        )
    }

    async fn test_store() -> RelayEventInboxStore {
        test_stores().await.0
    }

    async fn test_outbox_store() -> RelayEventOutboxStore {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite test pool should connect");
        sqlx::query(
            "CREATE TABLE relay_event_outbox (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                instance_id TEXT NOT NULL,
                event_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                created_at_unix_ms INTEGER NOT NULL,
                UNIQUE (instance_id, event_id)
            )",
        )
        .execute(&pool)
        .await
        .expect("event outbox table should be created");
        RelayEventOutboxStore::sqlite(pool)
    }

    fn event(id: &str, event_type: &str) -> PersistedRelayEvent {
        let request_id = format!("request-{id}");
        PersistedRelayEvent {
            id: id.to_string(),
            dedupe_key: Some(format!("dedupe:{id}")),
            event_type: event_type.to_string(),
            payload_json: format!(r#"{{"event_id":"{id}","request_id":"{request_id}"}}"#),
            quota_per_unit: "500000".to_string(),
            occurred_at: 1_784_073_600,
            created_at: 1_784_073_601,
        }
    }

    fn usage_event(id: &str, request_id: &str) -> PersistedRelayEvent {
        PersistedRelayEvent {
            id: id.to_string(),
            dedupe_key: Some(format!("dedupe:{id}")),
            event_type: "usage_settled".to_string(),
            payload_json: format!(r#"{{"event_id":"{id}","request_id":"{request_id}"}}"#),
            quota_per_unit: "500000".to_string(),
            occurred_at: 1_784_073_600,
            created_at: 1_784_073_601,
        }
    }

    fn profit_record(id: &str, instance_id: &str, request_id: &str) -> PersistedRelayProfitRecord {
        PersistedRelayProfitRecord {
            id: id.to_string(),
            instance_id: instance_id.to_string(),
            request_id: request_id.to_string(),
            channel_id: "channel-1".to_string(),
            model_id: "model-1".to_string(),
            prompt_tokens: 120,
            completion_tokens: 30,
            charged_quota: "1250".to_string(),
            quota_per_unit: Some("500000".to_string()),
            upstream_cost_usd: Some(0.001),
            downstream_revenue_usd: Some(0.0025),
            payment_fee_usd: Some(0.000015),
            net_profit_usd: Some(0.001485),
            margin_percent: Some(59.4),
            cost_confidence: RelayCostConfidence::Known,
            occurred_at_unix_ms: 1_784_073_600_000,
        }
    }

    fn outbox_event(id: &str) -> PersistedRelayOutboxEvent {
        PersistedRelayOutboxEvent {
            id: id.to_string(),
            event_type: "route_decision_changed".to_string(),
            payload_json: format!(r#"{{"event_id":"{id}"}}"#),
            created_at_unix_ms: 1_784_073_600_000,
        }
    }

    #[test]
    fn mysql_cursor_claim_decision_distinguishes_absence_empty_and_case_variants() {
        assert_eq!(
            super::mysql_cursor_claim_decision(None, ""),
            super::MysqlCursorClaimDecision::Insert,
            "an absent cursor row accepts only the initial empty cursor"
        );
        assert_eq!(
            super::mysql_cursor_claim_decision(Some(""), ""),
            super::MysqlCursorClaimDecision::Update,
            "an existing empty cursor is not the same state as an absent row"
        );
        assert_eq!(
            super::mysql_cursor_claim_decision(None, "opaque:cursor"),
            super::MysqlCursorClaimDecision::Conflict,
            "a nonempty fetch-time cursor cannot claim an absent row"
        );
        assert_eq!(
            super::mysql_cursor_claim_decision(Some("Cursor:ABC"), "cursor:abc"),
            super::MysqlCursorClaimDecision::Conflict,
            "opaque cursors must compare exactly rather than through a database collation"
        );
    }

    #[tokio::test]
    async fn cursor_advances_only_after_the_entire_batch_is_persisted() {
        let store = test_store().await;
        store
            .persist_batch(
                "aether-primary",
                "",
                &[event("1", "usage_settled")],
                "cursor:1",
            )
            .await
            .expect("first batch should persist");

        let error = store
            .persist_batch(
                "aether-primary",
                "cursor:1",
                &[event("2", "usage_settled"), event("3", "reject")],
                "cursor:3",
            )
            .await
            .expect_err("invalid second event should roll back the batch");

        assert!(error.to_string().contains("CHECK constraint failed"));
        assert_eq!(
            store.cursor("aether-primary").await.expect("cursor read"),
            "cursor:1"
        );
        assert_eq!(
            store
                .inbox_count("aether-primary")
                .await
                .expect("inbox count"),
            1
        );
    }

    #[tokio::test]
    async fn duplicate_event_is_idempotent_and_cursor_remains_an_opaque_string() {
        let store = test_store().await;
        let first = store
            .persist_batch("aether-primary", "", &[event("1", "usage_settled")], "0001")
            .await
            .expect("first delivery should persist");
        let duplicate = store
            .persist_batch(
                "aether-primary",
                "0001",
                &[event("1", "usage_settled")],
                "cursor:%E4%B8%8B%E4%B8%80%E9%A1%B5",
            )
            .await
            .expect("duplicate delivery should be idempotent");

        assert_eq!(first, RelayEventPersistOutcome::Applied { inserted: 1 });
        assert_eq!(duplicate, RelayEventPersistOutcome::Applied { inserted: 0 });
        assert_eq!(
            store
                .inbox_count("aether-primary")
                .await
                .expect("inbox count"),
            1
        );
        assert_eq!(
            store.cursor("aether-primary").await.expect("cursor read"),
            "cursor:%E4%B8%8B%E4%B8%80%E9%A1%B5"
        );
    }

    #[tokio::test]
    async fn stale_shared_file_cursor_write_is_rejected_without_regressing_inbox_or_profit() {
        let (first_store, second_store, first_ledger, second_ledger, database_path) =
            shared_file_test_stores().await;
        first_store
            .persist_batch("aether-primary", "", &[], "c0")
            .await
            .expect("the initial opaque cursor should persist");
        let stale_expected_cursor = first_store
            .cursor("aether-primary")
            .await
            .expect("the delayed consumer should capture c0");
        assert_eq!(stale_expected_cursor, "c0");

        second_store
            .persist_batch(
                "aether-primary",
                "c0",
                &[usage_event("event-1", "request-1")],
                "c1",
            )
            .await
            .expect("the first fresh page should commit");
        second_store
            .persist_batch(
                "aether-primary",
                "c1",
                &[usage_event("event-2", "request-2")],
                "c2",
            )
            .await
            .expect("the newer page should commit");

        let stale_write = first_store
            .persist_batch(
                "aether-primary",
                &stale_expected_cursor,
                &[usage_event("event-1", "request-1")],
                "c1",
            )
            .await
            .expect("the cursor CAS should resolve a stale response explicitly");
        assert_eq!(
            stale_write,
            RelayEventPersistOutcome::CursorConflict {
                current_cursor: "c2".to_string(),
            },
            "a stale response must be explicitly rejected instead of overwriting c2 with c1"
        );
        assert_eq!(
            first_store
                .cursor("aether-primary")
                .await
                .expect("cursor should reload"),
            "c2",
            "the stale c1 page must not regress the durable cursor"
        );
        assert_eq!(
            first_store
                .inbox_count("aether-primary")
                .await
                .expect("inbox count should load"),
            2,
            "each fresh event should have exactly one durable inbox record"
        );

        assert!(first_ledger
            .append(&profit_record("profit-1", "aether-primary", "request-1"))
            .await
            .expect("the first profit record should persist"));
        assert!(second_ledger
            .append(&profit_record("profit-2", "aether-primary", "request-2"))
            .await
            .expect("the second profit record should persist"));
        assert!(!second_ledger
            .append(&profit_record(
                "profit-1-retry",
                "aether-primary",
                "request-1"
            ))
            .await
            .expect("the duplicate profit record should be idempotent"));
        assert!(first_store
            .mark_profit_recorded("aether-primary", "event-1")
            .await
            .expect("first marker should observe its ledger record"));
        assert!(second_store
            .mark_profit_recorded("aether-primary", "event-2")
            .await
            .expect("second marker should observe its ledger record"));
        assert_eq!(
            first_ledger
                .list_for_filter(&RelayProfitLedgerFilter {
                    instance_id: "aether-primary".to_string(),
                    start_unix_ms: 0,
                    end_unix_ms: 2_000_000_000_000,
                })
                .await
                .expect("profit records should load")
                .len(),
            2,
            "each durable inbox event should have exactly one profit record"
        );
        assert_eq!(
            first_store
                .profit_status("aether-primary", "event-1")
                .await
                .expect("first marker should load"),
            "recorded"
        );
        assert_eq!(
            second_store
                .profit_status("aether-primary", "event-2")
                .await
                .expect("second marker should load"),
            "recorded"
        );

        drop((first_store, second_store, first_ledger, second_ledger));
        let _ = std::fs::remove_file(database_path);
    }

    #[tokio::test]
    async fn pending_usage_cannot_be_marked_recorded_without_matching_profit_ledger_entry() {
        let store = test_store().await;
        let event = usage_event("usage-without-ledger", "request-without-ledger");
        store
            .persist_batch(
                "aether-primary",
                "",
                &[event],
                "cursor:usage-without-ledger",
            )
            .await
            .expect("pending usage event should persist");

        assert!(
            !store
                .mark_profit_recorded("aether-primary", "usage-without-ledger")
                .await
                .expect("a missing ledger record should not make the marker update fail"),
            "a pending inbox event must remain replayable until its idempotent ledger record exists"
        );
        assert_eq!(
            store
                .profit_status("aether-primary", "usage-without-ledger")
                .await
                .expect("profit marker should load"),
            "pending"
        );
    }

    #[tokio::test]
    async fn invalid_usage_replay_is_excluded_from_future_pending_queries() {
        let store = test_store().await;
        let invalid = usage_event("invalid-usage", "request-invalid");
        let valid = usage_event("valid-usage", "request-valid");
        store
            .persist_batch(
                "aether-primary",
                "",
                &[invalid, valid.clone()],
                "cursor:invalid-and-valid",
            )
            .await
            .expect("usage events should persist before one is isolated");

        assert!(
            store
                .mark_profit_invalid("aether-primary", "invalid-usage", "payload is malformed")
                .await
                .expect("invalid replay state should persist"),
            "an eligible pending event should move to the invalid replay state"
        );
        assert_eq!(
            store
                .pending_usage_events("aether-primary", 10)
                .await
                .expect("pending usage events should load"),
            vec![valid],
            "an invalid replay row must no longer starve later eligible usage rows"
        );
    }

    #[tokio::test]
    async fn invalid_usage_replay_cannot_be_marked_recorded_even_with_a_matching_ledger_row() {
        let (store, ledger) = test_stores().await;
        let event = usage_event("invalid-recorded", "request-invalid-recorded");
        store
            .persist_batch("aether-primary", "", &[event], "cursor:invalid-recorded")
            .await
            .expect("usage event should persist before it is isolated");
        assert!(store
            .mark_profit_invalid("aether-primary", "invalid-recorded", "payload is malformed",)
            .await
            .expect("invalid replay state should persist"));
        assert!(ledger
            .append(&profit_record(
                "profit-invalid-recorded",
                "aether-primary",
                "request-invalid-recorded",
            ))
            .await
            .expect("matching ledger record should persist"));

        assert!(
            !store
                .mark_profit_recorded("aether-primary", "invalid-recorded")
                .await
                .expect("invalid replay rows should be ignored by the marker update"),
            "a diagnostic ledger row must not turn an invalid event into a completed replay"
        );
        assert_eq!(
            store
                .profit_status("aether-primary", "invalid-recorded")
                .await
                .expect("profit status should load"),
            "pending",
            "invalid replay rows retain the ledger fence instead of becoming recorded"
        );
    }

    #[tokio::test]
    async fn crashed_profit_append_retries_idempotently_before_marking_the_inbox_recorded() {
        let (store, ledger) = test_stores().await;
        let event = usage_event("usage-crash-window", "request-crash-window");
        store
            .persist_batch("aether-primary", "", &[event], "cursor:crash-window")
            .await
            .expect("inbox and cursor should commit before the ledger append");

        assert!(
            ledger
                .append(&profit_record(
                    "profit-before-crash",
                    "aether-primary",
                    "request-crash-window",
                ))
                .await
                .expect("the initial ledger append should succeed"),
            "the first attempt represents the append that completed before a process crash"
        );
        assert!(
            !ledger
                .append(&profit_record(
                    "profit-after-restart",
                    "aether-primary",
                    "request-crash-window",
                ))
                .await
                .expect("a replayed ledger append should be idempotent"),
            "the restart must recognize the earlier append by its instance/request business key"
        );
        assert!(
            store
                .mark_profit_recorded("aether-primary", "usage-crash-window")
                .await
                .expect("the duplicate-safe ledger row should authorize the marker update"),
            "the marker may advance only after the durable ledger entry exists"
        );

        let records = ledger
            .list_for_filter(&RelayProfitLedgerFilter {
                instance_id: "aether-primary".to_string(),
                start_unix_ms: 0,
                end_unix_ms: 2_000_000_000_000,
            })
            .await
            .expect("ledger records should load");
        assert_eq!(
            records.len(),
            1,
            "the crash replay must not duplicate profit"
        );
        assert_eq!(records[0].id, "profit-before-crash");
        assert!(
            store
                .pending_usage_events("aether-primary", 10)
                .await
                .expect("pending events should reload")
                .is_empty(),
            "the source event should stop replaying after the duplicate-safe marker update"
        );
    }

    #[tokio::test]
    async fn committed_inbox_cursor_survives_the_profit_crash_window_and_replays_per_instance() {
        let (store, ledger) = test_stores().await;
        store
            .persist_batch(
                "aether-primary",
                "",
                &[
                    event("usage-primary", "usage_settled"),
                    event("financial-primary", "financial_posted"),
                ],
                "cursor:primary",
            )
            .await
            .expect("inbox and cursor should commit together before profit append");
        store
            .persist_batch(
                "aether-secondary",
                "",
                &[event("usage-secondary", "usage_settled")],
                "cursor:secondary",
            )
            .await
            .expect("other instance inbox should persist independently");

        assert_eq!(
            store.cursor("aether-primary").await.expect("cursor read"),
            "cursor:primary",
            "the fetch cursor must survive a crash after the inbox transaction commits"
        );

        let restarted_store = store.clone();
        let primary_pending = restarted_store
            .pending_usage_events("aether-primary", 10)
            .await
            .expect("restart should find primary pending usage events");
        assert_eq!(
            primary_pending,
            vec![event("usage-primary", "usage_settled")],
            "only the durable pending usage fact should be replayed"
        );
        assert_eq!(
            restarted_store
                .profit_status("aether-primary", "financial-primary")
                .await
                .expect("non-usage event status should load"),
            "recorded",
            "events without a profit append must not remain permanently pending"
        );
        assert_eq!(
            restarted_store
                .pending_usage_events("aether-secondary", 10)
                .await
                .expect("secondary pending usage should load"),
            vec![event("usage-secondary", "usage_settled")],
            "pending replay must remain instance scoped"
        );

        assert!(
            ledger
                .append(&profit_record(
                    "profit-primary",
                    "aether-primary",
                    "request-usage-primary",
                ))
                .await
                .expect("successful ledger append should persist before marking the inbox"),
            "the test fixture should insert the matching profit business key"
        );

        assert!(
            restarted_store
                .mark_profit_recorded("aether-primary", "usage-primary")
                .await
                .expect("successful ledger append should mark the source event recorded"),
            "the pending marker should transition after the append succeeds"
        );
        assert!(
            restarted_store
                .pending_usage_events("aether-primary", 10)
                .await
                .expect("primary pending events should reload")
                .is_empty(),
            "a recorded usage event must not replay again"
        );
    }

    #[tokio::test]
    async fn outbox_pages_are_instance_scoped_and_use_opaque_event_cursors() {
        let store = test_outbox_store().await;
        assert!(store
            .append("aether-primary", &outbox_event("event-a"))
            .await
            .expect("first event should persist"));
        assert!(store
            .append("aether-primary", &outbox_event("event-b"))
            .await
            .expect("second event should persist"));
        assert!(store
            .append("aether-secondary", &outbox_event("event-c"))
            .await
            .expect("other instance event should persist"));

        let first_page = store
            .page("aether-primary", "", 1)
            .await
            .expect("first page should load");
        assert_eq!(first_page.events, vec![outbox_event("event-a")]);
        assert_eq!(first_page.next_cursor, "1");
        assert!(first_page.has_more);

        let second_page = store
            .page("aether-primary", &first_page.next_cursor, 10)
            .await
            .expect("second page should load");
        assert_eq!(second_page.events, vec![outbox_event("event-b")]);
        assert_eq!(second_page.next_cursor, "2");
        assert!(!second_page.has_more);
    }

    #[tokio::test]
    async fn mysql_case_variant_relay_identities_keep_cursors_and_profit_markers_isolated() {
        let Some(database_url) = std::env::var("AETHER_TEST_MYSQL_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
        else {
            eprintln!("skipping mysql relay identity test because AETHER_TEST_MYSQL_URL is unset");
            return;
        };

        let pool = MySqlPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .expect("mysql relay identity test pool should connect");
        crate::lifecycle::migrate::run_mysql_migrations(&pool)
            .await
            .expect("mysql relay migrations should run before identity assertions");
        let store = RelayEventInboxStore {
            backend: RelayEventInboxBackend::Mysql(pool.clone()),
        };
        let instance_id = format!("RelayCase{}", Uuid::new_v4().simple());
        let lowercase_instance_id = instance_id.to_ascii_lowercase();
        let event_id = format!("EventCase{}", Uuid::new_v4().simple());
        let lowercase_event_id = event_id.to_ascii_lowercase();
        let request_id = format!("RequestCase{}", Uuid::new_v4().simple());

        store
            .persist_batch(
                &instance_id,
                "",
                &[usage_event(&event_id, &request_id)],
                "cursor:uppercase",
            )
            .await
            .expect("uppercase identity should persist");
        store
            .persist_batch(
                &lowercase_instance_id,
                "",
                &[usage_event(&lowercase_event_id, &request_id)],
                "cursor:lowercase",
            )
            .await
            .expect("lowercase identity should persist independently");

        assert_eq!(
            store
                .cursor(&instance_id)
                .await
                .expect("uppercase cursor should load"),
            "cursor:uppercase"
        );
        assert_eq!(
            store
                .cursor(&lowercase_instance_id)
                .await
                .expect("lowercase cursor should load"),
            "cursor:lowercase"
        );

        sqlx::query(
            "INSERT INTO relay_profit_ledger
             (id, instance_id, request_id, channel_id, model_id, prompt_tokens,
              completion_tokens, charged_quota, quota_per_unit, upstream_cost_usd,
              downstream_revenue_usd, payment_fee_usd, net_profit_usd, margin_percent,
              cost_confidence, occurred_at_unix_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(format!("profit-case-{}", Uuid::new_v4().simple()))
        .bind(&lowercase_instance_id)
        .bind(&request_id)
        .bind("channel-case")
        .bind("model-case")
        .bind(0_i64)
        .bind(0_i64)
        .bind("1")
        .bind("1")
        .bind(1.0_f64)
        .bind(1.0_f64)
        .bind(0.0_f64)
        .bind(0.0_f64)
        .bind(0.0_f64)
        .bind("known")
        .bind(1_i64)
        .execute(&pool)
        .await
        .expect("lowercase ledger fact should persist");

        assert!(
            !store
                .mark_profit_recorded(&instance_id, &event_id)
                .await
                .expect("case-isolated marker query should succeed"),
            "a lowercase ledger key must not authorize the uppercase pending event"
        );
        assert_eq!(
            store
                .pending_usage_events(&instance_id, 10)
                .await
                .expect("uppercase pending event should load"),
            vec![usage_event(&event_id, &request_id)],
            "the uppercase usage fact must remain replayable after the lowercase ledger append"
        );
    }
}
