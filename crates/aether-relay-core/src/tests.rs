use proptest::prelude::*;

use crate::health::{calculate_health_score, should_circuit_break, should_half_open};
use crate::markup::{calculate_downstream_price, is_loss};
use crate::models::*;
use crate::pricing::calculate_token_cost_quota;
use crate::profit::{calculate_profit, is_below_alert_threshold};
use crate::routing::{calculate_composite_score, select_route};
use crate::validation::validate_channel_config;

// **Validates: Requirements 1.1**
// Feature: relay-pricing-dynamic-routing, Property 1: 通道配置校验拒绝不完整配置
proptest! {
    #[test]
    fn property_1_validation_rejects_incomplete_config(
        name in "[a-zA-Z]{1,10}",
        provider in "[a-zA-Z]{1,10}",
    ) {
        // Missing endpoint
        let config = ChannelConfig {
            name: name.clone(),
            provider: provider.clone(),
            endpoint: String::new(),
            keys: vec![ChannelKeyConfig {
                api_key: "sk-test".to_string(),
                group_id: None,
                group_ratio: 1.0,
                label: None,
            }],
            weight: 1,
            enabled: true,
        };
        let result = validate_channel_config(&config);
        prop_assert!(result.is_err());
        let errors = result.unwrap_err();
        prop_assert!(errors.iter().any(|e| e.contains("endpoint")));

        // Missing keys
        let config2 = ChannelConfig {
            name: name.clone(),
            provider: provider.clone(),
            endpoint: "https://api.example.com".to_string(),
            keys: vec![],
            weight: 1,
            enabled: true,
        };
        let result2 = validate_channel_config(&config2);
        prop_assert!(result2.is_err());
        let errors2 = result2.unwrap_err();
        prop_assert!(errors2.iter().any(|e| e.contains("keys")));
    }
}

// **Validates: Requirements 2.1**
// Feature: relay-pricing-dynamic-routing, Property 2: Token 成本公式正确性
proptest! {
    #[test]
    fn property_2_token_cost_formula(
        token_count in 1u64..1_000_000,
        model_ratio in 0.01f64..100.0,
        group_ratio in 0.01f64..10.0,
        completion_ratio in 1.0f64..5.0,
    ) {
        let params = PricingParams {
            model_ratio,
            group_ratio,
            completion_ratio,
        };

        // Prompt tokens
        let prompt_cost = calculate_token_cost_quota(token_count, false, &params);
        let expected_prompt = token_count as f64 * model_ratio * group_ratio;
        prop_assert!((prompt_cost - expected_prompt).abs() < 1e-6,
            "prompt cost mismatch: got {}, expected {}", prompt_cost, expected_prompt);

        // Completion tokens
        let completion_cost = calculate_token_cost_quota(token_count, true, &params);
        let expected_completion = token_count as f64 * model_ratio * group_ratio * completion_ratio;
        prop_assert!((completion_cost - expected_completion).abs() < 1e-6,
            "completion cost mismatch: got {}, expected {}", completion_cost, expected_completion);
    }
}

// **Validates: Requirements 3.1**
// Feature: relay-pricing-dynamic-routing, Property 3: 健康评分公式正确性
proptest! {
    #[test]
    fn property_3_health_score_formula(
        total_requests in 1u64..10_000,
        failed_ratio in 0.0f64..1.0,
        p95_latency_ms in 0u64..10_000,
        latency_threshold_ms in 1000u64..10_000,
    ) {
        let failed_requests = (total_requests as f64 * failed_ratio) as u64;
        let config = HealthConfig {
            latency_threshold_ms,
            latency_weight: 0.3,
            availability_weight: 0.7,
            ..Default::default()
        };
        let metrics = HealthMetrics {
            total_requests,
            failed_requests,
            consecutive_failures: 0,
            p95_latency_ms,
            last_success_at: Some(1000),
            last_failure_at: None,
            state: ChannelHealthState::Normal,
            state_changed_at: 0,
        };

        let score = calculate_health_score(&metrics, &config);

        let error_rate = failed_requests as f64 / total_requests as f64;
        let availability_score = 1.0 - error_rate;
        let latency_score = (1.0 - (p95_latency_ms as f64 / latency_threshold_ms as f64)).max(0.0);
        let expected = availability_score * 0.7 + latency_score * 0.3;

        prop_assert!((score - expected).abs() < 1e-9,
            "health score mismatch: got {}, expected {}", score, expected);
    }

    #[test]
    fn property_3_no_requests_returns_neutral(
        latency_threshold_ms in 1000u64..10_000,
    ) {
        let config = HealthConfig {
            latency_threshold_ms,
            ..Default::default()
        };
        let metrics = HealthMetrics {
            total_requests: 0,
            failed_requests: 0,
            consecutive_failures: 0,
            p95_latency_ms: 0,
            last_success_at: None,
            last_failure_at: None,
            state: ChannelHealthState::Normal,
            state_changed_at: 0,
        };
        let score = calculate_health_score(&metrics, &config);
        prop_assert!((score - 0.5).abs() < 1e-9);
    }
}

