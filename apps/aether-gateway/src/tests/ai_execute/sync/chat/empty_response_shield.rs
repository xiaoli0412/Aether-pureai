//! Integration coverage for the detachable empty-response shield (F4):
//! when an upstream keeps returning empty 200 responses, the gateway blocks
//! the offending session locally after a threshold and answers with a
//! Google-safety-review style response instead of dispatching upstream.

use super::{
    any, build_router_with_state, build_state_with_execution_runtime_override,
    encrypt_python_fernet_plaintext, json, run_async_test_on_large_stack, start_server, to_bytes,
    Arc, Digest, InMemoryAuthApiKeySnapshotRepository,
    InMemoryMinimalCandidateSelectionReadRepository, InMemoryProviderCatalogReadRepository,
    InMemoryRequestCandidateRepository, Json, Mutex, Request, Router, Sha256, StatusCode,
    StoredAuthApiKeySnapshot, StoredMinimalCandidateSelectionRow, StoredProviderCatalogEndpoint,
    StoredProviderCatalogKey, StoredProviderCatalogProvider, StoredProviderModelMapping,
    DEVELOPMENT_ENCRYPTION_KEY, TRACE_ID_HEADER,
};
use aether_data::repository::usage::InMemoryUsageReadRepository;
use aether_data::repository::wallet::InMemoryWalletRepository;
use aether_data_contracts::repository::wallet::StoredWalletSnapshot;

fn hash_api_key(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn sample_auth_snapshot(api_key_id: &str, user_id: &str) -> StoredAuthApiKeySnapshot {
    StoredAuthApiKeySnapshot::new(
        user_id.to_string(),
        "alice".to_string(),
        Some("alice@example.com".to_string()),
        "user".to_string(),
        "local".to_string(),
        true,
        false,
        None,
        None,
        Some(serde_json::json!(["gpt-5"])),
        api_key_id.to_string(),
        Some("default".to_string()),
        true,
        false,
        false,
        Some(60),
        Some(5),
        Some(4_102_444_800),
        None,
        None,
        Some(serde_json::json!(["gpt-5"])),
    )
    .expect("auth snapshot should build")
}

fn candidate_row() -> StoredMinimalCandidateSelectionRow {
    StoredMinimalCandidateSelectionRow {
        provider_id: "provider-shield".to_string(),
        provider_name: "shield-provider".to_string(),
        provider_type: "custom".to_string(),
        provider_priority: 10,
        provider_is_active: true,
        endpoint_id: "endpoint-shield".to_string(),
        endpoint_api_format: "openai:chat".to_string(),
        endpoint_api_family: Some("openai".to_string()),
        endpoint_kind: Some("chat".to_string()),
        endpoint_is_active: true,
        key_id: "key-shield".to_string(),
        key_name: "key-shield".to_string(),
        key_auth_type: "api_key".to_string(),
        key_is_active: true,
        key_api_formats: Some(vec!["openai:chat".to_string()]),
        key_allowed_models: None,
        key_capabilities: None,
        key_internal_priority: 5,
        key_global_priority_by_format: Some(serde_json::json!({"openai:chat": 1})),
        model_id: "model-shield".to_string(),
        global_model_id: "global-model-shield".to_string(),
        global_model_name: "gpt-5".to_string(),
        global_model_mappings: None,
        global_model_supports_streaming: Some(true),
        model_provider_model_name: "gpt-5-shield".to_string(),
        model_provider_model_mappings: Some(vec![StoredProviderModelMapping {
            name: "gpt-5-shield".to_string(),
            priority: 1,
            api_formats: Some(vec!["openai:chat".to_string()]),
            endpoint_ids: None,
            operations: None,
        }]),
        model_supports_streaming: Some(true),
        model_is_active: true,
        model_is_available: true,
    }
}

/// Provider config with empty-response detection enabled so every empty 200
/// from the upstream records a shield strike.
fn shield_provider_config() -> serde_json::Value {
    json!({
        "upstream_policy": {
            "empty_response": {
                "detect": true,
                "max_attempts": 1,
                "on_exhausted": "error"
            }
        }
    })
}

fn catalog_provider(config: serde_json::Value) -> StoredProviderCatalogProvider {
    StoredProviderCatalogProvider::new(
        "provider-shield".to_string(),
        "shield-provider".to_string(),
        Some("https://example.com".to_string()),
        "custom".to_string(),
    )
    .expect("provider should build")
    .with_transport_fields(
        true,
        false,
        false,
        None,
        Some(2),
        None,
        Some(20.0),
        None,
        Some(config),
    )
}

fn catalog_endpoint(base_url: &str) -> StoredProviderCatalogEndpoint {
    StoredProviderCatalogEndpoint::new(
        "endpoint-shield".to_string(),
        "provider-shield".to_string(),
        "openai:chat".to_string(),
        Some("openai".to_string()),
        Some("chat".to_string()),
        true,
    )
    .expect("endpoint should build")
    .with_transport_fields(
        base_url.to_string(),
        None,
        None,
        Some(2),
        None,
        None,
        None,
        None,
    )
    .expect("endpoint transport should build")
}

fn catalog_key(secret: &str) -> StoredProviderCatalogKey {
    StoredProviderCatalogKey::new(
        "key-shield".to_string(),
        "provider-shield".to_string(),
        "key-shield".to_string(),
        "api_key".to_string(),
        None,
        true,
    )
    .expect("key should build")
    .with_transport_fields(
        Some(serde_json::json!(["openai:chat"])),
        encrypt_python_fernet_plaintext(DEVELOPMENT_ENCRYPTION_KEY, secret)
            .expect("api key should encrypt"),
        None,
        None,
        Some(serde_json::json!({"openai:chat": 1})),
        None,
        None,
        None,
        None,
    )
    .expect("key transport should build")
}

fn funded_wallet(
    id: &str,
    user_id: Option<&str>,
    api_key_id: Option<&str>,
) -> StoredWalletSnapshot {
    StoredWalletSnapshot::new(
        id.to_string(),
        user_id.map(str::to_string),
        api_key_id.map(str::to_string),
        1_000.0,
        0.0,
        "unlimited".to_string(),
        "USD".to_string(),
        "active".to_string(),
        1_000.0,
        0.0,
        0.0,
        0.0,
        1_710_000_000,
    )
    .expect("wallet should build")
}

async fn send_chat_request(
    gateway_url: &str,
    trace_id: &str,
    session_id: &str,
) -> (StatusCode, String) {
    let body = json!({
        "model": "gpt-5",
        "session_id": session_id,
        "messages": [{ "role": "user", "content": "hi" }],
    });
    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, "Bearer sk-client-shield")
        .header(TRACE_ID_HEADER, trace_id)
        .body(body.to_string())
        .send()
        .await
        .expect("request should succeed");
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    (status, text)
}

