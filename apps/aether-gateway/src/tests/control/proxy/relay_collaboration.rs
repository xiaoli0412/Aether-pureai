use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use aether_crypto::{encrypt_python_fernet_plaintext, DEVELOPMENT_ENCRYPTION_KEY};
use aether_data::repository::auth::InMemoryAuthApiKeySnapshotRepository;
use aether_data::repository::billing::InMemoryBillingReadRepository;
use aether_data::repository::candidate_selection::InMemoryMinimalCandidateSelectionReadRepository;
use aether_data::repository::candidates::InMemoryRequestCandidateRepository;
use aether_data::repository::integration_configs::{
    IntegrationConfigStore, IntegrationConfigUpdate,
};
use aether_data::repository::provider_catalog::InMemoryProviderCatalogReadRepository;
use aether_data::repository::routing_profiles::InMemoryRoutingGroupRepository;
use aether_data::repository::settlement::InMemorySettlementRepository;
use aether_data::repository::usage::InMemoryUsageReadRepository;
use aether_data::repository::wallet::{
    InMemoryWalletRepository, StoredWalletSnapshot, WalletLookupKey, WalletReadRepository,
};
use aether_data_contracts::repository::billing::StoredBillingModelContext;
use aether_data_contracts::repository::candidates::{
    RequestCandidateReadRepository, RequestCandidateStatus,
};
use aether_data_contracts::repository::routing_profiles::{
    CreateRoutingGroupBindingRecord, CreateRoutingGroupRecord, RoutingGroupBindingSubject,
    StoredRoutingGroup, StoredRoutingGroupBinding, StoredRoutingGroupVersion,
};
use aether_data_contracts::repository::usage::{StoredRequestUsageAudit, UsageReadRepository};
use aether_usage_runtime::UsageRuntimeConfig;
use axum::body::{to_bytes, Body};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{extract::Request, Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::Utc;
use hmac::{Hmac, Mac};
use http::{HeaderValue, StatusCode};
use sha2::Sha256;

use super::super::{
    build_router_with_state, hash_api_key, sample_currently_usable_auth_snapshot, start_server,
    AppState,
};

const RELAY_PROXY_TEST_STACK_BYTES: usize = 16 * 1024 * 1024;

fn run_relay_proxy_test<F, Fut>(test_name: &'static str, make_future: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    let handle = std::thread::Builder::new()
        .name(test_name.to_string())
        .stack_size(RELAY_PROXY_TEST_STACK_BYTES)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("relay proxy test runtime should build");
            runtime.block_on(make_future());
        })
        .expect("relay proxy test thread should spawn");

    if let Err(payload) = handle.join() {
        std::panic::resume_unwind(payload);
    }
}

fn relay_headers(secret: &str, request_id: &str) -> Vec<(&'static str, String)> {
    relay_headers_with_context(secret, request_id, "gpt-5", "openai")
}

fn relay_headers_with_context(
    secret: &str,
    request_id: &str,
    model: &str,
    relay_format: &str,
) -> Vec<(&'static str, String)> {
    let context = serde_json::json!({
        "instance_id": "aether-primary",
        "request_id": request_id,
        "subject_id": "subject-1",
        "token_subject_id": "token-subject-1",
        "channel_id": "41",
        "group": "pro",
        "model": model,
        "relay_format": relay_format,
        "config_revision": 7,
        "expires_at": Utc::now().timestamp() + 30
    });
    let encoded = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&context).expect("relay context should serialize"));
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("relay secret should initialize HMAC");
    mac.update(encoded.as_bytes());
    let signature = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    vec![
        ("X-Aether-Instance-ID", "aether-primary".to_string()),
        ("X-Aether-Relay-Context", encoded),
        ("X-Aether-Relay-Signature", signature),
    ]
}

#[derive(Debug, Clone)]
struct SeenProviderRequest {
    authorization: String,
    relay_headers_present: bool,
    request_id: Option<String>,
    model: String,
    stream: bool,
}

fn assert_relay_usage_metadata(usage: &StoredRequestUsageAudit, request_id: &str) {
    let metadata = usage
        .request_metadata
        .as_ref()
        .expect("relay usage should retain request metadata");
    let relay = metadata
        .get("aether_relay")
        .expect("relay usage should retain trusted relay metadata");
    assert_eq!(
        relay,
        &serde_json::json!({
            "instance_id": "aether-primary",
            "request_id": request_id,
            "subject_id": "subject-1",
            "token_subject_id": "token-subject-1",
            "channel_id": "41",
            "group": "pro",
            "model": "gpt-5",
            "relay_format": "openai",
            "config_revision": 7,
        })
    );
    let relay = relay
        .as_object()
        .expect("trusted relay metadata should remain an object");
    for forbidden in [
        "relay_secret",
        "api_key",
        "authorization",
        "balance",
        "payment",
    ] {
        assert!(
            !relay.contains_key(forbidden),
            "trusted relay metadata must not contain {forbidden}"
        );
    }
}

fn assert_usage_has_no_relay_metadata(usage: &StoredRequestUsageAudit) {
    assert!(
        usage
            .request_metadata
            .as_ref()
            .and_then(|metadata| metadata.get("aether_relay"))
            .is_none(),
        "ordinary requests must not receive trusted relay metadata"
    );
}

fn relay_routing_group_repository() -> Arc<InMemoryRoutingGroupRepository> {
    let signed_group = StoredRoutingGroup::new(CreateRoutingGroupRecord {
        id: "pro".to_string(),
        name: "relay-pro".to_string(),
        description: Some("signed relay test group".to_string()),
        enabled: true,
        is_system_default: true,
        config_json: serde_json::json!({
            "default_policy": {
                "priority_mode": "provider",
                "scheduling_mode": "fixed_order"
            },
            "allowed_models": ["gpt-5"],
            "model_policies": [{
                "model": "gpt-5",
                "provider_priority_overrides": {
                    "provider-openai-usage-local-1": 1,
                    "provider-openai-relay-retry-backup": 2
                }
            }]
        }),
        version: 7,
        created_at: 0,
        updated_at: 0,
        published_at: Some(0),
    })
    .expect("relay routing group should build");
    let configured_profile = StoredRoutingGroup::new(CreateRoutingGroupRecord {
        id: "balanced".to_string(),
        name: "balanced".to_string(),
        description: Some("persisted relay route profile".to_string()),
        enabled: true,
        is_system_default: false,
        config_json: serde_json::json!({
            "allowed_models": ["gpt-5"],
            "model_policies": [{
                "model": "gpt-5",
                "provider_priority_overrides": {
                    "provider-openai-usage-local-1": 1,
                    "provider-openai-relay-retry-backup": 2
                }
            }]
        }),
        version: 11,
        created_at: 0,
        updated_at: 0,
        published_at: Some(0),
    })
    .expect("persisted relay route profile should build");
    let model_rewrite_profile = StoredRoutingGroup::new(CreateRoutingGroupRecord {
        id: "rewrite-model".to_string(),
        name: "rewrite-model".to_string(),
        description: Some("rejects rewrites of a signed relay model".to_string()),
        enabled: true,
        is_system_default: false,
        config_json: serde_json::json!({
            "allowed_models": ["gpt-5"],
            "rules": [{
                "id": "rewrite-signed-model",
                "priority": 1,
                "enabled": true,
                "phase": "client_request",
                "conditions": {},
                "actions": [{
                    "type": "json_patch_body",
                    "patch": [{
                        "op": "replace",
                        "path": "/model",
                        "value": "gpt-4.1"
                    }]
                }]
            }]
        }),
        version: 12,
        created_at: 0,
        updated_at: 0,
        published_at: Some(0),
    })
    .expect("signed-model rewrite profile should build");

    Arc::new(InMemoryRoutingGroupRepository::seed(
        vec![signed_group, configured_profile, model_rewrite_profile],
        vec![
            StoredRoutingGroupBinding::new(CreateRoutingGroupBindingRecord {
                id: "relay-balanced-profile-binding".to_string(),
                group_id: "balanced".to_string(),
                subject_type: RoutingGroupBindingSubject::ApiKey,
                subject_id: "relay-client-api-key".to_string(),
                is_default: false,
                allow_explicit_select: true,
                created_at: 0,
                updated_at: 0,
            })
            .expect("persisted relay route profile binding should build"),
            StoredRoutingGroupBinding::new(CreateRoutingGroupBindingRecord {
                id: "relay-model-rewrite-profile-binding".to_string(),
                group_id: "rewrite-model".to_string(),
                subject_type: RoutingGroupBindingSubject::ApiKey,
                subject_id: "relay-client-api-key".to_string(),
                is_default: false,
                allow_explicit_select: true,
                created_at: 0,
                updated_at: 0,
            })
            .expect("signed-model rewrite profile binding should build"),
        ],
        Vec::<StoredRoutingGroupVersion>::new(),
    ))
}

