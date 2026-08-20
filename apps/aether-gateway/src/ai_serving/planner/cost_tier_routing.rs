//! Detachable cost-tier routing module (Plan C2/C3).
//!
//! Opt-in per provider via `config.cost_tier`: when the directive policy is
//! inactive (absent, `enabled=false`, or missing threshold) this module
//! returns the base-ranked candidate order unchanged, so removing the config
//! restores the stock routing behavior exactly.
//!
//! When active, the pass estimates the request context size, picks the target
//! billing tier (`below`/`above` the configured threshold), and re-ranks the
//! resolved candidates so tier-matching candidates come first ordered by
//! estimated profit (revenue is constant across candidates serving the same
//! global model, so profit comparison reduces to negated settlement cost).
//! Stickiness (session/cache affinity) is honored afterwards, but only when
//! the sacrificed profit stays within `max_profit_sacrifice_usd`.
//!
//! The pass runs after candidate resolution/ranking and after the resolved
//! page cache read, so cached pages keep the canonical base ordering and the
//! per-request tier decision never contaminates shared cache state.

use super::candidate_resolution::{EligibleLocalExecutionCandidate, LocalExecutionCandidateKind};
use super::PlannerAppState;
use crate::handlers::shared::provider_pool::{
    admin_provider_pool_cache_affinity_enabled, admin_provider_pool_config_from_config_value,
};
use crate::orchestration::{
    cost_tier_policy_from_provider_config, cost_tier_routing_policy_from_system_config,
    parse_cost_billing_class_value, CostTierBillingPreference, CostTierPolicy,
    CostTierStickiness, COST_BILLING_CLASS_CONFIG_KEY, COST_TIER_ROUTING_CONFIG_KEY,
};
use aether_billing::{BillingModelPricingSnapshot, BillingService, BillingUsageInput};
use aether_scheduler_core::RANKING_REASON_CACHED_AFFINITY;
use serde_json::Value;
use tracing::debug;

/// Tolerance applied when comparing the profit sacrifice against the
/// configured budget, guarding against float equality edge cases.
const COST_TIER_PROFIT_EPSILON_USD: f64 = 1e-9;

/// Per-request inputs for the cost-tier re-ranking pass.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CostTierRoutingRequest<'a> {
    /// Estimated upstream context size for this request (tokens). `None`
    /// disables the pass entirely (zero-impact default).
    pub(crate) estimated_context_tokens: Option<u64>,
    /// Pool sticky-session token extracted from the request body, when any.
    pub(crate) sticky_session_token: Option<&'a str>,
}

/// Whether a candidate's billing model matches the target tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CostTierCandidateClass {
    /// Billing model matches the target tier preference.
    Match,
    /// Billing model could not be determined (or free tier): keeps relative
    /// base order between the matching and non-matching groups.
    Neutral,
    /// Billing model is known and does not match the target tier.
    NonMatch,
}

/// Per-candidate facts used by the re-ranking decision.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CostTierCandidateFacts {
    pub(crate) class: CostTierCandidateClass,
    /// Estimated profit (USD) for serving the request through this candidate.
    /// `None` when the settlement cost could not be computed.
    pub(crate) profit_usd: Option<f64>,
}

/// Which stickiness claim (if any) a candidate carries in base rank order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CostTierStickyClaim {
    None,
    SessionAffinity,
    CacheAffinity,
}

/// Estimates the upstream context size of a request body in tokens, reusing
/// the billing estimator. Returns `None` when nothing estimable is present.
pub(crate) fn estimate_request_context_tokens(body_json: &Value) -> Option<u64> {
    let tokens = aether_usage_runtime::estimate_request_context_tokens(body_json);
    if tokens == 0 {
        None
    } else {
        Some(tokens)
    }
}

/// Selects the directive policy: the first candidate in base order whose
/// provider carries an active `cost_tier` configuration.
fn select_directive_policy(
    candidates: &[EligibleLocalExecutionCandidate],
) -> Option<CostTierPolicy> {
    candidates.iter().find_map(|candidate| {
        let policy =
            cost_tier_policy_from_provider_config(candidate.transport.provider.config.as_ref());
        policy.is_active().then_some(policy)
    })
}

