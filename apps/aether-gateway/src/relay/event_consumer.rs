//! New API 只读同步事件消费器
//!
//! 从 New API 拉取增量事件。使用 HMAC-SHA256 签名认证。
//! 持久化游标、实例隔离、幂等去重、断线补拉。
//!
//! 禁止：
//! - 使用 /api/dashboard/ 做财务对账
//! - 硬编码 quota_per_unit (如 500000)
//! - 回写 New API 用户余额

use std::sync::Arc;
use std::time::Duration;

use aether_data::repository::relay_events::{
    PersistedRelayEvent, RelayEventInboxStore, RelayEventPersistOutcome,
};
use chrono::Utc;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::sync::watch;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::error::RelayError;
use super::profit::{
    is_base10_decimal, NewApiUsageSettlement, ProfitLedgerWriter, ProfitPersistenceOutcome,
};

type HmacSha256 = Hmac<Sha256>;
const MAX_INSTANCE_ID_CHARS: usize = 255;
const MAX_REQUEST_ID_CHARS: usize = 128;
const MAX_CHANNEL_ID_CHARS: usize = 64;
const MAX_MODEL_ID_CHARS: usize = 255;
const MAX_DATABASE_TOKEN_COUNT: u64 = i64::MAX as u64;

// ==================== Event Types ====================

pub mod event_types {
    pub const USAGE_SETTLED: &str = "usage_settled";
    pub const FINANCIAL_POSTED: &str = "financial_posted";
    pub const SUBSCRIPTION_CHANGED: &str = "subscription_changed";
    pub const PRICING_CHANGED: &str = "pricing_changed";
    pub const CHANNEL_CHANGED: &str = "channel_changed";
    pub const CHANNEL_BALANCE_OBSERVED: &str = "channel_balance_observed";
}

// ==================== Types ====================

/// 从 New API 接收的事件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncEvent {
    pub id: String,
    #[serde(default)]
    pub dedupe_key: Option<String>,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub occurred_at: i64,
    pub created_at: i64,
    pub quota_per_unit: String,
}

/// 事件消费配置
#[derive(Debug, Clone)]
pub struct SyncConsumerConfig {
    /// New API base URL
    pub new_api_base_url: String,
    /// Control secret for HMAC signing
    pub control_secret: String,
    /// This Aether instance ID
    pub instance_id: String,
    /// Poll interval seconds
    pub poll_interval_secs: u64,
    /// Batch size per poll
    pub batch_size: u64,
}