fn relay_integration_config_update(
    route_profile: &str,
    enabled: bool,
    execution_mode: &str,
    updated_at_unix_ms: i64,
) -> IntegrationConfigUpdate {
    IntegrationConfigUpdate {
        route_profile: route_profile.to_string(),
        execution_mode: execution_mode.to_string(),
        enabled,
        capability_version: "0.1.0".to_string(),
        updated_at_unix_ms,
    }
}

async fn relay_integration_config_store() -> IntegrationConfigStore {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("relay integration sqlite pool should connect");
    sqlx::query(
        "CREATE TABLE new_api_integration_configs (
            instance_id TEXT PRIMARY KEY,
            route_profile TEXT NOT NULL,
            execution_mode TEXT NOT NULL,
            enabled INTEGER NOT NULL,
            capability_version TEXT NOT NULL,
            revision INTEGER NOT NULL,
            updated_at_unix_ms INTEGER NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .expect("relay integration config table should be created");
    sqlx::query(
        "CREATE TABLE new_api_integration_credentials (
            instance_id TEXT PRIMARY KEY,
            current_control_secret_ciphertext TEXT NOT NULL,
            previous_control_secret_ciphertext TEXT,
            current_relay_secret_ciphertext TEXT NOT NULL,
            previous_relay_secret_ciphertext TEXT,
            transition_expires_at_unix_ms INTEGER,
            rotation_id TEXT NOT NULL,
            last_rotation_payload_sha256 TEXT NOT NULL,
            credential_revision INTEGER NOT NULL,
            updated_at_unix_ms INTEGER NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .expect("relay integration credential table should be created");
    let store = IntegrationConfigStore::sqlite(pool);
    store
        .compare_and_set(
            "aether-primary",
            0,
            &relay_integration_config_update("pro", true, "direct_channel", 1_784_073_600_000),
        )
        .await
        .expect("relay integration config should be inserted");
    store
}

async fn relay_test_state_with_persisted_integration_config(
    auth_repository: Arc<InMemoryAuthApiKeySnapshotRepository>,
    candidate_selection_repository: Arc<InMemoryMinimalCandidateSelectionReadRepository>,
    provider_catalog_repository: Arc<InMemoryProviderCatalogReadRepository>,
    request_candidate_repository: Arc<InMemoryRequestCandidateRepository>,
    usage_repository: Arc<InMemoryUsageReadRepository>,
    persist_integration_config: bool,
) -> (AppState, Arc<InMemoryWalletRepository>) {
    let integration_config_store = if persist_integration_config {
        Some(relay_integration_config_store().await)
    } else {
        None
    };
    let billing_repository = Arc::new(InMemoryBillingReadRepository::seed(vec![
        StoredBillingModelContext::new(
            "provider-openai-usage-local-1".to_string(),
            Some("pay_as_you_go".to_string()),
            Some("key-openai-usage-local-1".to_string()),
            Some(serde_json::json!({"openai:chat": 1.0})),
            Some(60),
            "global-model-openai-usage-local-1".to_string(),
            "gpt-5".to_string(),
            None,
            Some(0.02),
            Some(serde_json::json!({
                "tiers": [{"up_to": null, "input_price_per_1m": 3.0, "output_price_per_1m": 15.0}]
            })),
            Some("model-openai-usage-local-1".to_string()),
            Some("gpt-5".to_string()),
            None,
            None,
            None,
        )
        .expect("relay billing context should build"),
        StoredBillingModelContext::new(
            "provider-openai-relay-retry-backup".to_string(),
            Some("pay_as_you_go".to_string()),
            Some("key-openai-relay-stream-retry-backup".to_string()),
            Some(serde_json::json!({"openai:chat": 1.0})),
            Some(60),
            "global-model-openai-usage-local-1".to_string(),
            "gpt-5".to_string(),
            None,
            Some(0.02),
            Some(serde_json::json!({
                "tiers": [{"up_to": null, "input_price_per_1m": 3.0, "output_price_per_1m": 15.0}]
            })),
            Some("model-openai-relay-stream-retry-backup".to_string()),
            Some("gpt-5-upstream-stream-backup".to_string()),
            None,
            None,
            None,
        )
        .expect("relay stream retry backup billing context should build"),
    ]));
    let wallet_repository = Arc::new(InMemoryWalletRepository::seed(vec![
        StoredWalletSnapshot::new(
            "wallet-relay-client".to_string(),
            Some("relay-client-user".to_string()),
            None,
            10.0,
            0.0,
            "finite".to_string(),
            "USD".to_string(),
            "active".to_string(),
            0.0,
            0.0,
            0.0,
            0.0,
            100,
        )
        .expect("relay test wallet should build"),
    ]));
    let data_state = crate::data::GatewayDataState::with_auth_candidate_selection_provider_catalog_request_candidates_usage_billing_and_wallet_for_tests(
        auth_repository,
        candidate_selection_repository,
        provider_catalog_repository,
        request_candidate_repository,
        usage_repository,
        billing_repository,
        Arc::clone(&wallet_repository),
        DEVELOPMENT_ENCRYPTION_KEY,
    )
    .with_routing_group_repository_for_tests(relay_routing_group_repository())
    .with_settlement_writer_for_tests(Arc::new(
        InMemorySettlementRepository::from_wallet_repository(Arc::clone(&wallet_repository)),
    ));
    let mut state = AppState::new()
        .expect("gateway state should build")
        .with_data_state_for_tests(data_state)
        .with_usage_runtime_for_tests(UsageRuntimeConfig {
            enabled: true,
            ..UsageRuntimeConfig::default()
        });
    if let Some(store) = integration_config_store {
        state = state.with_relay_integration_config_store_for_tests(store);
    }
    let mut config = crate::relay::RelayEngineConfig::default();
    config.enabled = true;
    state.configure_relay_engine_with_config_and_verifier(
        config,
        crate::relay::collaboration::RelayVerifier::new(
            "relay-secret".to_string(),
            "aether-primary".to_string(),
            aether_runtime_state::RuntimeState::memory(Default::default()),
        ),
    );
    (state, wallet_repository)
}

async fn relay_test_state(
    auth_repository: Arc<InMemoryAuthApiKeySnapshotRepository>,
    candidate_selection_repository: Arc<InMemoryMinimalCandidateSelectionReadRepository>,
    provider_catalog_repository: Arc<InMemoryProviderCatalogReadRepository>,
    request_candidate_repository: Arc<InMemoryRequestCandidateRepository>,
    usage_repository: Arc<InMemoryUsageReadRepository>,
) -> (AppState, Arc<InMemoryWalletRepository>) {
    relay_test_state_with_persisted_integration_config(
        auth_repository,
        candidate_selection_repository,
        provider_catalog_repository,
        request_candidate_repository,
        usage_repository,
        true,
    )
    .await
}

async fn wait_for_completed_usage(
    repository: &InMemoryUsageReadRepository,
    request_id: &str,
) -> StoredRequestUsageAudit {
    for _ in 0..100 {
        if let Some(usage) = repository
            .find_by_request_id(request_id)
            .await
            .expect("usage should read")
        {
            if usage.status == "completed" {
                return usage;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("usage {request_id} should complete");
}

#[test]
fn signed_relay_direct_channel_executes_real_provider_catalog_without_leaking_relay_credentials() {
    run_relay_proxy_test(
        "signed_relay_direct_channel_executes_real_provider_catalog_without_leaking_relay_credentials",
        signed_relay_direct_channel_executes_real_provider_catalog_without_leaking_relay_credentials_inner,
    );
}

async fn signed_relay_direct_channel_executes_real_provider_catalog_without_leaking_relay_credentials_inner(
) {
    let provider_hits = Arc::new(AtomicUsize::new(0));
    let provider_hits_clone = Arc::clone(&provider_hits);
    let seen_provider_request = Arc::new(Mutex::new(None::<SeenProviderRequest>));
    let seen_provider_request_clone = Arc::clone(&seen_provider_request);
    let provider = Router::new().route(
        "/chat/completions",
        any(move |request: Request| {
            let provider_hits = Arc::clone(&provider_hits_clone);
            let seen_provider_request = Arc::clone(&seen_provider_request_clone);
            async move {
                let (parts, body) = request.into_parts();
                let payload: serde_json::Value = serde_json::from_slice(
                    &to_bytes(body, usize::MAX)
                        .await
                        .expect("provider body should read"),
                )
                .expect("provider body should parse");
                provider_hits.fetch_add(1, Ordering::SeqCst);
                *seen_provider_request.lock().expect("mutex should lock") =
                    Some(SeenProviderRequest {
                        authorization: parts
                            .headers
                            .get(http::header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string(),
                        relay_headers_present: [
                            crate::relay::collaboration::HEADER_INSTANCE_ID,
                            crate::relay::collaboration::HEADER_RELAY_CONTEXT,
                            crate::relay::collaboration::HEADER_RELAY_SIGNATURE,
                        ]
                        .iter()
                        .any(|name| parts.headers.contains_key(*name)),
                        request_id: parts
                            .headers
                            .get(crate::relay::collaboration::HEADER_ONEAPI_REQUEST_ID)
                            .and_then(|value| value.to_str().ok())
                            .map(ToOwned::to_owned),
                        model: payload["model"].as_str().unwrap_or_default().to_string(),
                        stream: payload["stream"].as_bool().unwrap_or(false),
                    });
                Json(serde_json::json!({
                    "id": "chatcmpl-relay-direct-123",
                    "object": "chat.completion",
                    "model": "gpt-5-upstream",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "relay direct"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5}
                }))
            }
        }),
    );
    let (provider_url, provider_handle) = start_server(provider).await;

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-relay-client-valid")),
        super::super::super::usage::sample_local_openai_auth_snapshot(
            "relay-client-api-key",
            "relay-client-user",
        ),
    )]));
    let candidate_selection_repository =
        Arc::new(InMemoryMinimalCandidateSelectionReadRepository::seed(vec![
            super::super::super::usage::sample_local_openai_candidate_row(),
        ]));
    let mut endpoint = super::super::super::usage::sample_local_openai_endpoint();
    endpoint.base_url = provider_url;
    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![super::super::super::usage::sample_local_openai_provider()],
        vec![endpoint],
        vec![super::super::super::usage::sample_local_openai_key()],
    ));
    let request_candidate_repository = Arc::new(InMemoryRequestCandidateRepository::default());
    let usage_repository = Arc::new(InMemoryUsageReadRepository::default());
    let (gateway_state, wallet_repository) = relay_test_state(
        auth_repository,
        candidate_selection_repository,
        provider_catalog_repository,
        Arc::clone(&request_candidate_repository),
        Arc::clone(&usage_repository),
    )
    .await;
    let integration_config_store = gateway_state
        .relay_integration_config_store()
        .expect("relay test state should expose the persisted integration config store");
    integration_config_store
        .compare_and_set(
            "aether-primary",
            1,
            &relay_integration_config_update("balanced", true, "direct_channel", 1_784_073_600_000),
        )
        .await
        .expect("persisted relay route profile should update");
    let gateway = build_router_with_state(gateway_state);
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let client = reqwest::Client::new();
    let mut rejected = client
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(
            http::header::AUTHORIZATION,
            "Bearer sk-relay-client-invalid",
        );
    for (name, value) in relay_headers("relay-secret", "newapi-relay-direct-auth-denied") {
        rejected = rejected.header(name, value);
    }
    let rejected = rejected
        .body(r#"{"model":"gpt-5","messages":[]}"#)
        .send()
        .await
        .expect("rejected relay request should complete");
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(provider_hits.load(Ordering::SeqCst), 0);

    let request_id = "newapi-relay-direct-provider-123";
    let mut request = client
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, "Bearer sk-relay-client-valid")
        .header(crate::constants::TRACE_ID_HEADER, "forged-client-trace")
        .header(crate::routing::ROUTING_GROUP_HEADER, "forged-client-group");
    for (name, value) in relay_headers("relay-secret", request_id) {
        request = request.header(name, value);
    }
    let response = request
        .body(r#"{"model":"gpt-5","messages":[]}"#)
        .send()
        .await
        .expect("relay request should complete");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(crate::constants::TRACE_ID_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(request_id)
    );
    assert_eq!(
        response
            .headers()
            .get(crate::constants::CONTROL_REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(request_id)
    );
    assert_eq!(
        response
            .headers()
            .get(crate::constants::EXECUTION_PATH_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(crate::constants::EXECUTION_PATH_EXECUTION_RUNTIME_SYNC)
    );
    let payload: serde_json::Value = response.json().await.expect("response should parse");
    assert_eq!(payload["id"], "chatcmpl-relay-direct-123");

    assert_eq!(provider_hits.load(Ordering::SeqCst), 1);
    let seen = seen_provider_request
        .lock()
        .expect("mutex should lock")
        .clone()
        .expect("provider request should be captured");
    assert_eq!(seen.authorization, "Bearer sk-upstream-openai");
    assert!(!seen.relay_headers_present);
    assert_eq!(seen.request_id.as_deref(), Some(request_id));
    assert_eq!(seen.model, "gpt-5-upstream");
    assert!(!seen.stream);

    let candidates = request_candidate_repository
        .list_by_request_id(request_id)
        .await
        .expect("request candidates should read");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].request_id, request_id);
    assert_eq!(candidates[0].status, RequestCandidateStatus::Success);
    assert_eq!(
        candidates[0]
            .extra_data
            .as_ref()
            .and_then(|value| value.get("routing_trace"))
            .and_then(|value| value.get("group_id")),
        Some(&serde_json::json!("balanced"))
    );
    assert_eq!(
        candidates[0]
            .extra_data
            .as_ref()
            .and_then(|value| value.get("routing_trace"))
            .and_then(|value| value.get("selection_source")),
        Some(&serde_json::json!("trusted_relay_config"))
    );

    let usage = wait_for_completed_usage(usage_repository.as_ref(), request_id).await;
    assert_eq!(usage.request_id, request_id);
    assert_eq!(usage.api_key_id.as_deref(), Some("relay-client-api-key"));
    assert_eq!(usage.status, "completed");
    assert_relay_usage_metadata(&usage, request_id);
    let mut settled_wallet = None;
    for _ in 0..100 {
        settled_wallet = wallet_repository
            .find(WalletLookupKey::UserId("relay-client-user"))
            .await
            .expect("relay wallet should read");
        if settled_wallet
            .as_ref()
            .is_some_and(|wallet| wallet.total_consumed > 0.0)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        settled_wallet
            .as_ref()
            .is_some_and(|wallet| wallet.total_consumed > 0.0),
        "usage settlement for {request_id} should debit the relay client's wallet"
    );

    integration_config_store
        .compare_and_set(
            "aether-primary",
            2,
            &relay_integration_config_update(
                "balanced",
                false,
                "direct_channel",
                1_784_073_600_001,
            ),
        )
        .await
        .expect("disabled integration config should persist");
    let mut disabled_request = client
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, "Bearer sk-relay-client-valid");
    for (name, value) in relay_headers("relay-secret", "newapi-relay-config-disabled") {
        disabled_request = disabled_request.header(name, value);
    }
    let disabled_response = disabled_request
        .body(r#"{"model":"gpt-5","messages":[]}"#)
        .send()
        .await
        .expect("disabled relay request should complete");
    assert_eq!(disabled_response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(provider_hits.load(Ordering::SeqCst), 1);

    integration_config_store
        .compare_and_set(
            "aether-primary",
            3,
            &relay_integration_config_update("balanced", true, "disabled", 1_784_073_600_002),
        )
        .await
        .expect("disabled execution-mode config should persist");
    let mut disabled_mode_request = client
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, "Bearer sk-relay-client-valid");
    for (name, value) in relay_headers("relay-secret", "newapi-relay-execution-mode-disabled") {
        disabled_mode_request = disabled_mode_request.header(name, value);
    }
    let disabled_mode_response = disabled_mode_request
        .body(r#"{"model":"gpt-5","messages":[]}"#)
        .send()
        .await
        .expect("disabled-mode relay request should complete");
    assert_eq!(
        disabled_mode_response.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(provider_hits.load(Ordering::SeqCst), 1);

    integration_config_store
        .compare_and_set(
            "aether-primary",
            4,
            &relay_integration_config_update(
                "missing-profile",
                true,
                "direct_channel",
                1_784_073_600_003,
            ),
        )
        .await
        .expect("missing route profile config should persist");
    let mut unavailable_profile_request = client
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, "Bearer sk-relay-client-valid");
    for (name, value) in relay_headers("relay-secret", "newapi-relay-missing-profile") {
        unavailable_profile_request = unavailable_profile_request.header(name, value);
    }
    let unavailable_profile_response = unavailable_profile_request
        .body(r#"{"model":"gpt-5","messages":[]}"#)
        .send()
        .await
        .expect("missing-profile relay request should complete");
    assert_eq!(unavailable_profile_response.status(), StatusCode::FORBIDDEN);
    assert_eq!(provider_hits.load(Ordering::SeqCst), 1);

    integration_config_store
        .compare_and_set(
            "aether-primary",
            5,
            &relay_integration_config_update(
                "rewrite-model",
                true,
                "direct_channel",
                1_784_073_600_004,
            ),
        )
        .await
        .expect("signed-model rewrite config should persist");
    let mut rewritten_model_request = client
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, "Bearer sk-relay-client-valid");
    for (name, value) in relay_headers("relay-secret", "newapi-relay-model-rewrite") {
        rewritten_model_request = rewritten_model_request.header(name, value);
    }
    let rewritten_model_response = rewritten_model_request
        .body(r#"{"model":"gpt-5","messages":[]}"#)
        .send()
        .await
        .expect("signed-model rewrite relay request should complete");
    assert_eq!(rewritten_model_response.status(), StatusCode::UNAUTHORIZED);
    let rewritten_model_payload: serde_json::Value = rewritten_model_response
        .json()
        .await
        .expect("signed-model rewrite response should be JSON");
    assert_eq!(
        rewritten_model_payload["error"]["message"],
        "invalid Aether relay context binding"
    );
    assert_eq!(provider_hits.load(Ordering::SeqCst), 1);

    let ordinary_request_id = "ordinary-api-key-request-without-relay";
    let ordinary_response = client
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, "Bearer sk-relay-client-valid")
        .header(crate::constants::TRACE_ID_HEADER, ordinary_request_id)
        .body(r#"{"model":"gpt-5","messages":[]}"#)
        .send()
        .await
        .expect("ordinary request should complete");
    assert_eq!(ordinary_response.status(), StatusCode::OK);
    let ordinary_usage =
        wait_for_completed_usage(usage_repository.as_ref(), ordinary_request_id).await;
    assert_usage_has_no_relay_metadata(&ordinary_usage);

    gateway_handle.abort();
    provider_handle.abort();
}

#[tokio::test]
async fn signed_relay_without_persisted_integration_config_fails_closed_before_provider_execution()
{
    let provider_hits = Arc::new(AtomicUsize::new(0));
    let provider_hits_clone = Arc::clone(&provider_hits);
    let provider = Router::new().route(
        "/chat/completions",
        any(move || {
            let provider_hits = Arc::clone(&provider_hits_clone);
            async move {
                provider_hits.fetch_add(1, Ordering::SeqCst);
                Json(serde_json::json!({
                    "id": "chatcmpl-relay-missing-config",
                    "object": "chat.completion",
                    "model": "gpt-5-upstream",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "should not execute"},
                        "finish_reason": "stop"
                    }]
                }))
            }
        }),
    );
    let (provider_url, provider_handle) = start_server(provider).await;

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-relay-missing-config")),
        super::super::super::usage::sample_local_openai_auth_snapshot(
            "relay-missing-config-api-key",
            "relay-client-user",
        ),
    )]));
    let candidate_selection_repository =
        Arc::new(InMemoryMinimalCandidateSelectionReadRepository::seed(vec![
            super::super::super::usage::sample_local_openai_candidate_row(),
        ]));
    let mut endpoint = super::super::super::usage::sample_local_openai_endpoint();
    endpoint.base_url = provider_url;
    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![super::super::super::usage::sample_local_openai_provider()],
        vec![endpoint],
        vec![super::super::super::usage::sample_local_openai_key()],
    ));
    let request_candidate_repository = Arc::new(InMemoryRequestCandidateRepository::default());
    let usage_repository = Arc::new(InMemoryUsageReadRepository::default());
    let (gateway_state, _) = relay_test_state_with_persisted_integration_config(
        auth_repository,
        candidate_selection_repository,
        provider_catalog_repository,
        request_candidate_repository,
        usage_repository,
        false,
    )
    .await;
    let gateway = build_router_with_state(gateway_state);
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let mut request = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(
            http::header::AUTHORIZATION,
            "Bearer sk-relay-missing-config",
        );
    for (name, value) in relay_headers("relay-secret", "newapi-relay-missing-config") {
        request = request.header(name, value);
    }
    let response = request
        .body(r#"{"model":"gpt-5","messages":[]}"#)
        .send()
        .await
        .expect("missing-config relay request should complete");

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(provider_hits.load(Ordering::SeqCst), 0);

    gateway_handle.abort();
    provider_handle.abort();
}

