use aether_contracts::ExecutionPlan;
use base64::Engine as _;
use serde_json::{json, Map, Value};

use crate::AppState;

mod adaptive;
mod attempt;
mod classifier;
mod codex_quota_breaker;
mod effects;
mod health;
mod oauth_error;
mod policy;
mod recovery;
mod report_effects;

pub(crate) use self::adaptive::{
    project_local_adaptive_rate_limit, project_local_adaptive_success,
    LocalAdaptiveRateLimitProjection, LocalAdaptiveSuccessProjection,
};
pub(crate) use self::attempt::{
    attempt_identity_from_report_context, build_local_attempt_identities,
    insert_pool_key_lease_report_context_fields, local_attempt_slot_count,
    local_execution_candidate_metadata_from_report_context, ExecutionAttemptIdentity,
    LocalExecutionCandidateMetadata, ROUTING_POOL_POLICY_OVERRIDE_REPORT_FIELD,
    SCHEDULER_AFFINITY_EPOCH_REPORT_FIELD,
};
pub(crate) use self::classifier::{
    classify_anthropic_failure_disposition, classify_failure_disposition, classify_local_failover,
    classify_local_transport_error, failure_disposition_from_local_classification,
    local_failover_error_message, FailureDisposition, FailureRetryAction, FailureScope,
    FailureTokenAction, LocalFailoverClassification, LocalFailoverInput,
    LocalTransportFailoverClassification,
};
pub(crate) use self::codex_quota_breaker::{
    codex_account_id_from_headers, codex_quota_breaker_blocks_candidate,
    codex_quota_exhaustion_reset_at, install_codex_quota_exhaustion_breaker,
    log_codex_quota_breaker_check_failure, log_codex_quota_breaker_install_failure,
};
pub(crate) use self::effects::{
    apply_local_execution_effect, apply_local_stream_failure_effects,
    apply_local_stream_success_effects, release_local_pool_key_lease,
    release_pool_key_lease_from_report_context, spawn_local_oauth_success_effect,
    LocalAdaptiveRateLimitEffect, LocalAdaptiveSuccessEffect, LocalAttemptFailureEffect,
    LocalExecutionEffect, LocalExecutionEffectContext, LocalHealthFailureEffect,
    LocalHealthSuccessEffect, LocalOAuthInvalidationEffect, LocalOAuthSuccessEffect,
    LocalPoolErrorEffect, LocalStreamFailureEffect,
};
pub(crate) use self::health::{
    project_local_failure_health, project_local_key_circuit_closed,
    project_local_key_circuit_failure, project_local_success_health,
};
pub(crate) use self::oauth_error::{
    oauth_status_may_be_invalid, oauth_status_proves_access_token_invalid,
};
pub(crate) use self::policy::{
    append_local_failover_policy_to_value, codex_cyber_flag_passthrough_enabled,
    cyber_continue_failover_enabled, local_failover_policy_from_report_context,
    local_failover_policy_from_transport, resolve_local_failover_policy,
    responses_websocket_adapter, CostTierBillingPreference, CostTierPolicy, CostTierStickiness,
    LocalEmptyResponseExhaustion, LocalEmptyResponsePolicy, LocalFailoverPolicy,
    LocalFailoverRegexRule, ResponsesWebSocketAdapter, COST_TIER_CONFIG_KEY,
    CYBER_CONTINUE_FAILOVER_CONFIG_KEY, RESPONSES_WEBSOCKET_CONFIG_KEY, UPSTREAM_POLICY_CONFIG_KEY,
};
pub(crate) use self::recovery::{
    analyze_local_failover, analyze_local_transport_error, apply_provider_failure_disposition,
    recover_local_failover_decision, LocalFailoverAnalysis, LocalFailoverDecision,
    LocalTransportFailoverAnalysis,
};
#[cfg(test)]
pub(crate) use self::report_effects::clear_local_report_effect_caches_for_tests;
pub(crate) use self::report_effects::{
    apply_local_report_effect, store_local_gemini_file_mapping,
    sync_codex_websocket_quota_metadata, LocalReportEffect,
};

