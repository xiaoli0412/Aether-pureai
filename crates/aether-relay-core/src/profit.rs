//! 利润核算（纯函数，无 I/O）

use crate::models::{ProfitInput, ProfitResult};
use crate::pricing::{calculate_token_cost_quota, quota_to_usd};

/// 计算单次请求的利润
///
/// quota_per_unit 从 ProfitInput 获取，严禁使用硬编码常量。
pub fn calculate_profit(input: &ProfitInput) -> ProfitResult {
    let qpu = input.quota_per_unit;

    // Upstream cost
    let prompt_cost_quota =
        calculate_token_cost_quota(input.prompt_tokens, false, &input.upstream_pricing);
    let completion_cost_quota =
        calculate_token_cost_quota(input.completion_tokens, true, &input.upstream_pricing);
    let upstream_cost_usd = quota_to_usd(prompt_cost_quota + completion_cost_quota, qpu);

    // Downstream revenue
    let downstream_prompt_quota =
        input.prompt_tokens as f64 * input.downstream_price_per_prompt_quota;
    let downstream_completion_quota =
        input.completion_tokens as f64 * input.downstream_price_per_completion_quota;
    let downstream_revenue_usd =
        quota_to_usd(downstream_prompt_quota + downstream_completion_quota, qpu);

    // Fees and profit
    let payment_fee_usd = downstream_revenue_usd * input.payment_fee_rate;
    let net_profit_usd = downstream_revenue_usd - upstream_cost_usd - payment_fee_usd;

    let margin_percent = if downstream_revenue_usd > 0.0 {
        (net_profit_usd / downstream_revenue_usd) * 100.0
    } else {
        0.0
    };

    ProfitResult {
        upstream_cost_usd,
        downstream_revenue_usd,
        payment_fee_usd,
        net_profit_usd,
        margin_percent,
    }
}

/// 判断利润率是否低于告警阈值
pub fn is_below_alert_threshold(result: &ProfitResult, threshold_percent: f64) -> bool {
    result.margin_percent < threshold_percent
}