async fn build_shield_gateway(
    shield_config: Option<serde_json::Value>,
) -> (
    String,
    tokio::task::JoinHandle<()>,
    Arc<Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    let captured_urls = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_urls_clone = Arc::clone(&captured_urls);
    // The mock upstream always returns an empty success (HTTP 200 without
    // visible model output) — the risk-control shape the shield guards
    // against.
    let execution_runtime = Router::new().route(
        "/v1/execute/sync",
        any(move |request: Request| {
            let captured = Arc::clone(&captured_urls_clone);
            async move {
                let (_parts, body) = request.into_parts();
                let raw_body = to_bytes(body, usize::MAX).await.expect("body should read");
                let payload: serde_json::Value =
                    serde_json::from_slice(&raw_body).expect("payload should parse");
                let url = payload
                    .get("url")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string();
                captured.lock().expect("mutex should lock").push(url);
                Json(json!({
                    "request_id": "trace-shield-123",
                    "status_code": 200,
                    "headers": { "content-type": "application/json" },
                    "body": {
                        "json_body": {
                            "id": "chatcmpl-shield",
                            "object": "chat.completion",
                            "model": "gpt-5",
                            "choices": [],
                            "usage": {
                                "prompt_tokens": 1,
                                "completion_tokens": 0,
                                "total_tokens": 1
                            }
                        }
                    },
                    "telemetry": { "elapsed_ms": 10 }
                }))
            }
        }),
    );

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-client-shield")),
        sample_auth_snapshot("api-key-shield-1", "user-shield-1"),
    )]));
    let candidate_selection_repository =
        Arc::new(InMemoryMinimalCandidateSelectionReadRepository::seed(vec![
            candidate_row(),
        ]));
    let request_candidate_repository = Arc::new(InMemoryRequestCandidateRepository::default());
    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![catalog_provider(shield_provider_config())],
        vec![catalog_endpoint("https://api.shield.example")],
        vec![catalog_key("sk-upstream-shield")],
    ));
    let wallet_repository = Arc::new(InMemoryWalletRepository::seed(vec![
        funded_wallet("wallet-shield-user", Some("user-shield-1"), None),
        funded_wallet("wallet-shield-key", None, Some("api-key-shield-1")),
    ]));

    let (execution_runtime_url, execution_runtime_handle) = start_server(execution_runtime).await;
    let mut system_config = vec![("provider_priority_mode".to_string(), json!("global_key"))];
    if let Some(shield_config) = shield_config {
        system_config.push(("empty_response_shield".to_string(), shield_config));
    }
    let gateway_state = build_state_with_execution_runtime_override(execution_runtime_url)
        .with_data_state_for_tests(
            crate::data::GatewayDataState::with_auth_candidate_selection_provider_catalog_request_candidates_usage_billing_and_wallet_for_tests(
                auth_repository,
                candidate_selection_repository,
                provider_catalog_repository,
                Arc::clone(&request_candidate_repository),
                Arc::new(InMemoryUsageReadRepository::default()),
                Arc::new(aether_data::repository::billing::InMemoryBillingReadRepository::seed(vec![])),
                wallet_repository,
                DEVELOPMENT_ENCRYPTION_KEY,
            )
            .with_system_config_values_for_tests(system_config),
        );
    let gateway = build_router_with_state(gateway_state);
    let (gateway_url, gateway_handle) = start_server(gateway).await;
    (
        gateway_url,
        gateway_handle,
        captured_urls,
        execution_runtime_handle,
    )
}

