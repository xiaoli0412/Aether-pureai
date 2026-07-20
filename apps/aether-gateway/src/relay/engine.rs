use std::sync::{Arc, Mutex};

use aether_data::repository::relay_profit::RelayProfitLedgerStore;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::balance_monitor::BalanceMonitor;
use super::collaboration::{RelayVerifier, RoutingMode};
use super::config::ChannelConfigStore;
use super::discovery::PriceDiscoveryService;
use super::event_consumer::SyncConsumer;
use super::event_outbox::EventOutbox;
use super::health::HealthStore;
use super::profit::{
    spawn_profit_writer_task, ProfitLedgerWork, ProfitLedgerWriter,
};
use super::reconcile::ReconciliationService;
use super::routing::RouteSelectionService;
use crate::data::GatewayDataState;

/// Relay 引擎配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayEngineConfig {
    /// 是否启用 relay 引擎
    pub enabled: bool,
    /// 价格同步间隔（秒）
    pub price_sync_interval_secs: u64,
    /// 健康评分滚动窗口（秒）
    pub health_rolling_window_secs: u64,
    /// 价格权重（路由决策）
    pub price_weight: f64,
    /// 健康权重（路由决策）
    pub health_weight: f64,
    /// 熔断连续失败阈值
    pub circuit_breaker_threshold: u32,
    /// 熔断冷却时间（秒）
    pub circuit_breaker_cooldown_secs: u64,
    /// 延迟阈值（毫秒）
    pub latency_threshold_ms: u64,
    /// 默认利润率（新模型自动定价）
    pub default_target_margin: f64,
    /// 利润率告警阈值（百分比）
    pub profit_alert_threshold_percent: f64,
    /// 价格变化告警阈值（百分比）
    pub price_change_threshold_percent: f64,
    /// 对账间隔（秒）
    pub reconciliation_interval_secs: u64,
    /// 对账异常阈值（百分比）
    pub settlement_anomaly_threshold_percent: f64,
    /// 支付手续费率（默认 0.006 = 0.6%）
    pub default_payment_fee_rate: f64,
    /// 分组缓存刷新间隔（秒）
    pub group_cache_refresh_secs: u64,
    /// 余额检查间隔（秒），默认 60
    pub balance_check_interval_secs: Option<u64>,
    /// 余额低阈值百分比，默认 10
    pub balance_low_threshold_percent: Option<f64>,
}

impl Default for RelayEngineConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            price_sync_interval_secs: 300,
            health_rolling_window_secs: 300,
            price_weight: 0.6,
            health_weight: 0.4,
            circuit_breaker_threshold: 5,
            circuit_breaker_cooldown_secs: 60,
            latency_threshold_ms: 5000,
            default_target_margin: 0.3,
            profit_alert_threshold_percent: 10.0,
            price_change_threshold_percent: 10.0,
            reconciliation_interval_secs: 3600,
            settlement_anomaly_threshold_percent: 5.0,
            default_payment_fee_rate: 0.006,
            group_cache_refresh_secs: 1800,
            balance_check_interval_secs: None,
            balance_low_threshold_percent: None,
        }
    }
}