impl SyncConsumerConfig {
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var("AETHER_NEWAPI_BASE_URL").ok()?;
        let secret = std::env::var("AETHER_CONTROL_SECRET").ok()?;
        let instance_id =
            std::env::var("AETHER_INSTANCE_ID").unwrap_or_else(|_| "default".to_string());
        Some(Self {
            new_api_base_url: base_url,
            control_secret: secret,
            instance_id,
            poll_interval_secs: std::env::var("AETHER_SYNC_POLL_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30),
            batch_size: std::env::var("AETHER_SYNC_BATCH_SIZE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(100),
        })
    }
}

/// 事件消费器
#[derive(Clone)]
pub struct SyncConsumer {
    config: Arc<SyncConsumerConfig>,
    event_store: RelayEventInboxStore,
    profit_writer: Arc<ProfitLedgerWriter>,
    http_client: reqwest::Client,
}

impl SyncConsumer {
    pub fn new(
        config: SyncConsumerConfig,
        event_store: RelayEventInboxStore,
        profit_writer: Arc<ProfitLedgerWriter>,
    ) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            config: Arc::new(config),
            event_store,
            profit_writer,
            http_client,
        }
    }

    /// 启动后台消费任务
    pub fn spawn_task(
        self: Arc<Self>,
        mut shutdown: watch::Receiver<()>,
    ) -> tokio::task::JoinHandle<()> {
        let interval = Duration::from_secs(self.config.poll_interval_secs);
        tokio::spawn(async move {
            info!(
                endpoint = %self.config.new_api_base_url,
                interval_secs = self.config.poll_interval_secs,
                "starting New API sync consumer"
            );
            let mut ticker = tokio::time::interval(interval);
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if let Err(e) = self.drain_pending_profit().await {
                            error!(error = %e, "pending profit replay failed");
                        }
                        if let Err(e) = self.poll_events().await {
                            error!(error = %e, "sync poll failed");
                        }
                    }
                    _ = shutdown.changed() => {
                        info!("sync consumer shutting down");
                        break;
                    }
                }
            }
        })
    }

    /// 拉取事件
    pub async fn poll_events(&self) -> Result<u64, RelayError> {
        let cursor = self
            .event_store
            .cursor(&self.config.instance_id)
            .await
            .map_err(|error| RelayError::Internal(format!("read event cursor: {error}")))?;
        let path = "/api/aether/v1/events";
        let limit = self.config.batch_size.to_string();

        let resp = self
            .signed_get(
                path,
                &[("after", cursor.as_str()), ("limit", limit.as_str())],
            )
            .await?;
        let body: EventsResponse = resp
            .json()
            .await
            .map_err(|e| RelayError::Internal(format!("parse events response: {}", e)))?;

        if !body.success {
            return Err(RelayError::Internal(
                body.message
                    .unwrap_or_else(|| "event sync request failed".to_string()),
            ));
        }

        let events = body
            .data
            .events
            .iter()
            .map(|event| {
                Ok(PersistedRelayEvent {
                    id: event.id.clone(),
                    dedupe_key: event.dedupe_key.clone(),
                    event_type: event.event_type.clone(),
                    payload_json: serde_json::to_string(&event.payload).map_err(|error| {
                        RelayError::Internal(format!("serialize event payload: {error}"))
                    })?,
                    quota_per_unit: event.quota_per_unit.clone(),
                    occurred_at: event.occurred_at,
                    created_at: event.created_at,
                })
            })
            .collect::<Result<Vec<_>, RelayError>>()?;
        let outcome = self
            .event_store
            .persist_batch(
                &self.config.instance_id,
                &cursor,
                &events,
                &body.data.next_cursor,
            )
            .await
            .map_err(|error| RelayError::Internal(format!("persist event batch: {error}")))?;
        let processed = match outcome {
            RelayEventPersistOutcome::Applied { inserted } => inserted,
            RelayEventPersistOutcome::CursorConflict { current_cursor } => {
                warn!(
                    expected_cursor = %cursor,
                    current_cursor = %current_cursor,
                    "New API event cursor changed before the fetched page could persist"
                );
                return Ok(0);
            }
        };

        self.drain_pending_profit().await?;

        if processed > 0 {
            info!(processed = processed, cursor = %cursor, "events synced");
        }
        Ok(processed)
    }

    /// Replays durable usage-settlement inbox records until their SQL profit append is confirmed.
    ///
    /// A retry after an append/marker crash is safe: the profit ledger is idempotent by
    /// `(instance_id, request_id)`, and only a `Recorded` worker outcome transitions the marker.
    async fn drain_pending_profit(&self) -> Result<u64, RelayError> {
        let limit = usize::try_from(self.config.batch_size.clamp(1, 1_000))
            .expect("clamped pending relay event limit must fit usize");
        let events = self
            .event_store
            .pending_usage_events(&self.config.instance_id, limit)
            .await
            .map_err(|error| {
                RelayError::Internal(format!("read pending relay usage events: {error}"))
            })?;
        let mut recorded = 0;

        for persisted_event in events {
            let event_id = persisted_event.id.clone();
            let event = match sync_event_from_persisted_event(persisted_event) {
                Ok(event) => event,
                Err(error) => {
                    self.isolate_invalid_profit_replay(&event_id, &error)
                        .await?;
                    continue;
                }
            };
            let settlement =
                match new_api_usage_settlement_from_event(&self.config.instance_id, &event) {
                    Ok(Some(settlement)) => settlement,
                    Ok(None) => {
                        let error = RelayError::Internal(
                            "pending relay profit event was not a usage_settled event".to_string(),
                        );
                        self.isolate_invalid_profit_replay(&event_id, &error)
                            .await?;
                        continue;
                    }
                    Err(error) => {
                        self.isolate_invalid_profit_replay(&event_id, &error)
                            .await?;
                        continue;
                    }
                };

            match self
                .profit_writer
                .record_new_api_usage_settlement(settlement)
                .await?
            {
                ProfitPersistenceOutcome::Recorded => {
                    if self
                        .event_store
                        .mark_profit_recorded(&self.config.instance_id, &event_id)
                        .await
                        .map_err(|error| {
                            RelayError::Internal(format!(
                                "mark relay profit event recorded: {error}"
                            ))
                        })?
                    {
                        recorded += 1;
                    }
                }
                ProfitPersistenceOutcome::Pending => {
                    debug!(event_id = %event_id, "AETHER usage is not settled; retaining relay profit replay marker");
                }
            }
        }

        Ok(recorded)
    }

    async fn isolate_invalid_profit_replay(
        &self,
        event_id: &str,
        error: &RelayError,
    ) -> Result<(), RelayError> {
        let isolated = self
            .event_store
            .mark_profit_invalid(&self.config.instance_id, event_id, &error.to_string())
            .await
            .map_err(|store_error| {
                RelayError::Internal(format!("isolate invalid relay profit event: {store_error}"))
            })?;
        if isolated {
            warn!(event_id = %event_id, error = %error, "isolated malformed relay profit replay event");
        } else {
            debug!(event_id = %event_id, "relay profit replay event was already isolated or completed");
        }
        Ok(())
    }

    /// 获取定价快照
    pub async fn fetch_pricing(&self, group: &str) -> Result<serde_json::Value, RelayError> {
        let path = "/api/aether/v1/pricing";
        let resp = self.signed_get(path, &[("group", group)]).await?;
        resp.json()
            .await
            .map_err(|e| RelayError::Internal(format!("parse pricing: {}", e)))
    }

    /// 获取历史快照
    pub async fn fetch_snapshot(
        &self,
        from_unix: u64,
        to_unix: u64,
    ) -> Result<serde_json::Value, RelayError> {
        let path = "/api/aether/v1/snapshot";
        let from = from_unix.to_string();
        let to = to_unix.to_string();
        let resp = self
            .signed_get(path, &[("from", from.as_str()), ("to", to.as_str())])
            .await?;
        resp.json()
            .await
            .map_err(|e| RelayError::Internal(format!("parse snapshot: {}", e)))
    }

    /// 发送 HMAC-SHA256 签名的 GET 请求
    async fn signed_get(
        &self,
        path: &str,
        query_params: &[(&str, &str)],
    ) -> Result<reqwest::Response, RelayError> {
        let timestamp = Utc::now().timestamp().to_string();
        let nonce = Uuid::new_v4().to_string();
        let query = canonical_query(query_params);
        let sign_payload = signature_payload("GET", path, &query, &timestamp, &nonce);

        let mut mac = HmacSha256::new_from_slice(self.config.control_secret.as_bytes())
            .map_err(|_| RelayError::Internal("invalid control secret".into()))?;
        mac.update(sign_payload.as_bytes());
        let signature = hex_encode(&mac.finalize().into_bytes());

        let mut url = reqwest::Url::parse(&format!(
            "{}{}",
            self.config.new_api_base_url.trim_end_matches('/'),
            path
        ))
        .map_err(|error| RelayError::Internal(format!("invalid sync URL: {error}")))?;
        url.set_query(Some(&query));

        let resp = self
            .http_client
            .get(url)
            .header("X-Aether-Instance-ID", &*self.config.instance_id)
            .header("X-Aether-Timestamp", &timestamp)
            .header("X-Aether-Nonce", &nonce)
            .header("X-Aether-Signature", &signature)
            .send()
            .await
            .map_err(|e| RelayError::Internal(format!("sync request failed: {}", e)))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(RelayError::Internal(format!(
                "sync API returned HTTP {}: {}",
                status, body
            )));
        }

        Ok(resp)
    }
}

#[derive(Deserialize)]
struct UsageSettledPayload {
    request_id: String,
    channel_id: String,
    model: String,
    prompt_tokens: String,
    completion_tokens: String,
    charged_quota: String,
}

