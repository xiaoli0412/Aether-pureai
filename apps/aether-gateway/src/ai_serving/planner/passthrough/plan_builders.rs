use aether_contracts::RequestBody;

use super::{
    augment_sync_report_context, build_ai_execution_plan_from_decision,
    resolve_ai_passthrough_sync_request_body, take_ai_decision_plan_core, take_non_empty_string,
    AiExecutionPlanFromDecisionParts, AiStreamAttempt, AiSyncAttempt,
};
use crate::{AiExecutionDecision, GatewayError};

pub(crate) fn build_passthrough_sync_plan_from_decision(
    parts: &http::request::Parts,
    payload: AiExecutionDecision,
) -> Result<Option<AiSyncAttempt>, GatewayError> {
    let mut payload = payload;
    let Some(core) = take_ai_decision_plan_core(&mut payload) else {
        return Ok(None);
    };
    let Some(upstream_url) = take_non_empty_string(&mut payload.upstream_url) else {
        return Ok(None);
    };
    let mut provider_request_headers = std::mem::take(&mut payload.provider_request_headers);
    crate::relay::collaboration::apply_trusted_relay_request_id_to_provider_headers(
        parts,
        &mut provider_request_headers,
    );
    let ignored_provider_request_body = serde_json::Value::Null;
    let report_context = augment_sync_report_context(
        parts,
        payload.report_context.take(),
        &provider_request_headers,
        &ignored_provider_request_body,
    )?;
    let request_body = resolve_ai_passthrough_sync_request_body(
        payload.provider_request_body.take(),
        payload.provider_request_body_base64.take(),
    );
    let provider_request_method = take_non_empty_string(&mut payload.provider_request_method);
    let content_type = payload
        .content_type
        .take()
        .or_else(|| provider_request_headers.get("content-type").cloned());

    let plan = build_ai_execution_plan_from_decision(
        &mut payload,
        AiExecutionPlanFromDecisionParts {
            core,
            method: provider_request_method.unwrap_or_else(|| parts.method.to_string()),
            url: upstream_url,
            headers: provider_request_headers,
            content_type,
            body: request_body,
            stream: false,
        },
    );

    Ok(Some(AiSyncAttempt {
        plan,
        report_kind: payload.report_kind,
        report_context,
    }))
}

pub(crate) fn build_passthrough_stream_plan_from_decision(
    parts: &http::request::Parts,
    payload: AiExecutionDecision,
) -> Result<Option<AiStreamAttempt>, GatewayError> {
    let mut payload = payload;
    let Some(core) = take_ai_decision_plan_core(&mut payload) else {
        return Ok(None);
    };
    let Some(upstream_url) = take_non_empty_string(&mut payload.upstream_url) else {
        return Ok(None);
    };
    let mut provider_request_headers = std::mem::take(&mut payload.provider_request_headers);
    crate::relay::collaboration::apply_trusted_relay_request_id_to_provider_headers(
        parts,
        &mut provider_request_headers,
    );
    let content_type = payload
        .content_type
        .take()
        .or_else(|| provider_request_headers.get("content-type").cloned());
    let plan = build_ai_execution_plan_from_decision(
        &mut payload,
        AiExecutionPlanFromDecisionParts {
            core,
            method: parts.method.to_string(),
            url: upstream_url,
            headers: provider_request_headers,
            content_type,
            body: RequestBody {
                json_body: None,
                body_bytes_b64: None,
                body_ref: None,
            },
            stream: true,
        },
    );
    let mut report_context = payload.report_context;
    crate::relay::collaboration::apply_trusted_relay_usage_metadata_to_report_context(
        parts,
        &mut report_context,
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

    use super::{build_passthrough_stream_plan_from_decision, build_passthrough_sync_plan_from_decision};
    use crate::relay::collaboration::{
        RelayContext, TrustedRelayContext, HEADER_ONEAPI_REQUEST_ID,
    };
    use crate::AiExecutionDecision;

    fn trusted_relay_parts() -> http::request::Parts {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/images/generations")
            .body(())
            .expect("request should build");
        let (mut parts, _) = request.into_parts();
        parts.extensions.insert(TrustedRelayContext(RelayContext {
            instance_id: "aether-primary".to_string(),
            request_id: "newapi-passthrough-plan-request-123".to_string(),
            subject_id: "subject-1".to_string(),
            token_subject_id: "token-subject-1".to_string(),
            channel_id: "41".to_string(),
            group: "default".to_string(),
            model: "gpt-image-1".to_string(),
            relay_format: "openai".to_string(),
            config_revision: 7,
            expires_at: 1_784_073_600,
        }));
        parts
    }

    fn passthrough_payload(provider_api_format: &str) -> AiExecutionDecision {
        AiExecutionDecision {
            action: "execution_runtime".to_string(),
            decision_kind: Some("passthrough".to_string()),
            execution_strategy: Some("local_same_format".to_string()),
            conversion_mode: None,
            request_id: Some("gateway-generated-request-id".to_string()),
            candidate_id: Some("candidate-1".to_string()),
            provider_name: Some("provider".to_string()),
            provider_id: Some("provider-1".to_string()),
            endpoint_id: Some("endpoint-1".to_string()),
            key_id: Some("key-1".to_string()),
            upstream_base_url: Some("https://provider.example".to_string()),
            upstream_url: Some("https://provider.example/v1/images/generations".to_string()),
            provider_request_method: None,
            auth_header: Some("authorization".to_string()),
            auth_value: Some("Bearer upstream-key".to_string()),
            provider_api_format: Some(provider_api_format.to_string()),
            client_api_format: Some(provider_api_format.to_string()),
            provider_contract: Some(provider_api_format.to_string()),
            client_contract: Some(provider_api_format.to_string()),
            model_name: Some("gpt-image-1".to_string()),
            mapped_model: Some("gpt-image-1".to_string()),
            prompt_cache_key: None,
            extra_headers: BTreeMap::new(),
            provider_request_headers: BTreeMap::from([(
                "content-type".to_string(),
                "application/json".to_string(),
            )]),
            provider_request_body: Some(json!({
                "model": "gpt-image-1",
                "prompt": "relay request"
            })),
            provider_request_body_base64: None,
            content_type: Some("application/json".to_string()),
            content_encoding: None,
            request_gzip: None,
            proxy: None,
            transport_profile: None,
            timeouts: None,
            upstream_is_stream: false,
            report_kind: Some("passthrough_success".to_string()),
            report_context: Some(json!({})),
            auth_context: None,
        }
    }

    #[test]
    fn passthrough_openai_plans_propagate_the_trusted_relay_request_id() {
        let parts = trusted_relay_parts();

        let sync = build_passthrough_sync_plan_from_decision(
            &parts,
            passthrough_payload("openai:image"),
        )
        .expect("sync plan should build")
        .expect("sync plan should be present");
        assert_eq!(
            sync.plan
                .headers
                .get(HEADER_ONEAPI_REQUEST_ID)
                .map(String::as_str),
            Some("newapi-passthrough-plan-request-123")
        );

        let stream = build_passthrough_stream_plan_from_decision(
            &parts,
            passthrough_payload("openai:video"),
        )
        .expect("stream plan should build")
        .expect("stream plan should be present");
        assert_eq!(
            stream
                .plan
                .headers
                .get(HEADER_ONEAPI_REQUEST_ID)
                .map(String::as_str),
            Some("newapi-passthrough-plan-request-123")
        );
    }
}