const RELAY_ENABLED_ENV: &str = "AETHER_RELAY_ENABLED";
const RELAY_PRICE_SYNC_INTERVAL_ENV: &str = "AETHER_RELAY_PRICE_SYNC_INTERVAL_SECS";
const RELAY_HEALTH_WINDOW_ENV: &str = "AETHER_RELAY_HEALTH_WINDOW_SECS";
const RELAY_PRICE_WEIGHT_ENV: &str = "AETHER_RELAY_PRICE_WEIGHT";
const RELAY_HEALTH_WEIGHT_ENV: &str = "AETHER_RELAY_HEALTH_WEIGHT";
const RELAY_CB_THRESHOLD_ENV: &str = "AETHER_RELAY_CIRCUIT_BREAKER_THRESHOLD";
const RELAY_CB_COOLDOWN_ENV: &str = "AETHER_RELAY_CIRCUIT_BREAKER_COOLDOWN_SECS";
const RELAY_LATENCY_THRESHOLD_ENV: &str = "AETHER_RELAY_LATENCY_THRESHOLD_MS";
const RELAY_DEFAULT_MARGIN_ENV: &str = "AETHER_RELAY_DEFAULT_TARGET_MARGIN";
const RELAY_PROFIT_ALERT_ENV: &str = "AETHER_RELAY_PROFIT_ALERT_THRESHOLD_PERCENT";
const RELAY_PRICE_CHANGE_ENV: &str = "AETHER_RELAY_PRICE_CHANGE_THRESHOLD_PERCENT";
const RELAY_RECONCILIATION_INTERVAL_ENV: &str = "AETHER_RELAY_RECONCILIATION_INTERVAL_SECS";
const RELAY_SETTLEMENT_ANOMALY_ENV: &str = "AETHER_RELAY_SETTLEMENT_ANOMALY_THRESHOLD_PERCENT";
const RELAY_PAYMENT_FEE_ENV: &str = "AETHER_RELAY_DEFAULT_PAYMENT_FEE_RATE";
const RELAY_GROUP_CACHE_REFRESH_ENV: &str = "AETHER_RELAY_GROUP_CACHE_REFRESH_SECS";

impl RelayEngineConfig {
    /// 从环境变量构建配置，缺失项使用默认值
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(v) = std::env::var(RELAY_ENABLED_ENV) {
            config.enabled = v == "1" || v.eq_ignore_ascii_case("true");
        }
        if let Ok(v) = std::env::var(RELAY_PRICE_SYNC_INTERVAL_ENV) {
            if let Ok(n) = v.parse() {
                config.price_sync_interval_secs = n;
            }
        }
        if let Ok(v) = std::env::var(RELAY_HEALTH_WINDOW_ENV) {
            if let Ok(n) = v.parse() {
                config.health_rolling_window_secs = n;
            }
        }
        if let Ok(v) = std::env::var(RELAY_PRICE_WEIGHT_ENV) {
            if let Ok(n) = v.parse() {
                config.price_weight = n;
            }
        }
        if let Ok(v) = std::env::var(RELAY_HEALTH_WEIGHT_ENV) {
            if let Ok(n) = v.parse() {
                config.health_weight = n;
            }
        }
        if let Ok(v) = std::env::var(RELAY_CB_THRESHOLD_ENV) {
            if let Ok(n) = v.parse() {
                config.circuit_breaker_threshold = n;
            }
        }
        if let Ok(v) = std::env::var(RELAY_CB_COOLDOWN_ENV) {
            if let Ok(n) = v.parse() {
                config.circuit_breaker_cooldown_secs = n;
            }
        }
        if let Ok(v) = std::env::var(RELAY_LATENCY_THRESHOLD_ENV) {
            if let Ok(n) = v.parse() {
                config.latency_threshold_ms = n;
            }
        }
        if let Ok(v) = std::env::var(RELAY_DEFAULT_MARGIN_ENV) {
            if let Ok(n) = v.parse() {
                config.default_target_margin = n;
            }
        }
        if let Ok(v) = std::env::var(RELAY_PROFIT_ALERT_ENV) {
            if let Ok(n) = v.parse() {
                config.profit_alert_threshold_percent = n;
            }
        }
        if let Ok(v) = std::env::var(RELAY_PRICE_CHANGE_ENV) {
            if let Ok(n) = v.parse() {
                config.price_change_threshold_percent = n;
            }
        }
        if let Ok(v) = std::env::var(RELAY_RECONCILIATION_INTERVAL_ENV) {
            if let Ok(n) = v.parse() {
                config.reconciliation_interval_secs = n;
            }
        }
        if let Ok(v) = std::env::var(RELAY_SETTLEMENT_ANOMALY_ENV) {
            if let Ok(n) = v.parse() {
                config.settlement_anomaly_threshold_percent = n;
            }
        }
        if let Ok(v) = std::env::var(RELAY_PAYMENT_FEE_ENV) {
            if let Ok(n) = v.parse() {
                config.default_payment_fee_rate = n;
            }
        }
        if let Ok(v) = std::env::var(RELAY_GROUP_CACHE_REFRESH_ENV) {
            if let Ok(n) = v.parse() {
                config.group_cache_refresh_secs = n;
            }
        }

