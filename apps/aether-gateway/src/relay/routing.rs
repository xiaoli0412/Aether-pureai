//! 路由决策服务
//!
//! 连接 aether-relay-core 的纯逻辑路由算法与 RuntimeState，
//! 实现完整的路由决策流程。目标：每次决策 < 10ms。

use std::sync::Arc;
use std::time::Instant;

use aether_relay_core::models::{ChannelHealthState, RouteCandidate, RouteConfig, RouteDecision};
use aether_relay_core::routing::select_route;
use tracing::{debug, warn};

use super::config::ChannelConfigStore;
use super::discovery::{CachedPricingEntry, PriceDiscoveryService};
use super::engine::RelayEngineConfig;
use super::error::RelayError;
use super::health::HealthStore;

/// 路由决策服务
#[derive(Clone)]
pub struct RouteSelectionService {
    pub(crate) config: Arc<RelayEngineConfig>,
    pub(crate) config_store: Arc<ChannelConfigStore>,
    pub(crate) price_discovery: Arc<PriceDiscoveryService>,
    pub(crate) health_store: Arc<HealthStore>,
}

impl RouteSelectionService {
    pub fn new(
        config: Arc<RelayEngineConfig>,
        config_store: Arc<ChannelConfigStore>,
        price_discovery: Arc<PriceDiscoveryService>,
        health_store: Arc<HealthStore>,
    ) -> Self {
        Self {
            config,
            config_store,
            price_discovery,
            health_store,
        }
    }

    /// 为指定模型执行路由决策
    ///
    /// 返回 RouteDecision（含选中通道和备选列表），或 NoAvailableChannel 错误。
    /// 性能目标：< 10ms（所有数据在 RuntimeState 中，无 DB 查询）。
    pub async fn select_route_for_model(
        &self,
        model_id: &str,
    ) -> Result<RouteDecision, RelayError> {
        let start = Instant::now();

        // 1. 获取该模型的所有通道定价
        let pricing_entries = self.price_discovery.get_model_pricing(model_id).await?;
        if pricing_entries.is_empty() {
            return Err(RelayError::NoAvailableChannel {
                model_id: model_id.to_string(),
            });
        }

        // 2. 获取通道健康数据
        let channel_ids: Vec<String> = pricing_entries
            .iter()
            .map(|e| e.channel_id.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let health_data = self
            .health_store
            .get_all_health_scores(&channel_ids)
            .await?;

        // 3. 构建 RouteCandidate 列表
        let candidates = self
            .build_candidates(&pricing_entries, &health_data)
            .await?;
        if candidates.is_empty() {
            return Err(RelayError::NoAvailableChannel {
                model_id: model_id.to_string(),
            });
        }

        // 4. 构建路由配置（支持按模型覆盖）
        let route_config = RouteConfig {
            price_weight: self.config.price_weight,
            health_weight: self.config.health_weight,
        };

        // 5. 执行路由选择（纯函数，使用时间戳作为 seed 实现随机性）
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;

        let decision = select_route(candidates, &route_config, seed).ok_or_else(|| {
            RelayError::NoAvailableChannel {
                model_id: model_id.to_string(),
            }
        })?;

        let elapsed = start.elapsed();
        debug!(
            model_id = %model_id,
            selected_channel = %decision.selected.channel_id,
            composite_score = decision.composite_score,
            fallback_count = decision.fallbacks.len(),
            elapsed_us = elapsed.as_micros(),
            "route decision completed"
        );

        if elapsed.as_millis() > 10 {
            warn!(
                model_id = %model_id,
                elapsed_ms = elapsed.as_millis(),
                "route decision exceeded 10ms target"
            );
        }

        Ok(decision)
    }

    /// 构建 RouteCandidate 列表
    async fn build_candidates(
        &self,
        pricing_entries: &[CachedPricingEntry],
        health_data: &[(String, f64, ChannelHealthState)],
    ) -> Result<Vec<RouteCandidate>, RelayError> {
        let mut candidates = Vec::with_capacity(pricing_entries.len());

        // Build lookup map for health data
        let health_map: std::collections::HashMap<&str, (f64, ChannelHealthState)> = health_data
            .iter()
            .map(|(id, score, state)| (id.as_str(), (*score, *state)))
            .collect();

        // Deduplicate by channel_id (pick lowest cost key for each channel)
        let mut best_per_channel: std::collections::HashMap<&str, &CachedPricingEntry> =
            std::collections::HashMap::new();

        for entry in pricing_entries {
            let existing = best_per_channel.get(entry.channel_id.as_str());
            let is_better = match existing {
                None => true,
                Some(prev) => entry.cost_per_prompt_token_quota < prev.cost_per_prompt_token_quota,
            };
            if is_better {
                best_per_channel.insert(&entry.channel_id, entry);
            }
        }

        for (channel_id, entry) in &best_per_channel {
            let (health_score, state) = health_map
                .get(*channel_id)
                .copied()
                .unwrap_or((0.5, ChannelHealthState::Normal));

            // Get channel weight from runtime config
            let weight = match self
                .config_store
                .get_channel_from_runtime(channel_id)
                .await?
            {
                Some(ch) => ch.weight as u32,
                None => 1,
            };

            candidates.push(RouteCandidate {
                channel_id: channel_id.to_string(),
                price_per_prompt_token_quota: entry.cost_per_prompt_token_quota,
                price_per_completion_token_quota: entry.cost_per_completion_token_quota,
                health_score,
                weight,
                state,
            });
        }

        Ok(candidates)
    }

    /// 获取路由决策的备选通道（用于故障转移）
    pub async fn get_fallback<'a>(
        &self,
        decision: &'a RouteDecision,
        failed_channel_id: &str,
    ) -> Option<&'a RouteCandidate> {
        decision
            .fallbacks
            .iter()
            .find(|c| c.channel_id != failed_channel_id)
    }
}