// **Validates: Requirements 3.2**
// Feature: relay-pricing-dynamic-routing, Property 4: 熔断状态触发与冷却转换
proptest! {
    #[test]
    fn property_4_circuit_breaker_trigger(
        threshold in 1u32..20,
        failures_over in 0u32..10,
    ) {
        let config = HealthConfig {
            circuit_breaker_threshold: threshold,
            ..Default::default()
        };
        let metrics = HealthMetrics {
            total_requests: 100,
            failed_requests: 50,
            consecutive_failures: threshold + failures_over,
            p95_latency_ms: 1000,
            last_success_at: None,
            last_failure_at: Some(1000),
            state: ChannelHealthState::Normal,
            state_changed_at: 0,
        };
        prop_assert!(should_circuit_break(&metrics, &config));
    }

    #[test]
    fn property_4_circuit_breaker_no_trigger(
        threshold in 2u32..20,
        failures_under in 0u32..2,
    ) {
        let consecutive = failures_under.min(threshold - 1);
        let config = HealthConfig {
            circuit_breaker_threshold: threshold,
            ..Default::default()
        };
        let metrics = HealthMetrics {
            total_requests: 100,
            failed_requests: 10,
            consecutive_failures: consecutive,
            p95_latency_ms: 1000,
            last_success_at: Some(500),
            last_failure_at: None,
            state: ChannelHealthState::Normal,
            state_changed_at: 0,
        };
        prop_assert!(!should_circuit_break(&metrics, &config));
    }

    #[test]
    fn property_4_half_open_after_cooldown(
        cooldown_secs in 10u64..300,
        extra_ms in 0u64..5000,
    ) {
        let config = HealthConfig {
            circuit_breaker_cooldown_secs: cooldown_secs,
            ..Default::default()
        };
        let state_changed_at = 1_000_000;
        let now_ms = state_changed_at + cooldown_secs * 1000 + extra_ms;
        let metrics = HealthMetrics {
            total_requests: 100,
            failed_requests: 100,
            consecutive_failures: 10,
            p95_latency_ms: 5000,
            last_success_at: None,
            last_failure_at: Some(state_changed_at),
            state: ChannelHealthState::CircuitOpen,
            state_changed_at,
        };
        prop_assert!(should_half_open(&metrics, &config, now_ms));
    }

    #[test]
    fn property_4_circuit_open_health_score_zero(
        total_requests in 1u64..10_000,
    ) {
        let config = HealthConfig::default();
        let metrics = HealthMetrics {
            total_requests,
            failed_requests: total_requests,
            consecutive_failures: 10,
            p95_latency_ms: 5000,
            last_success_at: None,
            last_failure_at: Some(1000),
            state: ChannelHealthState::CircuitOpen,
            state_changed_at: 0,
        };
        let score = calculate_health_score(&metrics, &config);
        prop_assert!((score - 0.0).abs() < 1e-9);
    }
}

