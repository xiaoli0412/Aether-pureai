//! Relay Prometheus 指标
//!
//! 定义并暴露中转引擎的关键运行指标。
//! 这些指标通过现有的 /metrics 端点暴露。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use aether_runtime::{MetricKind, MetricSample};

/// Relay 引擎 Prometheus 指标收集器
#[derive(Debug, Clone, Default)]
pub struct RelayMetrics {
    /// 上游总成本 (USD × 1_000_000，整数避免浮点精度问题)
    pub upstream_cost_micros: Arc<AtomicU64>,
    /// 下游总收入 (USD × 1_000_000)
    pub downstream_revenue_micros: Arc<AtomicU64>,
    /// 总利润 (USD × 1_000_000)
    pub profit_micros: Arc<AtomicU64>,
    /// 路由决策总次数
    pub route_decisions_total: Arc<AtomicU64>,
    /// 价格同步成功次数
    pub price_sync_success_total: Arc<AtomicU64>,
    /// 价格同步失败次数
    pub price_sync_errors_total: Arc<AtomicU64>,
    /// 通道错误总计
    pub channel_errors_total: Arc<AtomicU64>,
    /// 最后一次价格同步成功时间戳（unix secs）
    pub price_sync_last_success_ts: Arc<AtomicU64>,
}

impl RelayMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次利润
    ///
    /// 注意：仅累加非负值。负利润（亏损）不会加入累加器，
    /// 因为 AtomicU64 无法表示负数。亏损情况通过 profit_alert 机制追踪。
    pub fn record_profit(
        &self,
        upstream_cost_usd: f64,
        downstream_revenue_usd: f64,
        net_profit_usd: f64,
    ) {
        if upstream_cost_usd > 0.0 {
            self.upstream_cost_micros
                .fetch_add((upstream_cost_usd * 1_000_000.0) as u64, Ordering::Relaxed);
        }
        if downstream_revenue_usd > 0.0 {
            self.downstream_revenue_micros.fetch_add(
                (downstream_revenue_usd * 1_000_000.0) as u64,
                Ordering::Relaxed,
            );
        }
        if net_profit_usd > 0.0 {
            self.profit_micros
                .fetch_add((net_profit_usd * 1_000_000.0) as u64, Ordering::Relaxed);
        }
    }

    /// 记录一次路由决策
    pub fn record_route_decision(&self) {
        self.route_decisions_total.fetch_add(1, Ordering::Relaxed);
    }

    /// 记录价格同步成功
    pub fn record_price_sync_success(&self) {
        self.price_sync_success_total
            .fetch_add(1, Ordering::Relaxed);
        self.price_sync_last_success_ts
            .store(chrono::Utc::now().timestamp() as u64, Ordering::Relaxed);
    }

    /// 记录价格同步失败
    pub fn record_price_sync_error(&self) {
        self.price_sync_errors_total.fetch_add(1, Ordering::Relaxed);
    }

    /// 记录通道错误
    pub fn record_channel_error(&self) {
        self.channel_errors_total.fetch_add(1, Ordering::Relaxed);
    }

    /// 导出为 Prometheus MetricSample 列表
    ///
    /// 注意：金额指标以微美元（micros, 1 USD = 1_000_000 micros）为单位导出，
    /// 使用 Gauge 类型以便 Prometheus 可以做除法转换为 USD。
    pub fn metric_samples(&self) -> Vec<MetricSample> {
        vec![
            MetricSample::new(
                "relay_upstream_cost_micros_total",
                "Total upstream cost in micro-USD (1 USD = 1000000 micros).",
                MetricKind::Counter,
                self.upstream_cost_micros.load(Ordering::Relaxed),
            ),
            MetricSample::new(
                "relay_downstream_revenue_micros_total",
                "Total downstream revenue in micro-USD.",
                MetricKind::Counter,
                self.downstream_revenue_micros.load(Ordering::Relaxed),
            ),
            MetricSample::new(
                "relay_profit_micros_total",
                "Total net profit in micro-USD.",
                MetricKind::Counter,
                self.profit_micros.load(Ordering::Relaxed),
            ),
            MetricSample::new(
                "relay_route_decisions_total",
                "Total number of relay routing decisions made.",
                MetricKind::Counter,
                self.route_decisions_total.load(Ordering::Relaxed),
            ),
            MetricSample::new(
                "relay_price_sync_success_total",
                "Total number of successful price sync operations.",
                MetricKind::Counter,
                self.price_sync_success_total.load(Ordering::Relaxed),
            ),
            MetricSample::new(
                "relay_price_sync_errors_total",
                "Total number of failed price sync operations.",
                MetricKind::Counter,
                self.price_sync_errors_total.load(Ordering::Relaxed),
            ),
            MetricSample::new(
                "relay_price_sync_last_success_timestamp",
                "Unix timestamp of last successful price sync.",
                MetricKind::Gauge,
                self.price_sync_last_success_ts.load(Ordering::Relaxed),
            ),
            MetricSample::new(
                "relay_channel_errors_total",
                "Total number of relay channel errors.",
                MetricKind::Counter,
                self.channel_errors_total.load(Ordering::Relaxed),
            ),
        ]
    }
}