fn new_api_usage_settlement_from_event(
    instance_id: &str,
    event: &SyncEvent,
) -> Result<Option<NewApiUsageSettlement>, RelayError> {
    if event.event_type != event_types::USAGE_SETTLED {
        return Ok(None);
    }
    let payload: UsageSettledPayload =
        serde_json::from_value(event.payload.clone()).map_err(|_| {
            RelayError::Internal("New API usage_settled payload is invalid".to_string())
        })?;
    let occurred_at_unix_ms = event.occurred_at.checked_mul(1_000).ok_or_else(|| {
        RelayError::Internal(
            "New API usage_settled timestamp is outside database range".to_string(),
        )
    })?;
    if occurred_at_unix_ms < 0 {
        return Err(RelayError::Internal(
            "New API usage_settled timestamp must be non-negative".to_string(),
        ));
    }

    Ok(Some(NewApiUsageSettlement {
        instance_id: required_payload_identifier(
            instance_id,
            "instance_id",
            MAX_INSTANCE_ID_CHARS,
        )?,
        request_id: required_payload_identifier(
            &payload.request_id,
            "request_id",
            MAX_REQUEST_ID_CHARS,
        )?,
        channel_id: required_payload_identifier(
            &payload.channel_id,
            "channel_id",
            MAX_CHANNEL_ID_CHARS,
        )?,
        model_id: required_payload_identifier(&payload.model, "model", MAX_MODEL_ID_CHARS)?,
        prompt_tokens: parse_nonnegative_token_count(&payload.prompt_tokens, "prompt_tokens")?,
        completion_tokens: parse_nonnegative_token_count(
            &payload.completion_tokens,
            "completion_tokens",
        )?,
        charged_quota: required_payload_decimal(&payload.charged_quota, "charged_quota")?,
        quota_per_unit: optional_payload_decimal(&event.quota_per_unit, "quota_per_unit")?,
        occurred_at_unix_ms,
    }))
}

fn sync_event_from_persisted_event(event: PersistedRelayEvent) -> Result<SyncEvent, RelayError> {
    let payload = serde_json::from_str(&event.payload_json).map_err(|error| {
        RelayError::Internal(format!("parse persisted relay event payload: {error}"))
    })?;
    Ok(SyncEvent {
        id: event.id,
        dedupe_key: event.dedupe_key,
        event_type: event.event_type,
        payload,
        occurred_at: event.occurred_at,
        created_at: event.created_at,
        quota_per_unit: event.quota_per_unit,
    })
}

fn required_payload_identifier(
    value: &str,
    field: &str,
    max_chars: usize,
) -> Result<String, RelayError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(RelayError::Internal(format!(
            "New API usage_settled {field} must not be blank"
        )));
    }
    if value.chars().count() > max_chars {
        return Err(RelayError::Internal(format!(
            "New API usage_settled {field} exceeds the database identifier limit"
        )));
    }
    Ok(value.to_string())
}

fn required_payload_decimal(value: &str, field: &str) -> Result<String, RelayError> {
    if !is_base10_decimal(value) {
        return Err(RelayError::Internal(format!(
            "New API usage_settled {field} must be a non-negative base-10 decimal"
        )));
    }
    Ok(value.to_string())
}

fn optional_payload_decimal(value: &str, field: &str) -> Result<Option<String>, RelayError> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    required_payload_decimal(value, field).map(Some)
}

fn parse_nonnegative_token_count(value: &str, field: &str) -> Result<u64, RelayError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(RelayError::Internal(format!(
            "New API usage_settled {field} must be a non-negative integer string"
        )));
    }
    let parsed = value.parse::<u64>().map_err(|_| {
        RelayError::Internal(format!(
            "New API usage_settled {field} is outside u64 range"
        ))
    })?;
    if parsed > MAX_DATABASE_TOKEN_COUNT {
        return Err(RelayError::Internal(format!(
            "New API usage_settled {field} exceeds the database range"
        )));
    }
    Ok(parsed)
}

