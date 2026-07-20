//! 加价策略（纯函数，无 I/O）

use crate::models::MarkupStrategy;

/// 根据加价策略计算下游价格（Quota Point 单位）
///
/// - TargetMargin: downstream = upstream_cost / (1 - margin)
/// - FixedPrice: 直接返回固定价格
pub fn calculate_downstream_price(
    upstream_cost_per_token_quota: f64,
    strategy: &MarkupStrategy,
) -> f64 {
    match strategy {
        MarkupStrategy::TargetMargin { margin } => {
            if *margin >= 1.0 {
                // Avoid division by zero or negative
                upstream_cost_per_token_quota * 100.0
            } else {
                upstream_cost_per_token_quota / (1.0 - margin)
            }
        }
        MarkupStrategy::FixedPrice {
            price_per_prompt_quota,
            ..
        } => *price_per_prompt_quota,
    }
}

/// 判断是否亏损（下游价格低于上游成本）
pub fn is_loss(downstream_price: f64, upstream_cost: f64) -> bool {
    downstream_price < upstream_cost
}