#[test]
fn signed_relay_direct_channel_streams_real_provider_sse_once() {
    run_relay_proxy_test(
        "signed_relay_direct_channel_streams_real_provider_sse_once",
        signed_relay_direct_channel_streams_real_provider_sse_once_inner,
    );
}

async fn signed_relay_direct_channel_streams_real_provider_sse_once_inner() {
    let provider_hits = Arc::new(AtomicUsize::new(0));
    let provider_hits_clone = Arc::clone(&provider_hits);
    let seen_provider_request = Arc::new(Mutex::new(None::<SeenProviderRequest>));
    let seen_provider_request_clone = Arc::clone(&seen_provider_request);
    let provider = Router::new().route(
        "/chat/completions",
        any(move |request: Request| {
            let provider_hits = Arc::clone(&provider_hits_clone);
            let seen_provider_request = Arc::clone(&seen_provider_request_clone);
            async move {
                let (parts, body) = request.into_parts();
                let payload: serde_json::Value = serde_json::from_slice(
                    &to_bytes(body, usize::MAX)
                        .await
                        .expect("provider body should read"),
                )
                .expect("provider body should parse");
                provider_hits.fetch_add(1, Ordering::SeqCst);
                *seen_provider_request.lock().expect("mutex should lock") =
                    Some(SeenProviderRequest {
                        authorization: parts
                            .headers
                            .get(http::header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string(),
                        relay_headers_present: [
                            crate::relay::collaboration::HEADER_INSTANCE_ID,
                            crate::relay::collaboration::HEADER_RELAY_CONTEXT,
                            crate::relay::collaboration::HEADER_RELAY_SIGNATURE,
                        ]
                        .iter()
                        .any(|name| parts.headers.contains_key(*name)),
                        request_id: parts
                            .headers
                            .get(crate::relay::collaboration::HEADER_ONEAPI_REQUEST_ID)
                            .and_then(|value| value.to_str().ok())
                            .map(ToOwned::to_owned),
                        model: payload["model"].as_str().unwrap_or_default().to_string(),
                        stream: payload["stream"].as_bool().unwrap_or(false),
                    });
                let mut response = Response::new(Body::from(
                    "data: {\"id\":\"chatcmpl-relay-stream-123\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-5-upstream\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"relay stream\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n",
                ));
                response.headers_mut().insert(
                    http::header::CONTENT_TYPE,
                    HeaderValue::from_static("text/event-stream"),
                );
                response
            }
        }),
    );
    let (provider_url, provider_handle) = start_server(provider).await;

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-relay-stream-client")),
        super::super::super::usage::sample_local_openai_auth_snapshot(
            "relay-stream-api-key",
            "relay-client-user",
        ),
    )]));
    let candidate_selection_repository =
        Arc::new(InMemoryMinimalCandidateSelectionReadRepository::seed(vec![
            super::super::super::usage::sample_local_openai_candidate_row(),
        ]));
    let mut endpoint = super::super::super::usage::sample_local_openai_endpoint();
    endpoint.base_url = provider_url;
    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![super::super::super::usage::sample_local_openai_provider()],
        vec![endpoint],
        vec![super::super::super::usage::sample_local_openai_key()],
    ));
    let request_candidate_repository = Arc::new(InMemoryRequestCandidateRepository::default());
    let usage_repository = Arc::new(InMemoryUsageReadRepository::default());
    let (gateway_state, _) = relay_test_state(
        auth_repository,
        candidate_selection_repository,
        provider_catalog_repository,
        Arc::clone(&request_candidate_repository),
        Arc::clone(&usage_repository),
    )
    .await;
    let gateway = build_router_with_state(gateway_state);
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let request_id = "newapi-relay-stream-provider-123";
    let mut request = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, "Bearer sk-relay-stream-client")
        .header(crate::constants::TRACE_ID_HEADER, "forged-stream-trace");
    for (name, value) in relay_headers("relay-secret", request_id) {
        request = request.header(name, value);
    }
    let response = request
        .body(r#"{"model":"gpt-5","messages":[],"stream":true}"#)
        .send()
        .await
        .expect("relay stream request should complete");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(
        response
            .headers()
            .get(crate::constants::TRACE_ID_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(request_id)
    );
    assert_eq!(
        response
            .headers()
            .get(crate::constants::CONTROL_REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(request_id)
    );
    assert_eq!(
        response
            .headers()
            .get(crate::constants::EXECUTION_PATH_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(crate::constants::EXECUTION_PATH_EXECUTION_RUNTIME_STREAM)
    );
    let response_text = response.text().await.expect("stream response should read");
    assert_eq!(response_text.matches("relay stream").count(), 1);
    assert_eq!(response_text.matches("data: [DONE]").count(), 1);

    assert_eq!(provider_hits.load(Ordering::SeqCst), 1);
    let seen = seen_provider_request
        .lock()
        .expect("mutex should lock")
        .clone()
        .expect("provider stream request should be captured");
    assert_eq!(seen.authorization, "Bearer sk-upstream-openai");
    assert!(!seen.relay_headers_present);
    assert_eq!(seen.request_id.as_deref(), Some(request_id));
    assert_eq!(seen.model, "gpt-5-upstream");
    assert!(seen.stream);

    let candidates = request_candidate_repository
        .list_by_request_id(request_id)
        .await
        .expect("stream request candidates should read");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].request_id, request_id);
    assert_eq!(candidates[0].status, RequestCandidateStatus::Success);
    let usage = wait_for_completed_usage(usage_repository.as_ref(), request_id).await;
    assert_eq!(usage.request_id, request_id);
    assert_eq!(usage.status, "completed");

    gateway_handle.abort();
    provider_handle.abort();
}

#[test]
fn signed_relay_direct_channel_retries_next_real_provider_candidate_once() {
    run_relay_proxy_test(
        "signed_relay_direct_channel_retries_next_real_provider_candidate_once",
        signed_relay_direct_channel_retries_next_real_provider_candidate_once_inner,
    );
}

async fn signed_relay_direct_channel_retries_next_real_provider_candidate_once_inner() {
    let provider_hits = Arc::new(AtomicUsize::new(0));
    let provider_hits_clone = Arc::clone(&provider_hits);
    let seen_provider_requests = Arc::new(Mutex::new(Vec::<SeenProviderRequest>::new()));
    let seen_provider_requests_clone = Arc::clone(&seen_provider_requests);
    let provider = Router::new().route(
        "/chat/completions",
        any(move |request: Request| {
            let provider_hits = Arc::clone(&provider_hits_clone);
            let seen_provider_requests = Arc::clone(&seen_provider_requests_clone);
            async move {
                let (parts, body) = request.into_parts();
                let payload: serde_json::Value = serde_json::from_slice(
                    &to_bytes(body, usize::MAX)
                        .await
                        .expect("provider body should read"),
                )
                .expect("provider body should parse");
                let attempt = provider_hits.fetch_add(1, Ordering::SeqCst) + 1;
                seen_provider_requests
                    .lock()
                    .expect("mutex should lock")
                    .push(SeenProviderRequest {
                        authorization: parts
                            .headers
                            .get(http::header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string(),
                        relay_headers_present: [
                            crate::relay::collaboration::HEADER_INSTANCE_ID,
                            crate::relay::collaboration::HEADER_RELAY_CONTEXT,
                            crate::relay::collaboration::HEADER_RELAY_SIGNATURE,
                        ]
                        .iter()
                        .any(|name| parts.headers.contains_key(*name)),
                        request_id: parts
                            .headers
                            .get(crate::relay::collaboration::HEADER_ONEAPI_REQUEST_ID)
                            .and_then(|value| value.to_str().ok())
                            .map(ToOwned::to_owned),
                        model: payload["model"].as_str().unwrap_or_default().to_string(),
                        stream: payload["stream"].as_bool().unwrap_or(false),
                    });

                if attempt == 1 {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(serde_json::json!({
                            "error": {"message": "relay primary credential rejected"}
                        })),
                    )
                        .into_response();
                }

                Json(serde_json::json!({
                    "id": "chatcmpl-relay-retry-123",
                    "object": "chat.completion",
                    "model": "gpt-5-upstream-backup",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "relay retry success"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 2, "completion_tokens": 4, "total_tokens": 6}
                }))
                .into_response()
            }
        }),
    );
    let (provider_url, provider_handle) = start_server(provider).await;

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-relay-retry-client")),
        super::super::super::usage::sample_local_openai_auth_snapshot(
            "relay-retry-api-key",
            "relay-client-user",
        ),
    )]));
    let primary_candidate = super::super::super::usage::sample_local_openai_candidate_row();
    let mut backup_candidate = primary_candidate.clone();
    backup_candidate.provider_id = "provider-openai-relay-retry-backup".to_string();
    backup_candidate.provider_name = "openai".to_string();
    backup_candidate.provider_priority = 20;
    backup_candidate.endpoint_id = "endpoint-openai-relay-retry-backup".to_string();
    backup_candidate.key_id = "key-openai-relay-retry-backup".to_string();
    backup_candidate.key_name = "backup".to_string();
    backup_candidate.key_internal_priority = 10;
    backup_candidate.key_global_priority_by_format = Some(serde_json::json!({"openai:chat": 2}));
    backup_candidate.model_id = "model-openai-relay-retry-backup".to_string();
    backup_candidate.model_provider_model_name = "gpt-5-upstream-backup".to_string();
    if let Some(mappings) = backup_candidate.model_provider_model_mappings.as_mut() {
        mappings[0].name = "gpt-5-upstream-backup".to_string();
    }
    let candidate_selection_repository =
        Arc::new(InMemoryMinimalCandidateSelectionReadRepository::seed(vec![
            primary_candidate,
            backup_candidate,
        ]));

    let primary_provider = super::super::super::usage::sample_local_openai_provider();
    let mut primary_endpoint = super::super::super::usage::sample_local_openai_endpoint();
    primary_endpoint.base_url = provider_url.clone();
    let primary_key = super::super::super::usage::sample_local_openai_key();
    let mut backup_provider = primary_provider.clone();
    backup_provider.id = "provider-openai-relay-retry-backup".to_string();
    backup_provider.name = "openai".to_string();
    let mut backup_endpoint = primary_endpoint.clone();
    backup_endpoint.id = "endpoint-openai-relay-retry-backup".to_string();
    backup_endpoint.provider_id = backup_provider.id.clone();
    let mut backup_key = primary_key.clone();
    backup_key.id = "key-openai-relay-retry-backup".to_string();
    backup_key.provider_id = backup_provider.id.clone();
    backup_key.name = "backup".to_string();
    backup_key.internal_priority = 10;
    backup_key.global_priority_by_format = Some(serde_json::json!({"openai:chat": 2}));
    backup_key.encrypted_api_key = Some(
        encrypt_python_fernet_plaintext(DEVELOPMENT_ENCRYPTION_KEY, "sk-upstream-relay-backup")
            .expect("backup provider key should encrypt"),
    );
    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![primary_provider, backup_provider],
        vec![primary_endpoint, backup_endpoint],
        vec![primary_key, backup_key],
    ));
    let request_candidate_repository = Arc::new(InMemoryRequestCandidateRepository::default());
    let usage_repository = Arc::new(InMemoryUsageReadRepository::default());
    let (gateway_state, _) = relay_test_state(
        auth_repository,
        candidate_selection_repository,
        provider_catalog_repository,
        Arc::clone(&request_candidate_repository),
        Arc::clone(&usage_repository),
    )
    .await;
    let gateway = build_router_with_state(gateway_state);
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let request_id = "newapi-relay-retry-provider-123";
    let mut request = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, "Bearer sk-relay-retry-client")
        .header(crate::constants::TRACE_ID_HEADER, "forged-retry-trace");
    for (name, value) in relay_headers("relay-secret", request_id) {
        request = request.header(name, value);
    }
    let response = request
        .body(r#"{"model":"gpt-5","messages":[]}"#)
        .send()
        .await
        .expect("relay retry request should complete");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(crate::constants::TRACE_ID_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(request_id)
    );
    assert_eq!(
        response
            .headers()
            .get(crate::constants::CONTROL_REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(request_id)
    );
    let payload: serde_json::Value = response.json().await.expect("response should parse");
    assert_eq!(payload["model"], "gpt-5-upstream-backup");

    assert_eq!(provider_hits.load(Ordering::SeqCst), 2);
    let seen = seen_provider_requests
        .lock()
        .expect("mutex should lock")
        .clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].authorization, "Bearer sk-upstream-openai");
    assert_eq!(seen[1].authorization, "Bearer sk-upstream-relay-backup");
    assert!(seen.iter().all(|request| !request.relay_headers_present));
    assert_eq!(seen[0].model, "gpt-5-upstream");
    assert_eq!(seen[1].model, "gpt-5-upstream-backup");
    assert!(seen.iter().all(|request| !request.stream));

    let candidates = request_candidate_repository
        .list_by_request_id(request_id)
        .await
        .expect("retry request candidates should read");
    assert_eq!(candidates.len(), 2);
    assert!(candidates
        .iter()
        .all(|candidate| candidate.request_id == request_id));
    assert_eq!(candidates[0].candidate_index, 0);
    assert_eq!(candidates[0].status, RequestCandidateStatus::Failed);
    assert_eq!(candidates[0].status_code, Some(401));
    assert_eq!(
        candidates[0].error_message.as_deref(),
        Some("relay primary credential rejected")
    );
    assert_eq!(candidates[1].candidate_index, 1);
    assert_eq!(candidates[1].status, RequestCandidateStatus::Success);
    assert_eq!(candidates[1].status_code, Some(200));
    assert_eq!(
        candidates[1].provider_id.as_deref(),
        Some("provider-openai-relay-retry-backup")
    );

    let usage = wait_for_completed_usage(usage_repository.as_ref(), request_id).await;
    assert_eq!(usage.request_id, request_id);
    assert_eq!(usage.status, "completed");
    assert_eq!(
        usage.provider_id.as_deref(),
        Some("provider-openai-relay-retry-backup")
    );

    gateway_handle.abort();
    provider_handle.abort();
}

