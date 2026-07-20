//! 健康指标存储与更新
//!
//! 管理上游通道的健康数据：接收请求结果报告，更新 RuntimeState 中的健康指标，
//! 执行熔断状态转换。

use std::sync::Arc;
use std::time::Duration;

use aether_relay_core::health::{calculate_health_score, should_circuit_break, should_half_open};
use aether_relay_core::models::{ChannelHealthState, HealthConfig, HealthMetrics};
use aether_runtime_state::RuntimeState;
use chrono::Utc;
use tracing::{debug, info};

use super::engine::RelayEngineConfig;
use super::error::RelayError;

/// 请求结果报告（请求完成后调用）
#[derive(Debug, Clone)]
pub struct RequestOutcome {
    pub channel_id: String,
    pub success: bool,
    pub latency_ms: u64,
}

/// 健康指标存储服务
#[derive(Clone)]
pub struct HealthStore {
    pub(crate) config: Arc<RelayEngineConfig>,
    pub(crate) runtime_state: RuntimeState,
}

impl HealthStore {
    pub fn new(config: Arc<RelayEngineConfig>, runtime_state: RuntimeState) -> Self {
        Self {
            config,
            runtime_state,
        }
    }

    /// 构建 HealthConfig from RelayEngineConfig
    fn health_config(&self) -> HealthConfig {
        HealthConfig {
            rolling_window_secs: self.config.health_rolling_window_secs,
            latency_threshold_ms: self.config.latency_threshold_ms,
            latency_weight: 0.3,
            availability_weight: 0.7,
            circuit_breaker_threshold: self.config.circuit_breaker_threshold,
            circuit_breaker_cooldown_secs: self.config.circuit_breaker_cooldown_secs,
        }
    }

    /// 报告一次请求结果，更新健康指标
    pub async fn report_outcome(&self, outcome: &RequestOutcome) -> Result<(), RelayError> {
        let key = format!("relay:health:{}", outcome.channel_id);
        let now_ms = Utc::now().timestamp_millis() as u64;

        // Get current metrics or create default
        let mut metrics = self
            .get_metrics(&outcome.channel_id)
            .await?
            .unwrap_or(HealthMetrics {
                total_requests: 0,
                failed_requests: 0,
                consecutive_failures: 0,
                p95_latency_ms: 0,
                last_success_at: None,
                last_failure_at: None,
                state: ChannelHealthState::Normal,
                state_changed_at: now_ms,
            });

        // Update metrics based on outcome
        metrics.total_requests += 1;
        if outcome.success {
            metrics.consecutive_failures = 0;
            metrics.last_success_at = Some(now_ms);
            // Simple moving average for p95 approximation
            metrics.p95_latency_ms = ((metrics.p95_latency_ms as f64 * 0.95)
                + (outcome.latency_ms as f64 * 0.05)) as u64;
        } else {
            metrics.failed_requests += 1;
            metrics.consecutive_failures += 1;
            metrics.last_failure_at = Some(now_ms);
        }

        // Check circuit breaker state transitions
        let health_config = self.health_config();
        match metrics.state {
            ChannelHealthState::Normal => {
                if should_circuit_break(&metrics, &health_config) {
                    info!(channel_id = %outcome.channel_id, consecutive_failures = metrics.consecutive_failures, "circuit breaker OPENED");
                    metrics.state = ChannelHealthState::CircuitOpen;
                    metrics.state_changed_at = now_ms;
                }
            }
            ChannelHealthState::CircuitOpen => {
                if should_half_open(&metrics, &health_config, now_ms) {
                    info!(channel_id = %outcome.channel_id, "circuit breaker entering HALF-OPEN");
                    metrics.state = ChannelHealthState::HalfOpen;
                    metrics.state_changed_at = now_ms;
                }
            }
            ChannelHealthState::HalfOpen => {
                if outcome.success {
                    info!(channel_id = %outcome.channel_id, "circuit breaker CLOSED (probe success)");
                    metrics.state = ChannelHealthState::Normal;
                    metrics.state_changed_at = now_ms;
                    metrics.consecutive_failures = 0;
                } else {
                    info!(channel_id = %outcome.channel_id, "circuit breaker re-OPENED (probe failed)");
                    metrics.state = ChannelHealthState::CircuitOpen;
                    metrics.state_changed_at = now_ms;
                }
            }
        }

        // Write updated metrics to RuntimeState
        let ttl = Duration::from_secs(self.config.health_rolling_window_secs * 2);
        let value = serde_json::to_string(&metrics)
            .map_err(|e| RelayError::Internal(format!("serialize health metrics: {}", e)))?;
        self.runtime_state
            .kv_set(&key, value, Some(ttl))
            .await
            .map_err(|e| RelayError::RuntimeState(e.to_string()))?;

        // Update health score in sorted set for fast lookup
        let score = calculate_health_score(&metrics, &health_config);
        let score_key = "relay:health:score";
        self.runtime_state
            .score_set(score_key, &outcome.channel_id, score)
            .await
            .map_err(|e| RelayError::RuntimeState(e.to_string()))?;

        debug!(
            channel_id = %outcome.channel_id,
            health_score = score,
            state = ?metrics.state,
            "health metrics updated"
        );

        Ok(())
    }

    /// 获取通道的当前健康指标
    pub async fn get_metrics(&self, channel_id: &str) -> Result<Option<HealthMetrics>, RelayError> {
        let key = format!("relay:health:{}", channel_id);
        match self.runtime_state.kv_get(&key).await {
            Ok(Some(value)) => {
                let metrics: HealthMetrics = serde_json::from_str(&value)
                    .map_err(|e| RelayError::Internal(format!("deserialize health: {}", e)))?;
                Ok(Some(metrics))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(RelayError::RuntimeState(e.to_string())),
        }
    }

    /// 获取通道的当前健康分数
    pub async fn get_health_score(&self, channel_id: &str) -> Result<f64, RelayError> {
        let metrics = self.get_metrics(channel_id).await?;
        match metrics {
            Some(m) => {
                let config = self.health_config();
                Ok(calculate_health_score(&m, &config))
            }
            None => Ok(0.5), // 无数据时返回中性分数
        }
    }

    /// 获取通道的当前健康状态
    pub async fn get_channel_state(
        &self,
        channel_id: &str,
    ) -> Result<ChannelHealthState, RelayError> {
        let metrics = self.get_metrics(channel_id).await?;
        Ok(metrics
            .map(|m| m.state)
            .unwrap_or(ChannelHealthState::Normal))
    }

    /// 检查通道是否应允许探测请求（HalfOpen 状态）
    pub async fn should_allow_probe(&self, channel_id: &str) -> Result<bool, RelayError> {
        let state = self.get_channel_state(channel_id).await?;
        Ok(state == ChannelHealthState::HalfOpen)
    }

    /// 获取所有通道的健康分数（批量，用于路由决策）
    pub async fn get_all_health_scores(
        &self,
        channel_ids: &[String],
    ) -> Result<Vec<(String, f64, ChannelHealthState)>, RelayError> {
        let mut results = Vec::with_capacity(channel_ids.len());
        for channel_id in channel_ids {
            let metrics = self.get_metrics(channel_id).await?;
            match metrics {
                Some(m) => {
                    let config = self.health_config();
                    let score = calculate_health_score(&m, &config);
                    results.push((channel_id.clone(), score, m.state));
                }
                None => {
                    results.push((channel_id.clone(), 0.5, ChannelHealthState::Normal));
                }
            }
        }
        Ok(results)
    }
}
