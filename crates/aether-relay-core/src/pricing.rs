//! 定价计算（纯函数，无 I/O）
//!
//! quota_per_unit 必须始终从上游 pricing/event/snapshot 动态读取，
//! 严禁使用任何硬编码值。

use crate::models::PricingParams;

/// 计算上游 token 成本（Quota Point 单位）
///
/// Prompt: token_count × model_ratio × group_ratio
/// Completion: token_count × model_ratio × group_ratio × completion_ratio
pub fn calculate_token_cost_quota(
    token_count: u64,
    is_completion: bool,
    params: &PricingParams,
) -> f64 {
    let base = token_count as f64 * params.model_ratio * params.group_ratio;
    if is_completion {
        base * params.completion_ratio
    } else {
        base
    }
}

/// 将 Quota Point 成本转换为 USD
///
/// `quota_per_unit` 是每 1 USD 对应的 quota 数量，必须从上游动态获取。
/// 严禁传入硬编码值。
pub fn quota_to_usd(quota_points: f64, quota_per_unit: f64) -> f64 {
    try_quota_to_usd(quota_points, quota_per_unit).unwrap_or(0.0)
}

/// 将 Quota Point 转换为 USD；输入不足或无效时返回未知。
pub fn try_quota_to_usd(quota_points: f64, quota_per_unit: f64) -> Option<f64> {
    if !quota_points.is_finite()
        || quota_points < 0.0
        || !quota_per_unit.is_finite()
        || quota_per_unit <= 0.0
    {
        return None;
    }
    Some(quota_points / quota_per_unit)
}

/// 将 USD 转换为 Quota Point
///
/// `quota_per_unit` 是每 1 USD 对应的 quota 数量，必须从上游动态获取。
pub fn usd_to_quota(usd: f64, quota_per_unit: f64) -> f64 {
    usd * quota_per_unit
}
