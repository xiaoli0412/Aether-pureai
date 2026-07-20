//! 上游 New API 管理 API 客户端
//!
//! 支持从 New API 兼容平台获取分组信息和模型倍率配置。
//! 使用 trait 抽象以便测试时 mock。

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{debug, warn};

use super::error::RelayError;

/// 上游 API 返回的分组信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamGroup {
    pub id: String,
    pub name: String,
    pub ratio: f64,
}

/// 上游 API 返回的模型倍率配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamRatioConfig {
    /// 模型倍率映射: model_id -> ratio
    pub model_ratios: HashMap<String, f64>,
    /// 模型补全倍率映射: model_id -> completion_ratio
    pub completion_ratios: HashMap<String, f64>,
}

/// 上游用户余额信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamBalance {
    /// 剩余额度 (quota points)
    pub remaining_quota: f64,
    /// 已使用额度
    pub used_quota: f64,
    /// 总额度 (如果可获取)
    pub total_quota: Option<f64>,
    /// 是否欠费
    pub is_overdue: bool,
}

/// 上游渠道健康信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamChannelHealth {
    /// 渠道 ID
    pub channel_id: String,
    /// 是否启用
    pub enabled: bool,
    /// 平均响应时间 (ms)
    pub avg_response_time_ms: Option<u64>,
    /// 成功率 (0.0-1.0)
    pub success_rate: Option<f64>,
    /// 最后测试时间
    pub last_test_at: Option<String>,
}

/// 上游 API 客户端 trait
#[async_trait]
pub trait UpstreamApiClient: Send + Sync {
    /// 获取所有分组 (GET /api/group/)
    async fn get_groups(
        &self,
        endpoint: &str,
        api_key: &str,
    ) -> Result<Vec<UpstreamGroup>, RelayError>;

    /// 获取用户所属分组 (GET /api/user/groups)
    async fn get_user_groups(
        &self,
        endpoint: &str,
        api_key: &str,
    ) -> Result<Vec<String>, RelayError>;

    /// 获取系统倍率配置 (GET /api/ratio_config)
    async fn get_ratio_config(
        &self,
        endpoint: &str,
        api_key: &str,
    ) -> Result<UpstreamRatioConfig, RelayError>;

    /// 获取用户余额 (GET /api/user/self)
    async fn get_balance(
        &self,
        endpoint: &str,
        api_key: &str,
    ) -> Result<UpstreamBalance, RelayError>;

    /// 获取渠道健康状态 (GET /api/channel/health 或 GET /api/channel/)
    async fn get_channel_health(
        &self,
        endpoint: &str,
        api_key: &str,
    ) -> Result<Vec<UpstreamChannelHealth>, RelayError>;
}

/// 基于 reqwest 的上游 API 客户端实现
#[derive(Clone)]
pub struct HttpUpstreamClient {
    client: reqwest::Client,
}

impl HttpUpstreamClient {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|e| {
                warn!(error = %e, "failed to build reqwest client with timeout, falling back to default");
                reqwest::Client::new()
            });
        Self { client }
    }
}

