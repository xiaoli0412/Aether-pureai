//! 通道配置存储 - 数据库 CRUD + RuntimeState 同步

use aether_relay_core::models::{ChannelConfig, ChannelKeyConfig};
use aether_relay_core::validation::validate_channel_config;
use aether_runtime_state::RuntimeState;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{error, info, warn};
use uuid::Uuid;

use super::error::RelayError;

/// 数据库中的通道记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredRelayChannel {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub endpoint: String,
    pub weight: i32,
    pub enabled: bool,
    pub price_weight_override: Option<f64>,
    pub health_weight_override: Option<f64>,
    pub config_json: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// 数据库中的密钥记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredRelayChannelKey {
    pub id: String,
    pub channel_id: String,
    pub api_key: String,
    pub group_id: Option<String>,
    pub group_ratio: f64,
    pub label: Option<String>,
    pub enabled: bool,
    pub created_at: String,
}

/// 创建通道的输入
#[derive(Debug, Clone, Deserialize)]
pub struct CreateChannelInput {
    pub name: String,
    pub provider: String,
    pub endpoint: String,
    pub weight: Option<u32>,
    pub enabled: Option<bool>,
    pub price_weight_override: Option<f64>,
    pub health_weight_override: Option<f64>,
    pub config_json: Option<String>,
    pub keys: Vec<CreateChannelKeyInput>,
}

/// 创建密钥的输入
#[derive(Debug, Clone, Deserialize)]
pub struct CreateChannelKeyInput {
    pub api_key: String,
    pub group_id: Option<String>,
    pub group_ratio: Option<f64>,
    pub label: Option<String>,
}

/// 更新通道的输入
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateChannelInput {
    pub name: Option<String>,
    pub provider: Option<String>,
    pub endpoint: Option<String>,
    pub weight: Option<u32>,
    pub enabled: Option<bool>,
    pub price_weight_override: Option<f64>,
    pub health_weight_override: Option<f64>,
    pub config_json: Option<String>,
}

/// 通道配置存储服务
#[derive(Clone)]
pub struct ChannelConfigStore {
    pub(crate) runtime_state: RuntimeState,
}

impl ChannelConfigStore {
    pub fn new(runtime_state: RuntimeState) -> Self {
        Self { runtime_state }
    }

    /// 校验创建通道输入
    pub fn validate_create_input(&self, input: &CreateChannelInput) -> Result<(), RelayError> {
        let config = ChannelConfig {
            name: input.name.clone(),
            provider: input.provider.clone(),
            endpoint: input.endpoint.clone(),
            keys: input
                .keys
                .iter()
                .map(|k| ChannelKeyConfig {
                    api_key: k.api_key.clone(),
                    group_id: k.group_id.clone(),
                    group_ratio: k.group_ratio.unwrap_or(1.0),
                    label: k.label.clone(),
                })
                .collect(),
            weight: input.weight.unwrap_or(1),
            enabled: input.enabled.unwrap_or(true),
        };
        validate_channel_config(&config).map_err(|errors| {
            RelayError::InvalidConfig(format!("missing fields: {}", errors.join(", ")))
        })
    }

    /// 生成新的通道 ID
    pub fn generate_id() -> String {
        Uuid::new_v4().to_string()
    }

    /// 将通道配置同步到 RuntimeState（供路由决策使用）
    pub async fn sync_channel_to_runtime(
        &self,
        channel: &StoredRelayChannel,
    ) -> Result<(), RelayError> {
        let key = format!("relay:channel:config:{}", channel.id);
        let value = serde_json::to_string(channel)
            .map_err(|e| RelayError::Internal(format!("serialize channel: {}", e)))?;
        self.runtime_state
            .kv_set(&key, value, Some(Duration::from_secs(1800)))
            .await
            .map_err(|e| RelayError::RuntimeState(e.to_string()))?;

        // Update enabled set
        if channel.enabled {
            self.runtime_state
                .set_add("relay:channel:enabled", &channel.id)
                .await
                .map_err(|e| RelayError::RuntimeState(e.to_string()))?;
        } else {
            self.runtime_state
                .set_remove("relay:channel:enabled", &channel.id)
                .await
                .map_err(|e| RelayError::RuntimeState(e.to_string()))?;
        }

        info!(channel_id = %channel.id, enabled = channel.enabled, "synced channel config to runtime state");
        Ok(())
    }

    /// 获取所有已启用通道 ID
    pub async fn get_enabled_channel_ids(&self) -> Result<Vec<String>, RelayError> {
        self.runtime_state
            .set_members("relay:channel:enabled")
            .await
            .map_err(|e| RelayError::RuntimeState(e.to_string()))
    }

    /// 从 RuntimeState 获取通道配置
    pub async fn get_channel_from_runtime(
        &self,
        channel_id: &str,
    ) -> Result<Option<StoredRelayChannel>, RelayError> {
        let key = format!("relay:channel:config:{}", channel_id);
        match self.runtime_state.kv_get(&key).await {
            Ok(Some(value)) => {
                let channel: StoredRelayChannel = serde_json::from_str(&value)
                    .map_err(|e| RelayError::Internal(format!("deserialize channel: {}", e)))?;
                Ok(Some(channel))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(RelayError::RuntimeState(e.to_string())),
        }
    }
}
