//! 路由选择算法（纯函数，无 I/O）

use crate::models::{ChannelHealthState, RouteCandidate, RouteConfig, RouteDecision};

/// 计算单个候选的综合评分
///
/// composite_score = price_score × price_weight + health_score × health_weight
/// price_score = 1 - (candidate_price - min_price) / (max_price - min_price)
/// 当 max_price == min_price 时，price_score = 1.0
pub fn calculate_composite_score(
    candidate: &RouteCandidate,
    min_price: f64,
    max_price: f64,
    config: &RouteConfig,
) -> f64 {
    let price_score = if (max_price - min_price).abs() < f64::EPSILON {
        1.0
    } else {
        1.0 - (candidate.price_per_prompt_token_quota - min_price) / (max_price - min_price)
    };

    price_score * config.price_weight + candidate.health_score * config.health_weight
}

/// 从候选列表中选择最优路由
///
/// 1. 过滤掉 CircuitOpen 状态的通道
/// 2. 计算每个候选的 composite_score
/// 3. 按 composite_score 降序排序
/// 4. 相同分数时使用加权随机（基于 weight 和 seed）
/// 5. 返回选中通道 + 备选列表
pub fn select_route(
    candidates: Vec<RouteCandidate>,
    config: &RouteConfig,
    seed: u64,
) -> Option<RouteDecision> {
    // Filter out circuit-open channels
    let eligible: Vec<RouteCandidate> = candidates
        .into_iter()
        .filter(|c| c.state != ChannelHealthState::CircuitOpen)
        .collect();

    if eligible.is_empty() {
        return None;
    }

    // Calculate min/max price for normalization
    let min_price = eligible
        .iter()
        .map(|c| c.price_per_prompt_token_quota)
        .fold(f64::INFINITY, f64::min);
    let max_price = eligible
        .iter()
        .map(|c| c.price_per_prompt_token_quota)
        .fold(f64::NEG_INFINITY, f64::max);

    // Calculate composite scores
    let mut scored: Vec<(RouteCandidate, f64)> = eligible
        .into_iter()
        .map(|c| {
            let score = calculate_composite_score(&c, min_price, max_price, config);
            (c, score)
        })
        .collect();

    // Sort by composite score descending
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Handle ties using weighted random with seed
    resolve_ties(&mut scored, seed);

    let (selected, composite_score) = scored.remove(0);
    let fallbacks: Vec<RouteCandidate> = scored.into_iter().map(|(c, _)| c).collect();

    let decision_reason = format!(
        "selected channel {} with composite_score {:.4} (price_weight={}, health_weight={})",
        selected.channel_id, composite_score, config.price_weight, config.health_weight
    );

    Some(RouteDecision {
        selected,
        fallbacks,
        composite_score,
        decision_reason,
    })
}

/// 解决平局：对相同分数的候选使用加权随机
fn resolve_ties(scored: &mut [(RouteCandidate, f64)], seed: u64) {
    if scored.len() <= 1 {
        return;
    }

    let mut i = 0;
    while i < scored.len() {
        // Find the end of the tie group
        let mut j = i + 1;
        while j < scored.len() && (scored[j].1 - scored[i].1).abs() < 1e-9 {
            j += 1;
        }

        // If there's a tie group, shuffle based on weights
        if j - i > 1 {
            weighted_shuffle(&mut scored[i..j], seed.wrapping_add(i as u64));
        }

        i = j;
    }
}

/// 基于权重的确定性洗牌（使用简单 LCG PRNG）
fn weighted_shuffle(group: &mut [(RouteCandidate, f64)], seed: u64) {
    let total_weight: u64 = group.iter().map(|(c, _)| c.weight as u64).sum();
    if total_weight == 0 {
        return;
    }

    // Simple LCG for deterministic behavior in tests
    let mut rng_state = seed;
    for i in 0..group.len().saturating_sub(1) {
        rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let remaining_weight: u64 = group[i..].iter().map(|(c, _)| c.weight as u64).sum();
        if remaining_weight == 0 {
            break;
        }
        let target = rng_state % remaining_weight;
        let mut cumulative: u64 = 0;
        let mut swap_idx = i;
        for k in i..group.len() {
            cumulative += group[k].0.weight as u64;
            if cumulative > target {
                swap_idx = k;
                break;
            }
        }
        group.swap(i, swap_idx);
    }
}