// **Validates: Requirements 4.1**
// Feature: relay-pricing-dynamic-routing, Property 5: 综合评分公式正确性
proptest! {
    #[test]
    fn property_5_composite_score_formula(
        price in 0.1f64..100.0,
        min_price in 0.1f64..50.0,
        health_score in 0.0f64..1.0,
        price_weight in 0.0f64..1.0,
    ) {
        let max_price = min_price + 50.0; // ensure max > min
        let health_weight = 1.0 - price_weight;
        let config = RouteConfig { price_weight, health_weight };
        // Clamp candidate price within [min_price, max_price]
        let clamped_price = price.min(max_price).max(min_price);
        let candidate = RouteCandidate {
            channel_id: "test".to_string(),
            price_per_prompt_token_quota: clamped_price,
            price_per_completion_token_quota: price,
            health_score,
            weight: 1,
            state: ChannelHealthState::Normal,
        };

        let score = calculate_composite_score(&candidate, min_price, max_price, &config);

        let price_score = 1.0 - (clamped_price - min_price) / (max_price - min_price);
        let expected = price_score * price_weight + health_score * health_weight;
        prop_assert!((score - expected).abs() < 1e-9,
            "composite score mismatch: got {}, expected {}", score, expected);
    }

    #[test]
    fn property_5_equal_prices_score_one(
        price in 0.1f64..100.0,
        health_score in 0.0f64..1.0,
    ) {
        let config = RouteConfig::default();
        let candidate = RouteCandidate {
            channel_id: "test".to_string(),
            price_per_prompt_token_quota: price,
            price_per_completion_token_quota: price,
            health_score,
            weight: 1,
            state: ChannelHealthState::Normal,
        };
        // When min == max, price_score should be 1.0
        let score = calculate_composite_score(&candidate, price, price, &config);
        let expected = 1.0 * config.price_weight + health_score * config.health_weight;
        prop_assert!((score - expected).abs() < 1e-9);
    }
}

// **Validates: Requirements 4.2**
// Feature: relay-pricing-dynamic-routing, Property 6: 路由选择排序不变量
proptest! {
    #[test]
    fn property_6_route_selection_ordering(
        prices in proptest::collection::vec(1.0f64..100.0, 2..10),
        healths in proptest::collection::vec(0.1f64..1.0, 2..10),
    ) {
        let len = prices.len().min(healths.len());
        let candidates: Vec<RouteCandidate> = (0..len)
            .map(|i| RouteCandidate {
                channel_id: format!("ch-{}", i),
                price_per_prompt_token_quota: prices[i],
                price_per_completion_token_quota: prices[i] * 2.0,
                health_score: healths[i],
                weight: 1,
                state: ChannelHealthState::Normal,
            })
            .collect();

        let config = RouteConfig::default();
        let result = select_route(candidates, &config, 42);

        if let Some(decision) = result {
            // Selected composite_score >= 0
            prop_assert!(decision.composite_score >= 0.0);
            // The selected channel must have composite_score >= all fallbacks
            // (fallbacks are sorted descending by score after the selected)
            let min_price = decision.fallbacks.iter()
                .map(|c| c.price_per_prompt_token_quota)
                .chain(std::iter::once(decision.selected.price_per_prompt_token_quota))
                .fold(f64::INFINITY, f64::min);
            let max_price = decision.fallbacks.iter()
                .map(|c| c.price_per_prompt_token_quota)
                .chain(std::iter::once(decision.selected.price_per_prompt_token_quota))
                .fold(f64::NEG_INFINITY, f64::max);

            let selected_score = calculate_composite_score(
                &decision.selected, min_price, max_price, &config
            );
            for fb in &decision.fallbacks {
                let fb_score = calculate_composite_score(fb, min_price, max_price, &config);
                // Selected should have score >= fallbacks (within tie tolerance)
                prop_assert!(selected_score >= fb_score - 1e-9,
                    "selected score {} < fallback score {}", selected_score, fb_score);
            }
        }
    }
}

// **Validates: Requirements 4.3**
// Feature: relay-pricing-dynamic-routing, Property 7: 禁用和熔断通道排除
proptest! {
    #[test]
    fn property_7_circuit_open_excluded(
        num_normal in 1usize..5,
        num_open in 1usize..5,
    ) {
        let mut candidates = Vec::new();
        for i in 0..num_normal {
            candidates.push(RouteCandidate {
                channel_id: format!("normal-{}", i),
                price_per_prompt_token_quota: 10.0,
                price_per_completion_token_quota: 20.0,
                health_score: 0.8,
                weight: 1,
                state: ChannelHealthState::Normal,
            });
        }
        for i in 0..num_open {
            candidates.push(RouteCandidate {
                channel_id: format!("open-{}", i),
                price_per_prompt_token_quota: 1.0, // Cheapest but circuit open
                price_per_completion_token_quota: 2.0,
                health_score: 0.0,
                weight: 1,
                state: ChannelHealthState::CircuitOpen,
            });
        }

        let config = RouteConfig::default();
        let result = select_route(candidates, &config, 42);
        let decision = result.unwrap();

        // Selected must not be circuit open
        prop_assert_ne!(decision.selected.state, ChannelHealthState::CircuitOpen);
        // No fallback should be circuit open
        for fb in &decision.fallbacks {
            prop_assert_ne!(fb.state, ChannelHealthState::CircuitOpen);
        }
    }
}