#[async_trait]
impl UpstreamApiClient for HttpUpstreamClient {
    async fn get_groups(
        &self,
        endpoint: &str,
        api_key: &str,
    ) -> Result<Vec<UpstreamGroup>, RelayError> {
        let url = format!("{}/api/group/", endpoint.trim_end_matches('/'));
        debug!(url = %url, "fetching upstream groups");

        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .send()
            .await
            .map_err(|e| RelayError::PriceDiscoveryFailed {
                channel_id: endpoint.to_string(),
                source: Box::new(e),
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            warn!(status = %status, body = %body, "upstream groups API returned error");
            return Err(RelayError::PriceDiscoveryFailed {
                channel_id: endpoint.to_string(),
                source: format!("HTTP {}: {}", status, body).into(),
            });
        }

        // New API returns { "success": true, "data": [...] } or just an array
        let body = resp
            .text()
            .await
            .map_err(|e| RelayError::PriceDiscoveryFailed {
                channel_id: endpoint.to_string(),
                source: Box::new(e),
            })?;

        // Try parsing as wrapped response first
        if let Ok(wrapped) = serde_json::from_str::<WrappedResponse<Vec<UpstreamGroup>>>(&body) {
            return Ok(wrapped.data.unwrap_or_default());
        }
        // Try parsing as direct array
        serde_json::from_str::<Vec<UpstreamGroup>>(&body).map_err(|e| {
            RelayError::PriceDiscoveryFailed {
                channel_id: endpoint.to_string(),
                source: Box::new(e),
            }
        })
    }

    async fn get_user_groups(
        &self,
        endpoint: &str,
        api_key: &str,
    ) -> Result<Vec<String>, RelayError> {
        let url = format!("{}/api/user/groups", endpoint.trim_end_matches('/'));
        debug!(url = %url, "fetching upstream user groups");

        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .send()
            .await
            .map_err(|e| RelayError::PriceDiscoveryFailed {
                channel_id: endpoint.to_string(),
                source: Box::new(e),
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            return Err(RelayError::PriceDiscoveryFailed {
                channel_id: endpoint.to_string(),
                source: format!("HTTP {} from user/groups", status).into(),
            });
        }

        let body = resp
            .text()
            .await
            .map_err(|e| RelayError::PriceDiscoveryFailed {
                channel_id: endpoint.to_string(),
                source: Box::new(e),
            })?;

        if let Ok(wrapped) = serde_json::from_str::<WrappedResponse<Vec<String>>>(&body) {
            return Ok(wrapped.data.unwrap_or_default());
        }
        serde_json::from_str::<Vec<String>>(&body).map_err(|e| RelayError::PriceDiscoveryFailed {
            channel_id: endpoint.to_string(),
            source: Box::new(e),
        })
    }

    async fn get_ratio_config(
        &self,
        endpoint: &str,
        api_key: &str,
    ) -> Result<UpstreamRatioConfig, RelayError> {
        let url = format!("{}/api/ratio_config", endpoint.trim_end_matches('/'));
        debug!(url = %url, "fetching upstream ratio config");

        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .send()
            .await
            .map_err(|e| RelayError::PriceDiscoveryFailed {
                channel_id: endpoint.to_string(),
                source: Box::new(e),
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            return Err(RelayError::PriceDiscoveryFailed {
                channel_id: endpoint.to_string(),
                source: format!("HTTP {} from ratio_config", status).into(),
            });
        }

        let body = resp
            .text()
            .await
            .map_err(|e| RelayError::PriceDiscoveryFailed {
                channel_id: endpoint.to_string(),
                source: Box::new(e),
            })?;

        // New API ratio_config returns: { "model_ratio": {"gpt-4": 15, ...}, "completion_ratio": {"gpt-4": 2, ...} }
        // or wrapped in { "success": true, "data": {...} }
        if let Ok(wrapped) = serde_json::from_str::<WrappedResponse<RawRatioConfig>>(&body) {
            if let Some(data) = wrapped.data {
                return Ok(UpstreamRatioConfig {
                    model_ratios: data.model_ratio.unwrap_or_default(),
                    completion_ratios: data.completion_ratio.unwrap_or_default(),
                });
            }
        }
        if let Ok(raw) = serde_json::from_str::<RawRatioConfig>(&body) {
            return Ok(UpstreamRatioConfig {
                model_ratios: raw.model_ratio.unwrap_or_default(),
                completion_ratios: raw.completion_ratio.unwrap_or_default(),
            });
        }

        Err(RelayError::PriceDiscoveryFailed {
            channel_id: endpoint.to_string(),
            source: "failed to parse ratio_config response".into(),
        })
    }

    async fn get_balance(
        &self,
        endpoint: &str,
        api_key: &str,
    ) -> Result<UpstreamBalance, RelayError> {
        let url = format!("{}/api/user/self", endpoint.trim_end_matches('/'));
        debug!(url = %url, "fetching upstream user balance");

        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .send()
            .await
            .map_err(|e| RelayError::PriceDiscoveryFailed {
                channel_id: endpoint.to_string(),
                source: Box::new(e),
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            warn!(status = %status, body = %body, "upstream balance API returned error");
            return Err(RelayError::PriceDiscoveryFailed {
                channel_id: endpoint.to_string(),
                source: format!("HTTP {}: {}", status, body).into(),
            });
        }

        let body = resp
            .text()
            .await
            .map_err(|e| RelayError::PriceDiscoveryFailed {
                channel_id: endpoint.to_string(),
                source: Box::new(e),
            })?;

        // New API typically returns { "success": true, "data": { "quota": N, "used_quota": N, ... } }
        let raw: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| RelayError::PriceDiscoveryFailed {
                channel_id: endpoint.to_string(),
                source: Box::new(e),
            })?;

        // Extract data object (may be wrapped or direct)
        let data = if let Some(d) = raw.get("data") {
            d
        } else {
            &raw
        };

        // Handle different naming conventions for remaining quota
        let remaining_quota = data
            .get("remaining_quota")
            .or_else(|| data.get("quota"))
            .or_else(|| data.get("balance"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);

        let used_quota = data
            .get("used_quota")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);

        // Total quota: some APIs provide it explicitly, otherwise compute from remaining + used
        let total_quota = data
            .get("total_quota")
            .and_then(|v| v.as_f64())
            .or_else(|| {
                if remaining_quota > 0.0 || used_quota > 0.0 {
                    Some(remaining_quota + used_quota)
                } else {
                    None
                }
            });

        let is_overdue = remaining_quota < 0.0;

        Ok(UpstreamBalance {
            remaining_quota,
            used_quota,
            total_quota,
            is_overdue,
        })
    }

    async fn get_channel_health(
        &self,
        endpoint: &str,
        api_key: &str,
    ) -> Result<Vec<UpstreamChannelHealth>, RelayError> {
        // Try /api/channel/health first
        let health_url = format!("{}/api/channel/health", endpoint.trim_end_matches('/'));
        debug!(url = %health_url, "fetching upstream channel health");

        let resp = self
            .client
            .get(&health_url)
            .header("Authorization", format!("Bearer {}", api_key))
            .send()
            .await;

        let body = match resp {
            Ok(r) if r.status().is_success() => r.text().await.ok(),
            Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => {
                // Fallback to /api/channel/
                let fallback_url = format!("{}/api/channel/", endpoint.trim_end_matches('/'));
                debug!(url = %fallback_url, "health endpoint 404, trying channel list fallback");
                match self
                    .client
                    .get(&fallback_url)
                    .header("Authorization", format!("Bearer {}", api_key))
                    .send()
                    .await
                {
                    Ok(r2) if r2.status().is_success() => r2.text().await.ok(),
                    _ => return Ok(vec![]), // Graceful degradation
                }
            }
            _ => return Ok(vec![]), // Graceful degradation on network errors
        };

        let body = match body {
            Some(b) => b,
            None => return Ok(vec![]),
        };

        // Parse channel health list
        let raw: serde_json::Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => return Ok(vec![]),
        };

        // Extract array (may be wrapped in { "data": [...] } or direct)
        let channels_array = if let Some(arr) = raw.get("data").and_then(|d| d.as_array()) {
            arr.clone()
        } else if let Some(arr) = raw.as_array() {
            arr.clone()
        } else {
            return Ok(vec![]);
        };

        let mut results = Vec::new();
        for ch in &channels_array {
            let channel_id = ch
                .get("id")
                .or_else(|| ch.get("channel_id"))
                .and_then(|v| {
                    v.as_str()
                        .map(|s| s.to_string())
                        .or_else(|| v.as_u64().map(|n| n.to_string()))
                })
                .unwrap_or_default();

            if channel_id.is_empty() {
                continue;
            }

            let enabled = ch
                .get("enabled")
                .or_else(|| ch.get("status"))
                .and_then(|v| v.as_bool().or_else(|| v.as_u64().map(|n| n == 1)))
                .unwrap_or(true);

            let avg_response_time_ms = ch
                .get("response_time")
                .or_else(|| ch.get("avg_response_time"))
                .and_then(|v| v.as_u64());

            let success_rate = ch.get("success_rate").and_then(|v| v.as_f64());

            let last_test_at = ch
                .get("test_time")
                .or_else(|| ch.get("last_test_at"))
                .or_else(|| ch.get("tested_time"))
                .and_then(|v| {
                    v.as_str()
                        .map(|s| s.to_string())
                        .or_else(|| v.as_u64().map(|n| n.to_string()))
                });

            results.push(UpstreamChannelHealth {
                channel_id,
                enabled,
                avg_response_time_ms,
                success_rate,
                last_test_at,
            });
        }

        Ok(results)
    }
}

/// New API 标准响应包装
#[derive(Debug, Deserialize)]
struct WrappedResponse<T> {
    #[serde(default)]
    #[allow(dead_code)]
    success: bool,
    data: Option<T>,
    #[serde(default)]
    #[allow(dead_code)]
    message: Option<String>,
}

/// 原始倍率配置结构
#[derive(Debug, Deserialize)]
struct RawRatioConfig {
    model_ratio: Option<HashMap<String, f64>>,
    completion_ratio: Option<HashMap<String, f64>>,
}
