//! 上游余额实时监控
//!
//! 定期查询每个上游 key 的余额，计算消耗速率，预测余额耗尽时间，
//! 余额不足时自动降权或熔断。

use std::sync::Arc;
use std::time::Duration;

use aether_runtime_state::RuntimeState;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use super::config::ChannelConfigStore;
use super::engine::RelayEngineConfig;
use super::error::RelayError;
use super::health::{HealthStore, RequestOutcome};
use super::upstream_client::UpstreamApiClient;

/// 余额快照记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceSnapshot {
    pub channel_id: String,
    pub key_id: String,
    pub remaining_quota: f64,
    pub used_quota: f64,
    pub total_quota: Option<f64>,
    pub is_overdue: bool,
    /// 与上次查询的差值
    pub delta_quota: f64,
    /// 查询时间 (RFC3339)
    pub queried_at: String,
}

/// 余额预测
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalancePrediction {
    pub channel_id: String,
    pub key_id: String,
    /// 每分钟消耗速率 (quota/min)
    pub burn_rate_per_minute: f64,
    /// 预计耗尽时间 (UTC RFC3339)，None 表示无法预测
    pub estimated_exhaustion_at: Option<String>,
    /// 剩余比例 (0.0 - 1.0)
    pub remaining_ratio: f64,
}

/// 余额监控器
#[derive(Clone)]
pub struct BalanceMonitor {
    pub(crate) config: Arc<RelayEngineConfig>,
    pub(crate) runtime_state: RuntimeState,
    pub(crate) config_store: Arc<ChannelConfigStore>,
    pub(crate) health_store: Arc<HealthStore>,
    pub(crate) upstream_client: Arc<dyn UpstreamApiClient>,
}

impl BalanceMonitor {
    pub fn new(
        config: Arc<RelayEngineConfig>,
        runtime_state: RuntimeState,
        config_store: Arc<ChannelConfigStore>,
        health_store: Arc<HealthStore>,
        upstream_client: Arc<dyn UpstreamApiClient>,
    ) -> Self {
        Self {
            config,
            runtime_state,
            config_store,
            health_store,
            upstream_client,
        }
    }