#[test]
fn signed_relay_direct_channel_stream_retry_preserves_correlation_and_settles_once() {
    run_relay_proxy_test(
        "signed_relay_direct_channel_stream_retry_preserves_correlation_and_settles_once",
        signed_relay_direct_channel_stream_retry_preserves_correlation_and_settles_once_inner,
    );
}

async fn signed_relay_direct_channel_stream_retry_preserves_correlation_and_settles_once_inner() {
    let provider_hits = Arc::new(AtomicUsize::new(0));
    let provider_hits_clone = Arc::clone(&provider_hits);
    let successful_generations = Arc::new(AtomicUsize::new(0));
    let successful_generations_clone = Arc::clone(&successful_generations);
    let seen_provider_requests = Arc::new(Mutex::new(Vec::<SeenProviderRequest>::new()));
    let seen_provider_requests_clone = Arc::clone(&seen_provider_requests);
    let provider = Router::new().route(
        "/chat/completions",
        any(move |request: Request| {
            let provider_hits = Arc::clone(&provider_hits_clone);
            let successful_generations = Arc::clone(&successful_generations_clone);
            let seen_provider_requests = Arc::clone(&seen_provider_requests_clone);
            async move {
                let (parts, body) = request.into_parts();
                let payload: serde_json::Value = serde_json::from_slice(
                    &to_bytes(body, usize::MAX)
                        .await
                        .expect("provider body should read"),
                )
                .expect("provider body should parse");
                let attempt = provider_hits.fetch_add(1, Ordering::SeqCst) + 1;
                seen_provider_requests
                    .lock()
                    .expect("mutex should lock")
                    .push(SeenProviderRequest {
                        authorization: parts
                            .headers
                            .get(http::header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_string(),
                        relay_headers_present: [
                            crate::relay::collaboration::HEADER_INSTANCE_ID,
                            crate::relay::collaboration::HEADER_RELAY_CONTEXT,
                            crate::relay::collaboration::HEADER_RELAY_SIGNATURE,
                        ]
                        .iter()
                        .any(|name| parts.headers.contains_key(*name)),
                        request_id: parts
                            .headers
                            .get(crate::relay::collaboration::HEADER_ONEAPI_REQUEST_ID)
                            .and_then(|value| value.to_str().ok())
                            .map(ToOwned::to_owned),
                        model: payload["model"].as_str().unwrap_or_default().to_string(),
                        stream: payload["stream"].as_bool().unwrap_or(false),
                    });

                if attempt == 1 {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(serde_json::json!({
                            "error": {"message": "relay stream primary credential rejected"}
                        })),
                    )
                        .into_response();
                }

                successful_generations.fetch_add(1, Ordering::SeqCst);
                let mut response = Response::new(Body::from(
                    "data: {\"id\":\"chatcmpl-relay-stream-retry-123\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-5-upstream-stream-backup\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"relay stream retry success\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"chatcmpl-relay-stream-retry-123\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-5-upstream-stream-backup\",\"choices\":[],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":4,\"total_tokens\":6}}\n\ndata: [DONE]\n\n",
                ));
                response.headers_mut().insert(
                    http::header::CONTENT_TYPE,
                    HeaderValue::from_static("text/event-stream"),
                );
                response
            }
        }),
    );
    let (provider_url, provider_handle) = start_server(provider).await;

    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-relay-stream-retry-client")),
        super::super::super::usage::sample_local_openai_auth_snapshot(
            "relay-stream-retry-api-key",
            "relay-client-user",
        ),
    )]));
    let primary_candidate = super::super::super::usage::sample_local_openai_candidate_row();
    let mut backup_candidate = primary_candidate.clone();
    backup_candidate.provider_id = "provider-openai-relay-retry-backup".to_string();
    backup_candidate.provider_name = "openai".to_string();
    backup_candidate.provider_priority = 20;
    backup_candidate.endpoint_id = "endpoint-openai-relay-stream-retry-backup".to_string();
    backup_candidate.key_id = "key-openai-relay-stream-retry-backup".to_string();
    backup_candidate.key_name = "stream-backup".to_string();
    backup_candidate.key_internal_priority = 10;
    backup_candidate.key_global_priority_by_format = Some(serde_json::json!({"openai:chat": 2}));
    backup_candidate.model_id = "model-openai-relay-stream-retry-backup".to_string();
    backup_candidate.model_provider_model_name = "gpt-5-upstream-stream-backup".to_string();
    if let Some(mappings) = backup_candidate.model_provider_model_mappings.as_mut() {
        mappings[0].name = "gpt-5-upstream-stream-backup".to_string();
    }
    let candidate_selection_repository =
        Arc::new(InMemoryMinimalCandidateSelectionReadRepository::seed(vec![
            primary_candidate,
            backup_candidate,
        ]));

    let primary_provider = super::super::super::usage::sample_local_openai_provider();
    let mut primary_endpoint = super::super::super::usage::sample_local_openai_endpoint();
    primary_endpoint.base_url = provider_url;
    let primary_key = super::super::super::usage::sample_local_openai_key();
    let mut backup_provider = primary_provider.clone();
    backup_provider.id = "provider-openai-relay-retry-backup".to_string();
    backup_provider.name = "openai".to_string();
    let mut backup_endpoint = primary_endpoint.clone();
    backup_endpoint.id = "endpoint-openai-relay-stream-retry-backup".to_string();
    backup_endpoint.provider_id = backup_provider.id.clone();
    let mut backup_key = primary_key.clone();
    backup_key.id = "key-openai-relay-stream-retry-backup".to_string();
    backup_key.provider_id = backup_provider.id.clone();
    backup_key.name = "stream-backup".to_string();
    backup_key.internal_priority = 10;
    backup_key.global_priority_by_format = Some(serde_json::json!({"openai:chat": 2}));
    backup_key.encrypted_api_key = Some(
        encrypt_python_fernet_plaintext(
            DEVELOPMENT_ENCRYPTION_KEY,
            "sk-upstream-relay-stream-backup",
        )
        .expect("backup provider key should encrypt"),
    );
    let provider_catalog_repository = Arc::new(InMemoryProviderCatalogReadRepository::seed(
        vec![primary_provider, backup_provider],
        vec![primary_endpoint, backup_endpoint],
        vec![primary_key, backup_key],
    ));
    let request_candidate_repository = Arc::new(InMemoryRequestCandidateRepository::default());
    let usage_repository = Arc::new(InMemoryUsageReadRepository::default());
    let (gateway_state, wallet_repository) = relay_test_state(
        auth_repository,
        candidate_selection_repository,
        provider_catalog_repository,
        Arc::clone(&request_candidate_repository),
        Arc::clone(&usage_repository),
    )
    .await;
    let gateway = build_router_with_state(gateway_state);
    let (gateway_url, gateway_handle) = start_server(gateway).await;

    let request_id = "newapi-relay-stream-retry-provider-123";
    let mut request = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(
            http::header::AUTHORIZATION,
            "Bearer sk-relay-stream-retry-client",
        )
        .header(
            crate::constants::TRACE_ID_HEADER,
            "forged-stream-retry-trace",
        );
    for (name, value) in relay_headers("relay-secret", request_id) {
        request = request.header(name, value);
    }
    let response = request
        .body(r#"{"model":"gpt-5","messages":[],"stream":true}"#)
        .send()
        .await
        .expect("relay stream retry request should complete");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(
        response
            .headers()
            .get(crate::constants::TRACE_ID_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(request_id)
    );
    assert_eq!(
        response
            .headers()
            .get(crate::constants::CONTROL_REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(request_id)
    );
    let response_text = response.text().await.expect("stream response should read");
    assert_eq!(
        response_text.matches("relay stream retry success").count(),
        1
    );
    assert_eq!(response_text.matches("data: [DONE]").count(), 1);

    assert_eq!(provider_hits.load(Ordering::SeqCst), 2);
    assert_eq!(successful_generations.load(Ordering::SeqCst), 1);
    let seen = seen_provider_requests
        .lock()
        .expect("mutex should lock")
        .clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].authorization, "Bearer sk-upstream-openai");
    assert_eq!(
        seen[1].authorization,
        "Bearer sk-upstream-relay-stream-backup"
    );
    assert!(seen.iter().all(|request| !request.relay_headers_present));
    assert!(seen
        .iter()
        .all(|request| request.request_id.as_deref() == Some(request_id)));
    assert_eq!(seen[0].model, "gpt-5-upstream");
    assert_eq!(seen[1].model, "gpt-5-upstream-stream-backup");
    assert!(seen.iter().all(|request| request.stream));

    let candidates = request_candidate_repository
        .list_by_request_id(request_id)
        .await
        .expect("stream retry candidates should read");
    assert_eq!(candidates.len(), 2);
    assert!(candidates
        .iter()
        .all(|candidate| candidate.request_id == request_id));
    assert_eq!(candidates[0].candidate_index, 0);
    assert_eq!(candidates[0].status, RequestCandidateStatus::Failed);
    assert_eq!(candidates[0].status_code, Some(401));
    assert_eq!(candidates[1].candidate_index, 1);
    assert_eq!(candidates[1].status, RequestCandidateStatus::Success);
    assert_eq!(candidates[1].status_code, Some(200));
    assert_eq!(
        candidates[1].key_id.as_deref(),
        Some("key-openai-relay-stream-retry-backup")
    );

    let usage = wait_for_completed_usage(usage_repository.as_ref(), request_id).await;
    assert_eq!(usage.request_id, request_id);
    assert_eq!(usage.status, "completed");
    assert_relay_usage_metadata(&usage, request_id);
    assert_eq!(
        usage.provider_api_key_id.as_deref(),
        Some("key-openai-relay-stream-retry-backup")
    );
    assert!(usage.actual_total_cost_usd > 0.0);
    let expected_debit = usage.actual_total_cost_usd;
    let mut settled_wallet = None;
    for _ in 0..100 {
        settled_wallet = wallet_repository
            .find(WalletLookupKey::UserId("relay-client-user"))
            .await
            .expect("relay stream retry wallet should read");
        if settled_wallet
            .as_ref()
            .is_some_and(|wallet| wallet.total_consumed >= expected_debit)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let settled_wallet = settled_wallet.expect("relay stream retry wallet should exist");
    assert!(
        (settled_wallet.total_consumed - expected_debit).abs() < 1e-9,
        "retry must settle exactly once for {request_id}"
    );
    assert!(
        ((settled_wallet.balance + settled_wallet.gift_balance) - (10.0 - expected_debit)).abs()
            < 1e-9,
        "retry must debit the wallet exactly once for {request_id}"
    );

    gateway_handle.abort();
    provider_handle.abort();
}

#[tokio::test]
async fn valid_relay_signature_continues_into_existing_api_key_authentication() {
    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-other")),
        sample_currently_usable_auth_snapshot("key-123", "user-123"),
    )]));
    let mut state = AppState::new()
        .expect("gateway state should build")
        .with_auth_api_key_data_reader_for_tests(auth_repository)
        .with_relay_integration_config_store_for_tests(relay_integration_config_store().await);
    let mut config = crate::relay::RelayEngineConfig::default();
    config.enabled = true;
    state.configure_relay_engine_with_config_and_verifier(
        config,
        crate::relay::collaboration::RelayVerifier::new(
            "relay-secret".to_string(),
            "aether-primary".to_string(),
            aether_runtime_state::RuntimeState::memory(Default::default()),
        ),
    );
    let gateway = build_router_with_state(state);
    let (gateway_url, gateway_handle) = start_server(gateway).await;
    let mut request = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, "Bearer sk-invalid");
    for (name, value) in relay_headers("relay-secret", "relay-auth-chain") {
        request = request.header(name, value);
    }

    let response = request
        .body(r#"{"model":"gpt-5","messages":[]}"#)
        .send()
        .await
        .expect("gateway request should complete");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let payload: serde_json::Value = response.json().await.expect("response should be JSON");
    assert_eq!(payload["error"]["message"], "无效的API密钥");
    gateway_handle.abort();
}

