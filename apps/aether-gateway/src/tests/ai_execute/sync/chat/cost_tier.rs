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
use aether_data::repository::billing::InMemoryBillingReadRepository;
use aether_data::repository::usage::InMemoryUsageReadRepository;
use aether_data::repository::wallet::InMemoryWalletRepository;
use aether_data_contracts::repository::billing::StoredBillingModelContext;
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
        Some(serde_json::json!(["openai:chat"])),
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
        Some(serde_json::json!(["openai:chat"])),
        Some(serde_json::json!(["gpt-5"])),
    )
    .expect("auth snapshot should build")
}

fn candidate_row(provider_suffix: &str, key_priority: i64) -> StoredMinimalCandidateSelectionRow {
    StoredMinimalCandidateSelectionRow {
        provider_id: format!("provider-cost-tier-{provider_suffix}"),
        provider_name: format!("cost-tier-{provider_suffix}"),
        provider_type: "custom".to_string(),
        provider_priority: 10,
        provider_is_active: true,
        endpoint_id: format!("endpoint-cost-tier-{provider_suffix}"),
        endpoint_api_format: "openai:chat".to_string(),
        endpoint_api_family: Some("openai".to_string()),
        endpoint_kind: Some("chat".to_string()),
        endpoint_is_active: true,
        key_id: format!("key-cost-tier-{provider_suffix}"),
        key_name: format!("key-{provider_suffix}"),
        key_auth_type: "api_key".to_string(),
        key_is_active: true,
        key_api_formats: Some(vec!["openai:chat".to_string()]),
        key_allowed_models: None,
        key_capabilities: None,
        key_internal_priority: 5,
        key_global_priority_by_format: Some(serde_json::json!({"openai:chat": key_priority})),
        model_id: format!("model-cost-tier-{provider_suffix}"),
        global_model_id: "global-model-cost-tier".to_string(),
        global_model_name: "gpt-5".to_string(),
        global_model_mappings: None,
        global_model_supports_streaming: Some(true),
        model_provider_model_name: format!("gpt-5-{provider_suffix}"),
        model_provider_model_mappings: Some(vec![StoredProviderModelMapping {
            name: format!("gpt-5-{provider_suffix}"),
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

fn catalog_provider(
    provider_suffix: &str,
    config: Option<serde_json::Value>,
) -> StoredProviderCatalogProvider {
    StoredProviderCatalogProvider::new(
        format!("provider-cost-tier-{provider_suffix}"),
        format!("cost-tier-{provider_suffix}"),
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
        config,
    )
}

fn catalog_endpoint(provider_suffix: &str, base_url: &str) -> StoredProviderCatalogEndpoint {
    StoredProviderCatalogEndpoint::new(
        format!("endpoint-cost-tier-{provider_suffix}"),
        format!("provider-cost-tier-{provider_suffix}"),
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

fn catalog_key(provider_suffix: &str, secret: &str) -> StoredProviderCatalogKey {
    StoredProviderCatalogKey::new(
        format!("key-cost-tier-{provider_suffix}"),
        format!("provider-cost-tier-{provider_suffix}"),
        format!("key-{provider_suffix}"),
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

fn billing_context(provider_suffix: &str, pricing: BillingShape) -> StoredBillingModelContext {
    let (price_per_request, tiered) = match pricing {
        BillingShape::PerRequest => (Some(0.05_f64), None),
        BillingShape::PerUse => (
            None,
            Some(json!({
                "tiers": [{
                    "up_to": null,
                    "input_price_per_1m": 3.0,
                    "output_price_per_1m": 15.0,
                    "cache_creation_price_per_1m": 3.75,
                    "cache_read_price_per_1m": 0.30
                }]
            })),
        ),
    };
    StoredBillingModelContext::new(
        format!("provider-cost-tier-{provider_suffix}"),
        None,
        None,
        None,
        None,
        "global-model-cost-tier".to_string(),
        "gpt-5".to_string(),
        None,
        price_per_request,
        tiered,
        Some(format!("model-cost-tier-{provider_suffix}")),
        Some(format!("gpt-5-{provider_suffix}")),
        None,
        None,
        None,
    )
    .expect("billing context should build")
}

enum BillingShape {
    PerRequest,
    PerUse,
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

fn cost_tier_provider_config(enabled: bool) -> serde_json::Value {
    json!({
        "cost_tier": {
            "enabled": enabled,
            "context_threshold_tokens": 100,
            "tiers": {
                "below": { "prefer": "per_use" },
                "above": { "prefer": "per_request" }
            }
        }
    })
}

async fn send_chat_request(
    gateway_url: &str,
    trace_id: &str,
    message_content: &str,
) -> (StatusCode, String) {
    let body = json!({
        "model": "gpt-5",
        "messages": [{ "role": "user", "content": message_content }],
    });
    let response = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, "Bearer sk-client-cost-tier")
        .header(TRACE_ID_HEADER, trace_id)
        .body(body.to_string())
        .send()
        .await
        .expect("request should succeed");
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    (status, text)
}

async fn build_cost_tier_gateway(
    cost_tier_enabled: bool,
) -> (
    String,
    tokio::task::JoinHandle<()>,
    Arc<Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    let captured_urls = Arc::new(Mutex::new(Vec::<String>::new()));
    let captured_urls_clone = Arc::clone(&captured_urls);
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
                    "request_id": "trace-cost-tier-123",
                    "status_code": 200,
                    "headers": { "content-type": "application/json" },
                    "body": {
                        "json_body": {
                            "id": "chatcmpl-cost-tier",
                            "object": "chat.completion",
                            "model": "gpt-5",
                            "choices": [],
                            "usage": {
                                "prompt_tokens": 2,
                                "completion_tokens": 3,
                                "total_tokens": 5
                            }
                        }
                    },
                    "telemetry": { "elapsed_ms": 25 }
                }))
            }
        }),
    );

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-client-cost-tier")),
        sample_auth_snapshot("api-key-cost-tier-1", "user-cost-tier-1"),
    )]));
    let candidate_selection_repository =
        Arc::new(InMemoryMinimalCandidateSelectionReadRepository::seed(vec![
            candidate_row("a", 1),
            candidate_row("b", 2),
        ]));
    let request_candidate_repository = Arc::new(InMemoryRequestCandidateRepository::default());
    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![
            catalog_provider("a", Some(cost_tier_provider_config(cost_tier_enabled))),
            catalog_provider("b", None),
        ],
        vec![
            catalog_endpoint("a", "https://api.peruse.example"),
            catalog_endpoint("b", "https://api.perreq.example"),
        ],
        vec![
            catalog_key("a", "sk-upstream-peruse"),
            catalog_key("b", "sk-upstream-perreq"),
        ],
    ));
    let billing_repository = Arc::new(InMemoryBillingReadRepository::seed(vec![
        billing_context("a", BillingShape::PerUse),
        billing_context("b", BillingShape::PerRequest),
    ]));
    let wallet_repository = Arc::new(InMemoryWalletRepository::seed(vec![
        funded_wallet("wallet-cost-tier-user", Some("user-cost-tier-1"), None),
        funded_wallet("wallet-cost-tier-key", None, Some("api-key-cost-tier-1")),
    ]));

    let (execution_runtime_url, execution_runtime_handle) = start_server(execution_runtime).await;
    let gateway_state =
        build_state_with_execution_runtime_override(execution_runtime_url)
            .with_data_state_for_tests(
                crate::data::GatewayDataState::with_auth_candidate_selection_provider_catalog_request_candidates_usage_billing_and_wallet_for_tests(
                    auth_repository,
                    candidate_selection_repository,
                    provider_catalog_repository,
                    Arc::clone(&request_candidate_repository),
                    Arc::new(InMemoryUsageReadRepository::default()),
                    billing_repository,
                    wallet_repository,
                    DEVELOPMENT_ENCRYPTION_KEY,
                )
                .with_system_config_values_for_tests(vec![(
                    "provider_priority_mode".to_string(),
                    json!("global_key"),
                )]),
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

large_stack_async_test!(
    gateway_cost_tier_routes_by_context_size_when_enabled,
    gateway_cost_tier_routes_by_context_size_when_enabled_impl
);

async fn gateway_cost_tier_routes_by_context_size_when_enabled_impl() {
    let (gateway_url, gateway_handle, captured_urls, execution_runtime_handle) =
        build_cost_tier_gateway(true).await;

    // Short context (< threshold) targets per_use: provider A (tiered pricing)
    // is the matching candidate and has base priority.
    let (status, body) = send_chat_request(&gateway_url, "trace-cost-tier-short", "hi").await;
    assert_eq!(status, StatusCode::OK, "short request body: {body}");

    // Long context (>= threshold) targets per_request: provider B (flat
    // per-request pricing) is the only matching candidate and must win even
    // though provider A has base priority.
    let long_content = "x".repeat(2_000);
    let (status, body) =
        send_chat_request(&gateway_url, "trace-cost-tier-long", &long_content).await;
    assert_eq!(status, StatusCode::OK, "long request body: {body}");

    let urls = captured_urls.lock().expect("mutex should lock").clone();
    assert_eq!(
        urls.len(),
        2,
        "both requests should hit the execution runtime"
    );
    assert!(
        urls[0].starts_with("https://api.peruse.example"),
        "short context should route to the per-use provider, got {}",
        urls[0]
    );
    assert!(
        urls[1].starts_with("https://api.perreq.example"),
        "long context should route to the per-request provider, got {}",
        urls[1]
    );

    gateway_handle.abort();
    execution_runtime_handle.abort();
}

large_stack_async_test!(
    gateway_cost_tier_inactive_keeps_priority_order,
    gateway_cost_tier_inactive_keeps_priority_order_impl
);

async fn gateway_cost_tier_inactive_keeps_priority_order_impl() {
    let (gateway_url, gateway_handle, captured_urls, execution_runtime_handle) =
        build_cost_tier_gateway(false).await;

    let (status, body) = send_chat_request(&gateway_url, "trace-cost-tier-off-short", "hi").await;
    assert_eq!(status, StatusCode::OK, "short request body: {body}");
    let long_content = "x".repeat(2_000);
    let (status, body) =
        send_chat_request(&gateway_url, "trace-cost-tier-off-long", &long_content).await;
    assert_eq!(status, StatusCode::OK, "long request body: {body}");

    let urls = captured_urls.lock().expect("mutex should lock").clone();
    assert_eq!(
        urls.len(),
        2,
        "both requests should hit the execution runtime"
    );
    for url in &urls {
        assert!(
            url.starts_with("https://api.peruse.example"),
            "with cost_tier disabled the base-priority provider should always serve, got {url}"
        );
    }

    gateway_handle.abort();
    execution_runtime_handle.abort();
}
