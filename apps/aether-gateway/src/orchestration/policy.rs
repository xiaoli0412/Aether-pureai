use std::collections::BTreeSet;

use aether_contracts::ExecutionPlan;
use serde_json::{json, Value};
use tracing::debug;

use crate::provider_transport::GatewayProviderTransportSnapshot;
use crate::AppState;

pub(crate) const CYBER_CONTINUE_FAILOVER_CONFIG_KEY: &str = "cyber_continue_failover";
pub(crate) const RESPONSES_WEBSOCKET_CONFIG_KEY: &str = "responses_websocket";
pub(crate) const UPSTREAM_POLICY_CONFIG_KEY: &str = "upstream_policy";

/// How a provider responds once its empty-response retry budget is exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalEmptyResponseExhaustion {
    /// Deliver the empty success response to the client unchanged.
    Passthrough,
    /// Keep the legacy failure flow (retry/503 exhaustion).
    Error,
}

/// Provider-scoped policy for "HTTP 200 without visible output" responses,
/// e.g. Gemini risk-control blocks proxied through passthrough upstreams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalEmptyResponsePolicy {
    pub(crate) detect: bool,
    pub(crate) max_attempts: u64,
    pub(crate) on_exhausted: LocalEmptyResponseExhaustion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalFailoverPolicy {
    pub(crate) max_retries: Option<u64>,
    pub(crate) max_transfer_count: u64,
    pub(crate) max_transfer_timeout_seconds: u64,
    pub(crate) stop_status_codes: BTreeSet<u16>,
    pub(crate) continue_status_codes: BTreeSet<u16>,
    pub(crate) stop_on_transport_errors: bool,
    pub(crate) success_failover_patterns: Vec<LocalFailoverRegexRule>,
    pub(crate) error_stop_patterns: Vec<LocalFailoverRegexRule>,
    pub(crate) stop_cyber_policy_errors: bool,
    pub(crate) retry_client_errors_by_default: bool,
    pub(crate) upstream_passthrough_mode: bool,
    pub(crate) enforced_max_attempts: Option<u64>,
    pub(crate) passthrough_upstream_errors: bool,
    pub(crate) empty_response_policy: Option<LocalEmptyResponsePolicy>,
}

