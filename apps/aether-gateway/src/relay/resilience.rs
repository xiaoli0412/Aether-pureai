//! 多节点协调与韧性
//!
//! 提供 Redis 断连降级、本地缓存回退等韧性机制。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;

use super::config::StoredRelayChannel;
use super::discovery::CachedPricingEntry;

/// 本地降级缓存
///
/// 当 Redis 连接断开时，使用最近一次成功加载的数据继续路由。
/// 恢复后自动从 RuntimeState 重新加载。
#[derive(Debug, Clone)]
pub struct LocalFallbackCache {
    /// 通道配置本地缓存
    channels: Arc<RwLock<HashMap<String, (StoredRelayChannel, Instant)>>>,
    /// 定价本地缓存 key=(channel_id:model_id)
    pricing: Arc<RwLock<HashMap<String, (CachedPricingEntry, Instant)>>>,
    /// 缓存最大存活时间
    max_age: Duration,
}

impl LocalFallbackCache {
    pub fn new(max_age: Duration) -> Self {
        Self {
            channels: Arc::new(RwLock::new(HashMap::new())),
            pricing: Arc::new(RwLock::new(HashMap::new())),
            max_age,
        }
    }

    /// 缓存通道配置
    pub fn cache_channel(&self, channel: &StoredRelayChannel) {
        self.channels
            .write()
            .insert(channel.id.clone(), (channel.clone(), Instant::now()));
    }

    /// 缓存定价条目
    pub fn cache_pricing(&self, entry: &CachedPricingEntry) {
        let key = format!("{}:{}", entry.channel_id, entry.model_id);
        self.pricing
            .write()
            .insert(key, (entry.clone(), Instant::now()));
    }

    /// 从本地缓存获取通道配置（仅在 Redis 不可用时使用）
    pub fn get_channel_fallback(&self, channel_id: &str) -> Option<StoredRelayChannel> {
        let cache = self.channels.read();
        cache.get(channel_id).and_then(|(ch, cached_at)| {
            if cached_at.elapsed() < self.max_age {
                Some(ch.clone())
            } else {
                None
            }
        })
    }

    /// 从本地缓存获取模型定价（仅在 Redis 不可用时使用）
    pub fn get_pricing_fallback(
        &self,
        channel_id: &str,
        model_id: &str,
    ) -> Option<CachedPricingEntry> {
        let key = format!("{}:{}", channel_id, model_id);
        let cache = self.pricing.read();
        cache.get(&key).and_then(|(entry, cached_at)| {
            if cached_at.elapsed() < self.max_age {
                Some(entry.clone())
            } else {
                None
            }
        })
    }

    /// 获取所有有效的本地缓存定价（用于 Redis 断连时的路由决策）
    pub fn get_all_pricing_fallback_for_model(&self, model_id: &str) -> Vec<CachedPricingEntry> {
        let cache = self.pricing.read();
        cache
            .values()
            .filter(|(entry, cached_at)| {
                entry.model_id == model_id && cached_at.elapsed() < self.max_age
            })
            .map(|(entry, _)| entry.clone())
            .collect()
    }

    /// 清除过期缓存条目
    pub fn evict_expired(&self) {
        let now = Instant::now();
        self.channels
            .write()
            .retain(|_, (_, cached_at)| now.duration_since(*cached_at) < self.max_age);
        self.pricing
            .write()
            .retain(|_, (_, cached_at)| now.duration_since(*cached_at) < self.max_age);
    }

    /// 获取缓存统计
    pub fn stats(&self) -> (usize, usize) {
        (self.channels.read().len(), self.pricing.read().len())
    }
}
