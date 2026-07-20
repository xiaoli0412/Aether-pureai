use std::collections::BTreeMap;

use aether_contracts::RequestBody;

use super::{
    augment_sync_report_context, build_ai_execution_plan_from_decision, take_ai_decision_plan_core,
    take_ai_upstream_auth_pair, take_non_empty_string, AiExecutionPlanFromDecisionParts,
    AiStreamAttempt, AiSyncAttempt,
};
use crate::ai_serving::transport::{
    build_standard_plan_fallback_headers, StandardPlanFallbackAcceptPolicy,
    StandardPlanFallbackHeadersInput,
};
use crate::ai_serving::{
    generic_decision_missing_exact_provider_request,
    provider_adaptation_requires_eventstream_accept,
};
use crate::{AiExecutionDecision, GatewayError};

pub(crate) fn build_standard_sync_plan_from_decision(
    parts: &http::request::Parts,
    _body_json: &serde_json::Value,
    payload: AiExecutionDecision,
) -> Result<Option<AiSyncAttempt>, GatewayError> {
    let mut payload = payload;
    if generic_decision_missing_exact_provider_request(&payload) {
        return Ok(None);
    }
    let Some(core) = take_ai_decision_plan_core(&mut payload) else {
        return Ok(None);
    };
    let Some(url) = take_non_empty_string(&mut payload.upstream_url) else {
        return Ok(None);
    };
    let Some(auth_pair) = take_ai_upstream_auth_pair(&mut payload) else {
        return Ok(None);
    };
    let Some(provider_request_body_value) = payload.provider_request_body.take() else {
        return Ok(None);
    };
    let mut provider_request_headers =
        build_standard_plan_fallback_headers(StandardPlanFallbackHeadersInput {
            request_headers: &parts.headers,
            existing_provider_request_headers: std::mem::take(
                &mut payload.provider_request_headers,
            ),
            auth_header: auth_pair.as_ref().map(|pair| pair.header.as_str()),
            auth_value: auth_pair.as_ref().map(|pair| pair.value.as_str()),
            extra_headers: &BTreeMap::new(),
            content_type: payload.content_type.as_deref(),
            provider_api_format: core.provider_api_format.as_str(),
            client_api_format: core.client_api_format.as_str(),
            upstream_is_stream: payload.upstream_is_stream,
            build_from_request_when_empty: false,
            accept_policy: StandardPlanFallbackAcceptPolicy::TextEventStreamIfStreaming,
        });
    crate::relay::collaboration::apply_trusted_relay_request_id_to_provider_headers(
        parts,
        &mut provider_request_headers,
    );
    let content_type = payload
        .content_type
        .take()
        .or_else(|| Some("application/json".to_string()));
    let report_context = augment_sync_report_context(
        parts,
        payload.report_context.take(),
        &provider_request_headers,
        &provider_request_body_value,
    )?;
    let stream = payload.upstream_is_stream;
    let plan = build_ai_execution_plan_from_decision(
        &mut payload,
        AiExecutionPlanFromDecisionParts {
            core,
            method: "POST".to_string(),
            url,
            headers: std::mem::take(&mut provider_request_headers),
            content_type,
            body: RequestBody::from_json(provider_request_body_value),
            stream,
        },
    );

    Ok(Some(AiSyncAttempt {
        plan,
        report_kind: payload.report_kind,
        report_context,
    }))
}

