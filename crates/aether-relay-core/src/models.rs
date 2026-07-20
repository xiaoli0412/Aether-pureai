use serde::{Deserialize, Serialize};

// ============ Health Scoring ============

/// 健康评分配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthConfig {
    /// 滚动窗口（秒），默认 300
    pub rolling_window_secs: u64,
    /// 延迟阈值（毫秒），默认 5000
    pub latency_threshold_ms: u64,
    /// 延迟权重，默认 0.3
    pub latency_weight: f64,
    /// 可用性权重，默认 0.7
    pub availability_weight: f64,
    /// 熔断连续失败阈值，默认 5
    pub circuit_breaker_threshold: u32,
    /// 熔断冷却时间（秒），默认 60
    pub circuit_breaker_cooldown_secs: u64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            rolling_window_secs: 300,
            latency_threshold_ms: 5000,
            latency_weight: 0.3,
            availability_weight: 0.7,
            circuit_breaker_threshold: 5,
            circuit_breaker_cooldown_secs: 60,
        }
    }
}

/// 通道健康状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelHealthState {
    /// 正常
    Normal,
    /// 熔断（不接受流量）
    CircuitOpen,
    /// 半开（允许探测请求）
    HalfOpen,
}

/// 滚动窗口内的健康指标快照
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthMetrics {
    /// 窗口内总请求数
    pub total_requests: u64,
    /// 窗口内失败请求数
    pub failed_requests: u64,
    /// 连续失败次数
    pub consecutive_failures: u32,
    /// P95 延迟（毫秒）
    pub p95_latency_ms: u64,
    /// 最后一次成功时间（unix timestamp ms）
    pub last_success_at: Option<u64>,
    /// 最后一次失败时间（unix timestamp ms）
    pub last_failure_at: Option<u64>,
    /// 当前健康状态
    pub state: ChannelHealthState,
    /// 状态变更时间（unix timestamp ms）
    pub state_changed_at: u64,
}

// ============ Pricing ============

/// New API 定价参数
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PricingParams {
    /// 模型倍率
    pub model_ratio: f64,
    /// 分组倍率
    pub group_ratio: f64,
    /// 补全倍率（相对 prompt 的倍数），默认通常为 2.0-3.0
    pub completion_ratio: f64,
}

// ============ Routing ============

/// 路由候选通道
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteCandidate {
    /// 通道 ID
    pub channel_id: String,
    /// Prompt token 单价（Quota Point）
    pub price_per_prompt_token_quota: f64,
    /// Completion token 单价（Quota Point）
    pub price_per_completion_token_quota: f64,
    /// 健康分数 [0.0, 1.0]
    pub health_score: f64,
    /// 权重（用于平局打破）
    pub weight: u32,
    /// 当前健康状态
    pub state: ChannelHealthState,
}

/// 路由配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteConfig {
    /// 价格权重，默认 0.6
    pub price_weight: f64,
    /// 健康权重，默认 0.4
    pub health_weight: f64,
}

impl Default for RouteConfig {
    fn default() -> Self {
        Self {
            price_weight: 0.6,
            health_weight: 0.4,
        }
    }
}

/// 路由决策结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteDecision {
    /// 选中的通道
    pub selected: RouteCandidate,
    /// 备选通道列表（按 composite_score 降序）
    pub fallbacks: Vec<RouteCandidate>,
    /// 选中通道的综合评分
    pub composite_score: f64,
    /// 决策原因描述
    pub decision_reason: String,
}

// ============ Profit ============

/// 单次请求的利润计算输入
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfitInput {
    /// Prompt token 数
    pub prompt_tokens: u64,
    /// Completion token 数
    pub completion_tokens: u64,
    /// 上游定价参数
    pub upstream_pricing: PricingParams,
    /// 下游 prompt 单价（Quota Point）
    pub downstream_price_per_prompt_quota: f64,
    /// 下游 completion 单价（Quota Point）
    pub downstream_price_per_completion_quota: f64,
    /// 支付平台手续费率（如 0.006 = 0.6%）
    pub payment_fee_rate: f64,
    /// 每 1 USD 对应的 quota 数量（从上游动态读取，严禁硬编码）
    pub quota_per_unit: f64,
}

/// 利润计算结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfitResult {
    /// 上游成本（USD）
    pub upstream_cost_usd: f64,
    /// 下游收入（USD）
    pub downstream_revenue_usd: f64,
    /// 支付手续费（USD）
    pub payment_fee_usd: f64,
    /// 净利润（USD）
    pub net_profit_usd: f64,
    /// 利润率（百分比）
    pub margin_percent: f64,
}

// ============ Markup ============

/// 加价策略
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MarkupStrategy {
    /// 目标利润率模式：downstream_price = upstream_cost / (1 - margin)
    TargetMargin {
        /// 目标利润率，如 0.3 = 30%
        margin: f64,
    },
    /// 固定定价模式
    FixedPrice {
        /// Prompt token 固定单价（Quota Point）
        price_per_prompt_quota: f64,
        /// Completion token 固定单价（Quota Point）
        price_per_completion_quota: f64,
    },
}

// ============ Channel Config ============

/// 上游通道配置（用于校验）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfig {
    /// 通道名称
    pub name: String,
    /// 供应商类型
    pub provider: String,
    /// 端点 URL
    pub endpoint: String,
    /// API 密钥列表
    pub keys: Vec<ChannelKeyConfig>,
    /// 权重
    pub weight: u32,
    /// 是否启用
    pub enabled: bool,
}

/// 通道 API 密钥配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelKeyConfig {
    /// API 密钥
    pub api_key: String,
    /// 上游分组 ID
    pub group_id: Option<String>,
    /// 分组倍率
    pub group_ratio: f64,
    /// 标签
    pub label: Option<String>,
}
