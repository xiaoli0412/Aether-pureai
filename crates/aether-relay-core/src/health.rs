//! 健康评分计算（纯函数，无 I/O）

use crate::models::{ChannelHealthState, HealthConfig, HealthMetrics};

/// 计算通道健康分数
///
/// 公式: (1 - error_rate) × availability_weight + latency_score × latency_weight
/// 其中 latency_score = max(0, 1 - (p95_latency_ms / latency_threshold_ms))
///
/// 特殊情况:
/// - 窗口内无请求: 返回 0.5
/// - CircuitOpen 状态: 返回 0.0
pub fn calculate_health_score(metrics: &HealthMetrics, config: &HealthConfig) -> f64 {
    // CircuitOpen always returns 0.0
    if metrics.state == ChannelHealthState::CircuitOpen {
        return 0.0;
    }

    // No requests in window: neutral score
    if metrics.total_requests == 0 {
        return 0.5;
    }

    let error_rate = metrics.failed_requests as f64 / metrics.total_requests as f64;
    let availability_score = 1.0 - error_rate;

    let latency_score =
        (1.0 - (metrics.p95_latency_ms as f64 / config.latency_threshold_ms as f64)).max(0.0);

    availability_score * config.availability_weight + latency_score * config.latency_weight
}

/// 判断是否应触发熔断
pub fn should_circuit_break(metrics: &HealthMetrics, config: &HealthConfig) -> bool {
    metrics.consecutive_failures >= config.circuit_breaker_threshold
}

/// 判断熔断冷却是否结束，应进入半开状态
pub fn should_half_open(metrics: &HealthMetrics, config: &HealthConfig, now_ms: u64) -> bool {
    metrics.state == ChannelHealthState::CircuitOpen
        && now_ms.saturating_sub(metrics.state_changed_at)
            >= config.circuit_breaker_cooldown_secs * 1000
}