#[tokio::test]
async fn valid_relay_signature_promotes_signed_request_id_into_the_gateway_trace() {
    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-other")),
        sample_currently_usable_auth_snapshot("key-123", "user-123"),
    )]));
    let mut state = AppState::new()
        .expect("gateway state should build")
        .with_auth_api_key_data_reader_for_tests(auth_repository)
        .with_relay_integration_config_store_for_tests(relay_integration_config_store().await);
    let mut config = crate::relay::RelayEngineConfig::default();
    config.enabled = true;
    state.configure_relay_engine_with_config_and_verifier(
        config,
        crate::relay::collaboration::RelayVerifier::new(
            "relay-secret".to_string(),
            "aether-primary".to_string(),
            aether_runtime_state::RuntimeState::memory(Default::default()),
        ),
    );
    let gateway = build_router_with_state(state);
    let (gateway_url, gateway_handle) = start_server(gateway).await;
    let mut request = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, "Bearer sk-invalid")
        .header(crate::constants::TRACE_ID_HEADER, "forged-client-trace-123");
    for (name, value) in relay_headers("relay-secret", "newapi-request-123") {
        request = request.header(name, value);
    }

    let response = request
        .body(r#"{"model":"gpt-5","messages":[]}"#)
        .send()
        .await
        .expect("gateway request should complete");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response
            .headers()
            .get(crate::constants::TRACE_ID_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("newapi-request-123")
    );
    gateway_handle.abort();
}

