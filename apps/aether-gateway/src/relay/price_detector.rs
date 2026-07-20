//! 价格变化检测与自动切换
//!
//! 比对每次价格同步的结果与历史缓存，检测价格变化，
//! 触发告警并立即调整路由决策。

use std::sync::Arc;
use std::time::Duration;

use aether_runtime_state::RuntimeState;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::discovery::CachedPricingEntry;
use super::engine::RelayEngineConfig;
use super::error::RelayError;

/// 价格变化记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceChangeRecord {
    pub id: String,
    pub channel_id: String,
    pub model_id: String,
    pub old_model_ratio: f64,
    pub new_model_ratio: f64,
    pub old_group_ratio: f64,
    pub new_group_ratio: f64,
    /// 成本变化百分比 (正值=涨价, 负值=降价)
    pub cost_change_percent: f64,
    pub detected_at: String,
}

/// 价格飙升告警
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceSurgeAlert {
    pub channel_id: String,
    pub model_id: String,
    pub old_cost: f64,
    pub new_cost: f64,
    pub change_percent: f64,
    pub action_taken: String,
    pub alerted_at: String,
}

/// 价格变化检测器
#[derive(Clone)]
pub struct PriceChangeDetector {
    pub(crate) config: Arc<RelayEngineConfig>,
    pub(crate) runtime_state: RuntimeState,
}

impl PriceChangeDetector {
    pub fn new(config: Arc<RelayEngineConfig>, runtime_state: RuntimeState) -> Self {
        Self {
            config,
            runtime_state,
        }
    }

    /// 检测定价变化（在每次 price sync 完成后调用）
    ///
    /// 比对新同步的定价与之前缓存的定价，返回所有变化记录。
    pub async fn detect_changes(
        &self,
        channel_id: &str,
        new_entries: &[CachedPricingEntry],
    ) -> Result<Vec<PriceChangeRecord>, RelayError> {
        let mut changes = Vec::new();
        let surge_threshold = self.config.price_change_threshold_percent / 100.0;

        for entry in new_entries {
            let prev_key = format!("relay:price_prev:{}:{}", channel_id, entry.model_id);

            // Get previous pricing
            let prev = match self.runtime_state.kv_get(&prev_key).await {
                Ok(Some(json)) => serde_json::from_str::<PreviousPricing>(&json).ok(),
                _ => None,
            };

            if let Some(prev) = prev {
                let old_cost = prev.model_ratio * prev.group_ratio;
                let new_cost = entry.model_ratio * entry.group_ratio;

                if (old_cost - new_cost).abs() > f64::EPSILON {
                    let change_percent = if old_cost > 0.0 {
                        ((new_cost - old_cost) / old_cost) * 100.0
                    } else {
                        0.0
                    };

                    let record = PriceChangeRecord {
                        id: uuid::Uuid::new_v4().to_string(),
                        channel_id: channel_id.to_string(),
                        model_id: entry.model_id.clone(),
                        old_model_ratio: prev.model_ratio,
                        new_model_ratio: entry.model_ratio,
                        old_group_ratio: prev.group_ratio,
                        new_group_ratio: entry.group_ratio,
                        cost_change_percent: change_percent,
                        detected_at: Utc::now().to_rfc3339(),
                    };

                    // Check for price surge
                    if change_percent > surge_threshold * 100.0 {
                        self.handle_price_surge(
                            channel_id,
                            &entry.model_id,
                            old_cost,
                            new_cost,
                            change_percent,
                        )
                        .await?;
                    }

                    // Store change record
                    let history_key = format!(
                        "relay:price_history:{}:{}:{}",
                        channel_id,
                        entry.model_id,
                        Utc::now().timestamp()
                    );
                    if let Ok(json) = serde_json::to_string(&record) {
                        let _ = self
                            .runtime_state
                            .kv_set(&history_key, json, Some(Duration::from_secs(86400 * 30)))
                            .await;
                    }

                    info!(
                        channel_id = %channel_id,
                        model_id = %entry.model_id,
                        change_percent = change_percent,
                        "price change detected"
                    );

                    changes.push(record);
                }
            }

            // Update previous pricing cache
            let new_prev = PreviousPricing {
                model_ratio: entry.model_ratio,
                group_ratio: entry.group_ratio,
                completion_ratio: entry.completion_ratio,
                updated_at: Utc::now().to_rfc3339(),
            };
            if let Ok(json) = serde_json::to_string(&new_prev) {
                let _ = self.runtime_state.kv_set(&prev_key, json, None).await;
            }
        }

        Ok(changes)
    }

    /// 处理价格飙升
    async fn handle_price_surge(
        &self,
        channel_id: &str,
        model_id: &str,
        old_cost: f64,
        new_cost: f64,
        change_percent: f64,
    ) -> Result<(), RelayError> {
        warn!(
            channel_id = %channel_id,
            model_id = %model_id,
            old_cost = old_cost,
            new_cost = new_cost,
            change_percent = change_percent,
            "PRICE SURGE DETECTED - traffic will be shifted away"
        );

        let alert = PriceSurgeAlert {
            channel_id: channel_id.to_string(),
            model_id: model_id.to_string(),
            old_cost,
            new_cost,
            change_percent,
            action_taken: "route_weight_reduced".to_string(),
            alerted_at: Utc::now().to_rfc3339(),
        };

        // Store alert
        let alert_key = format!(
            "relay:price_surge:{}:{}:{}",
            channel_id,
            model_id,
            Utc::now().timestamp()
        );
        if let Ok(json) = serde_json::to_string(&alert) {
            let _ = self
                .runtime_state
                .kv_set(&alert_key, json, Some(Duration::from_secs(86400)))
                .await;
        }

        // Mark this channel+model as "price surged" to reduce its composite score
        let surge_flag = format!("relay:price_surged:{}:{}", channel_id, model_id);
        let _ = self
            .runtime_state
            .kv_set(
                &surge_flag,
                change_percent.to_string(),
                Some(Duration::from_secs(
                    self.config.price_sync_interval_secs * 3,
                )),
            )
            .await;

        Ok(())
    }

    /// 获取最近的价格变化记录
    pub async fn get_recent_changes(
        &self,
        _channel_id: &str,
        _limit: usize,
    ) -> Result<Vec<PriceChangeRecord>, RelayError> {
        // In full implementation, query from database or scan Redis keys
        // For now return empty
        Ok(vec![])
    }

    /// 检查某通道+模型是否处于价格飙升状态
    pub async fn is_price_surged(
        &self,
        channel_id: &str,
        model_id: &str,
    ) -> Result<bool, RelayError> {
        let key = format!("relay:price_surged:{}:{}", channel_id, model_id);
        match self.runtime_state.kv_get(&key).await {
            Ok(Some(_)) => Ok(true),
            _ => Ok(false),
        }
    }
}

/// 存储的前次价格数据（用于变化比对）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PreviousPricing {
    model_ratio: f64,
    group_ratio: f64,
    completion_ratio: f64,
    updated_at: String,
}