pub(crate) async fn resolve_local_failover_analysis_for_attempt(
    state: &AppState,
    plan: &ExecutionPlan,
    report_context: Option<&serde_json::Value>,
    status_code: u16,
    response_text: Option<&str>,
) -> LocalFailoverAnalysis {
    if attempt_identity_from_report_context(report_context).is_none() {
        return LocalFailoverAnalysis::use_default();
    }

    let policy = resolve_local_failover_policy(state, plan, report_context).await;
    let analysis =
        analyze_local_failover(&policy, LocalFailoverInput::new(status_code, response_text));
    apply_provider_failure_disposition(&plan.provider_api_format, status_code, analysis)
}

pub(crate) async fn resolve_local_failover_decision_for_attempt(
    state: &AppState,
    plan: &ExecutionPlan,
    report_context: Option<&serde_json::Value>,
    status_code: u16,
    response_text: Option<&str>,
) -> LocalFailoverDecision {
    resolve_local_failover_analysis_for_attempt(
        state,
        plan,
        report_context,
        status_code,
        response_text,
    )
    .await
    .decision
}

pub(crate) async fn resolve_local_transport_failover_analysis_for_attempt(
    state: &AppState,
    plan: &ExecutionPlan,
    report_context: Option<&serde_json::Value>,
) -> LocalTransportFailoverAnalysis {
    let policy = resolve_local_failover_policy(state, plan, report_context).await;
    analyze_local_transport_error(&policy)
}

pub(crate) fn build_local_error_flow_metadata(
    status_code: u16,
    response_text: Option<&str>,
    analysis: LocalFailoverAnalysis,
) -> Value {
    let safe_to_expose = matches!(
        analysis.classification,
        LocalFailoverClassification::StopStatusCode
            | LocalFailoverClassification::StopErrorPattern
            | LocalFailoverClassification::StopExecutionError
            | LocalFailoverClassification::StopCyberPolicy
            | LocalFailoverClassification::StopPassthrough
    );
    let propagation = match analysis.decision {
        LocalFailoverDecision::RetryNextCandidate => "suppressed",
        LocalFailoverDecision::StopLocalFailover if safe_to_expose => "converted",
        LocalFailoverDecision::StopLocalFailover => "suppressed",
        LocalFailoverDecision::UseDefault if status_code >= 400 => "passthrough",
        LocalFailoverDecision::UseDefault => "none",
    };
    json!({
        "stage": "candidate",
        "source": "upstream_response",
        "status_code": status_code,
        "classification": analysis.classification.as_str(),
        "decision": analysis.decision.as_str(),
        "retryable": matches!(analysis.decision, LocalFailoverDecision::RetryNextCandidate),
        "safe_to_expose": safe_to_expose,
        "propagation": propagation,
        "message": local_failover_error_message(response_text),
    })
}

pub(crate) fn with_error_flow_report_context(
    report_context: Option<&Value>,
    error_flow: Value,
) -> Option<Value> {
    let mut object = report_context?.as_object()?.clone();
    object.insert("error_flow".to_string(), error_flow);
    Some(Value::Object(object))
}

/// Attaches a compact, queryable `error_diagnostic` marker to the report
/// context so the usage pipeline can persist WHY a request failed (empty
/// upstream response, upstream 4xx/5xx) without storing full bodies again.
pub(crate) fn with_error_diagnostic_report_context(
    report_context: Option<&Value>,
    kind: &str,
    upstream_status: u16,
    analysis: Option<LocalFailoverAnalysis>,
    response_text: Option<&str>,
) -> Option<Value> {
    let mut object = report_context?.as_object()?.clone();
    let mut diagnostic = Map::new();
    diagnostic.insert("kind".to_string(), json!(kind));
    diagnostic.insert("upstream_status".to_string(), json!(upstream_status));
    if let Some(analysis) = analysis {
        diagnostic.insert(
            "classification".to_string(),
            json!(analysis.classification.as_str()),
        );
        diagnostic.insert("decision".to_string(), json!(analysis.decision.as_str()));
    }
    if let Some(message) =
        crate::orchestration::classifier::local_failover_error_message(response_text)
    {
        diagnostic.insert(
            "message".to_string(),
            json!(limit_error_diagnostic_message(&message)),
        );
    }
    object.insert("error_diagnostic".to_string(), Value::Object(diagnostic));
    Some(Value::Object(object))
}

