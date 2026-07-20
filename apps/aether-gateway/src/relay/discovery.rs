//! 价格发现后台任务
//!
//! 定期从所有已启用的上游通道同步模型定价数据，缓存至 RuntimeState。

use std::sync::Arc;
use std::time::Duration;

use aether_relay_core::models::PricingParams;
use aether_relay_core::pricing::calculate_token_cost_quota;
use aether_runtime_state::RuntimeState;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{error, info, warn};

use super::config::{ChannelConfigStore, StoredRelayChannelKey};
use super::engine::RelayEngineConfig;
use super::error::RelayError;
use super::upstream_client::{HttpUpstreamClient, UpstreamApiClient};

/// 缓存的模型定价条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedPricingEntry {
    pub channel_id: String,
    pub key_id: String,
    pub model_id: String,
    pub model_ratio: f64,
    pub group_ratio: f64,
    pub completion_ratio: f64,
    pub cost_per_prompt_token_quota: f64,
    pub cost_per_completion_token_quota: f64,
    pub synced_at: String,
}

/// 价格同步结果摘要
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceSyncSummary {
    pub synced_at: String,
    pub total_models: usize,
    pub total_channels_synced: usize,
    pub failed_channels: Vec<String>,
}

/// 价格发现服务
#[derive(Clone)]
pub struct PriceDiscoveryService {
    pub(crate) config: Arc<RelayEngineConfig>,
    pub(crate) runtime_state: RuntimeState,
    pub(crate) config_store: Arc<ChannelConfigStore>,
    pub(crate) upstream_client: Arc<dyn UpstreamApiClient>,
}

impl PriceDiscoveryService {
    pub fn new(
        config: Arc<RelayEngineConfig>,
        runtime_state: RuntimeState,
        config_store: Arc<ChannelConfigStore>,
    ) -> Self {
        Self {
            config,
            runtime_state,
            config_store,
            upstream_client: Arc::new(HttpUpstreamClient::new()),
        }
    }