/// Picks the target billing preference for the tier the estimated context
/// falls into. `None` when that tier does not participate in re-ranking.
fn target_preference(
    policy: &CostTierPolicy,
    estimated_tokens: u64,
) -> Option<CostTierBillingPreference> {
    let threshold = policy.context_threshold_tokens?;
    if estimated_tokens < threshold {
        policy.below_preference
    } else {
        policy.above_preference
    }
}

/// Classifies a candidate's billing model from its resolved pricing.
/// Free-tier candidates stay neutral so zero-priced providers never
/// masquerade as "most profitable".
fn classify_billing_model(
    resolution: &aether_billing::BillingPricingResolution,
) -> Option<CostTierBillingPreference> {
    let has_per_request = resolution
        .price_per_request
        .is_some_and(|price| price.is_finite() && price > 0.0);
    if has_per_request {
        return Some(CostTierBillingPreference::PerRequest);
    }
    if resolution.tiered_pricing.is_some() {
        return Some(CostTierBillingPreference::PerUse);
    }
    None
}

/// Explicit billing classification declared by the operator on the
/// standalone cost-routing page: a per-key override in the key's
/// `capabilities.cost_billing_class` wins over the provider-level
/// `config.cost_billing_class`.
fn explicit_billing_class_for_candidate(
    candidate: &EligibleLocalExecutionCandidate,
) -> Option<CostTierBillingPreference> {
    let key_class = candidate
        .candidate
        .key_capabilities
        .as_ref()
        .and_then(|capabilities| capabilities.get(COST_BILLING_CLASS_CONFIG_KEY))
        .and_then(|value| parse_cost_billing_class_value(Some(value)));
    if key_class.is_some() {
        return key_class;
    }
    candidate
        .transport
        .provider
        .config
        .as_ref()
        .and_then(|config| config.get(COST_BILLING_CLASS_CONFIG_KEY))
        .and_then(|value| parse_cost_billing_class_value(Some(value)))
}

/// Detects the stickiness claim of a candidate. Session affinity is recorded
/// by the scheduler ranking (`promoted_by = cached_affinity`); cache affinity
/// applies to pool-group candidates of providers whose pool scheduling preset
/// enables `cache_affinity` while the request carries a sticky token. The
/// sticky token itself is applied later within the pool's key scheduling, so
/// honoring the claim here only affects provider-group ordering.
fn sticky_claim_for_candidate(
    candidate: &EligibleLocalExecutionCandidate,
    sticky_session_token: Option<&str>,
) -> CostTierStickyClaim {
    if candidate
        .ranking
        .as_ref()
        .and_then(|ranking| ranking.promoted_by)
        == Some(RANKING_REASON_CACHED_AFFINITY)
    {
        return CostTierStickyClaim::SessionAffinity;
    }
    if candidate.kind == LocalExecutionCandidateKind::PoolGroup
        && sticky_session_token.is_some()
        && admin_provider_pool_config_from_config_value(
            candidate.transport.provider.config.as_ref(),
        )
        .is_some_and(|pool_config| admin_provider_pool_cache_affinity_enabled(&pool_config))
    {
        return CostTierStickyClaim::CacheAffinity;
    }
    CostTierStickyClaim::None
}