/// Classifies the diagnostic kind for a failed upstream response observed in
/// the candidate failover path.
pub(crate) fn error_diagnostic_kind(
    status_code: u16,
    response_text: Option<&str>,
) -> Option<&'static str> {
    if response_text.is_some_and(|text| {
        text.contains("did not contain visible model output")
            || text.contains("without visible model output")
    }) {
        return Some("empty_response");
    }
    if (400..500).contains(&status_code) {
        return Some("upstream_4xx");
    }
    if status_code >= 500 {
        return Some("upstream_5xx");
    }
    None
}

fn limit_error_diagnostic_message(message: &str) -> String {
    const MAX_ERROR_DIAGNOSTIC_MESSAGE_BYTES: usize = 1024;
    if message.len() <= MAX_ERROR_DIAGNOSTIC_MESSAGE_BYTES {
        return message.to_string();
    }
    let mut end = MAX_ERROR_DIAGNOSTIC_MESSAGE_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &message[..end])
}

pub(crate) fn with_upstream_response_report_context(
    report_context: Option<&Value>,
    status_code: u16,
    headers: Option<&std::collections::BTreeMap<String, String>>,
    body: Option<&Value>,
    body_ref: Option<&str>,
    body_state: Option<&str>,
) -> Option<Value> {
    let mut object = report_context?.as_object()?.clone();
    let mut upstream_response = serde_json::Map::new();
    upstream_response.insert("status_code".to_string(), json!(status_code));
    if let Some(headers) = headers {
        upstream_response.insert("headers".to_string(), trace_headers_to_json(headers));
    }
    if let Some(body) = body {
        upstream_response.insert("body".to_string(), body.clone());
    }
    if let Some(body_ref) = body_ref {
        upstream_response.insert("body_ref".to_string(), json!(body_ref));
    }
    if let Some(body_state) = body_state {
        upstream_response.insert("body_state".to_string(), json!(body_state));
    }
    object.insert(
        "upstream_response".to_string(),
        Value::Object(upstream_response),
    );
    Some(Value::Object(object))
}

pub(crate) fn trace_upstream_response_body(
    body_json: Option<&Value>,
    body_bytes: &[u8],
) -> Option<Value> {
    if let Some(body_json) = body_json {
        return Some(limit_trace_upstream_response_body_json(body_json));
    }

    if body_bytes.is_empty() {
        return None;
    }

    if let Ok(text) = std::str::from_utf8(body_bytes) {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        if let Ok(json_body) = serde_json::from_str::<Value>(text) {
            return Some(limit_trace_upstream_response_body_json(&json_body));
        }
        return Some(Value::String(limit_trace_upstream_response_text(text)));
    }

    Some(json!({
        "encoding": "base64",
        "data": base64::engine::general_purpose::STANDARD.encode(
            &body_bytes[..body_bytes.len().min(crate::MAX_ERROR_BODY_BYTES)]
        ),
        "truncated": body_bytes.len() > crate::MAX_ERROR_BODY_BYTES,
    }))
}

fn limit_trace_upstream_response_body_json(body_json: &Value) -> Value {
    let Ok(serialized) = serde_json::to_vec(body_json) else {
        return body_json.clone();
    };
    if serialized.len() <= crate::MAX_ERROR_BODY_BYTES {
        return body_json.clone();
    }
    Value::String(limit_trace_upstream_response_text(
        String::from_utf8_lossy(&serialized).as_ref(),
    ))
}

fn limit_trace_upstream_response_text(text: &str) -> String {
    let mut bytes = 0usize;
    let mut out = String::new();
    for ch in text.chars() {
        let len = ch.len_utf8();
        if bytes + len > crate::MAX_ERROR_BODY_BYTES {
            out.push_str("...[truncated]");
            return out;
        }
        bytes += len;
        out.push(ch);
    }
    out
}