#[tokio::test]
async fn invalid_relay_signature_is_rejected_before_api_key_authentication() {
    let mut state = AppState::new()
        .expect("gateway state should build")
        .with_relay_integration_config_store_for_tests(relay_integration_config_store().await);
    let mut config = crate::relay::RelayEngineConfig::default();
    config.enabled = true;
    state.configure_relay_engine_with_config_and_verifier(
        config,
        crate::relay::collaboration::RelayVerifier::new(
            "relay-secret".to_string(),
            "aether-primary".to_string(),
            aether_runtime_state::RuntimeState::memory(Default::default()),
        ),
    );
    let gateway = build_router_with_state(state);
    let (gateway_url, gateway_handle) = start_server(gateway).await;
    let mut request = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, "Bearer sk-invalid");
    for (name, value) in relay_headers("wrong-secret", "relay-invalid-signature") {
        request = request.header(name, value);
    }

    let response = request
        .body(r#"{"model":"gpt-5","messages":[]}"#)
        .send()
        .await
        .expect("gateway request should complete");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let payload: serde_json::Value = response.json().await.expect("response should be JSON");
    assert_eq!(
        payload["error"]["message"],
        "invalid Aether relay signature"
    );
    gateway_handle.abort();
}

#[tokio::test]
async fn signed_relay_model_mismatch_is_rejected_before_execution() {
    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-valid")),
        sample_currently_usable_auth_snapshot("key-123", "user-123"),
    )]));
    let mut state = AppState::new()
        .expect("gateway state should build")
        .with_auth_api_key_data_reader_for_tests(auth_repository)
        .with_relay_integration_config_store_for_tests(relay_integration_config_store().await);
    let mut config = crate::relay::RelayEngineConfig::default();
    config.enabled = true;
    state.configure_relay_engine_with_config_and_verifier(
        config,
        crate::relay::collaboration::RelayVerifier::new(
            "relay-secret".to_string(),
            "aether-primary".to_string(),
            aether_runtime_state::RuntimeState::memory(Default::default()),
        ),
    );
    let gateway = build_router_with_state(state);
    let (gateway_url, gateway_handle) = start_server(gateway).await;
    let mut request = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, "Bearer sk-valid");
    for (name, value) in
        relay_headers_with_context("relay-secret", "relay-model-mismatch", "gpt-5", "openai")
    {
        request = request.header(name, value);
    }

    let response = request
        .body(r#"{"model":"gpt-4.1","messages":[]}"#)
        .send()
        .await
        .expect("gateway request should complete");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let payload: serde_json::Value = response.json().await.expect("response should be JSON");
    assert_eq!(
        payload["error"]["message"],
        "invalid Aether relay context binding"
    );
    gateway_handle.abort();
}