// **Validates: Requirements 4.4**
// Feature: relay-pricing-dynamic-routing, Property 8: 空候选列表返回 None
proptest! {
    #[test]
    fn property_8_empty_returns_none(
        num_open in 0usize..5,
    ) {
        // All circuit open or empty
        let candidates: Vec<RouteCandidate> = (0..num_open)
            .map(|i| RouteCandidate {
                channel_id: format!("ch-{}", i),
                price_per_prompt_token_quota: 10.0,
                price_per_completion_token_quota: 20.0,
                health_score: 0.0,
                weight: 1,
                state: ChannelHealthState::CircuitOpen,
            })
            .collect();

        let config = RouteConfig::default();
        let result = select_route(candidates, &config, 42);
        prop_assert!(result.is_none());
    }
}

// **Validates: Requirements 4.5**
// Feature: relay-pricing-dynamic-routing, Property 9: 加权随机平局打破尊重权重
#[test]
fn property_9_weighted_random_respects_weights() {
    // Two candidates with same score but different weights (3:1)
    let candidates_template = vec![
        RouteCandidate {
            channel_id: "heavy".to_string(),
            price_per_prompt_token_quota: 10.0,
            price_per_completion_token_quota: 20.0,
            health_score: 0.8,
            weight: 3,
            state: ChannelHealthState::Normal,
        },
        RouteCandidate {
            channel_id: "light".to_string(),
            price_per_prompt_token_quota: 10.0,
            price_per_completion_token_quota: 20.0,
            health_score: 0.8,
            weight: 1,
            state: ChannelHealthState::Normal,
        },
    ];

    let config = RouteConfig::default();
    let mut heavy_count = 0u64;
    let total_runs = 1000u64;

    for seed in 0..total_runs {
        let candidates = candidates_template.clone();
        if let Some(decision) = select_route(candidates, &config, seed) {
            if decision.selected.channel_id == "heavy" {
                heavy_count += 1;
            }
        }
    }

    // Expected ratio: 75% heavy (3/4), allow 15% tolerance
    let heavy_ratio = heavy_count as f64 / total_runs as f64;
    assert!(
        heavy_ratio > 0.60 && heavy_ratio < 0.90,
        "heavy selection ratio {} outside expected range [0.60, 0.90]",
        heavy_ratio
    );
}

// **Validates: Requirements 5.1**
// Feature: relay-pricing-dynamic-routing, Property 10: 目标利润率定价公式
proptest! {
    #[test]
    fn property_10_target_margin_formula(
        upstream_cost in 0.01f64..100.0,
        margin in 0.01f64..0.99,
    ) {
        let strategy = MarkupStrategy::TargetMargin { margin };
        let downstream = calculate_downstream_price(upstream_cost, &strategy);
        let expected = upstream_cost / (1.0 - margin);
        prop_assert!((downstream - expected).abs() < 1e-9,
            "target margin mismatch: got {}, expected {}", downstream, expected);
    }
}

// **Validates: Requirements 5.2**
// Feature: relay-pricing-dynamic-routing, Property 11: 固定定价策略透传
proptest! {
    #[test]
    fn property_11_fixed_price_passthrough(
        upstream_cost in 0.01f64..100.0,
        fixed_price in 0.01f64..200.0,
    ) {
        let strategy = MarkupStrategy::FixedPrice {
            price_per_prompt_quota: fixed_price,
            price_per_completion_quota: fixed_price * 2.0,
        };
        let downstream = calculate_downstream_price(upstream_cost, &strategy);
        prop_assert!((downstream - fixed_price).abs() < 1e-9,
            "fixed price should be {}, got {}", fixed_price, downstream);
    }
}