fn trace_headers_to_json(headers: &std::collections::BTreeMap<String, String>) -> Value {
    Value::Object(Map::from_iter(headers.iter().map(|(key, value)| {
        (
            key.clone(),
            Value::String(mask_trace_header_value(key, value)),
        )
    })))
}

fn mask_trace_header_value(name: &str, value: &str) -> String {
    if !trace_header_is_sensitive(name) {
        return value.to_string();
    }
    if value.len() <= 8 {
        return "****".to_string();
    }
    format!("{}****{}", &value[..4], &value[value.len() - 4..])
}

fn trace_header_is_sensitive(name: &str) -> bool {
    [
        "authorization",
        "x-api-key",
        "api-key",
        "x-goog-api-key",
        "cookie",
        "set-cookie",
        "proxy-authorization",
    ]
    .iter()
    .any(|candidate| name.trim().eq_ignore_ascii_case(candidate))
}

#[cfg(test)]
mod error_diagnostic_tests {
    use super::{
        error_diagnostic_kind, limit_error_diagnostic_message, with_error_diagnostic_report_context,
    };
    use crate::orchestration::{
        LocalFailoverAnalysis, LocalFailoverClassification, LocalFailoverDecision,
    };
    use serde_json::json;

    fn sample_analysis() -> LocalFailoverAnalysis {
        LocalFailoverAnalysis {
            classification: LocalFailoverClassification::RetryUpstreamFailure,
            decision: LocalFailoverDecision::RetryNextCandidate,
            preserve_upstream_error_on_retry: false,
        }
    }

    #[test]
    fn diagnostic_kind_detects_empty_response_messages() {
        assert_eq!(
            error_diagnostic_kind(
                502,
                Some("Provider returned HTTP 200 but the response did not contain visible model output; treating it as an empty upstream response.")
            ),
            Some("empty_response")
        );
        assert_eq!(
            error_diagnostic_kind(
                502,
                Some("{\"error\":{\"message\":\"upstream stream ended without visible model output\"}}")
            ),
            Some("empty_response")
        );
    }

    #[test]
    fn diagnostic_kind_maps_status_bands() {
        assert_eq!(
            error_diagnostic_kind(400, Some("bad")),
            Some("upstream_4xx")
        );
        assert_eq!(error_diagnostic_kind(429, None), Some("upstream_4xx"));
        assert_eq!(error_diagnostic_kind(503, None), Some("upstream_5xx"));
        assert_eq!(error_diagnostic_kind(200, None), None);
    }

    #[test]
    fn diagnostic_marker_attaches_to_report_context() {
        let context = with_error_diagnostic_report_context(
            Some(&json!({"request_id": "req-1"})),
            "upstream_5xx",
            503,
            Some(sample_analysis()),
            Some("{\"error\":{\"message\":\"boom\"}}"),
        )
        .expect("diagnostic context");
        assert_eq!(context["request_id"], json!("req-1"));
        assert_eq!(context["error_diagnostic"]["kind"], json!("upstream_5xx"));
        assert_eq!(context["error_diagnostic"]["upstream_status"], json!(503));
        assert_eq!(
            context["error_diagnostic"]["classification"],
            json!("retry_upstream_failure")
        );
        assert_eq!(
            context["error_diagnostic"]["decision"],
            json!("retry_next_candidate")
        );
        assert_eq!(context["error_diagnostic"]["message"], json!("boom"));
    }

    #[test]
    fn diagnostic_marker_requires_report_context() {
        assert!(
            with_error_diagnostic_report_context(None, "upstream_4xx", 400, None, None).is_none()
        );
    }

    #[test]
    fn diagnostic_message_is_truncated_to_metadata_limits() {
        let long = "x".repeat(4000);
        let limited = limit_error_diagnostic_message(&long);
        assert!(limited.len() <= 1028);
        assert!(limited.ends_with("..."));
        assert_eq!(limit_error_diagnostic_message("short"), "short");
    }
}
