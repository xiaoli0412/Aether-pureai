//! Aether 事件发布 (Outbox)
//!
//! 保留至少 30 天的事件流，供 New API 或其他系统拉取。
//! 事件类型：路由决策变化、上游渠道健康变化、定价变化、成本异常。

use aether_data::repository::relay_events::{PersistedRelayOutboxEvent, RelayEventOutboxStore};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::debug;
use uuid::Uuid;

use super::error::RelayError;

/// 出站事件类型
pub mod outbound_event_types {
    pub const ROUTE_DECISION_CHANGED: &str = "route_decision_changed";
    pub const CHANNEL_HEALTH_CHANGED: &str = "channel_health_changed";
    pub const PRICING_UPDATED: &str = "pricing_updated";
    pub const COST_ANOMALY: &str = "cost_anomaly";
}

/// 出站事件记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundEvent {
    pub id: String,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub created_at: String,
}

/// 事件发布器
#[derive(Clone)]
pub struct EventOutbox {
    store: RelayEventOutboxStore,
    instance_id: String,
}

impl EventOutbox {
    pub fn new(store: RelayEventOutboxStore, instance_id: String) -> Self {
        Self { store, instance_id }
    }

    /// 发布一个事件到 outbox
    pub async fn publish(
        &self,
        event_type: &str,
        payload: serde_json::Value,
    ) -> Result<String, RelayError> {
        let event_id = Uuid::new_v4().to_string();
        let payload_json = serde_json::to_string(&payload)
            .map_err(|e| RelayError::Internal(format!("serialize outbox event: {}", e)))?;
        let event = PersistedRelayOutboxEvent {
            id: event_id.clone(),
            event_type: event_type.to_string(),
            payload_json,
            created_at_unix_ms: Utc::now().timestamp_millis(),
        };
        self.store
            .append(&self.instance_id, &event)
            .await
            .map_err(|error| RelayError::Internal(format!("persist outbox event: {error}")))?;

        debug!(event_id = %event_id, event_type = %event_type, "outbox event published");
        Ok(event_id)
    }

    /// 查询事件（cursor-based）
    pub async fn query_events(
        &self,
        after_cursor: &str,
        limit: usize,
    ) -> Result<(Vec<OutboundEvent>, String, bool), RelayError> {
        let page = self
            .store
            .page(&self.instance_id, after_cursor, limit)
            .await
            .map_err(|error| RelayError::Internal(format!("query outbox events: {error}")))?;
        let events = page
            .events
            .into_iter()
            .map(|event| {
                let payload = serde_json::from_str(&event.payload_json).map_err(|error| {
                    RelayError::Internal(format!("decode outbox event payload: {error}"))
                })?;
                let created_at =
                    chrono::DateTime::<Utc>::from_timestamp_millis(event.created_at_unix_ms)
                        .ok_or_else(|| {
                            RelayError::Internal("invalid outbox event timestamp".to_string())
                        })?
                        .to_rfc3339();
                Ok(OutboundEvent {
                    id: event.id,
                    event_type: event.event_type,
                    payload,
                    created_at,
                })
            })
            .collect::<Result<Vec<_>, RelayError>>()?;
        Ok((events, page.next_cursor, page.has_more))
    }
}

#[cfg(test)]
mod tests {
    use aether_data::repository::relay_events::RelayEventOutboxStore;
    use sqlx::sqlite::SqlitePoolOptions;

    use super::EventOutbox;

    async fn test_store() -> RelayEventOutboxStore {
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

    #[tokio::test]
    async fn published_events_are_read_from_the_database_in_cursor_order() {
        let outbox = EventOutbox::new(test_store().await, "aether-primary".to_string());
        let event_id = outbox
            .publish(
                "route_decision_changed",
                serde_json::json!({"request_id": "req-1"}),
            )
            .await
            .expect("event should publish");

        let (events, next_cursor, has_more) = outbox
            .query_events("", 10)
            .await
            .expect("events should load");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, event_id);
        assert_eq!(events[0].event_type, "route_decision_changed");
        assert_eq!(
            events[0].payload,
            serde_json::json!({"request_id": "req-1"})
        );
        assert_eq!(next_cursor, "1");
        assert!(!has_more);
    }
}