    /// 启动余额监控后台任务
    pub fn spawn_monitor_task(
        self: Arc<Self>,
        mut shutdown: watch::Receiver<()>,
    ) -> tokio::task::JoinHandle<()> {
        let interval = Duration::from_secs(self.config.balance_check_interval_secs.unwrap_or(60));
        tokio::spawn(async move {
            info!(interval_secs = ?interval, "starting balance monitor background task");
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if let Err(e) = self.check_all_balances().await {
                            error!(error = %e, "balance check failed");
                        }
                    }
                    _ = shutdown.changed() => {
                        info!("balance monitor shutting down");
                        break;
                    }
                }
            }
        })
    }

    /// 检查所有已启用通道的余额
    pub async fn check_all_balances(&self) -> Result<Vec<BalanceSnapshot>, RelayError> {
        let enabled_ids = self.config_store.get_enabled_channel_ids().await?;
        let mut snapshots = Vec::new();

        for channel_id in &enabled_ids {
            match self.check_channel_balance(channel_id).await {
                Ok(snapshot) => {
                    // 处理低余额告警和降权
                    self.evaluate_balance_action(&snapshot).await?;
                    snapshots.push(snapshot);
                }
                Err(e) => {
                    warn!(channel_id = %channel_id, error = %e, "balance check failed for channel");
                }
            }
        }
        Ok(snapshots)
    }

    /// 检查单个通道的余额
    async fn check_channel_balance(&self, channel_id: &str) -> Result<BalanceSnapshot, RelayError> {
        let channel = self
            .config_store
            .get_channel_from_runtime(channel_id)
            .await?
            .ok_or_else(|| RelayError::NotFound(format!("channel {}", channel_id)))?;

        // Get keys for this channel
        let keys_key = format!("relay:channel:keys:{}", channel_id);
        let keys_json = self
            .runtime_state
            .kv_get(&keys_key)
            .await
            .map_err(|e| RelayError::RuntimeState(e.to_string()))?;

        let first_key = match keys_json {
            Some(json) => {
                let keys: Vec<super::config::StoredRelayChannelKey> =
                    serde_json::from_str(&json)
                        .map_err(|e| RelayError::Internal(format!("parse keys: {}", e)))?;
                keys.into_iter()
                    .next()
                    .ok_or_else(|| RelayError::NotFound("no keys for channel".into()))?
            }
            None => return Err(RelayError::NotFound("no keys for channel".into())),
        };

        // Query upstream balance
        let balance = self
            .upstream_client
            .get_balance(&channel.endpoint, &first_key.api_key)
            .await?;

        // Get previous snapshot for delta calculation
        let prev_key = format!("relay:balance:latest:{}", channel_id);
        let prev_remaining = match self.runtime_state.kv_get(&prev_key).await {
            Ok(Some(v)) => v.parse::<f64>().unwrap_or(balance.remaining_quota),
            _ => balance.remaining_quota,
        };
        let delta = prev_remaining - balance.remaining_quota;

        let snapshot = BalanceSnapshot {
            channel_id: channel_id.to_string(),
            key_id: first_key.id.clone(),
            remaining_quota: balance.remaining_quota,
            used_quota: balance.used_quota,
            total_quota: balance.total_quota,
            is_overdue: balance.is_overdue,
            delta_quota: delta,
            queried_at: Utc::now().to_rfc3339(),
        };

        // Store latest remaining value
        let snap_key = format!("relay:balance:latest:{}", channel_id);
        let _ = self
            .runtime_state
            .kv_set(
                &snap_key,
                balance.remaining_quota.to_string(),
                Some(Duration::from_secs(3600)),
            )
            .await;

        // Append to history for burn rate calculation
        let history_key = format!("relay:balance:history:{}", channel_id);
        if let Ok(json) = serde_json::to_string(&snapshot) {
            let _ = self
                .runtime_state
                .kv_set(
                    &format!("{}:{}", history_key, Utc::now().timestamp()),
                    json,
                    Some(Duration::from_secs(600)), // Keep 10 min of history
                )
                .await;
        }

        debug!(channel_id = %channel_id, remaining = balance.remaining_quota, delta = delta, "balance checked");
        Ok(snapshot)
    }

    /// 评估余额状态并执行相应动作
    async fn evaluate_balance_action(&self, snapshot: &BalanceSnapshot) -> Result<(), RelayError> {
        let threshold = self.config.balance_low_threshold_percent.unwrap_or(10.0) / 100.0;

        if snapshot.is_overdue || snapshot.remaining_quota <= 0.0 {
            // 余额为零或欠费 — 立即熔断
            warn!(
                channel_id = %snapshot.channel_id,
                remaining = snapshot.remaining_quota,
                "channel balance depleted, marking as circuit open"
            );
            let outcome = RequestOutcome {
                channel_id: snapshot.channel_id.clone(),
                success: false,
                latency_ms: 0,
            };
            // Force circuit break by reporting multiple failures
            for _ in 0..10 {
                let _ = self.health_store.report_outcome(&outcome).await;
            }
            return Ok(());
        }

        // Check if below threshold
        let remaining_ratio = if let Some(total) = snapshot.total_quota {
            if total > 0.0 {
                snapshot.remaining_quota / total
            } else {
                1.0
            }
        } else {
            // If no total, use remaining vs used
            let total = snapshot.remaining_quota + snapshot.used_quota;
            if total > 0.0 {
                snapshot.remaining_quota / total
            } else {
                1.0
            }
        };

        if remaining_ratio < threshold {
            warn!(
                channel_id = %snapshot.channel_id,
                remaining_ratio = remaining_ratio,
                threshold = threshold,
                "channel balance below threshold, reducing route weight"
            );
            // Store low-balance flag for route selector to consider
            let flag_key = format!("relay:balance:low:{}", snapshot.channel_id);
            let _ = self
                .runtime_state
                .kv_set(&flag_key, "1", Some(Duration::from_secs(120)))
                .await;
        }

        Ok(())
    }

    /// 获取余额预测
    pub async fn get_prediction(
        &self,
        channel_id: &str,
    ) -> Result<Option<BalancePrediction>, RelayError> {
        let snap_key = format!("relay:balance:latest:{}", channel_id);
        let remaining = match self.runtime_state.kv_get(&snap_key).await {
            Ok(Some(v)) => v.parse::<f64>().unwrap_or(0.0),
            _ => return Ok(None),
        };

        // Simple burn rate from delta history (placeholder — full impl would use sliding window)
        let total_approx = remaining * 1.2; // rough estimate if total unknown
        let remaining_ratio = if total_approx > 0.0 {
            remaining / total_approx
        } else {
            1.0
        };

        Ok(Some(BalancePrediction {
            channel_id: channel_id.to_string(),
            key_id: String::new(),
            burn_rate_per_minute: 0.0, // TODO: calculate from history
            estimated_exhaustion_at: None,
            remaining_ratio,
        }))
    }
}