pub(crate) fn build_standard_stream_plan_from_decision(
    parts: &http::request::Parts,
    _body_json: &serde_json::Value,
    payload: AiExecutionDecision,
    _inject_stream_flag: bool,
) -> Result<Option<AiStreamAttempt>, GatewayError> {
    let mut payload = payload;
    if generic_decision_missing_exact_provider_request(&payload) {
        return Ok(None);
    }
    let Some(core) = take_ai_decision_plan_core(&mut payload) else {
        return Ok(None);
    };
    let Some(url) = take_non_empty_string(&mut payload.upstream_url) else {
        return Ok(None);
    };
    let Some(auth_pair) = take_ai_upstream_auth_pair(&mut payload) else {
        return Ok(None);
    };
    let Some(provider_request_body_value) = payload.provider_request_body.take() else {
        return Ok(None);
    };

    let envelope_name = payload
        .report_context
        .as_ref()
        .and_then(|context| context.get("envelope_name"))
        .and_then(serde_json::Value::as_str);
    let accept_policy = if payload.upstream_is_stream
        && provider_adaptation_requires_eventstream_accept(
            envelope_name,
            core.provider_api_format.as_str(),
        ) {
        StandardPlanFallbackAcceptPolicy::ProviderEventStreamIfMissing
    } else {
        StandardPlanFallbackAcceptPolicy::TextEventStreamIfStreaming
    };
    let mut provider_request_headers =
        build_standard_plan_fallback_headers(StandardPlanFallbackHeadersInput {
            request_headers: &parts.headers,
            existing_provider_request_headers: std::mem::take(
                &mut payload.provider_request_headers,
            ),
            auth_header: auth_pair.as_ref().map(|pair| pair.header.as_str()),
            auth_value: auth_pair.as_ref().map(|pair| pair.value.as_str()),
            extra_headers: &BTreeMap::new(),
            content_type: payload.content_type.as_deref(),
            provider_api_format: core.provider_api_format.as_str(),
            client_api_format: core.client_api_format.as_str(),
            upstream_is_stream: payload.upstream_is_stream,
            build_from_request_when_empty: false,
            accept_policy,
        });
    crate::relay::collaboration::apply_trusted_relay_request_id_to_provider_headers(
        parts,
        &mut provider_request_headers,
    );
    let content_type = payload
        .content_type
        .take()
        .or_else(|| Some("application/json".to_string()));
    let report_context = augment_sync_report_context(
        parts,
        payload.report_context.take(),
        &provider_request_headers,
        &provider_request_body_value,
    )?;
    let stream = payload.upstream_is_stream;
    let plan = build_ai_execution_plan_from_decision(
        &mut payload,
        AiExecutionPlanFromDecisionParts {
            core,
            method: "POST".to_string(),
            url,
            headers: std::mem::take(&mut provider_request_headers),
            content_type,
            body: RequestBody::from_json(provider_request_body_value),
            stream,
        },
    );

    Ok(Some(AiStreamAttempt {
        plan,
        report_kind: payload.report_kind,
        report_context,
    }))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use axum::http::Request;
    use serde_json::json;

    use super::{build_standard_stream_plan_from_decision, build_standard_sync_plan_from_decision};
    use crate::relay::collaboration::{
        RelayContext, TrustedRelayContext, HEADER_ONEAPI_REQUEST_ID,
    };
    use crate::AiExecutionDecision;

    fn trusted_relay_parts() -> http::request::Parts {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/embeddings")
            .body(())
            .expect("request should build");
        let (mut parts, _) = request.into_parts();
        parts.extensions.insert(TrustedRelayContext(RelayContext {
            instance_id: "aether-primary".to_string(),
            request_id: "newapi-generic-plan-request-123".to_string(),
            subject_id: "subject-1".to_string(),
            token_subject_id: "token-subject-1".to_string(),
            channel_id: "41".to_string(),
            group: "default".to_string(),
            model: "text-embedding-3-small".to_string(),
            relay_format: "openai".to_string(),
            config_revision: 7,
            expires_at: 1_784_073_600,
        }));
        parts
    }

    fn generic_payload(
        client_api_format: &str,
        provider_api_format: &str,
        upstream_is_stream: bool,
    ) -> AiExecutionDecision {
        AiExecutionDecision {
            action: "execution_runtime".to_string(),
            decision_kind: Some("generic".to_string()),
            execution_strategy: Some("local_same_format".to_string()),
            conversion_mode: None,
            request_id: Some("gateway-generated-request-id".to_string()),
            candidate_id: Some("candidate-1".to_string()),
            provider_name: Some("provider".to_string()),
            provider_type: Some("custom".to_string()),
            provider_id: Some("provider-1".to_string()),
            endpoint_id: Some("endpoint-1".to_string()),
            key_id: Some("key-1".to_string()),
            upstream_base_url: Some("https://provider.example".to_string()),
            upstream_url: Some("https://provider.example/v1/embeddings".to_string()),
            provider_request_method: None,
            auth_header: Some("authorization".to_string()),
            auth_value: Some("Bearer upstream-key".to_string()),
            provider_api_format: Some(provider_api_format.to_string()),
            client_api_format: Some(client_api_format.to_string()),
            provider_contract: Some(provider_api_format.to_string()),
            client_contract: Some(client_api_format.to_string()),
            model_name: Some("text-embedding-3-small".to_string()),
            mapped_model: Some("text-embedding-3-small".to_string()),
            prompt_cache_key: None,
            extra_headers: BTreeMap::new(),
            provider_request_headers: BTreeMap::from([(
                "content-type".to_string(),
                "application/json".to_string(),
            )]),
            provider_request_body: Some(json!({
                "model": "text-embedding-3-small",
                "input": "relay request"
            })),
            provider_request_body_base64: None,
            content_type: Some("application/json".to_string()),
            content_encoding: None,
            request_gzip: None,
            proxy: None,
            transport_profile: None,
            timeouts: None,
            upstream_is_stream,
            report_kind: Some("generic_success".to_string()),
            report_context: Some(json!({})),
            auth_context: None,
        }
    }

    #[test]
    fn generic_openai_plans_propagate_the_trusted_relay_request_id() {
        let parts = trusted_relay_parts();

        let sync = build_standard_sync_plan_from_decision(
            &parts,
            &json!({}),
            generic_payload("openai:embedding", "openai:embedding", false),
        )
        .expect("sync plan should build")
        .expect("sync plan should be present");
        assert_eq!(
            sync.plan
                .headers
                .get(HEADER_ONEAPI_REQUEST_ID)
                .map(String::as_str),
            Some("newapi-generic-plan-request-123")
        );

        let stream = build_standard_stream_plan_from_decision(
            &parts,
            &json!({}),
            generic_payload("openai:image", "openai:image", true),
            false,
        )
        .expect("stream plan should build")
        .expect("stream plan should be present");
        assert_eq!(
            stream
                .plan
                .headers
                .get(HEADER_ONEAPI_REQUEST_ID)
                .map(String::as_str),
            Some("newapi-generic-plan-request-123")
        );
    }
}