/// Pure re-ranking core: stable partition into matching/neutral/non-matching
/// groups (matching sorted by profit, best first), then a bounded sticky
/// override. Base order is preserved inside each group and for ties.
fn rerank_candidates(
    candidates: Vec<EligibleLocalExecutionCandidate>,
    facts: &[CostTierCandidateFacts],
    stickiness: &CostTierStickiness,
    sticky_claims: &[CostTierStickyClaim],
) -> Vec<EligibleLocalExecutionCandidate> {
    debug_assert_eq!(candidates.len(), facts.len());
    debug_assert_eq!(candidates.len(), sticky_claims.len());

    let mut matching: Vec<usize> = Vec::new();
    let mut neutral: Vec<usize> = Vec::new();
    let mut nonmatch: Vec<usize> = Vec::new();
    for (index, fact) in facts.iter().enumerate() {
        match fact.class {
            CostTierCandidateClass::Match => matching.push(index),
            CostTierCandidateClass::Neutral => neutral.push(index),
            CostTierCandidateClass::NonMatch => nonmatch.push(index),
        }
    }

    // Profit first inside the matching group; unknown profits sink. Stable,
    // so ties keep base order.
    matching.sort_by(|&left, &right| {
        let left_profit = facts[left].profit_usd;
        let right_profit = facts[right].profit_usd;
        match (left_profit, right_profit) {
            (Some(left), Some(right)) => right
                .partial_cmp(&left)
                .unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });

    let mut ordered: Vec<usize> = matching;
    ordered.extend(neutral);
    ordered.extend(nonmatch);

    // Stickiness second: honor the first sticky candidate in base order only
    // when its claim is respected and the profit sacrifice stays in budget.
    if let Some((order_position, &sticky_index)) = ordered
        .iter()
        .enumerate()
        .find(|(_, &index)| sticky_claims[index] != CostTierStickyClaim::None)
    {
        let respected = match sticky_claims[sticky_index] {
            CostTierStickyClaim::SessionAffinity => stickiness.respect_session_affinity,
            CostTierStickyClaim::CacheAffinity => stickiness.respect_cache_affinity,
            CostTierStickyClaim::None => false,
        };
        if respected && order_position > 0 {
            let best_index = ordered[0];
            let sacrifice = match (facts[best_index].profit_usd, facts[sticky_index].profit_usd) {
                (Some(best), Some(sticky)) => best - sticky,
                _ => f64::INFINITY,
            };
            if sacrifice.is_finite()
                && sacrifice <= stickiness.max_profit_sacrifice_usd + COST_TIER_PROFIT_EPSILON_USD
            {
                ordered.remove(order_position);
                ordered.insert(0, sticky_index);
            }
        }
    }

    ordered
        .into_iter()
        .map(|index| candidates[index].clone())
        .collect()
}

/// Loads the billing facts for one candidate. Explicit operator
/// classification (key override > provider `cost_billing_class`) wins; when
/// absent the class is inferred from pricing data. Missing billing context
/// degrades profit to `None` (and inferred classes to `Neutral`) but never
/// fails the request.
async fn load_candidate_billing_facts(
    state: PlannerAppState<'_>,
    candidate: &EligibleLocalExecutionCandidate,
    client_api_format: &str,
    estimated_tokens: u64,
    target: CostTierBillingPreference,
) -> CostTierCandidateFacts {
    let explicit_class = explicit_billing_class_for_candidate(candidate);
    let data = &state.app().data;
    let context = match data
        .find_billing_model_context_by_model_id(
            &candidate.candidate.provider_id,
            Some(&candidate.candidate.key_id),
            &candidate.candidate.model_id,
        )
        .await
    {
        Ok(Some(context)) => Some(context),
        Ok(None) => data
            .find_billing_model_context(
                &candidate.candidate.provider_id,
                Some(&candidate.candidate.key_id),
                &candidate.candidate.global_model_name,
            )
            .await
            .ok()
            .flatten(),
        Err(_) => None,
    };

    let snapshot = context.map(BillingModelPricingSnapshot::from);
    let inferred_class = match snapshot.as_ref() {
        Some(snapshot) if snapshot.is_free_tier() => None,
        Some(snapshot) => classify_billing_model(&snapshot.resolve_pricing(None, None)),
        None => None,
    };
    let effective_class = explicit_class.or(inferred_class);
    let class = match effective_class {
        Some(model) if model == target => CostTierCandidateClass::Match,
        Some(_) => CostTierCandidateClass::NonMatch,
        None => CostTierCandidateClass::Neutral,
    };

    let profit_usd = snapshot.as_ref().and_then(|snapshot| {
        let mut usage_input = BillingUsageInput::new("chat");
        usage_input.input_tokens = i64::try_from(estimated_tokens).unwrap_or(i64::MAX);
        usage_input.api_format = Some(client_api_format.to_string());
        BillingService::new()
            .calculate(snapshot, &usage_input)
            .ok()
            .map(|computation| -computation.actual_total_cost)
            .filter(|value| value.is_finite())
    });

    CostTierCandidateFacts { class, profit_usd }
}

/// Applies the detachable cost-tier re-ranking pass over resolved candidates.
/// Returns the input order unchanged whenever any guard fails: no context
/// estimate, fewer than two candidates, no active policy, the applicable tier
/// has no preference, or no candidate matches the target tier.
///
/// Policy source order: the global `cost_tier_routing` system config
/// (standalone cost-routing page) first; when it is inactive the legacy
/// per-provider `config.cost_tier` directive keeps working.
pub(crate) async fn apply_cost_tier_reranking(
    state: PlannerAppState<'_>,
    candidates: Vec<EligibleLocalExecutionCandidate>,
    request: &CostTierRoutingRequest<'_>,
    client_api_format: &str,
) -> Vec<EligibleLocalExecutionCandidate> {
    let Some(estimated_tokens) = request
        .estimated_context_tokens
        .filter(|tokens| *tokens > 0)
    else {
        return candidates;
    };
    if candidates.len() < 2 {
        return candidates;
    }
    let global_policy = match state
        .app()
        .read_system_config_json_value(COST_TIER_ROUTING_CONFIG_KEY)
        .await
    {
        Ok(value) => cost_tier_routing_policy_from_system_config(value.as_ref()),
        Err(_) => CostTierPolicy::default(),
    };
    let Some(policy) = global_policy
        .is_active()
        .then_some(global_policy)
        .or_else(|| select_directive_policy(&candidates))
    else {
        return candidates;
    };
    let Some(target) = target_preference(&policy, estimated_tokens) else {
        return candidates;
    };

    let mut facts = Vec::with_capacity(candidates.len());
    for candidate in &candidates {
        facts.push(
            load_candidate_billing_facts(
                state,
                candidate,
                client_api_format,
                estimated_tokens,
                target,
            )
            .await,
        );
    }
    if !facts
        .iter()
        .any(|fact| fact.class == CostTierCandidateClass::Match)
    {
        return candidates;
    }

    let sticky_claims: Vec<CostTierStickyClaim> = candidates
        .iter()
        .map(|candidate| sticky_claim_for_candidate(candidate, request.sticky_session_token))
        .collect();
    let base_order: Vec<String> = candidates
        .iter()
        .map(|candidate| candidate.candidate.key_id.clone())
        .collect();
    let reranked = rerank_candidates(candidates, &facts, &policy.stickiness, &sticky_claims);
    let reranked_order: Vec<String> = reranked
        .iter()
        .map(|candidate| candidate.candidate.key_id.clone())
        .collect();
    if base_order != reranked_order {
        debug!(
            event_name = "cost_tier_reranking_applied",
            log_type = "debug",
            estimated_context_tokens = estimated_tokens,
            threshold_tokens = policy.context_threshold_tokens.unwrap_or_default(),
            target = match target {
                CostTierBillingPreference::PerRequest => "per_request",
                CostTierBillingPreference::PerUse => "per_use",
            },
            base_order = ?base_order,
            reranked_order = ?reranked_order,
            "cost-tier routing re-ranked resolved candidates"
        );
    }
    reranked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai_serving::transport::GatewayProviderTransportSnapshot;
    use crate::orchestration::LocalExecutionCandidateMetadata;
    use aether_provider_transport::snapshot::{
        GatewayProviderTransportEndpoint, GatewayProviderTransportKey,
        GatewayProviderTransportProvider,
    };
    use aether_scheduler_core::{
        SchedulerMinimalCandidateSelectionCandidate, SchedulerPriorityMode, SchedulerRankingMode,
        SchedulerRankingOutcome,
    };
    use serde_json::json;
    use std::sync::Arc;

    fn cost_tier_config(threshold: u64, below: &str, above: &str) -> Value {
        json!({
            "cost_tier": {
                "enabled": true,
                "context_threshold_tokens": threshold,
                "tiers": {
                    "below": { "prefer": below },
                    "above": { "prefer": above },
                },
            }
        })
    }

    fn transport(
        provider_id: &str,
        config: Option<Value>,
    ) -> Arc<GatewayProviderTransportSnapshot> {
        Arc::new(GatewayProviderTransportSnapshot {
            provider: GatewayProviderTransportProvider {
                id: provider_id.to_string(),
                name: provider_id.to_string(),
                provider_type: "llm".to_string(),
                website: None,
                is_active: true,
                keep_priority_on_conversion: false,
                enable_format_conversion: true,
                concurrent_limit: None,
                max_retries: None,
                proxy: None,
                request_timeout_secs: None,
                stream_first_byte_timeout_secs: None,
                config,
            },
            endpoint: GatewayProviderTransportEndpoint {
                id: format!("{provider_id}-endpoint"),
                provider_id: provider_id.to_string(),
                api_format: "openai:chat".to_string(),
                api_family: Some("openai".to_string()),
                endpoint_kind: Some("chat".to_string()),
                is_active: true,
                base_url: "https://upstream.example/v1".to_string(),
                header_rules: None,
                body_rules: None,
                max_retries: None,
                custom_path: None,
                config: None,
                format_acceptance_config: None,
                proxy: None,
            },
            key: GatewayProviderTransportKey {
                id: format!("{provider_id}-key"),
                provider_id: provider_id.to_string(),
                name: format!("{provider_id}-key"),
                auth_type: "api_key".to_string(),
                is_active: true,
                api_formats: None,
                auth_type_by_format: None,
                allow_auth_channel_mismatch_formats: None,
                allowed_models: None,
                capabilities: None,
                rate_multipliers: None,
                global_priority_by_format: None,
                expires_at_unix_secs: None,
                proxy: None,
                fingerprint: None,
                upstream_metadata: None,
                decrypted_api_key: "sk-test".to_string(),
                decrypted_auth_config: None,
            },
        })
    }

    fn candidate(
        key_id: &str,
        config: Option<Value>,
        kind: LocalExecutionCandidateKind,
    ) -> EligibleLocalExecutionCandidate {
        let provider_id = format!("provider-{key_id}");
        EligibleLocalExecutionCandidate {
            kind,
            candidate: SchedulerMinimalCandidateSelectionCandidate {
                provider_id: provider_id.clone(),
                provider_name: provider_id.clone(),
                provider_type: "llm".to_string(),
                provider_priority: 0,
                endpoint_id: format!("{provider_id}-endpoint"),
                endpoint_api_format: "openai:chat".to_string(),
                key_id: key_id.to_string(),
                key_name: key_id.to_string(),
                key_auth_type: "api_key".to_string(),
                key_internal_priority: 0,
                key_global_priority_for_format: None,
                key_capabilities: None,
                model_id: format!("{provider_id}-model"),
                global_model_id: "global-model-1".to_string(),
                global_model_name: "gpt-test".to_string(),
                selected_provider_model_name: "gpt-test".to_string(),
                supports_streaming: true,
                mapping_matched_model: None,
            },
            transport: transport(&provider_id, config),
            provider_api_format: "openai:chat".to_string(),
            orchestration: LocalExecutionCandidateMetadata::default(),
            ranking: None,
        }
    }

    fn candidate_facts(
        classes: &[(CostTierCandidateClass, Option<f64>)],
    ) -> Vec<CostTierCandidateFacts> {
        classes
            .iter()
            .map(|(class, profit_usd)| CostTierCandidateFacts {
                class: *class,
                profit_usd: *profit_usd,
            })
            .collect()
    }

    fn no_claims(count: usize) -> Vec<CostTierStickyClaim> {
        vec![CostTierStickyClaim::None; count]
    }

    fn key_order(candidates: &[EligibleLocalExecutionCandidate]) -> Vec<String> {
        candidates
            .iter()
            .map(|candidate| candidate.candidate.key_id.clone())
            .collect()
    }

    fn default_stickiness() -> CostTierStickiness {
        CostTierStickiness::default()
    }

    #[test]
    fn estimate_maps_zero_to_none() {
        assert_eq!(estimate_request_context_tokens(&json!({})), None);
        assert!(estimate_request_context_tokens(&json!({
            "messages": [{ "role": "user", "content": "hello" }]
        }))
        .is_some());
    }

    #[test]
    fn target_preference_selects_tier_by_threshold() {
        let policy = cost_tier_policy_from_provider_config(Some(&cost_tier_config(
            100,
            "per_use",
            "per_request",
        )));
        assert_eq!(
            target_preference(&policy, 99),
            Some(CostTierBillingPreference::PerUse)
        );
        assert_eq!(
            target_preference(&policy, 100),
            Some(CostTierBillingPreference::PerRequest)
        );
        let no_below = cost_tier_policy_from_provider_config(Some(&json!({
            "cost_tier": {
                "enabled": true,
                "context_threshold_tokens": 100,
                "tiers": { "above": { "prefer": "per_request" } },
            }
        })));
        assert_eq!(target_preference(&no_below, 50), None);
    }

    #[test]
    fn select_directive_policy_picks_first_active() {
        let candidates = vec![
            candidate("key-inactive", None, LocalExecutionCandidateKind::SingleKey),
            candidate(
                "key-active",
                Some(cost_tier_config(100, "per_use", "per_request")),
                LocalExecutionCandidateKind::SingleKey,
            ),
        ];
        let policy = select_directive_policy(&candidates).expect("directive should exist");
        assert!(policy.is_active());
        assert!(select_directive_policy(&candidates[..1]).is_none());
    }

    #[test]
    fn rerank_orders_matching_by_profit_then_neutral_then_nonmatch() {
        let candidates = vec![
            candidate("key-nonmatch", None, LocalExecutionCandidateKind::SingleKey),
            candidate(
                "key-match-cheap",
                None,
                LocalExecutionCandidateKind::SingleKey,
            ),
            candidate("key-neutral", None, LocalExecutionCandidateKind::SingleKey),
            candidate(
                "key-match-rich",
                None,
                LocalExecutionCandidateKind::SingleKey,
            ),
        ];
        let facts = candidate_facts(&[
            (CostTierCandidateClass::NonMatch, Some(-0.30)),
            (CostTierCandidateClass::Match, Some(-0.20)),
            (CostTierCandidateClass::Neutral, None),
            (CostTierCandidateClass::Match, Some(-0.05)),
        ]);
        let reranked = rerank_candidates(candidates, &facts, &default_stickiness(), &no_claims(4));
        assert_eq!(
            key_order(&reranked),
            vec![
                "key-match-rich".to_string(),
                "key-match-cheap".to_string(),
                "key-neutral".to_string(),
                "key-nonmatch".to_string(),
            ]
        );
    }

    #[test]
    fn rerank_keeps_base_order_for_equal_profit_and_unknown_profits() {
        let candidates = vec![
            candidate("key-a", None, LocalExecutionCandidateKind::SingleKey),
            candidate("key-b", None, LocalExecutionCandidateKind::SingleKey),
            candidate("key-c", None, LocalExecutionCandidateKind::SingleKey),
        ];
        let facts = candidate_facts(&[
            (CostTierCandidateClass::Match, Some(-0.10)),
            (CostTierCandidateClass::Match, Some(-0.10)),
            (CostTierCandidateClass::Match, None),
        ]);
        let reranked = rerank_candidates(candidates, &facts, &default_stickiness(), &no_claims(3));
        assert_eq!(
            key_order(&reranked),
            vec![
                "key-a".to_string(),
                "key-b".to_string(),
                "key-c".to_string()
            ]
        );
    }

    #[test]
    fn rerank_honors_session_stickiness_within_budget() {
        let candidates = vec![
            candidate("key-sticky", None, LocalExecutionCandidateKind::SingleKey),
            candidate("key-best", None, LocalExecutionCandidateKind::SingleKey),
        ];
        let facts = candidate_facts(&[
            (CostTierCandidateClass::Match, Some(-0.20)),
            (CostTierCandidateClass::Match, Some(-0.05)),
        ]);
        let claims = vec![
            CostTierStickyClaim::SessionAffinity,
            CostTierStickyClaim::None,
        ];

        // Budget 0 (default): profit wins, sticky stays demoted.
        let reranked =
            rerank_candidates(candidates.clone(), &facts, &default_stickiness(), &claims);
        assert_eq!(
            key_order(&reranked),
            vec!["key-best".to_string(), "key-sticky".to_string()]
        );

        // Budget covering the 0.15 sacrifice: sticky promoted to front.
        let stickiness = CostTierStickiness {
            max_profit_sacrifice_usd: 0.2,
            ..CostTierStickiness::default()
        };
        let reranked = rerank_candidates(candidates, &facts, &stickiness, &claims);
        assert_eq!(
            key_order(&reranked),
            vec!["key-sticky".to_string(), "key-best".to_string()]
        );
    }

    #[test]
    fn rerank_ignores_stickiness_when_flag_disabled_or_profit_unknown() {
        let candidates = vec![
            candidate("key-sticky", None, LocalExecutionCandidateKind::SingleKey),
            candidate("key-best", None, LocalExecutionCandidateKind::SingleKey),
        ];
        let facts = candidate_facts(&[
            (CostTierCandidateClass::Match, Some(-0.20)),
            (CostTierCandidateClass::Match, Some(-0.05)),
        ]);
        let claims = vec![
            CostTierStickyClaim::SessionAffinity,
            CostTierStickyClaim::None,
        ];

        let stickiness = CostTierStickiness {
            respect_session_affinity: false,
            max_profit_sacrifice_usd: 1.0,
            ..CostTierStickiness::default()
        };
        let reranked = rerank_candidates(candidates.clone(), &facts, &stickiness, &claims);
        assert_eq!(
            key_order(&reranked),
            vec!["key-best".to_string(), "key-sticky".to_string()]
        );

        // Unknown sticky profit: sacrifice is unbounded, profit wins.
        let unknown_profit_facts = candidate_facts(&[
            (CostTierCandidateClass::Match, None),
            (CostTierCandidateClass::Match, Some(-0.05)),
        ]);
        let stickiness = CostTierStickiness {
            max_profit_sacrifice_usd: 1.0,
            ..CostTierStickiness::default()
        };
        let reranked = rerank_candidates(candidates, &unknown_profit_facts, &stickiness, &claims);
        assert_eq!(
            key_order(&reranked),
            vec!["key-best".to_string(), "key-sticky".to_string()]
        );
    }

    #[test]
    fn sticky_claim_detects_session_and_cache_affinity() {
        let mut session = candidate("key-session", None, LocalExecutionCandidateKind::SingleKey);
        session.ranking = Some(SchedulerRankingOutcome {
            original_index: 1,
            ranking_index: 0,
            priority_mode: SchedulerPriorityMode::Provider,
            ranking_mode: SchedulerRankingMode::CacheAffinity,
            priority_slot: 0,
            promoted_by: Some(RANKING_REASON_CACHED_AFFINITY),
            demoted_by: None,
        });
        assert_eq!(
            sticky_claim_for_candidate(&session, None),
            CostTierStickyClaim::SessionAffinity
        );

        let pool = candidate(
            "key-pool",
            Some(json!({
                "pool_advanced": {
                    "scheduling_presets": [{ "preset": "cache_affinity", "enabled": true }]
                }
            })),
            LocalExecutionCandidateKind::PoolGroup,
        );
        assert_eq!(
            sticky_claim_for_candidate(&pool, Some("sticky-token")),
            CostTierStickyClaim::CacheAffinity
        );
        assert_eq!(
            sticky_claim_for_candidate(&pool, None),
            CostTierStickyClaim::None
        );

        let single = candidate("key-single", None, LocalExecutionCandidateKind::SingleKey);
        assert_eq!(
            sticky_claim_for_candidate(&single, Some("sticky-token")),
            CostTierStickyClaim::None
        );
    }

    #[test]
    fn classify_billing_model_prefers_per_request_then_per_use() {
        use aether_billing::BillingPricingResolution;
        let per_request = BillingPricingResolution {
            requested_processing_tier: None,
            actual_processing_tier: None,
            billing_processing_tier: Some("standard".to_string()),
            tiered_pricing: Some(json!({"input": 1})),
            tiered_pricing_source: None,
            processing_tier_price_multiplier: None,
            price_per_request: Some(0.03),
            price_per_request_source: None,
        };
        assert_eq!(
            classify_billing_model(&per_request),
            Some(CostTierBillingPreference::PerRequest)
        );

        let per_use = BillingPricingResolution {
            price_per_request: None,
            ..per_request.clone()
        };
        assert_eq!(
            classify_billing_model(&per_use),
            Some(CostTierBillingPreference::PerUse)
        );

        let neither = BillingPricingResolution {
            tiered_pricing: None,
            price_per_request: None,
            ..per_request
        };
        assert_eq!(classify_billing_model(&neither), None);
    }
}