    /// 启动定期价格同步后台任务
    pub fn spawn_sync_task(
        self: Arc<Self>,
        mut shutdown: watch::Receiver<()>,
    ) -> tokio::task::JoinHandle<()> {
        let interval = Duration::from_secs(self.config.price_sync_interval_secs);
        tokio::spawn(async move {
            info!(
                interval_secs = self.config.price_sync_interval_secs,
                "starting price discovery background task"
            );

            // Initial sync on startup
            if let Err(e) = self.run_full_sync().await {
                error!(error = %e, "initial price sync failed");
            }

            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // Skip first immediate tick (already ran initial sync)

            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if let Err(e) = self.run_full_sync().await {
                            error!(error = %e, "periodic price sync failed");
                        }
                    }
                    _ = shutdown.changed() => {
                        info!("price discovery task shutting down");
                        break;
                    }
                }
            }
        })
    }

    /// 执行一次完整的价格同步（公开接口，可被手动触发）
    pub async fn run_full_sync(&self) -> Result<PriceSyncSummary, RelayError> {
        info!("starting full price sync");

        // Try to acquire distributed lock
        let lock_key = "relay:sync:lock:pricing";
        let lock_lease = self
            .runtime_state
            .lock_try_acquire(lock_key, "price-discovery", Duration::from_secs(60))
            .await
            .map_err(|e| RelayError::RuntimeState(e.to_string()))?;

        let lease = match lock_lease {
            Some(l) => l,
            None => {
                info!("price sync already in progress on another node, skipping");
                return Ok(PriceSyncSummary {
                    synced_at: Utc::now().to_rfc3339(),
                    total_models: 0,
                    total_channels_synced: 0,
                    failed_channels: vec!["(lock held by another node)".to_string()],
                });
            }
        };

        let enabled_ids = self.config_store.get_enabled_channel_ids().await?;
        let mut total_models = 0usize;
        let mut synced_channels = 0usize;
        let mut failed_channels = Vec::new();

        for channel_id in &enabled_ids {
            match self.sync_channel_pricing(channel_id).await {
                Ok(model_count) => {
                    total_models += model_count;
                    synced_channels += 1;
                    info!(channel_id = %channel_id, models = model_count, "channel pricing synced");
                }
                Err(e) => {
                    warn!(channel_id = %channel_id, error = %e, "channel pricing sync failed, keeping cached data");
                    failed_channels.push(channel_id.clone());
                }
            }
        }

        // Release lock
        let _ = self.runtime_state.lock_release(&lease).await;

        let summary = PriceSyncSummary {
            synced_at: Utc::now().to_rfc3339(),
            total_models,
            total_channels_synced: synced_channels,
            failed_channels,
        };

        // Store sync metadata
        let meta_key = "relay:meta:pricing:last_sync";
        if let Ok(json) = serde_json::to_string(&summary) {
            let _ = self.runtime_state.kv_set(meta_key, json, None).await;
        }

        info!(
            total_models = total_models,
            channels_synced = synced_channels,
            channels_failed = summary.failed_channels.len(),
            "price sync completed"
        );

        Ok(summary)
    }

    /// 同步单个通道的定价数据
    async fn sync_channel_pricing(&self, channel_id: &str) -> Result<usize, RelayError> {
        let channel = self
            .config_store
            .get_channel_from_runtime(channel_id)
            .await?
            .ok_or_else(|| RelayError::NotFound(format!("channel {}", channel_id)))?;

        // For now, we use the first enabled key to fetch ratio config
        // In future, each key might belong to a different group
        let key_data = self.get_channel_keys_from_runtime(channel_id).await?;
        if key_data.is_empty() {
            return Ok(0);
        }

        // Fetch ratio config from upstream using first key
        let first_key = &key_data[0];
        let ratio_config = self
            .upstream_client
            .get_ratio_config(&channel.endpoint, &first_key.api_key)
            .await?;

        let mut model_count = 0;
        let ttl = Some(Duration::from_secs(
            self.config.price_sync_interval_secs * 2,
        ));
        let now = Utc::now().to_rfc3339();

        // For each key (potentially different group), cache pricing per model
        for key in &key_data {
            let group_ratio = key.group_ratio;

            for (model_id, model_ratio) in &ratio_config.model_ratios {
                let completion_ratio = ratio_config
                    .completion_ratios
                    .get(model_id)
                    .copied()
                    .unwrap_or(2.0);

                let params = PricingParams {
                    model_ratio: *model_ratio,
                    group_ratio,
                    completion_ratio,
                };

                let cost_prompt = calculate_token_cost_quota(1, false, &params);
                let cost_completion = calculate_token_cost_quota(1, true, &params);

                let entry = CachedPricingEntry {
                    channel_id: channel_id.to_string(),
                    key_id: key.id.clone(),
                    model_id: model_id.clone(),
                    model_ratio: *model_ratio,
                    group_ratio,
                    completion_ratio,
                    cost_per_prompt_token_quota: cost_prompt,
                    cost_per_completion_token_quota: cost_completion,
                    synced_at: now.clone(),
                };

                // Cache to RuntimeState
                let cache_key = format!("relay:pricing:{}:{}", channel_id, model_id);
                if let Ok(json) = serde_json::to_string(&entry) {
                    let _ = self.runtime_state.kv_set(&cache_key, json, ttl).await;
                }

                model_count += 1;
            }
        }

        Ok(model_count)
    }

    /// 从 RuntimeState 获取通道的密钥列表
    async fn get_channel_keys_from_runtime(
        &self,
        channel_id: &str,
    ) -> Result<Vec<StoredRelayChannelKey>, RelayError> {
        let key = format!("relay:channel:keys:{}", channel_id);
        match self.runtime_state.kv_get(&key).await {
            Ok(Some(value)) => serde_json::from_str::<Vec<StoredRelayChannelKey>>(&value)
                .map_err(|e| RelayError::Internal(format!("deserialize keys: {}", e))),
            Ok(None) => Ok(vec![]),
            Err(e) => Err(RelayError::RuntimeState(e.to_string())),
        }
    }

    /// 获取某个模型在所有通道的定价
    pub async fn get_model_pricing(
        &self,
        model_id: &str,
    ) -> Result<Vec<CachedPricingEntry>, RelayError> {
        let enabled_ids = self.config_store.get_enabled_channel_ids().await?;
        let mut entries = Vec::new();

        for channel_id in &enabled_ids {
            let cache_key = format!("relay:pricing:{}:{}", channel_id, model_id);
            if let Ok(Some(value)) = self.runtime_state.kv_get(&cache_key).await {
                if let Ok(entry) = serde_json::from_str::<CachedPricingEntry>(&value) {
                    entries.push(entry);
                }
            }
        }

        Ok(entries)
    }
}