fn shield_response_body_contains_content_filter(body: &str) -> bool {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    parsed["choices"][0]["finish_reason"] == "content_filter"
        || parsed
            .get("aether_shield")
            .and_then(|value| value.get("blocked"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
}

large_stack_async_test!(
    gateway_empty_response_shield_blocks_session_after_threshold,
    gateway_empty_response_shield_blocks_session_after_threshold_impl
);

async fn gateway_empty_response_shield_blocks_session_after_threshold_impl() {
    let (gateway_url, gateway_handle, captured_urls, execution_runtime_handle) =
        build_shield_gateway(Some(json!({
            "enabled": true,
            "threshold": 2,
            "window_secs": 600,
            "block_secs": 300
        })))
        .await;

    // Warm-up requests: each returns an empty upstream response and records
    // strikes against the session. They reach the upstream.
    for index in 0..3 {
        let trace_id = format!("trace-shield-warmup-{index}");
        let _ = send_chat_request(&gateway_url, &trace_id, "session-blocked").await;
    }
    let warmup_hits = captured_urls.lock().expect("mutex should lock").len();
    assert!(
        warmup_hits > 0,
        "warm-up requests should reach the upstream"
    );

    // Subsequent requests for the same session are answered locally with a
    // safety-review style response and never reach the upstream.
    let (status, body) =
        send_chat_request(&gateway_url, "trace-shield-blocked-1", "session-blocked").await;
    assert_eq!(status, StatusCode::OK, "blocked request body: {body}");
    assert!(
        shield_response_body_contains_content_filter(&body),
        "expected local safety response, got: {body}"
    );
    let blocked_hits = captured_urls.lock().expect("mutex should lock").len();
    assert_eq!(
        blocked_hits, warmup_hits,
        "blocked request must not reach the upstream"
    );

    // A different session is not blocked.
    let (other_status, _other_body) =
        send_chat_request(&gateway_url, "trace-shield-other", "session-other").await;
    let other_hits = captured_urls.lock().expect("mutex should lock").len();
    assert!(
        other_hits > blocked_hits,
        "unblocked session should still reach the upstream (status {other_status})"
    );

    gateway_handle.abort();
    execution_runtime_handle.abort();
}

large_stack_async_test!(
    gateway_empty_response_shield_inert_without_config,
    gateway_empty_response_shield_inert_without_config_impl
);

async fn gateway_empty_response_shield_inert_without_config_impl() {
    let (gateway_url, gateway_handle, captured_urls, execution_runtime_handle) =
        build_shield_gateway(None).await;

    // Without the shield config, repeated empty responses never produce a
    // local safety block: every request still reaches the upstream.
    for index in 0..4 {
        let trace_id = format!("trace-shield-inert-{index}");
        let _ = send_chat_request(&gateway_url, &trace_id, "session-inert").await;
    }
    let hits = captured_urls.lock().expect("mutex should lock").len();
    assert!(
        hits >= 4,
        "without shield config every request should reach the upstream, got {hits}"
    );

    gateway_handle.abort();
    execution_runtime_handle.abort();
}