#[tokio::test]
async fn signed_relay_format_mismatch_is_rejected_before_execution() {
    let auth_repository = Arc::new(InMemoryAuthApiKeySnapshotRepository::seed(vec![(
        Some(hash_api_key("sk-valid")),
        sample_currently_usable_auth_snapshot("key-123", "user-123"),
    )]));
    let mut state = AppState::new()
        .expect("gateway state should build")
        .with_auth_api_key_data_reader_for_tests(auth_repository)
        .with_relay_integration_config_store_for_tests(relay_integration_config_store().await);
    let mut config = crate::relay::RelayEngineConfig::default();
    config.enabled = true;
    state.configure_relay_engine_with_config_and_verifier(
        config,
        crate::relay::collaboration::RelayVerifier::new(
            "relay-secret".to_string(),
            "aether-primary".to_string(),
            aether_runtime_state::RuntimeState::memory(Default::default()),
        ),
    );
    let gateway = build_router_with_state(state);
    let (gateway_url, gateway_handle) = start_server(gateway).await;
    let mut request = reqwest::Client::new()
        .post(format!("{gateway_url}/v1/chat/completions"))
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, "Bearer sk-valid");
    for (name, value) in
        relay_headers_with_context("relay-secret", "relay-format-mismatch", "gpt-5", "claude")
    {
        request = request.header(name, value);
    }

    let response = request
        .body(r#"{"model":"gpt-5","messages":[]}"#)
        .send()
        .await
        .expect("gateway request should complete");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let payload: serde_json::Value = response.json().await.expect("response should be JSON");
    assert_eq!(
        payload["error"]["message"],
        "invalid Aether relay context binding"
    );
    gateway_handle.abort();
}