fn canonical_query(params: &[(&str, &str)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in params {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

fn signature_payload(
    method: &str,
    path: &str,
    raw_query: &str,
    timestamp: &str,
    nonce: &str,
) -> String {
    format!("{method}\n{path}\n{raw_query}\n{timestamp}\n{nonce}")
}

#[derive(Debug, Deserialize)]
struct EventsResponse {
    success: bool,
    #[serde(default)]
    message: Option<String>,
    data: EventsPage,
}

#[derive(Debug, Deserialize)]
struct EventsPage {
    #[allow(dead_code)]
    contract_version: String,
    events: Vec<SyncEvent>,
    next_cursor: String,
    #[allow(dead_code)]
    has_more: bool,
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        canonical_query, hex_encode, new_api_usage_settlement_from_event, signature_payload,
        EventsResponse, HmacSha256, SyncConsumer, SyncConsumerConfig, SyncEvent,
    };
    use crate::relay::profit::{ProfitLedgerWork, ProfitLedgerWriter, ProfitPersistenceOutcome};
    use aether_data::repository::relay_events::{PersistedRelayEvent, RelayEventInboxStore};
    use axum::{
        extract::Request, http::StatusCode, response::IntoResponse, routing::get, Json, Router,
    };
    use hmac::Mac;
    use sqlx::{
        sqlite::{SqliteConnectOptions, SqlitePoolOptions},
        SqlitePool,
    };
    use std::{
        path::PathBuf,
        process::{Command, Output, Stdio},
        str::FromStr,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        },
        time::Duration,
    };
    use tokio::sync::{Barrier, Notify};
    use uuid::Uuid;

    const TWO_PROCESS_CHILD_ENV: &str = "AETHER_EVENT_CONSUMER_TWO_PROCESS_CHILD";
    const TWO_PROCESS_DATABASE_URL_ENV: &str = "AETHER_EVENT_CONSUMER_TWO_PROCESS_DATABASE_URL";
    const TWO_PROCESS_UPSTREAM_URL_ENV: &str = "AETHER_EVENT_CONSUMER_TWO_PROCESS_UPSTREAM_URL";
    const TWO_PROCESS_INSTANCE_ID_ENV: &str = "AETHER_EVENT_CONSUMER_TWO_PROCESS_INSTANCE_ID";
    const TWO_PROCESS_CHILD_RESULT_PREFIX: &str = "AETHER_EVENT_CONSUMER_TWO_PROCESS_RESULT=";

    #[derive(Debug, serde::Deserialize)]
    struct TwoProcessChildResult {
        processed: u64,
        profit_records_appended: u64,
    }

    async fn create_test_event_tables(pool: &SqlitePool) {
        sqlx::query(
            "CREATE TABLE relay_event_inbox (
                instance_id TEXT NOT NULL,
                event_id TEXT NOT NULL,
                dedupe_key TEXT,
                event_type TEXT NOT NULL,
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
        .expect("relay event inbox should be created");
        sqlx::query(
            "CREATE TABLE relay_event_cursors (
                instance_id TEXT PRIMARY KEY,
                cursor TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .execute(pool)
        .await
        .expect("relay event cursor should be created");
        sqlx::query(
            "CREATE TABLE relay_profit_ledger (
                instance_id TEXT NOT NULL,
                request_id TEXT NOT NULL,
                UNIQUE (instance_id, request_id)
            )",
        )
        .execute(pool)
        .await
        .expect("relay profit ledger should be created");
    }

    async fn test_event_stores() -> (RelayEventInboxStore, SqlitePool) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite test pool should connect");
        create_test_event_tables(&pool).await;
        (RelayEventInboxStore::sqlite(pool.clone()), pool)
    }

    async fn shared_file_test_event_stores() -> (
        RelayEventInboxStore,
        RelayEventInboxStore,
        SqlitePool,
        SqlitePool,
        PathBuf,
    ) {
        let database_path = std::env::temp_dir().join(format!(
            "aether-event-consumer-two-instance-{}.sqlite",
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
            .expect("first independent sqlite pool should connect");
        create_test_event_tables(&first_pool).await;
        let second_pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("second independent sqlite pool should connect");

        (
            RelayEventInboxStore::sqlite(first_pool.clone()),
            RelayEventInboxStore::sqlite(second_pool.clone()),
            first_pool,
            second_pool,
            database_path,
        )
    }

    async fn append_next_profit_work(
        receiver: &mut tokio::sync::mpsc::Receiver<ProfitLedgerWork>,
        pool: &SqlitePool,
        appended_profit_records: &AtomicUsize,
    ) {
        let work = receiver
            .recv()
            .await
            .expect("one consumer should enqueue the durable usage settlement");
        let settlement = work.settlement().clone();
        let result = sqlx::query(
            "INSERT OR IGNORE INTO relay_profit_ledger (instance_id, request_id) VALUES (?, ?)",
        )
        .bind(&settlement.instance_id)
        .bind(&settlement.request_id)
        .execute(pool)
        .await
        .expect("independent worker should persist its idempotent ledger record");
        if result.rows_affected() == 1 {
            appended_profit_records.fetch_add(1, Ordering::SeqCst);
        }
        work.complete(Ok(ProfitPersistenceOutcome::Recorded));
    }

    async fn test_event_store() -> RelayEventInboxStore {
        test_event_stores().await.0
    }

    fn usage_event(id: &str, request_id: &str) -> PersistedRelayEvent {
        PersistedRelayEvent {
            id: id.to_string(),
            dedupe_key: Some(format!("usage:aether-primary:{request_id}")),
            event_type: super::event_types::USAGE_SETTLED.to_string(),
            payload_json: serde_json::json!({
                "request_id": request_id,
                "channel_id": "41",
                "model": "gpt-5",
                "prompt_tokens": "120",
                "completion_tokens": "30",
                "charged_quota": "1250"
            })
            .to_string(),
            quota_per_unit: "500000".to_string(),
            occurred_at: 1_784_073_600,
            created_at: 1_784_073_601,
        }
    }

    fn consumer_config() -> SyncConsumerConfig {
        SyncConsumerConfig {
            new_api_base_url: "http://new-api.invalid".to_string(),
            control_secret: "test-secret".to_string(),
            instance_id: "aether-primary".to_string(),
            poll_interval_secs: 30,
            batch_size: 100,
        }
    }

    async fn two_process_test_database() -> (SqlitePool, PathBuf, String) {
        let database_path = std::env::temp_dir().join(format!(
            "aether-event-consumer-two-process-{}.sqlite",
            Uuid::new_v4()
        ));
        let database_url = format!("sqlite://{}", database_path.display());
        let options = SqliteConnectOptions::from_str(&database_url)
            .expect("temporary two-process sqlite URL should parse")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("parent two-process sqlite pool should connect");
        sqlx::query("PRAGMA busy_timeout = 5000")
            .execute(&pool)
            .await
            .expect("parent two-process sqlite pool should wait for a competing writer");
        create_test_event_tables(&pool).await;

        (pool, database_path, database_url)
    }

    async fn two_process_child_pool(database_url: &str) -> SqlitePool {
        let options = SqliteConnectOptions::from_str(database_url)
            .expect("child two-process sqlite URL should parse");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("child two-process sqlite pool should connect");
        sqlx::query("PRAGMA busy_timeout = 5000")
            .execute(&pool)
            .await
            .expect("child two-process sqlite pool should wait for a competing writer");
        pool
    }

    fn two_process_child_env(name: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for child test"))
    }

    fn spawn_two_process_consumer(
        database_url: &str,
        upstream_url: &str,
        instance_id: &str,
    ) -> std::process::Child {
        Command::new(std::env::current_exe().expect("test binary path should resolve"))
            .arg("--exact")
            .arg("relay::event_consumer::tests::two_os_process_consumer_child")
            .arg("--nocapture")
            .env(TWO_PROCESS_CHILD_ENV, "1")
            .env(TWO_PROCESS_DATABASE_URL_ENV, database_url)
            .env(TWO_PROCESS_UPSTREAM_URL_ENV, upstream_url)
            .env(TWO_PROCESS_INSTANCE_ID_ENV, instance_id)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("two-process event consumer child should start")
    }

    fn parse_two_process_child_result(output: Output) -> TwoProcessChildResult {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "two-process event consumer child failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );

        let raw_result = stdout
            .lines()
            .find_map(|line| {
                line.split_once(TWO_PROCESS_CHILD_RESULT_PREFIX)
                    .map(|(_, result)| result.trim())
            })
            .unwrap_or_else(|| {
                panic!(
                    "two-process child exited without its poll result:\nstdout:\n{stdout}\nstderr:\n{stderr}"
                )
            });
        serde_json::from_str(raw_result).expect("two-process child result should be valid JSON")
    }

    #[test]
    fn two_process_child_result_parser_accepts_a_libtest_display_prefix() {
        let mut output =
            Command::new(std::env::current_exe().expect("test binary path should resolve"))
                .arg("--list")
                .output()
                .expect("test binary list command should succeed");
        assert!(output.status.success());
        output.stdout = format!(
            "test relay::event_consumer::tests::two_os_process_consumer_child ... {TWO_PROCESS_CHILD_RESULT_PREFIX}{{\"processed\":1,\"profit_records_appended\":1}}\nok\n"
        )
        .into_bytes();
        output.stderr.clear();

        let result = parse_two_process_child_result(output);

        assert_eq!(result.processed, 1);
        assert_eq!(result.profit_records_appended, 1);
    }

    fn has_valid_control_signature(request: &Request) -> bool {
        let timestamp = request
            .headers()
            .get("X-Aether-Timestamp")
            .and_then(|value| value.to_str().ok());
        let nonce = request
            .headers()
            .get("X-Aether-Nonce")
            .and_then(|value| value.to_str().ok());
        let signature = request
            .headers()
            .get("X-Aether-Signature")
            .and_then(|value| value.to_str().ok());
        let (Some(timestamp), Some(nonce), Some(signature)) = (timestamp, nonce, signature) else {
            return false;
        };

        let mut mac =
            HmacSha256::new_from_slice(b"test-secret").expect("test HMAC key should be valid");
        mac.update(
            signature_payload(
                request.method().as_str(),
                request.uri().path(),
                request.uri().query().unwrap_or_default(),
                timestamp,
                nonce,
            )
            .as_bytes(),
        );
        hex_encode(&mac.finalize().into_bytes()) == signature
    }

    #[test]
    fn events_response_parses_new_api_service_envelope() {
        let response: EventsResponse = serde_json::from_str(
            r#"{
                "success":true,
                "message":"",
                "data":{
                    "contract_version":"aether-newapi-events/v1",
                    "events":[{
                        "id":"1",
                        "dedupe_key":"usage:aether-primary:req_123",
                        "event_type":"usage_settled",
                        "occurred_at":1784073600,
                        "created_at":1784073601,
                        "quota_per_unit":"500000",
                        "payload":{"request_id":"req_123","charged_quota":"42"}
                    }],
                    "next_cursor":"1",
                    "has_more":false
                }
            }"#,
        )
        .expect("service envelope should deserialize");

        assert!(response.success);
        assert_eq!(response.data.events.len(), 1);
        assert_eq!(response.data.events[0].id, "1");
        assert_eq!(response.data.next_cursor, "1");
    }

    #[test]
    fn canonical_query_percent_encodes_unicode_and_reserved_characters() {
        let query = canonical_query(&[("group", "中文 pro"), ("cursor", "a/b?")]);

        assert_eq!(query, "group=%E4%B8%AD%E6%96%87+pro&cursor=a%2Fb%3F");
    }

    #[test]
    fn signature_payload_uses_the_exact_canonical_raw_query() {
        let query = canonical_query(&[("group", "中文 pro")]);

        assert_eq!(
            signature_payload(
                "GET",
                "/api/aether/v1/pricing",
                &query,
                "1784073600",
                "nonce-1"
            ),
            "GET\n/api/aether/v1/pricing\ngroup=%E4%B8%AD%E6%96%87+pro\n1784073600\nnonce-1"
        );
    }

    #[test]
    fn usage_settled_event_builds_a_complete_profit_ledger_fact() {
        let event = SyncEvent {
            id: "101".to_string(),
            dedupe_key: Some("usage:aether-primary:req_01".to_string()),
            event_type: super::event_types::USAGE_SETTLED.to_string(),
            payload: serde_json::json!({
                "request_id": "req_01",
                "channel_id": "41",
                "model": "gpt-5",
                "prompt_tokens": "120",
                "completion_tokens": "30",
                "charged_quota": "1250"
            }),
            occurred_at: 1_784_073_600,
            created_at: 1_784_073_601,
            quota_per_unit: "500000".to_string(),
        };

        let fact = new_api_usage_settlement_from_event("aether-primary", &event)
            .expect("usage event should be valid")
            .expect("usage event should become a profit fact");

        assert_eq!(fact.instance_id, "aether-primary");
        assert_eq!(fact.request_id, "req_01");
        assert_eq!(fact.channel_id, "41");
        assert_eq!(fact.model_id, "gpt-5");
        assert_eq!(fact.prompt_tokens, 120);
        assert_eq!(fact.completion_tokens, 30);
        assert_eq!(fact.charged_quota, "1250");
        assert_eq!(fact.quota_per_unit.as_deref(), Some("500000"));
        assert_eq!(fact.occurred_at_unix_ms, 1_784_073_600_000);
    }

    #[tokio::test]
    async fn pending_usage_is_marked_recorded_only_after_the_profit_worker_confirms_append() {
        let (store, pool) = test_event_stores().await;
        let event = usage_event("usage-event-1", "request-1");
        store
            .persist_batch("aether-primary", "", &[event.clone()], "cursor:usage-1")
            .await
            .expect("inbox and cursor should commit before the worker append");
        let (writer, mut receiver) = ProfitLedgerWriter::new(Arc::new(Default::default()));
        let consumer = SyncConsumer::new(consumer_config(), store.clone(), Arc::new(writer));

        let worker = tokio::spawn(async move {
            let work = receiver
                .recv()
                .await
                .expect("pending usage should reach the profit worker");
            assert_eq!(work.settlement().request_id, "request-1");
            sqlx::query("INSERT INTO relay_profit_ledger (instance_id, request_id) VALUES (?, ?)")
                .bind("aether-primary")
                .bind("request-1")
                .execute(&pool)
                .await
                .expect("the fake worker should model a committed ledger append");
            work.complete(Ok(ProfitPersistenceOutcome::Recorded));
        });

        assert_eq!(
            consumer
                .drain_pending_profit()
                .await
                .expect("worker-confirmed append should drain the event"),
            1
        );
        worker.await.expect("profit worker task should finish");
        assert!(
            store
                .pending_usage_events("aether-primary", 10)
                .await
                .expect("pending events should reload")
                .is_empty(),
            "the marker must be recorded only after the append acknowledgement"
        );
    }

    #[tokio::test]
    async fn unsettled_usage_stays_pending_without_a_profit_unknown_record() {
        let store = test_event_store().await;
        let event = usage_event("usage-event-2", "request-2");
        store
            .persist_batch("aether-primary", "", &[event.clone()], "cursor:usage-2")
            .await
            .expect("inbox and cursor should commit before settlement exists");
        let (writer, mut receiver) = ProfitLedgerWriter::new(Arc::new(Default::default()));
        let consumer = SyncConsumer::new(consumer_config(), store.clone(), Arc::new(writer));

        let worker = tokio::spawn(async move {
            let work = receiver
                .recv()
                .await
                .expect("pending usage should reach the profit worker");
            work.complete(Ok(ProfitPersistenceOutcome::Pending));
        });

        assert_eq!(
            consumer
                .drain_pending_profit()
                .await
                .expect("an unsettled usage fact is a retryable state"),
            0
        );
        worker.await.expect("profit worker task should finish");
        assert_eq!(
            store
                .pending_usage_events("aether-primary", 10)
                .await
                .expect("pending events should reload"),
            vec![event],
            "unsettled usage must stay pending instead of persisting an Unknown profit record"
        );
    }

    #[tokio::test]
    async fn malformed_usage_is_isolated_so_later_pending_usage_replays() {
        let (store, pool) = test_event_stores().await;
        let malformed = PersistedRelayEvent {
            id: "bad-usage".to_string(),
            dedupe_key: Some("usage:aether-primary:bad".to_string()),
            event_type: super::event_types::USAGE_SETTLED.to_string(),
            payload_json: "{not-json".to_string(),
            quota_per_unit: "500000".to_string(),
            occurred_at: 1_784_073_600,
            created_at: 1_784_073_601,
        };
        let valid = usage_event("valid-usage", "request-valid");
        store
            .persist_batch(
                "aether-primary",
                "",
                &[malformed, valid],
                "cursor:malformed-and-valid",
            )
            .await
            .expect("both durable inbox records should persist before replay");
        let (writer, mut receiver) = ProfitLedgerWriter::new(Arc::new(Default::default()));
        let consumer = SyncConsumer::new(consumer_config(), store.clone(), Arc::new(writer));
        let worker_pool = pool.clone();

        let worker = tokio::spawn(async move {
            let work = receiver
                .recv()
                .await
                .expect("the valid event should reach the profit worker");
            assert_eq!(work.settlement().request_id, "request-valid");
            sqlx::query("INSERT INTO relay_profit_ledger (instance_id, request_id) VALUES (?, ?)")
                .bind("aether-primary")
                .bind("request-valid")
                .execute(&worker_pool)
                .await
                .expect("the fake worker should model the valid ledger append");
            work.complete(Ok(ProfitPersistenceOutcome::Recorded));
        });

        let drained = consumer.drain_pending_profit().await;
        if drained.is_err() {
            worker.abort();
        }
        assert_eq!(
            drained.expect("malformed usage must not block valid pending replay"),
            1
        );
        worker.await.expect("profit worker task should finish");

        let invalid: (String, Option<String>, String) = sqlx::query_as(
            "SELECT profit_replay_state, profit_replay_error, profit_status
             FROM relay_event_inbox
             WHERE instance_id = ? AND event_id = ?",
        )
        .bind("aether-primary")
        .bind("bad-usage")
        .fetch_one(&pool)
        .await
        .expect("malformed inbox state should load");
        assert_eq!(invalid.0, "invalid");
        assert!(invalid.1.is_some());
        assert_eq!(invalid.2, "pending");
    }

    #[tokio::test]
    async fn two_os_process_consumer_child() {
        if std::env::var_os(TWO_PROCESS_CHILD_ENV).is_none() {
            return;
        }

        let database_url = two_process_child_env(TWO_PROCESS_DATABASE_URL_ENV);
        let upstream_url = two_process_child_env(TWO_PROCESS_UPSTREAM_URL_ENV);
        let instance_id = two_process_child_env(TWO_PROCESS_INSTANCE_ID_ENV);
        let pool = two_process_child_pool(&database_url).await;
        let store = RelayEventInboxStore::sqlite(pool.clone());
        let (writer, mut receiver) = ProfitLedgerWriter::new(Arc::new(Default::default()));
        let worker_pool = pool.clone();
        let worker = tokio::spawn(async move {
            let mut profit_records_appended = 0_u64;
            while let Some(work) = receiver.recv().await {
                let settlement = work.settlement().clone();
                let result = sqlx::query(
                    "INSERT OR IGNORE INTO relay_profit_ledger (instance_id, request_id) VALUES (?, ?)",
                )
                .bind(&settlement.instance_id)
                .bind(&settlement.request_id)
                .execute(&worker_pool)
                .await
                .expect("child profit worker should persist its idempotent ledger record");
                if result.rows_affected() == 1 {
                    profit_records_appended += 1;
                }
                work.complete(Ok(ProfitPersistenceOutcome::Recorded));
            }
            profit_records_appended
        });
        let consumer = SyncConsumer::new(
            SyncConsumerConfig {
                new_api_base_url: upstream_url,
                control_secret: "test-secret".to_string(),
                instance_id,
                poll_interval_secs: 30,
                batch_size: 100,
            },
            store,
            Arc::new(writer),
        );

        let processed = consumer
            .poll_events()
            .await
            .expect("child consumer should finish its single New API poll");
        drop(consumer);
        let profit_records_appended = worker
            .await
            .expect("child profit worker should finish after its consumer exits");
        println!(
            "{TWO_PROCESS_CHILD_RESULT_PREFIX}{}",
            serde_json::json!({
                "processed": processed,
                "profit_records_appended": profit_records_appended,
            })
        );
    }

    #[tokio::test]
    async fn two_os_process_consumers_share_a_durable_cursor_and_settle_one_event_once() {
        let instance_id = format!("aether-two-process-{}", Uuid::new_v4().simple());
        let event_id = format!("usage-two-process-{}", Uuid::new_v4().simple());
        let request_id = format!("request-two-process-{}", Uuid::new_v4().simple());
        let arrivals = Arc::new(AtomicUsize::new(0));
        let fetch_barrier = Arc::new(Barrier::new(2));
        let observed_requests = Arc::new(Mutex::new(Vec::<(String, String, bool)>::new()));
        let upstream = Router::new().route(
            "/api/aether/v1/events",
            get({
                let arrivals = Arc::clone(&arrivals);
                let fetch_barrier = Arc::clone(&fetch_barrier);
                let observed_requests = Arc::clone(&observed_requests);
                let instance_id = instance_id.clone();
                let event_id = event_id.clone();
                let request_id = request_id.clone();
                move |request: Request| {
                    let arrivals = Arc::clone(&arrivals);
                    let fetch_barrier = Arc::clone(&fetch_barrier);
                    let observed_requests = Arc::clone(&observed_requests);
                    let instance_id = instance_id.clone();
                    let event_id = event_id.clone();
                    let request_id = request_id.clone();
                    async move {
                        let after = request
                            .uri()
                            .query()
                            .and_then(|query| {
                                query.split('&').find_map(|pair| {
                                    pair.strip_prefix("after=").map(ToOwned::to_owned)
                                })
                            })
                            .unwrap_or_default();
                        let request_instance_id = request
                            .headers()
                            .get("X-Aether-Instance-ID")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        let signature_is_valid = has_valid_control_signature(&request);
                        {
                            let mut requests = observed_requests
                                .lock()
                                .expect("two-process upstream request capture should lock");
                            requests.push((after, request_instance_id, signature_is_valid));
                        }
                        arrivals.fetch_add(1, Ordering::SeqCst);

                        if tokio::time::timeout(Duration::from_secs(10), fetch_barrier.wait())
                            .await
                            .is_err()
                        {
                            return (
                                StatusCode::GATEWAY_TIMEOUT,
                                Json(serde_json::json!({
                                    "success": false,
                                    "message": "both child consumers did not reach the same cursor"
                                })),
                            )
                                .into_response();
                        }

                        Json(serde_json::json!({
                            "success": true,
                            "data": {
                                "contract_version": "aether-newapi-events/v1",
                                "events": [{
                                    "id": event_id,
                                    "dedupe_key": format!("usage:{instance_id}:{request_id}"),
                                    "event_type": "usage_settled",
                                    "occurred_at": 1_784_073_600i64,
                                    "created_at": 1_784_073_601i64,
                                    "quota_per_unit": "500000",
                                    "payload": {
                                        "request_id": request_id,
                                        "channel_id": "41",
                                        "model": "gpt-5",
                                        "prompt_tokens": "120",
                                        "completion_tokens": "30",
                                        "charged_quota": "1250"
                                    }
                                }],
                                "next_cursor": "cursor:1",
                                "has_more": false
                            }
                        }))
                        .into_response()
                    }
                }
            }),
        );
        let (upstream_url, upstream_handle) = crate::tests::start_server(upstream).await;
        let (parent_pool, database_path, database_url) = two_process_test_database().await;

        let first_child = spawn_two_process_consumer(&database_url, &upstream_url, &instance_id);
        let second_child = spawn_two_process_consumer(&database_url, &upstream_url, &instance_id);
        let first_wait = tokio::task::spawn_blocking(move || {
            first_child
                .wait_with_output()
                .expect("first two-process consumer child should exit")
        });
        let second_wait = tokio::task::spawn_blocking(move || {
            second_child
                .wait_with_output()
                .expect("second two-process consumer child should exit")
        });
        let (first_output, second_output) = tokio::time::timeout(Duration::from_secs(20), async {
            let first_output = first_wait.await.expect("first child wait task should join");
            let second_output = second_wait
                .await
                .expect("second child wait task should join");
            (first_output, second_output)
        })
        .await
        .expect("both child consumers should finish their coordinated poll");
        let first_result = parse_two_process_child_result(first_output);
        let second_result = parse_two_process_child_result(second_output);

        assert_eq!(
            arrivals.load(Ordering::SeqCst),
            2,
            "both OS processes must fetch the same initial cursor before either response releases"
        );
        let observed_requests = observed_requests
            .lock()
            .expect("two-process upstream request capture should lock")
            .clone();
        assert_eq!(observed_requests.len(), 2);
        assert!(
            observed_requests
                .iter()
                .all(|(after, observed_instance_id, signature_is_valid)| {
                    after.is_empty()
                        && observed_instance_id == &instance_id
                        && *signature_is_valid
                }),
            "both OS processes must make correctly signed New API reads from the same durable cursor"
        );
        assert_eq!(
            first_result.processed + second_result.processed,
            1,
            "the cursor compare-and-set must let exactly one process persist the fetched page"
        );
        assert_eq!(
            first_result.profit_records_appended + second_result.profit_records_appended,
            1,
            "exactly one child profit worker must append the settled usage fact"
        );

        let durable_cursor: String =
            sqlx::query_scalar("SELECT cursor FROM relay_event_cursors WHERE instance_id = ?")
                .bind(&instance_id)
                .fetch_one(&parent_pool)
                .await
                .expect("shared two-process cursor should load");
        let inbox_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM relay_event_inbox WHERE instance_id = ? AND event_id = ?",
        )
        .bind(&instance_id)
        .bind(&event_id)
        .fetch_one(&parent_pool)
        .await
        .expect("shared two-process inbox count should load");
        let profit_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM relay_profit_ledger WHERE instance_id = ? AND request_id = ?",
        )
        .bind(&instance_id)
        .bind(&request_id)
        .fetch_one(&parent_pool)
        .await
        .expect("shared two-process profit count should load");
        let recorded_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM relay_event_inbox
             WHERE instance_id = ? AND event_id = ? AND profit_status = 'recorded'",
        )
        .bind(&instance_id)
        .bind(&event_id)
        .fetch_one(&parent_pool)
        .await
        .expect("shared two-process profit acknowledgement should load");
        assert_eq!(durable_cursor, "cursor:1");
        assert_eq!(
            inbox_count, 1,
            "one durable inbox event must survive two OS processes"
        );
        assert_eq!(
            profit_count, 1,
            "one durable profit ledger row must survive two OS processes"
        );
        assert_eq!(
            recorded_count, 1,
            "the single durable inbox event must receive exactly one recorded settlement acknowledgement"
        );

        upstream_handle.abort();
        drop(parent_pool);
        let _ = std::fs::remove_file(database_path);
    }

    #[tokio::test]
    async fn two_independent_consumers_share_a_durable_cursor_and_settle_one_event_once() {
        const INSTANCE_ID: &str = "aether-primary";
        const EVENT_ID: &str = "usage-two-instance";
        const REQUEST_ID: &str = "request-two-instance";

        let arrivals = Arc::new(AtomicUsize::new(0));
        let response_positions = Arc::new(AtomicUsize::new(0));
        let first_response = Arc::new(Notify::new());
        let second_response = Arc::new(Notify::new());
        let observed_requests = Arc::new(Mutex::new(Vec::<(String, String, bool)>::new()));
        let upstream = Router::new().route(
            "/api/aether/v1/events",
            get({
                let arrivals = Arc::clone(&arrivals);
                let response_positions = Arc::clone(&response_positions);
                let first_response = Arc::clone(&first_response);
                let second_response = Arc::clone(&second_response);
                let observed_requests = Arc::clone(&observed_requests);
                move |request: Request| {
                    let arrivals = Arc::clone(&arrivals);
                    let response_positions = Arc::clone(&response_positions);
                    let first_response = Arc::clone(&first_response);
                    let second_response = Arc::clone(&second_response);
                    let observed_requests = Arc::clone(&observed_requests);
                    async move {
                        let after = request
                            .uri()
                            .query()
                            .and_then(|query| {
                                query.split('&').find_map(|pair| {
                                    pair.strip_prefix("after=").map(ToOwned::to_owned)
                                })
                            })
                            .unwrap_or_default();
                        let instance_id = request
                            .headers()
                            .get("X-Aether-Instance-ID")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        let has_signature = request
                            .headers()
                            .get("X-Aether-Signature")
                            .map(|value| !value.as_bytes().is_empty())
                            .unwrap_or(false);
                        observed_requests
                            .lock()
                            .expect("upstream request capture should lock")
                            .push((after, instance_id, has_signature));

                        let position = response_positions.fetch_add(1, Ordering::SeqCst);
                        arrivals.fetch_add(1, Ordering::SeqCst);
                        if position == 0 {
                            first_response.notified().await;
                        } else {
                            second_response.notified().await;
                        }

                        Json(serde_json::json!({
                            "success": true,
                            "data": {
                                "contract_version": "aether-newapi-events/v1",
                                "events": [{
                                    "id": EVENT_ID,
                                    "dedupe_key": "usage:aether-primary:request-two-instance",
                                    "event_type": "usage_settled",
                                    "occurred_at": 1_784_073_600i64,
                                    "created_at": 1_784_073_601i64,
                                    "quota_per_unit": "500000",
                                    "payload": {
                                        "request_id": REQUEST_ID,
                                        "channel_id": "41",
                                        "model": "gpt-5",
                                        "prompt_tokens": "120",
                                        "completion_tokens": "30",
                                        "charged_quota": "1250"
                                    }
                                }],
                                "next_cursor": "cursor:1",
                                "has_more": false
                            }
                        }))
                    }
                }
            }),
        );
        let (upstream_url, upstream_handle) = crate::tests::start_server(upstream).await;
        let (first_store, second_store, first_pool, second_pool, database_path) =
            shared_file_test_event_stores().await;

        let appended_profit_records = Arc::new(AtomicUsize::new(0));
        let (first_writer, mut first_receiver) =
            ProfitLedgerWriter::new(Arc::new(Default::default()));
        let first_worker_pool = first_pool.clone();
        let first_worker_appends = Arc::clone(&appended_profit_records);
        let first_worker = tokio::spawn(async move {
            append_next_profit_work(
                &mut first_receiver,
                &first_worker_pool,
                first_worker_appends.as_ref(),
            )
            .await;
        });
        let (second_writer, mut second_receiver) =
            ProfitLedgerWriter::new(Arc::new(Default::default()));
        let second_worker_pool = second_pool.clone();
        let second_worker_appends = Arc::clone(&appended_profit_records);
        let second_worker = tokio::spawn(async move {
            append_next_profit_work(
                &mut second_receiver,
                &second_worker_pool,
                second_worker_appends.as_ref(),
            )
            .await;
        });

        let config = SyncConsumerConfig {
            new_api_base_url: upstream_url,
            control_secret: "test-secret".to_string(),
            instance_id: INSTANCE_ID.to_string(),
            poll_interval_secs: 30,
            batch_size: 100,
        };
        let first_consumer =
            SyncConsumer::new(config.clone(), first_store.clone(), Arc::new(first_writer));
        let second_consumer =
            SyncConsumer::new(config, second_store.clone(), Arc::new(second_writer));
        let first_poll = tokio::spawn({
            let consumer = first_consumer.clone();
            async move { consumer.poll_events().await }
        });
        let second_poll = tokio::spawn({
            let consumer = second_consumer.clone();
            async move { consumer.poll_events().await }
        });

        for _ in 0..100 {
            if arrivals.load(Ordering::SeqCst) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            arrivals.load(Ordering::SeqCst),
            2,
            "both independent consumers must fetch the same initial cursor before either response releases"
        );

        first_response.notify_one();
        for _ in 0..100 {
            if first_store
                .cursor(INSTANCE_ID)
                .await
                .expect("shared cursor should read")
                == "cursor:1"
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            first_store
                .cursor(INSTANCE_ID)
                .await
                .expect("first response should advance the shared cursor"),
            "cursor:1"
        );

        second_response.notify_one();
        let first_processed = first_poll
            .await
            .expect("first consumer task should join")
            .expect("first consumer poll should complete");
        let second_processed = second_poll
            .await
            .expect("second consumer task should join")
            .expect("second consumer poll should complete");
        let processed = [first_processed, second_processed];
        assert_eq!(
            processed.iter().filter(|&&count| count == 1).count(),
            1,
            "exactly one consumer may commit the shared New API page"
        );
        assert_eq!(
            processed.iter().filter(|&&count| count == 0).count(),
            1,
            "the stale consumer must observe the durable cursor conflict"
        );

        let observed_requests = observed_requests
            .lock()
            .expect("upstream request capture should lock")
            .clone();
        assert_eq!(observed_requests.len(), 2);
        assert!(
            observed_requests
                .iter()
                .all(|(after, instance_id, signed)| {
                    after.is_empty() && instance_id == INSTANCE_ID && *signed
                }),
            "both independent consumers must issue signed New API reads from the same durable cursor"
        );

        let inbox_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM relay_event_inbox WHERE instance_id = ? AND event_id = ?",
        )
        .bind(INSTANCE_ID)
        .bind(EVENT_ID)
        .fetch_one(&first_pool)
        .await
        .expect("shared inbox count should load");
        let profit_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM relay_profit_ledger WHERE instance_id = ? AND request_id = ?",
        )
        .bind(INSTANCE_ID)
        .bind(REQUEST_ID)
        .fetch_one(&first_pool)
        .await
        .expect("shared profit count should load");
        let profit_status: String = sqlx::query_scalar(
            "SELECT profit_status FROM relay_event_inbox WHERE instance_id = ? AND event_id = ?",
        )
        .bind(INSTANCE_ID)
        .bind(EVENT_ID)
        .fetch_one(&first_pool)
        .await
        .expect("shared profit marker should load");
        assert_eq!(
            inbox_count, 1,
            "one shared inbox event must survive two consumers"
        );
        assert_eq!(
            profit_count, 1,
            "one shared ledger row must survive two consumers"
        );
        assert_eq!(profit_status, "recorded");
        assert_eq!(
            appended_profit_records.load(Ordering::SeqCst),
            1,
            "only one worker may append the settlement fact"
        );

        first_worker.abort();
        second_worker.abort();
        let _ = first_worker.await;
        let _ = second_worker.await;
        upstream_handle.abort();
        drop((
            first_consumer,
            second_consumer,
            first_store,
            second_store,
            first_pool,
            second_pool,
        ));
        let _ = std::fs::remove_file(database_path);
    }
}