impl Default for LocalFailoverPolicy {
    fn default() -> Self {
        Self {
            max_retries: None,
            max_transfer_count: 0,
            max_transfer_timeout_seconds: 0,
            stop_status_codes: BTreeSet::new(),
            continue_status_codes: BTreeSet::new(),
            stop_on_transport_errors: false,
            success_failover_patterns: Vec::new(),
            error_stop_patterns: Vec::new(),
            stop_cyber_policy_errors: true,
            retry_client_errors_by_default: true,
            upstream_passthrough_mode: false,
            enforced_max_attempts: None,
            passthrough_upstream_errors: false,
            empty_response_policy: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalFailoverRegexRule {
    pub(crate) pattern: String,
    pub(crate) status_codes: BTreeSet<u16>,
}

pub(crate) async fn resolve_local_failover_policy(
    state: &AppState,
    plan: &ExecutionPlan,
    _report_context: Option<&serde_json::Value>,
) -> LocalFailoverPolicy {
    let mut policy = match state
        .read_provider_transport_snapshot(&plan.provider_id, &plan.endpoint_id, &plan.key_id)
        .await
    {
        Ok(Some(transport)) => local_failover_policy_from_transport(&transport),
        Ok(None) | Err(_) => LocalFailoverPolicy::default(),
    };
    let cyber_continue_failover = cyber_continue_failover_enabled(state).await;
    policy.stop_cyber_policy_errors = !cyber_continue_failover;
    debug!(
        event_name = "local_failover_policy_loaded",
        log_type = "debug",
        request_id = %plan.request_id,
        provider_id = %plan.provider_id,
        endpoint_id = %plan.endpoint_id,
        key_id = %plan.key_id,
        source = "transport_snapshot",
        max_retries = ?policy.max_retries,
        max_transfer_count = policy.max_transfer_count,
        max_transfer_timeout_seconds = policy.max_transfer_timeout_seconds,
        stop_status_code_count = policy.stop_status_codes.len(),
        continue_status_code_count = policy.continue_status_codes.len(),
        stop_on_transport_errors = policy.stop_on_transport_errors,
        success_failover_pattern_count = policy.success_failover_patterns.len(),
        error_stop_pattern_count = policy.error_stop_patterns.len(),
        cyber_continue_failover,
        "gateway loaded local failover policy from transport snapshot"
    );
    policy
}

pub(crate) async fn cyber_continue_failover_enabled(state: &AppState) -> bool {
    state
        .read_system_config_json_value(CYBER_CONTINUE_FAILOVER_CONFIG_KEY)
        .await
        .ok()
        .flatten()
        .as_ref()
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub(crate) fn local_failover_policy_from_transport(
    transport: &GatewayProviderTransportSnapshot,
) -> LocalFailoverPolicy {
    let provider_config = transport.provider.config.as_ref();
    let rules = transport
        .provider
        .config
        .as_ref()
        .and_then(|config| config.get("failover_rules"))
        .and_then(Value::as_object);
    let max_retries = rules
        .and_then(|value| value.get("max_retries"))
        .and_then(parse_u64_value)
        .or_else(|| {
            transport
                .endpoint
                .max_retries
                .and_then(|value| u64::try_from(value).ok())
        })
        .or_else(|| {
            transport
                .provider
                .max_retries
                .and_then(|value| u64::try_from(value).ok())
        });

    let upstream_policy = parse_upstream_policy_fields(provider_config.and_then(Value::as_object));

    LocalFailoverPolicy {
        max_retries,
        max_transfer_count: provider_config
            .and_then(|value| value.get("max_transfer_count"))
            .and_then(parse_u64_value)
            .unwrap_or(0),
        max_transfer_timeout_seconds: provider_config
            .and_then(|value| value.get("max_transfer_timeout_seconds"))
            .and_then(parse_u64_value)
            .unwrap_or(0),
        retry_client_errors_by_default:
            crate::ai_serving::api_format_defaults_to_client_error_failover(
                &transport.endpoint.api_format,
            ),
        stop_cyber_policy_errors: true,
        upstream_passthrough_mode: upstream_policy.passthrough_mode,
        enforced_max_attempts: upstream_policy.max_attempts,
        passthrough_upstream_errors: upstream_policy.passthrough_errors,
        empty_response_policy: upstream_policy.empty_response,
        stop_status_codes: rules
            .map(|value| {
                parse_status_code_set(
                    value,
                    &[
                        "stop_on_status_codes",
                        "early_stop_status_codes",
                        "non_retryable_status_codes",
                        "stop_status_codes",
                    ],
                )
            })
            .unwrap_or_default(),
        continue_status_codes: rules
            .map(|value| {
                parse_status_code_set(
                    value,
                    &[
                        "continue_on_status_codes",
                        "retryable_status_codes",
                        "retry_on_status_codes",
                        "continue_status_codes",
                    ],
                )
            })
            .unwrap_or_default(),
        stop_on_transport_errors: rules
            .and_then(|value| value.get("stop_on_transport_errors"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        success_failover_patterns: rules
            .map(|value| parse_regex_rules(value, "success_failover_patterns"))
            .unwrap_or_default(),
        error_stop_patterns: rules
            .map(|value| parse_regex_rules(value, "error_stop_patterns"))
            .unwrap_or_default(),
    }
}

pub(crate) fn local_failover_policy_from_report_context(
    report_context: Option<&Value>,
) -> Option<LocalFailoverPolicy> {
    let object = report_context
        .and_then(Value::as_object)?
        .get("local_failover_policy")?
        .as_object()?;

    let upstream_policy = parse_upstream_policy_fields(Some(object));

    Some(LocalFailoverPolicy {
        max_retries: object.get("max_retries").and_then(parse_u64_value),
        max_transfer_count: object
            .get("max_transfer_count")
            .and_then(parse_u64_value)
            .unwrap_or(0),
        max_transfer_timeout_seconds: object
            .get("max_transfer_timeout_seconds")
            .and_then(parse_u64_value)
            .unwrap_or(0),
        stop_status_codes: object
            .get("stop_status_codes")
            .map(parse_status_code_list)
            .unwrap_or_default(),
        continue_status_codes: object
            .get("continue_status_codes")
            .map(parse_status_code_list)
            .unwrap_or_default(),
        stop_on_transport_errors: object
            .get("stop_on_transport_errors")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        success_failover_patterns: parse_regex_rules(object, "success_failover_patterns"),
        error_stop_patterns: parse_regex_rules(object, "error_stop_patterns"),
        stop_cyber_policy_errors: object
            .get("stop_cyber_policy_errors")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        retry_client_errors_by_default: object
            .get("retry_client_errors_by_default")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        upstream_passthrough_mode: upstream_policy.passthrough_mode,
        enforced_max_attempts: upstream_policy.max_attempts,
        passthrough_upstream_errors: upstream_policy.passthrough_errors,
        empty_response_policy: upstream_policy.empty_response,
    })
}

pub(crate) fn append_local_failover_policy_to_value(
    value: Value,
    transport: &GatewayProviderTransportSnapshot,
) -> Value {
    let Value::Object(mut object) = value else {
        return value;
    };
    object.insert(
        "local_failover_policy".to_string(),
        local_failover_policy_to_value(&local_failover_policy_from_transport(transport)),
    );
    if transport
        .provider
        .provider_type
        .trim()
        .eq_ignore_ascii_case("codex")
    {
        let codex = transport
            .key
            .upstream_metadata
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|metadata| metadata.get("codex"));
        object.insert(
            "codex_quota_reset_generation".to_string(),
            Value::from(aether_admin::provider::quota::codex_quota_account_reset_generation(codex)),
        );
        if let Some(generation) = aether_admin::provider::quota::codex_credential_generation(codex)
        {
            object.insert(
                "codex_credential_generation".to_string(),
                Value::String(generation.to_string()),
            );
        }
    }
    Value::Object(object)
}

fn parse_status_code_list(value: &Value) -> BTreeSet<u16> {
    value
        .as_array()
        .into_iter()
        .flat_map(|values| values.iter())
        .filter_map(|value| parse_u64_value(value).and_then(|value| u16::try_from(value).ok()))
        .collect()
}

fn local_failover_policy_to_value(policy: &LocalFailoverPolicy) -> Value {
    json!({
        "max_retries": policy.max_retries,
        "max_transfer_count": policy.max_transfer_count,
        "max_transfer_timeout_seconds": policy.max_transfer_timeout_seconds,
        "stop_status_codes": policy.stop_status_codes.iter().copied().collect::<Vec<_>>(),
        "continue_status_codes": policy.continue_status_codes.iter().copied().collect::<Vec<_>>(),
        "stop_on_transport_errors": policy.stop_on_transport_errors,
        "success_failover_patterns": policy.success_failover_patterns.iter().map(local_failover_regex_rule_to_value).collect::<Vec<_>>(),
        "error_stop_patterns": policy.error_stop_patterns.iter().map(local_failover_regex_rule_to_value).collect::<Vec<_>>(),
        "stop_cyber_policy_errors": policy.stop_cyber_policy_errors,
        "retry_client_errors_by_default": policy.retry_client_errors_by_default,
        UPSTREAM_POLICY_CONFIG_KEY: upstream_policy_to_value(policy),
    })
}

#[derive(Debug, Clone, Default)]
struct UpstreamPolicyFields {
    passthrough_mode: bool,
    max_attempts: Option<u64>,
    passthrough_errors: bool,
    empty_response: Option<LocalEmptyResponsePolicy>,
}

fn parse_upstream_policy_fields(
    container: Option<&serde_json::Map<String, Value>>,
) -> UpstreamPolicyFields {
    let Some(policy) = container
        .and_then(|config| config.get(UPSTREAM_POLICY_CONFIG_KEY))
        .and_then(Value::as_object)
    else {
        return UpstreamPolicyFields::default();
    };

    UpstreamPolicyFields {
        passthrough_mode: policy
            .get("mode")
            .and_then(Value::as_str)
            .is_some_and(|mode| mode.trim().eq_ignore_ascii_case("full_passthrough")),
        max_attempts: policy
            .get("max_attempts")
            .and_then(parse_u64_value)
            .filter(|value| (1..=100).contains(value)),
        passthrough_errors: policy
            .get("passthrough_upstream_errors")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        empty_response: policy
            .get("empty_response")
            .and_then(Value::as_object)
            .map(parse_empty_response_policy),
    }
}

fn parse_empty_response_policy(
    empty: &serde_json::Map<String, Value>,
) -> LocalEmptyResponsePolicy {
    LocalEmptyResponsePolicy {
        detect: empty
            .get("detect")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        max_attempts: empty
            .get("max_attempts")
            .and_then(parse_u64_value)
            .map(|value| value.min(100))
            .unwrap_or(1),
        on_exhausted: match empty.get("on_exhausted").and_then(Value::as_str) {
            Some(value) if value.trim().eq_ignore_ascii_case("passthrough") => {
                LocalEmptyResponseExhaustion::Passthrough
            }
            _ => LocalEmptyResponseExhaustion::Error,
        },
    }
}

fn upstream_policy_to_value(policy: &LocalFailoverPolicy) -> Value {
    let mut object = serde_json::Map::new();
    if policy.upstream_passthrough_mode {
        object.insert("mode".to_string(), json!("full_passthrough"));
    }
    if let Some(max_attempts) = policy.enforced_max_attempts {
        object.insert("max_attempts".to_string(), json!(max_attempts));
    }
    if policy.passthrough_upstream_errors {
        object.insert("passthrough_upstream_errors".to_string(), json!(true));
    }
    if let Some(empty) = policy.empty_response_policy.as_ref() {
        object.insert(
            "empty_response".to_string(),
            json!({
                "detect": empty.detect,
                "max_attempts": empty.max_attempts,
                "on_exhausted": match empty.on_exhausted {
                    LocalEmptyResponseExhaustion::Passthrough => "passthrough",
                    LocalEmptyResponseExhaustion::Error => "error",
                },
            }),
        );
    }
    Value::Object(object)
}

pub(crate) fn codex_cyber_flag_passthrough_enabled(
    provider_type: &str,
    provider_config: Option<&Value>,
) -> bool {
    if !provider_type.trim().eq_ignore_ascii_case("codex") {
        return false;
    }
    provider_config
        .and_then(|config| config.get("codex"))
        .and_then(Value::as_object)
        .and_then(|codex| {
            codex
                .get("pass_through_cyber_flag_interrupt")
                .or_else(|| codex.get("passthrough_cyber_flag_interrupt"))
                .and_then(Value::as_bool)
        })
        .unwrap_or(true)
}

/// Selects the protocol adapter responsible for one eligible Responses
/// WebSocket upstream. Provider-scoped feature switches remain the source of
/// truth; this enum only identifies provider-specific extensions around the
/// otherwise standard Responses WebSocket protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponsesWebSocketAdapter {
    /// A provider that speaks the standard OpenAI Responses WebSocket protocol.
    Standard,
    /// Standard protocol plus Codex account and quota extensions.
    Codex,
}

impl ResponsesWebSocketAdapter {
    pub(crate) fn supports_provider_type(self, provider_type: &str) -> bool {
        match self {
            Self::Standard => {
                !provider_type.trim().is_empty()
                    && !provider_type.trim().eq_ignore_ascii_case("codex")
            }
            Self::Codex => provider_type.trim().eq_ignore_ascii_case("codex"),
        }
    }
}

/// Whether a provider explicitly enables the standard Responses WebSocket
/// bridge. The setting is provider-scoped so rollout remains opt-in per
/// verified upstream.
pub(crate) fn responses_websocket_enabled(provider_config: Option<&Value>) -> bool {
    provider_config
        .and_then(|config| config.get(RESPONSES_WEBSOCKET_CONFIG_KEY))
        .and_then(Value::as_object)
        .and_then(|responses| responses.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Returns the enabled Responses WebSocket adapter for a provider. The shared
/// protocol bridge remains opt-in, while this resolver isolates provider-only
/// extensions from candidate planning and the session engine.
pub(crate) fn responses_websocket_adapter(
    provider_type: &str,
    provider_config: Option<&Value>,
) -> Option<ResponsesWebSocketAdapter> {
    let provider_type = provider_type.trim();
    if provider_type.is_empty() {
        return None;
    }
    if !responses_websocket_enabled(provider_config) {
        return None;
    }
    Some(if provider_type.eq_ignore_ascii_case("codex") {
        ResponsesWebSocketAdapter::Codex
    } else {
        ResponsesWebSocketAdapter::Standard
    })
}

fn local_failover_regex_rule_to_value(rule: &LocalFailoverRegexRule) -> Value {
    json!({
        "pattern": rule.pattern,
        "status_codes": rule.status_codes.iter().copied().collect::<Vec<_>>(),
    })
}

fn parse_regex_rules(
    rules: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Vec<LocalFailoverRegexRule> {
    let allow_status_only = key == "error_stop_patterns";
    rules
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flat_map(|items| items.iter())
        .filter_map(|value| parse_regex_rule(value, allow_status_only))
        .collect()
}

fn parse_regex_rule(
    value: &serde_json::Value,
    allow_status_only: bool,
) -> Option<LocalFailoverRegexRule> {
    let object = value.as_object()?;
    let pattern = object
        .get("pattern")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    let status_codes: BTreeSet<u16> = object
        .get("status_codes")
        .and_then(Value::as_array)
        .into_iter()
        .flat_map(|values| values.iter())
        .filter_map(|value| parse_u64_value(value).and_then(|value| u16::try_from(value).ok()))
        .collect();
    if pattern.is_empty() && (!allow_status_only || status_codes.is_empty()) {
        return None;
    }
    Some(LocalFailoverRegexRule {
        pattern: pattern.to_string(),
        status_codes,
    })
}

fn parse_status_code_set(
    rules: &serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> BTreeSet<u16> {
    keys.iter()
        .filter_map(|key| rules.get(*key))
        .filter_map(Value::as_array)
        .flat_map(|values| values.iter())
        .filter_map(|value| parse_u64_value(value).and_then(|value| u16::try_from(value).ok()))
        .collect()
}

fn parse_u64_value(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        append_local_failover_policy_to_value, local_failover_policy_from_report_context,
        local_failover_policy_from_transport, responses_websocket_adapter,
        responses_websocket_enabled, LocalEmptyResponseExhaustion, LocalEmptyResponsePolicy,
        LocalFailoverPolicy, LocalFailoverRegexRule, ResponsesWebSocketAdapter,
    };
    use crate::provider_transport::snapshot::{
        GatewayProviderTransportEndpoint, GatewayProviderTransportKey,
        GatewayProviderTransportProvider, GatewayProviderTransportSnapshot,
    };

    fn sample_transport(
        provider_max_retries: Option<i32>,
        endpoint_max_retries: Option<i32>,
        provider_config: Option<serde_json::Value>,
    ) -> GatewayProviderTransportSnapshot {
        GatewayProviderTransportSnapshot {
            provider: GatewayProviderTransportProvider {
                id: "provider-1".to_string(),
                name: "OpenAI".to_string(),
                provider_type: "llm".to_string(),
                website: None,
                is_active: true,
                keep_priority_on_conversion: false,
                enable_format_conversion: true,
                concurrent_limit: None,
                max_retries: provider_max_retries,
                proxy: None,
                request_timeout_secs: None,
                stream_first_byte_timeout_secs: None,
                config: provider_config,
            },
            endpoint: GatewayProviderTransportEndpoint {
                id: "endpoint-1".to_string(),
                provider_id: "provider-1".to_string(),
                api_format: "openai:chat".to_string(),
                api_family: Some("openai".to_string()),
                endpoint_kind: Some("chat".to_string()),
                is_active: true,
                base_url: "https://example.com".to_string(),
                header_rules: None,
                body_rules: None,
                max_retries: endpoint_max_retries,
                custom_path: None,
                config: None,
                format_acceptance_config: None,
                proxy: None,
            },
            key: GatewayProviderTransportKey {
                id: "key-1".to_string(),
                provider_id: "provider-1".to_string(),
                name: "primary".to_string(),
                auth_type: "bearer".to_string(),
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
                decrypted_api_key: "secret".to_string(),
                decrypted_auth_config: None,
            },
        }
    }

    #[test]
    fn append_local_failover_policy_to_value_round_trips_policy_shape() {
        let report_context = append_local_failover_policy_to_value(
            json!({
                "request_id": "req-1",
            }),
            &sample_transport(
                Some(5),
                Some(4),
                Some(json!({
                    "max_transfer_count": 10,
                    "max_transfer_timeout_seconds": 60,
                    "failover_rules": {
                        "max_retries": 2,
                        "continue_status_codes": [429],
                        "stop_status_codes": [400],
                        "stop_on_transport_errors": true,
                        "success_failover_patterns": [{"pattern": "quota", "status_codes": [200]}],
                        "error_stop_patterns": [{"pattern": "validation", "status_codes": [422]}]
                    }
                })),
            ),
        );

        assert_eq!(
            local_failover_policy_from_report_context(Some(&report_context)),
            Some(LocalFailoverPolicy {
                max_retries: Some(2),
                max_transfer_count: 10,
                max_transfer_timeout_seconds: 60,
                stop_status_codes: [400].into_iter().collect(),
                continue_status_codes: [429].into_iter().collect(),
                stop_on_transport_errors: true,
                success_failover_patterns: vec![LocalFailoverRegexRule {
                    pattern: "quota".to_string(),
                    status_codes: [200].into_iter().collect(),
                }],
                error_stop_patterns: vec![LocalFailoverRegexRule {
                    pattern: "validation".to_string(),
                    status_codes: [422].into_iter().collect(),
                }],
                stop_cyber_policy_errors: true,
                retry_client_errors_by_default: true,
                upstream_passthrough_mode: false,
                enforced_max_attempts: None,
                passthrough_upstream_errors: false,
                empty_response_policy: None,
            })
        );
    }

    #[test]
    fn codex_report_context_captures_quota_and_credential_generations() {
        let mut transport = sample_transport(None, None, None);
        transport.provider.provider_type = "codex".to_string();
        transport.key.upstream_metadata = Some(json!({
            "codex": {
                "account_quota_reset_generation": 7,
                "credential_generation": "credential-generation-7"
            }
        }));

        let report_context = append_local_failover_policy_to_value(json!({}), &transport);

        assert_eq!(report_context["codex_quota_reset_generation"], json!(7u64));
        assert_eq!(
            report_context["codex_credential_generation"],
            json!("credential-generation-7")
        );
    }

    #[test]
    fn transport_error_failover_defaults_to_continue_and_accepts_explicit_stop() {
        let default_policy =
            local_failover_policy_from_transport(&sample_transport(None, None, None));
        assert!(!default_policy.stop_on_transport_errors);

        let stop_policy = local_failover_policy_from_transport(&sample_transport(
            None,
            None,
            Some(json!({
                "failover_rules": {
                    "stop_on_transport_errors": true,
                }
            })),
        ));
        assert!(stop_policy.stop_on_transport_errors);

        let invalid_policy = local_failover_policy_from_transport(&sample_transport(
            None,
            None,
            Some(json!({
                "failover_rules": {
                    "stop_on_transport_errors": "true",
                }
            })),
        ));
        assert!(!invalid_policy.stop_on_transport_errors);
    }

    #[test]
    fn transfer_limits_are_read_only_from_top_level_provider_config() {
        let top_level = local_failover_policy_from_transport(&sample_transport(
            None,
            None,
            Some(json!({
                "max_transfer_count": 3,
                "max_transfer_timeout_seconds": 45,
            })),
        ));
        assert_eq!(top_level.max_transfer_count, 3);
        assert_eq!(top_level.max_transfer_timeout_seconds, 45);

        let nested = local_failover_policy_from_transport(&sample_transport(
            None,
            None,
            Some(json!({
                "failover_rules": {
                    "max_transfer_count": 8,
                    "max_transfer_timeout_seconds": 90,
                }
            })),
        ));
        assert_eq!(nested.max_transfer_count, 0);
        assert_eq!(nested.max_transfer_timeout_seconds, 0);
    }

    #[test]
    fn search_transport_disables_default_client_error_failover() {
        let mut transport = sample_transport(None, None, None);
        transport.endpoint.api_format = "openai:search".to_string();
        let policy = local_failover_policy_from_transport(&transport);

        assert!(!policy.retry_client_errors_by_default);
        let report_context = append_local_failover_policy_to_value(json!({}), &transport);
        assert_eq!(
            local_failover_policy_from_report_context(Some(&report_context))
                .map(|policy| policy.retry_client_errors_by_default),
            Some(false)
        );
    }

    #[test]
    fn transport_policy_defaults_to_stopping_cyber_policy() {
        let mut transport = sample_transport(None, None, None);
        transport.provider.provider_type = "codex".to_string();
        assert!(local_failover_policy_from_transport(&transport).stop_cyber_policy_errors);

        transport.provider.config = Some(json!({
            "codex": {"pass_through_cyber_flag_interrupt": false}
        }));
        assert!(local_failover_policy_from_transport(&transport).stop_cyber_policy_errors);

        transport.provider.config = Some(json!({
            "codex": {"passthrough_cyber_flag_interrupt": true}
        }));
        assert!(local_failover_policy_from_transport(&transport).stop_cyber_policy_errors);

        transport.provider.provider_type = "llm".to_string();
        assert!(local_failover_policy_from_transport(&transport).stop_cyber_policy_errors);
    }

    #[test]
    fn responses_websocket_requires_an_explicit_provider_switch() {
        assert!(!responses_websocket_enabled(None));
        assert!(!responses_websocket_enabled(Some(&json!({
            "responses_websocket": {"enabled": false}
        }))));
        assert!(responses_websocket_enabled(Some(&json!({
            "responses_websocket": {"enabled": true}
        }))));

        assert_eq!(
            responses_websocket_adapter(
                "custom",
                Some(&json!({"responses_websocket": {"enabled": false}})),
            ),
            None
        );
        assert_eq!(
            responses_websocket_adapter(
                "custom",
                Some(&json!({"responses_websocket": {"enabled": true}})),
            ),
            Some(ResponsesWebSocketAdapter::Standard)
        );
        assert_eq!(
            responses_websocket_adapter(
                "codex",
                Some(&json!({"responses_websocket": {"enabled": true}})),
            ),
            Some(ResponsesWebSocketAdapter::Codex)
        );
        assert!(ResponsesWebSocketAdapter::Codex.supports_provider_type("CODEX"));
        assert!(!ResponsesWebSocketAdapter::Codex.supports_provider_type("openai"));
        assert!(ResponsesWebSocketAdapter::Standard.supports_provider_type("custom"));
        assert!(!ResponsesWebSocketAdapter::Standard.supports_provider_type("codex"));
    }

    #[test]
    fn upstream_policy_parses_full_passthrough_mode() {
        let policy = local_failover_policy_from_transport(&sample_transport(
            None,
            None,
            Some(json!({
                "upstream_policy": { "mode": "full_passthrough" }
            })),
        ));
        assert!(policy.upstream_passthrough_mode);
        assert!(policy.empty_response_policy.is_none());
    }

    #[test]
    fn upstream_policy_parses_capped_retry_and_empty_response() {
        let policy = local_failover_policy_from_transport(&sample_transport(
            None,
            None,
            Some(json!({
                "upstream_policy": {
                    "max_attempts": 2,
                    "passthrough_upstream_errors": true,
                    "empty_response": {
                        "detect": true,
                        "max_attempts": 1,
                        "on_exhausted": "passthrough"
                    }
                }
            })),
        ));
        assert!(!policy.upstream_passthrough_mode);
        assert_eq!(policy.enforced_max_attempts, Some(2));
        assert!(policy.passthrough_upstream_errors);
        assert_eq!(
            policy.empty_response_policy,
            Some(LocalEmptyResponsePolicy {
                detect: true,
                max_attempts: 1,
                on_exhausted: LocalEmptyResponseExhaustion::Passthrough,
            })
        );
    }

    #[test]
    fn upstream_policy_round_trips_through_report_context() {
        let report_context = append_local_failover_policy_to_value(
            json!({}),
            &sample_transport(
                None,
                None,
                Some(json!({
                    "upstream_policy": {
                        "mode": "full_passthrough",
                        "max_attempts": 3,
                        "passthrough_upstream_errors": true,
                        "empty_response": {
                            "detect": true,
                            "max_attempts": 0,
                            "on_exhausted": "error"
                        }
                    }
                })),
            ),
        );
        let parsed = local_failover_policy_from_report_context(Some(&report_context)).unwrap();
        assert!(parsed.upstream_passthrough_mode);
        assert_eq!(parsed.enforced_max_attempts, Some(3));
        assert!(parsed.passthrough_upstream_errors);
        assert_eq!(
            parsed.empty_response_policy,
            Some(LocalEmptyResponsePolicy {
                detect: true,
                max_attempts: 0,
                on_exhausted: LocalEmptyResponseExhaustion::Error,
            })
        );
    }

    #[test]
    fn upstream_policy_defaults_preserve_legacy_behavior() {
        let policy = local_failover_policy_from_transport(&sample_transport(None, None, None));
        assert!(!policy.upstream_passthrough_mode);
        assert_eq!(policy.enforced_max_attempts, None);
        assert!(!policy.passthrough_upstream_errors);
        assert!(policy.empty_response_policy.is_none());
    }

    #[test]
    fn upstream_policy_ignores_malformed_values() {
        let policy = local_failover_policy_from_transport(&sample_transport(
            None,
            None,
            Some(json!({
                "upstream_policy": {
                    "mode": "nonsense",
                    "max_attempts": "lots",
                    "passthrough_upstream_errors": "yes",
                    "empty_response": { "on_exhausted": "explode" }
                }
            })),
        ));
        assert!(!policy.upstream_passthrough_mode);
        assert_eq!(policy.enforced_max_attempts, None);
        assert!(!policy.passthrough_upstream_errors);
        let empty = policy.empty_response_policy.unwrap();
        assert!(!empty.detect);
        assert_eq!(empty.max_attempts, 1);
        assert_eq!(empty.on_exhausted, LocalEmptyResponseExhaustion::Error);
    }

    #[test]
    fn upstream_policy_clamps_out_of_range_attempt_budgets() {
        let policy = local_failover_policy_from_transport(&sample_transport(
            None,
            None,
            Some(json!({
                "upstream_policy": {
                    "max_attempts": 101,
                    "empty_response": { "max_attempts": 999 }
                }
            })),
        ));
        assert_eq!(policy.enforced_max_attempts, None);
        assert_eq!(policy.empty_response_policy.unwrap().max_attempts, 100);
    }
}