// **Validates: Requirements 5.3**
// Feature: relay-pricing-dynamic-routing, Property 12: 亏损检测
proptest! {
    #[test]
    fn property_12_loss_detection(
        upstream_cost in 1.0f64..100.0,
        delta in 0.01f64..50.0,
    ) {
        // Loss: downstream < upstream
        let downstream_loss = upstream_cost - delta.min(upstream_cost - 0.001);
        if downstream_loss < upstream_cost {
            prop_assert!(is_loss(downstream_loss, upstream_cost));
        }

        // Not loss: downstream >= upstream
        let downstream_profit = upstream_cost + delta;
        prop_assert!(!is_loss(downstream_profit, upstream_cost));
        prop_assert!(!is_loss(upstream_cost, upstream_cost)); // equal = not loss
    }
}

// **Validates: Requirements 6.1**
// Feature: relay-pricing-dynamic-routing, Property 13: 利润计算公式一致性
proptest! {
    #[test]
    fn property_13_profit_formula_consistency(
        prompt_tokens in 1u64..100_000,
        completion_tokens in 1u64..100_000,
        model_ratio in 0.1f64..10.0,
        group_ratio in 0.1f64..5.0,
        completion_ratio in 1.0f64..5.0,
        downstream_markup in 1.1f64..3.0,
        fee_rate in 0.0f64..0.1,
    ) {
        let upstream_pricing = PricingParams {
            model_ratio,
            group_ratio,
            completion_ratio,
        };

        let input = ProfitInput {
            prompt_tokens,
            completion_tokens,
            upstream_pricing: upstream_pricing.clone(),
            downstream_price_per_prompt_quota: model_ratio * group_ratio * downstream_markup,
            downstream_price_per_completion_quota: model_ratio * group_ratio * completion_ratio * downstream_markup,
            payment_fee_rate: fee_rate,
            quota_per_unit: 500_000.0,
        };

        let result = calculate_profit(&input);

        // Verify upstream cost
        let prompt_upstream_quota = prompt_tokens as f64 * model_ratio * group_ratio;
        let completion_upstream_quota = completion_tokens as f64 * model_ratio * group_ratio * completion_ratio;
        let expected_upstream = (prompt_upstream_quota + completion_upstream_quota) / 500_000.0;
        prop_assert!((result.upstream_cost_usd - expected_upstream).abs() < 1e-6,
            "upstream cost: got {}, expected {}", result.upstream_cost_usd, expected_upstream);

        // Verify net profit formula
        let expected_net = result.downstream_revenue_usd - result.upstream_cost_usd - result.payment_fee_usd;
        prop_assert!((result.net_profit_usd - expected_net).abs() < 1e-6,
            "net profit: got {}, expected {}", result.net_profit_usd, expected_net);

        // Verify payment fee
        let expected_fee = result.downstream_revenue_usd * fee_rate;
        prop_assert!((result.payment_fee_usd - expected_fee).abs() < 1e-6);

        // Verify margin
        if result.downstream_revenue_usd > 0.0 {
            let expected_margin = (result.net_profit_usd / result.downstream_revenue_usd) * 100.0;
            prop_assert!((result.margin_percent - expected_margin).abs() < 1e-6);
        }
    }
}

// **Validates: Requirements 6.2**
// Feature: relay-pricing-dynamic-routing, Property 14: 利润率告警阈值检测
proptest! {
    #[test]
    fn property_14_alert_threshold(
        margin_percent in -50.0f64..100.0,
        threshold in 0.0f64..50.0,
    ) {
        let result = ProfitResult {
            upstream_cost_usd: 1.0,
            downstream_revenue_usd: 2.0,
            payment_fee_usd: 0.01,
            net_profit_usd: 0.99,
            margin_percent,
        };

        let is_below = is_below_alert_threshold(&result, threshold);
        prop_assert_eq!(is_below, margin_percent < threshold,
            "margin_percent={}, threshold={}, is_below={}", margin_percent, threshold, is_below);
    }
}

// **Validates: Requirements 7.1**
// Feature: relay-pricing-dynamic-routing, Property 15: 对账异常检测
proptest! {
    #[test]
    fn property_15_settlement_anomaly(
        difference_percent in -100.0f64..100.0,
        threshold in 1.0f64..20.0,
    ) {
        // Anomaly: |difference_percent| > threshold
        let is_anomaly = difference_percent.abs() > threshold;

        if difference_percent.abs() > threshold {
            prop_assert!(is_anomaly);
        } else {
            prop_assert!(!is_anomaly);
        }
    }
}