        if let Ok(v) = std::env::var("AETHER_RELAY_BALANCE_CHECK_INTERVAL_SECS") {
            if let Ok(n) = v.parse() {
                config.balance_check_interval_secs = Some(n);
            }
        }
        if let Ok(v) = std::env::var("AETHER_RELAY_BALANCE_LOW_THRESHOLD_PERCENT") {
            if let Ok(n) = v.parse() {
                config.balance_low_threshold_percent = Some(n);
            }
        }

        config
    }
}

/// Relay 引擎核心结构，持有所有子服务
#[derive(Clone)]
pub struct RelayEngine {
    pub(crate) config: Arc<RelayEngineConfig>,
    pub(crate) instance_id: String,
    pub(crate) config_store: Arc<ChannelConfigStore>,
    pub(crate) price_discovery: Arc<PriceDiscoveryService>,
    pub(crate) health_store: Arc<HealthStore>,
    pub(crate) route_service: Arc<RouteSelectionService>,
    pub(crate) profit_writer: Arc<ProfitLedgerWriter>,
    pub(crate) profit_receiver: Arc<Mutex<Option<tokio::sync::mpsc::Receiver<ProfitLedgerWork>>>>,
    pub(crate) profit_store: Option<RelayProfitLedgerStore>,
    pub(crate) profit_data: Arc<GatewayDataState>,
    pub(crate) reconciler: Arc<ReconciliationService>,
    pub(crate) relay_verifier: Option<Arc<RelayVerifier>>,
    pub(crate) routing_mode: RoutingMode,
    pub(crate) balance_monitor: Arc<BalanceMonitor>,
    pub(crate) sync_consumer: Option<Arc<SyncConsumer>>,
    pub(crate) event_outbox: Option<Arc<EventOutbox>>,
    pub(crate) background_shutdown: watch::Sender<()>,
}

impl std::fmt::Debug for RelayEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayEngine")
            .field("enabled", &self.config.enabled)
            .finish_non_exhaustive()
    }
}

impl RelayEngine {
    /// 判断 relay 引擎是否已启用
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    pub(crate) fn relay_verifier(&self) -> Option<&RelayVerifier> {
        self.relay_verifier.as_deref()
    }

    pub(crate) fn routing_mode(&self) -> RoutingMode {
        self.routing_mode
    }

    pub(crate) fn background_shutdown_sender() -> watch::Sender<()> {
        let (sender, _) = watch::channel(());
        sender
    }

    pub(crate) fn spawn_background_tasks(
        &self,
    ) -> Vec<(&'static str, tokio::task::JoinHandle<()>)> {
        let mut tasks = vec![
            (
                "relay_price_discovery",
                Arc::clone(&self.price_discovery)
                    .spawn_sync_task(self.background_shutdown.subscribe()),
            ),
            (
                "relay_balance_monitor",
                Arc::clone(&self.balance_monitor)
                    .spawn_monitor_task(self.background_shutdown.subscribe()),
            ),
        ];
        if let Some(consumer) = &self.sync_consumer {
            tasks.push((
                "relay_newapi_sync_consumer",
                Arc::clone(consumer).spawn_task(self.background_shutdown.subscribe()),
            ));
        }
        if let Some(store) = &self.profit_store {
            let receiver = match self.profit_receiver.lock() {
                Ok(mut receiver) => receiver.take(),
                Err(_) => {
                    tracing::error!("relay profit worker receiver lock is poisoned");
                    None
                }
            };
            if let Some(receiver) = receiver {
                tasks.push((
                    "relay_profit_ledger",
                    tokio::spawn(spawn_profit_writer_task(
                        receiver,
                        store.clone(),
                        Arc::clone(&self.profit_data),
                        self.config.default_payment_fee_rate,
                        self.background_shutdown.subscribe(),
                    )),
                ));
            }
        }
        tasks
    }
}
