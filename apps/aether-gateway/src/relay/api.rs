//! Relay 管理 API handlers
//!
//! 所有端点注册在 /api/relay/ 路径前缀下。

use aether_data::repository::relay_profit::{
    PersistedRelayProfitRecord, RelayCostConfidence, RelayProfitLedgerFilter,
};
use axum::extract::{ConnectInfo, Json, Path, Query, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{delete, get, post, put};
use axum::Router;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::info;
use uuid::Uuid;

use super::super::state::AppState;
use super::config::{
    CreateChannelInput, StoredRelayChannel, StoredRelayChannelKey, UpdateChannelInput,
};
use super::discovery::PriceSyncSummary;
use super::error::RelayError;
use super::groups::{CreateGroupInput, DownstreamGroup, DownstreamGroupManager, UpdateGroupInput};
use super::reconcile::{CreateDownstreamInput, StoredDownstreamInstance};
use crate::{
    headers::{effective_client_ip, extract_or_generate_trace_id},
    GatewayError,
};

/// 挂载 relay 管理路由到 /api/relay/*
pub(crate) fn mount_relay_routes(
    router: Router<AppState>,
    admin_auth_state: AppState,
) -> Router<AppState> {
    let relay_routes = Router::<AppState>::new()
        // Channels
        .route(
            "/api/relay/channels",
            get(list_channels).post(create_channel),
        )
        .route(
            "/api/relay/channels/{id}",
            get(get_channel).put(update_channel).delete(delete_channel),
        )
        // Pricing (markup rules)
        .route(
            "/api/relay/pricing",
            get(list_pricing_rules).post(create_pricing_rule),
        )
        .route(
            "/api/relay/pricing/{id}",
            put(update_pricing_rule).delete(delete_pricing_rule),
        )
        // Downstream instances
        .route(
            "/api/relay/downstream",
            get(list_downstream).post(create_downstream),
        )
        .route("/api/relay/downstream/{id}", delete(delete_downstream))
        // Database-backed downstream groups
        .route(
            "/api/relay/groups",
            get(list_downstream_groups).post(create_downstream_group),
        )
        .route(
            "/api/relay/groups/{id}",
            put(update_downstream_group).delete(delete_downstream_group),
        )
        // Dashboard
        .route("/api/relay/dashboard", get(get_dashboard))
        // Sync triggers
        .route("/api/relay/sync/pricing", post(trigger_price_sync))
        .route("/api/relay/sync/health", post(trigger_health_reset))
        // Settlements
        .route("/api/relay/settlements", get(list_settlements))
        // New API integration state without credential material
        .route("/api/relay/integration-status", get(get_integration_status))
        .route_layer(axum::middleware::from_fn_with_state(
            admin_auth_state,
            require_relay_admin,
        ));

    router.merge(relay_routes)
}

async fn require_relay_admin(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, GatewayError> {
    let trace_id = extract_or_generate_trace_id(request.headers());
    let control_headers = relay_admin_control_headers(request.headers());
    let client_ip = request
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map(|ConnectInfo(remote_addr)| effective_client_ip(request.headers(), remote_addr));
    let mut request_context = crate::control::resolve_public_request_context(
        &state,
        request.method(),
        request.uri(),
        &control_headers,
        &trace_id,
    )
    .await?;

    if relay_bearer_is_active_service_credential(&control_headers) {
        return Ok(
            crate::handlers::shared::build_admin_proxy_auth_required_response(&request_context),
        );
    }

    if let Some(client_ip) = client_ip {
        crate::handlers::shared::promote_management_token_admin_principal(
            &state,
            client_ip,
            &control_headers,
            &trace_id,
            &mut request_context,
        )
        .await?;
    }

    let is_admin_route = request_context
        .control_decision
        .as_ref()
        .is_some_and(|decision| {
            decision.route_class.as_deref() == Some("admin_proxy")
                && matches!(
                    decision.route_family.as_deref(),
                    Some("relay_groups_manage" | "relay_manage")
                )
        });
    let has_admin_principal = request_context
        .control_decision
        .as_ref()
        .is_some_and(|decision| decision.admin_principal.is_some());
    if !is_admin_route || !has_admin_principal {
        return Ok(
            crate::handlers::shared::build_admin_proxy_auth_required_response(&request_context),
        );
    }
    if let Some(response) =
        crate::handlers::shared::management_token_permission_denied_response(&request_context)
    {
        return Ok(response);
    }

    Ok(next.run(request).await)
}

fn relay_bearer_is_active_service_credential(headers: &axum::http::HeaderMap) -> bool {
    let Some(value) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Some(token) = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))
    else {
        return false;
    };
    let token = token.trim();
    !token.is_empty() && super::collaboration::is_active_service_credential(token)
}

fn relay_admin_control_headers(headers: &axum::http::HeaderMap) -> axum::http::HeaderMap {
    let mut sanitized = headers.clone();
    for header in [
        crate::constants::GATEWAY_HEADER,
        crate::constants::TRUSTED_ADMIN_USER_ID_HEADER,
        crate::constants::TRUSTED_ADMIN_USER_ROLE_HEADER,
        crate::constants::TRUSTED_ADMIN_SESSION_ID_HEADER,
        crate::constants::TRUSTED_ADMIN_MANAGEMENT_TOKEN_ID_HEADER,
    ] {
        sanitized.remove(header);
    }
    sanitized
}

// ==================== Response Wrapper ====================

/// 统一响应包装
#[derive(Serialize)]
struct ApiResponse<T: Serialize> {
    success: bool,
    data: T,
}

fn ok_response<T: Serialize>(data: T) -> Json<ApiResponse<T>> {
    Json(ApiResponse {
        success: true,
        data,
    })
}

#[derive(Serialize)]
struct RelayIntegrationStatus {
    configured: bool,
    relay_enabled: bool,
    config: Option<RelayIntegrationStatusConfig>,
    credentials: Option<RelayIntegrationStatusCredentials>,
}

#[derive(Serialize)]
struct RelayIntegrationStatusConfig {
    instance_id: String,
    route_profile: String,
    execution_mode: String,
    enabled: bool,
    capability_version: String,
    revision: i64,
    updated_at_unix_ms: i64,
}

#[derive(Serialize)]
struct RelayIntegrationStatusCredentials {
    credential_revision: i64,
    rotation_id: String,
    transition_expires_at_unix_ms: Option<i64>,
    transition_active: bool,
}

// ==================== Channel Handlers ====================

/// GET /api/relay/channels
async fn list_channels(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<Json<ApiResponse<Vec<StoredRelayChannel>>>, RelayError> {
    let relay = state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;
    let channel_ids = relay.config_store.get_enabled_channel_ids().await?;
    let mut channels = Vec::new();
    for id in &channel_ids {
        if let Some(ch) = relay.config_store.get_channel_from_runtime(id).await? {
            channels.push(ch);
        }
    }
    Ok(ok_response(channels))
}

/// POST /api/relay/channels
async fn create_channel(
    axum::extract::State(state): axum::extract::State<AppState>,
    Json(input): Json<CreateChannelInput>,
) -> Result<Json<ApiResponse<StoredRelayChannel>>, RelayError> {
    let relay = state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;

    // Validate input
    relay.config_store.validate_create_input(&input)?;

    // Create channel record
    let channel = StoredRelayChannel {
        id: Uuid::new_v4().to_string(),
        name: input.name,
        provider: input.provider,
        endpoint: input.endpoint,
        weight: input.weight.unwrap_or(1) as i32,
        enabled: input.enabled.unwrap_or(true),
        price_weight_override: input.price_weight_override,
        health_weight_override: input.health_weight_override,
        config_json: input.config_json,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    };

    // Build key records
    let keys: Vec<StoredRelayChannelKey> = input
        .keys
        .iter()
        .map(|k| StoredRelayChannelKey {
            id: Uuid::new_v4().to_string(),
            channel_id: channel.id.clone(),
            api_key: k.api_key.clone(),
            group_id: k.group_id.clone(),
            group_ratio: k.group_ratio.unwrap_or(1.0),
            label: k.label.clone(),
            enabled: true,
            created_at: chrono::Utc::now().to_rfc3339(),
        })
        .collect();

    // Sync channel to runtime state
    relay.config_store.sync_channel_to_runtime(&channel).await?;

    // Store keys in runtime
    let keys_key = format!("relay:channel:keys:{}", channel.id);
    let keys_json = serde_json::to_string(&keys)
        .map_err(|e| RelayError::Internal(format!("serialize keys: {}", e)))?;
    relay
        .config_store
        .runtime_state
        .kv_set(&keys_key, keys_json, None)
        .await
        .map_err(|e| RelayError::RuntimeState(e.to_string()))?;

    info!(channel_id = %channel.id, name = %channel.name, "channel created");
    Ok(ok_response(channel))
}

/// GET /api/relay/channels/:id
async fn get_channel(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<StoredRelayChannel>>, RelayError> {
    let relay = state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;
    let channel = relay
        .config_store
        .get_channel_from_runtime(&id)
        .await?
        .ok_or(RelayError::NotFound(format!("channel {}", id)))?;
    Ok(ok_response(channel))
}

/// PUT /api/relay/channels/:id
async fn update_channel(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<UpdateChannelInput>,
) -> Result<Json<ApiResponse<StoredRelayChannel>>, RelayError> {
    let relay = state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;
    let mut channel = relay
        .config_store
        .get_channel_from_runtime(&id)
        .await?
        .ok_or(RelayError::NotFound(format!("channel {}", id)))?;

    // Apply partial updates
    if let Some(name) = input.name {
        channel.name = name;
    }
    if let Some(provider) = input.provider {
        channel.provider = provider;
    }
    if let Some(endpoint) = input.endpoint {
        channel.endpoint = endpoint;
    }
    if let Some(weight) = input.weight {
        channel.weight = weight as i32;
    }
    if let Some(enabled) = input.enabled {
        channel.enabled = enabled;
    }
    if let Some(pwo) = input.price_weight_override {
        channel.price_weight_override = Some(pwo);
    }
    if let Some(hwo) = input.health_weight_override {
        channel.health_weight_override = Some(hwo);
    }
    if let Some(cfg) = input.config_json {
        channel.config_json = Some(cfg);
    }
    channel.updated_at = chrono::Utc::now().to_rfc3339();

    relay.config_store.sync_channel_to_runtime(&channel).await?;
    info!(channel_id = %id, "channel updated");
    Ok(ok_response(channel))
}

/// DELETE /api/relay/channels/:id
async fn delete_channel(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<String>>, RelayError> {
    let relay = state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;

    // Remove channel config from runtime state
    let key = format!("relay:channel:config:{}", id);
    relay
        .config_store
        .runtime_state
        .kv_delete(&key)
        .await
        .map_err(|e| RelayError::RuntimeState(e.to_string()))?;
    relay
        .config_store
        .runtime_state
        .set_remove("relay:channel:enabled", &id)
        .await
        .map_err(|e| RelayError::RuntimeState(e.to_string()))?;

    // Remove associated keys
    let keys_key = format!("relay:channel:keys:{}", id);
    let _ = relay.config_store.runtime_state.kv_delete(&keys_key).await;

    info!(channel_id = %id, "channel deleted");
    Ok(ok_response(format!("channel {} deleted", id)))
}

// ==================== Pricing Rule Handlers ====================

/// 加价规则存储记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredMarkupRule {
    pub id: String,
    pub scope_type: String,
    pub scope_value: Option<String>,
    pub strategy_type: String,
    pub strategy_params: String,
    pub priority: i32,
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// 创建加价规则输入
#[derive(Debug, Clone, Deserialize)]
pub struct CreateMarkupRuleInput {
    pub scope_type: String,
    pub scope_value: Option<String>,
    pub strategy_type: String,
    pub strategy_params: String,
    pub priority: Option<i32>,
}

/// 更新加价规则输入
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateMarkupRuleInput {
    pub scope_type: Option<String>,
    pub scope_value: Option<String>,
    pub strategy_type: Option<String>,
    pub strategy_params: Option<String>,
    pub priority: Option<i32>,
    pub enabled: Option<bool>,
}

/// GET /api/relay/pricing
async fn list_pricing_rules(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<Json<ApiResponse<Vec<StoredMarkupRule>>>, RelayError> {
    let relay = state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;
    let members = relay
        .config_store
        .runtime_state
        .set_members("relay:markup:rules")
        .await
        .map_err(|e| RelayError::RuntimeState(e.to_string()))?;

    let mut rules = Vec::new();
    for id in &members {
        let key = format!("relay:markup:{}", id);
        if let Ok(Some(value)) = relay.config_store.runtime_state.kv_get(&key).await {
            if let Ok(rule) = serde_json::from_str::<StoredMarkupRule>(&value) {
                rules.push(rule);
            }
        }
    }
    rules.sort_by(|a, b| b.priority.cmp(&a.priority));
    Ok(ok_response(rules))
}

/// POST /api/relay/pricing
async fn create_pricing_rule(
    axum::extract::State(state): axum::extract::State<AppState>,
    Json(input): Json<CreateMarkupRuleInput>,
) -> Result<Json<ApiResponse<StoredMarkupRule>>, RelayError> {
    let relay = state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;

    let rule = StoredMarkupRule {
        id: Uuid::new_v4().to_string(),
        scope_type: input.scope_type,
        scope_value: input.scope_value,
        strategy_type: input.strategy_type,
        strategy_params: input.strategy_params,
        priority: input.priority.unwrap_or(0),
        enabled: true,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    };

    let key = format!("relay:markup:{}", rule.id);
    let json = serde_json::to_string(&rule).map_err(|e| RelayError::Internal(e.to_string()))?;
    relay
        .config_store
        .runtime_state
        .kv_set(&key, json, None)
        .await
        .map_err(|e| RelayError::RuntimeState(e.to_string()))?;
    relay
        .config_store
        .runtime_state
        .set_add("relay:markup:rules", &rule.id)
        .await
        .map_err(|e| RelayError::RuntimeState(e.to_string()))?;

    info!(rule_id = %rule.id, "markup rule created");
    Ok(ok_response(rule))
}

/// PUT /api/relay/pricing/:id
async fn update_pricing_rule(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<UpdateMarkupRuleInput>,
) -> Result<Json<ApiResponse<StoredMarkupRule>>, RelayError> {
    let relay = state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;

    let key = format!("relay:markup:{}", id);
    let value = relay
        .config_store
        .runtime_state
        .kv_get(&key)
        .await
        .map_err(|e| RelayError::RuntimeState(e.to_string()))?
        .ok_or(RelayError::NotFound(format!("markup rule {}", id)))?;
    let mut rule: StoredMarkupRule =
        serde_json::from_str(&value).map_err(|e| RelayError::Internal(e.to_string()))?;

    // Apply partial updates
    if let Some(v) = input.scope_type {
        rule.scope_type = v;
    }
    if let Some(v) = input.scope_value {
        rule.scope_value = Some(v);
    }
    if let Some(v) = input.strategy_type {
        rule.strategy_type = v;
    }
    if let Some(v) = input.strategy_params {
        rule.strategy_params = v;
    }
    if let Some(v) = input.priority {
        rule.priority = v;
    }
    if let Some(v) = input.enabled {
        rule.enabled = v;
    }
    rule.updated_at = chrono::Utc::now().to_rfc3339();

    let json = serde_json::to_string(&rule).map_err(|e| RelayError::Internal(e.to_string()))?;
    relay
        .config_store
        .runtime_state
        .kv_set(&key, json, None)
        .await
        .map_err(|e| RelayError::RuntimeState(e.to_string()))?;

    Ok(ok_response(rule))
}

/// DELETE /api/relay/pricing/:id
async fn delete_pricing_rule(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<String>>, RelayError> {
    let relay = state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;

    let key = format!("relay:markup:{}", id);
    let _ = relay.config_store.runtime_state.kv_delete(&key).await;
    let _ = relay
        .config_store
        .runtime_state
        .set_remove("relay:markup:rules", &id)
        .await;

    Ok(ok_response(format!("rule {} deleted", id)))
}

// ==================== Downstream Handlers ====================

/// GET /api/relay/downstream
async fn list_downstream(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<Json<ApiResponse<Vec<StoredDownstreamInstance>>>, RelayError> {
    let relay = state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;
    let instances = relay.reconciler.get_enabled_downstream_instances().await?;
    Ok(ok_response(instances))
}

/// POST /api/relay/downstream
async fn create_downstream(
    axum::extract::State(state): axum::extract::State<AppState>,
    Json(input): Json<CreateDownstreamInput>,
) -> Result<Json<ApiResponse<StoredDownstreamInstance>>, RelayError> {
    let relay = state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;

    // Validate downstream connection
    let valid = relay
        .reconciler
        .validate_connection(&input.endpoint, &input.api_key)
        .await?;
    if !valid {
        return Err(RelayError::InvalidConfig(
            "cannot connect to downstream instance".into(),
        ));
    }

    let instance = StoredDownstreamInstance {
        id: Uuid::new_v4().to_string(),
        name: input.name,
        endpoint: input.endpoint,
        api_key: input.api_key,
        enabled: input.enabled.unwrap_or(true),
        last_sync_at: None,
        created_at: chrono::Utc::now().to_rfc3339(),
    };

    relay.reconciler.save_downstream_instance(&instance).await?;
    info!(downstream_id = %instance.id, name = %instance.name, "downstream instance created");
    Ok(ok_response(instance))
}

/// DELETE /api/relay/downstream/:id
async fn delete_downstream(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<String>>, RelayError> {
    let relay = state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;

    let key = format!("relay:downstream:{}", id);
    let _ = relay.config_store.runtime_state.kv_delete(&key).await;
    let _ = relay
        .config_store
        .runtime_state
        .set_remove("relay:downstream:enabled", &id)
        .await;

    Ok(ok_response(format!("downstream {} deleted", id)))
}

// ==================== Downstream Group Handlers ====================

fn database_backed_group_manager(state: &AppState) -> Result<DownstreamGroupManager, RelayError> {
    let relay = state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;
    let group_store = state
        .data
        .relay_downstream_group_store()
        .ok_or(RelayError::DatabaseBackedGroupExportUnavailable)?;

    Ok(DownstreamGroupManager::with_store(
        relay.config.clone(),
        (*state.runtime_state).clone(),
        group_store,
    ))
}

/// GET /api/relay/groups
async fn list_downstream_groups(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<Json<ApiResponse<Vec<DownstreamGroup>>>, RelayError> {
    let groups = database_backed_group_manager(&state)?.list_groups().await?;
    Ok(ok_response(groups))
}

/// POST /api/relay/groups
async fn create_downstream_group(
    axum::extract::State(state): axum::extract::State<AppState>,
    Json(input): Json<CreateGroupInput>,
) -> Result<Json<ApiResponse<DownstreamGroup>>, RelayError> {
    let group = database_backed_group_manager(&state)?
        .create_group(input)
        .await?;
    Ok(ok_response(group))
}

/// PUT /api/relay/groups/:id
async fn update_downstream_group(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<UpdateGroupInput>,
) -> Result<Json<ApiResponse<DownstreamGroup>>, RelayError> {
    let group = database_backed_group_manager(&state)?
        .update_group(&id, input)
        .await?;
    Ok(ok_response(group))
}

/// DELETE /api/relay/groups/:id
async fn delete_downstream_group(
    axum::extract::State(state): axum::extract::State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<String>>, RelayError> {
    database_backed_group_manager(&state)?
        .delete_group(&id)
        .await?;
    Ok(ok_response(format!("group {} deleted", id)))
}

// ==================== Dashboard ====================

/// 仪表盘数据响应
#[derive(Serialize)]
struct DashboardResponse {
    total_channels: usize,
    enabled_channels: usize,
    total_downstream: usize,
    last_price_sync: Option<PriceSyncSummary>,
}

/// GET /api/relay/dashboard
async fn get_dashboard(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<Json<ApiResponse<DashboardResponse>>, RelayError> {
    let relay = state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;

    let enabled_ids = relay.config_store.get_enabled_channel_ids().await?;
    let downstream = relay.reconciler.get_enabled_downstream_instances().await?;

    // Get last sync summary from runtime state
    let last_sync: Option<PriceSyncSummary> = match relay
        .config_store
        .runtime_state
        .kv_get("relay:meta:pricing:last_sync")
        .await
    {
        Ok(Some(v)) => serde_json::from_str(&v).ok(),
        _ => None,
    };

    Ok(ok_response(DashboardResponse {
        total_channels: enabled_ids.len(),
        enabled_channels: enabled_ids.len(),
        total_downstream: downstream.len(),
        last_price_sync: last_sync,
    }))
}

/// GET /api/relay/integration-status
async fn get_integration_status(
    State(state): State<AppState>,
) -> Result<Json<ApiResponse<RelayIntegrationStatus>>, RelayError> {
    let relay = state.relay_engine();
    let relay_enabled = relay.is_some_and(|relay| relay.is_enabled());
    let Some(instance_id) = relay.map(|relay| relay.instance_id.clone()) else {
        return Ok(ok_response(RelayIntegrationStatus {
            configured: false,
            relay_enabled,
            config: None,
            credentials: None,
        }));
    };
    let Some(store) = state.relay_integration_config_store() else {
        return Ok(ok_response(RelayIntegrationStatus {
            configured: false,
            relay_enabled,
            config: None,
            credentials: None,
        }));
    };

    let config = store.get(&instance_id).await.map_err(|error| {
        RelayError::Internal(format!("load integration status config: {error}"))
    })?;
    let credentials = store.get_credentials(&instance_id).await.map_err(|error| {
        RelayError::Internal(format!("load integration status credentials: {error}"))
    })?;
    let config = config.map(|config| RelayIntegrationStatusConfig {
        instance_id: config.instance_id,
        route_profile: config.route_profile,
        execution_mode: config.execution_mode,
        enabled: config.enabled,
        capability_version: config.capability_version,
        revision: config.revision,
        updated_at_unix_ms: config.updated_at_unix_ms,
    });
    let credentials = credentials.map(|credentials| {
        let transition_active =
            credentials
                .transition_expires_at_unix_ms
                .is_some_and(|expires_at| {
                    expires_at > Utc::now().timestamp_millis()
                        && credentials.previous_control_secret_ciphertext.is_some()
                        && credentials.previous_relay_secret_ciphertext.is_some()
                });
        RelayIntegrationStatusCredentials {
            credential_revision: credentials.credential_revision,
            rotation_id: credentials.rotation_id,
            transition_expires_at_unix_ms: credentials.transition_expires_at_unix_ms,
            transition_active,
        }
    });

    Ok(ok_response(RelayIntegrationStatus {
        configured: config.is_some() && credentials.is_some(),
        relay_enabled,
        config,
        credentials,
    }))
}

// ==================== Sync Triggers ====================

/// POST /api/relay/sync/pricing
async fn trigger_price_sync(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<Json<ApiResponse<PriceSyncSummary>>, RelayError> {
    let relay = state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;
    let summary = relay.price_discovery.run_full_sync().await?;
    Ok(ok_response(summary))
}

/// POST /api/relay/sync/health
async fn trigger_health_reset(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<Json<ApiResponse<String>>, RelayError> {
    let relay = state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;

    // Reset health data for all enabled channels
    let channel_ids = relay.config_store.get_enabled_channel_ids().await?;
    for id in &channel_ids {
        let key = format!("relay:health:{}", id);
        let _ = relay.config_store.runtime_state.kv_delete(&key).await;
    }

    Ok(ok_response("health data reset".to_string()))
}

// ==================== Settlements ====================

/// 对账记录查询参数
#[derive(Deserialize)]
struct SettlementsQuery {
    page: Option<u64>,
    per_page: Option<u64>,
    start_time: Option<String>,
    end_time: Option<String>,
}

/// A persisted New API settlement fact enriched with AETHER's observed upstream cost.
/// Optional monetary values remain absent when the underlying ledger cannot establish them.
#[derive(Debug, Clone, Serialize)]
struct RelaySettlementExport {
    id: String,
    request_id: String,
    instance_id: String,
    channel_id: String,
    model_id: String,
    occurred_at: String,
    upstream_cost_usd: Option<f64>,
    downstream_revenue_usd: Option<f64>,
    payment_fee_usd: Option<f64>,
    net_profit_usd: Option<f64>,
    margin_percent: Option<f64>,
    cost_confidence: RelayCostConfidence,
}

/// GET /api/relay/settlements
async fn list_settlements(
    axum::extract::State(state): axum::extract::State<AppState>,
    Query(query): Query<SettlementsQuery>,
) -> Result<Json<ApiResponse<Vec<RelaySettlementExport>>>, RelayError> {
    let relay = state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;
    let now = Utc::now();
    let start_unix_ms = query
        .start_time
        .as_deref()
        .map(|value| parse_settlement_timestamp(value, "start_time"))
        .transpose()?
        .unwrap_or_else(|| (now - chrono::Duration::days(1)).timestamp_millis());
    let end_unix_ms = query
        .end_time
        .as_deref()
        .map(|value| parse_settlement_timestamp(value, "end_time"))
        .transpose()?
        .unwrap_or_else(|| now.timestamp_millis());
    if start_unix_ms >= end_unix_ms {
        return Err(RelayError::InvalidConfig(
            "settlement time range must have a positive duration".to_string(),
        ));
    }

    let store = relay.profit_store.as_ref().ok_or_else(|| {
        RelayError::Internal("relay profit ledger database is unavailable".to_string())
    })?;
    let records = store
        .list_for_filter(&RelayProfitLedgerFilter {
            instance_id: relay.instance_id.clone(),
            start_unix_ms,
            end_unix_ms,
        })
        .await
        .map_err(|error| RelayError::Internal(format!("load relay settlements: {error}")))?;
    let settlements = settlements_from_profit_records(&records)?;

    let per_page = query.per_page.unwrap_or(50).clamp(1, 100) as usize;
    let page = query.page.unwrap_or(1).max(1);
    let offset = page
        .saturating_sub(1)
        .saturating_mul(per_page as u64)
        .try_into()
        .unwrap_or(usize::MAX);
    Ok(ok_response(
        settlements
            .into_iter()
            .skip(offset)
            .take(per_page)
            .collect(),
    ))
}

fn parse_settlement_timestamp(value: &str, field: &str) -> Result<i64, RelayError> {
    DateTime::parse_from_rfc3339(value)
        .map_err(|_| RelayError::InvalidConfig(format!("invalid {field} timestamp")))
        .map(|value| value.timestamp_millis())
}

fn settlements_from_profit_records(
    records: &[PersistedRelayProfitRecord],
) -> Result<Vec<RelaySettlementExport>, RelayError> {
    let mut ordered_records = records.to_vec();
    ordered_records.sort_by(|left, right| {
        right
            .occurred_at_unix_ms
            .cmp(&left.occurred_at_unix_ms)
            .then_with(|| right.id.cmp(&left.id))
    });

    ordered_records
        .into_iter()
        .map(|record| {
            validate_settlement_amount("upstream_cost_usd", record.upstream_cost_usd, true)?;
            validate_settlement_amount(
                "downstream_revenue_usd",
                record.downstream_revenue_usd,
                true,
            )?;
            validate_settlement_amount("payment_fee_usd", record.payment_fee_usd, true)?;
            validate_settlement_amount("net_profit_usd", record.net_profit_usd, false)?;
            validate_settlement_amount("margin_percent", record.margin_percent, false)?;
            let occurred_at = DateTime::<Utc>::from_timestamp_millis(record.occurred_at_unix_ms)
                .ok_or_else(|| {
                    RelayError::Internal("relay settlement has an invalid timestamp".to_string())
                })?
                .to_rfc3339();
            Ok(RelaySettlementExport {
                id: record.id,
                request_id: record.request_id,
                instance_id: record.instance_id,
                channel_id: record.channel_id,
                model_id: record.model_id,
                occurred_at,
                upstream_cost_usd: record.upstream_cost_usd,
                downstream_revenue_usd: record.downstream_revenue_usd,
                payment_fee_usd: record.payment_fee_usd,
                net_profit_usd: record.net_profit_usd,
                margin_percent: record.margin_percent,
                cost_confidence: record.cost_confidence,
            })
        })
        .collect()
}

fn validate_settlement_amount(
    field: &str,
    value: Option<f64>,
    must_be_non_negative: bool,
) -> Result<(), RelayError> {
    if value.is_some_and(|value| !value.is_finite() || (must_be_non_negative && value < 0.0)) {
        return Err(RelayError::Internal(format!(
            "relay settlement has an invalid {field}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::{Arc, Mutex, OnceLock};

    use aether_data::repository::integration_configs::{
        IntegrationConfigUpdate, IntegrationCredentialRotation, OpaqueCredentialCiphertext,
    };
    use aether_data::repository::management_tokens::{
        InMemoryManagementTokenRepository, StoredManagementToken, StoredManagementTokenUserSummary,
        StoredManagementTokenWithUser,
    };
    use aether_data::repository::relay_profit::{PersistedRelayProfitRecord, RelayCostConfidence};
    use aether_data::{DatabaseDriver, SqlDatabaseConfig, SqlPoolConfig};
    use axum::body::{to_bytes, Body};
    use axum::extract::ConnectInfo;
    use axum::http::{
        header::{AUTHORIZATION, CONTENT_TYPE},
        Method, Request, StatusCode,
    };
    use axum::Router;
    use base64::Engine as _;
    use chrono::Utc;
    use hmac::Mac;
    use serde_json::{json, Value};
    use sha2::Digest;
    use tower::ServiceExt;

    use crate::data::{GatewayDataConfig, GatewayDataState};
    use crate::relay::RelayEngineConfig;
    use crate::state::AppState;

    use super::{mount_relay_routes, settlements_from_profit_records};

    const RELAY_GROUP_ADMIN_DEVICE_ID: &str = "relay-group-admin-device";

    struct EnvVarGuard {
        key: &'static str,
        original: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = self.original.take() {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    fn relay_api_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    async fn group_management_router() -> (Router, String) {
        let (router, _, admin_token) = relay_management_router_with_state().await;
        (router, admin_token)
    }

    async fn relay_management_router_with_state() -> (Router, AppState, String) {
        let mut pool = SqlPoolConfig::default();
        pool.min_connections = 0;
        pool.max_connections = 1;
        let database = SqlDatabaseConfig::new(DatabaseDriver::Sqlite, "sqlite::memory:", pool)
            .expect("sqlite test database configuration should be valid");
        let mut state = AppState::new()
            .expect("gateway state should build")
            .with_data_config(GatewayDataConfig::from_database_config(database))
            .expect("gateway data state should build");
        state
            .run_database_migrations()
            .await
            .expect("sqlite relay migrations should run");
        let admin_token = issue_test_admin_access_token(&state).await;

        let mut relay_config = RelayEngineConfig::default();
        relay_config.enabled = true;
        state.configure_relay_engine_with_config(relay_config);

        let router =
            mount_relay_routes(Router::<AppState>::new(), state.clone()).with_state(state.clone());
        (router, state, admin_token)
    }

    async fn relay_enabled_router_without_database_store() -> (Router, String) {
        let mut state = AppState::new().expect("gateway state should build");
        let admin_token = issue_test_admin_access_token(&state).await;
        let mut relay_config = RelayEngineConfig::default();
        relay_config.enabled = true;
        state.configure_relay_engine_with_config(relay_config);
        (
            mount_relay_routes(Router::<AppState>::new(), state.clone()).with_state(state),
            admin_token,
        )
    }

    fn full_router_with_relay_engine() -> Router {
        let mut state = AppState::new().expect("gateway state should build");
        let mut relay_config = RelayEngineConfig::default();
        relay_config.enabled = true;
        state.configure_relay_engine_with_config(relay_config);
        crate::build_router_with_state(state)
    }

    async fn full_router_with_relay_management_tokens() -> (Router, String, String, String) {
        let mut state = AppState::new().expect("gateway state should build");
        let user = state
            .create_local_auth_user_with_settings(
                Some("relay-management-token-admin@example.com".to_string()),
                true,
                "admin".to_string(),
                "hash".to_string(),
                "admin".to_string(),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("relay management token admin should be created")
            .expect("relay management token admin should exist");
        let read_token = "ae-relay-management-read".to_string();
        let write_token = "ae-relay-management-write".to_string();
        let admin_token = "ae-relay-management-admin".to_string();
        let token_repository = Arc::new(InMemoryManagementTokenRepository::seed_with_hashes(
            vec![
                relay_group_management_token(
                    "relay-management-read",
                    &user.id,
                    "relay-management-read",
                    &["admin:relay:read"],
                ),
                relay_group_management_token(
                    "relay-management-write",
                    &user.id,
                    "relay-management-write",
                    &["admin:relay:write"],
                ),
                relay_group_management_token(
                    "relay-management-admin",
                    &user.id,
                    "relay-management-admin",
                    &["admin:relay:admin"],
                ),
            ],
            vec![
                (
                    hash_management_token(&read_token),
                    "relay-management-read".to_string(),
                ),
                (
                    hash_management_token(&write_token),
                    "relay-management-write".to_string(),
                ),
                (
                    hash_management_token(&admin_token),
                    "relay-management-admin".to_string(),
                ),
            ],
        ));
        state = state.with_data_state_for_tests(
            GatewayDataState::with_management_token_repository_for_tests(token_repository),
        );
        let mut relay_config = RelayEngineConfig::default();
        relay_config.enabled = true;
        state.configure_relay_engine_with_config(relay_config);

        (
            crate::build_router_with_state(state),
            read_token,
            write_token,
            admin_token,
        )
    }

    async fn issue_test_admin_access_token(state: &AppState) -> String {
        let user = state
            .create_local_auth_user_with_settings(
                Some("relay-groups-admin@example.com".to_string()),
                true,
                "admin".to_string(),
                "hash".to_string(),
                "admin".to_string(),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("relay group admin user should be created")
            .expect("relay group admin user should exist");
        let now = Utc::now();
        let session_id = "relay-group-admin-session".to_string();
        let refresh_token = "relay-group-admin-refresh".to_string();
        let session = crate::data::state::StoredUserSessionRecord::new(
            session_id.clone(),
            user.id.clone(),
            RELAY_GROUP_ADMIN_DEVICE_ID.to_string(),
            None,
            crate::data::state::StoredUserSessionRecord::hash_refresh_token(&refresh_token),
            None,
            None,
            Some(now),
            Some(now + chrono::Duration::days(7)),
            None,
            None,
            Some("127.0.0.1".to_string()),
            Some("relay-group-admin-test".to_string()),
            Some(now),
            Some(now),
        )
        .expect("relay group admin session should build");
        state
            .create_user_session(session)
            .await
            .expect("relay group admin session should persist")
            .expect("relay group admin session should exist");

        build_test_auth_token(
            serde_json::Map::from_iter([
                ("user_id".to_string(), json!(user.id)),
                ("role".to_string(), json!("admin")),
                (
                    "created_at".to_string(),
                    json!(user.created_at.map(|value| value.to_rfc3339())),
                ),
                ("session_id".to_string(), json!(session_id)),
            ]),
            now + chrono::Duration::hours(12),
        )
    }

    fn build_test_auth_token(
        mut payload: serde_json::Map<String, Value>,
        expires_at: chrono::DateTime<Utc>,
    ) -> String {
        let header = json!({ "alg": "HS256", "typ": "JWT" });
        payload.insert("exp".to_string(), json!(expires_at.timestamp()));
        payload.insert("type".to_string(), json!("access"));
        let header_segment = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&header)
                .expect("relay group JWT header should serialize")
                .as_slice(),
        );
        let payload_segment = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&payload)
                .expect("relay group JWT payload should serialize")
                .as_slice(),
        );
        let signing_input = format!("{header_segment}.{payload_segment}");
        let secret = std::env::var("JWT_SECRET_KEY")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "aether-rust-dev-jwt-secret".to_string());
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())
            .expect("relay group JWT secret should build");
        mac.update(signing_input.as_bytes());
        let signature = mac.finalize().into_bytes();
        format!(
            "{signing_input}.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.as_slice())
        )
    }

    fn with_admin_authorization(mut request: Request<Body>, token: &str) -> Request<Body> {
        request.headers_mut().insert(
            AUTHORIZATION,
            format!("Bearer {token}")
                .parse()
                .expect("admin authorization should be valid"),
        );
        request.headers_mut().insert(
            "x-client-device-id",
            RELAY_GROUP_ADMIN_DEVICE_ID
                .parse()
                .expect("admin device header should be valid"),
        );
        request
    }

    fn json_request(method: Method, uri: &str, payload: Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(payload.to_string()))
            .expect("test request should build")
    }

    async fn json_body(response: axum::response::Response) -> Value {
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should be readable");
        serde_json::from_slice(&body).expect("response body should be JSON")
    }

    #[tokio::test]
    async fn database_backed_group_management_routes_persist_crud() {
        let (app, admin_token) = group_management_router().await;

        let response = app
            .clone()
            .oneshot(with_admin_authorization(
                json_request(
                    Method::POST,
                    "/api/relay/groups",
                    json!({
                        "name": "production",
                        "description": "database-backed group",
                        "global_ratio_multiplier": 1.25,
                        "priority": 3,
                        "model_whitelist": ["gpt-5"],
                    }),
                ),
                &admin_token,
            ))
            .await
            .expect("create route should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let created = json_body(response).await;
        let id = created["data"]["id"]
            .as_str()
            .expect("created group should have an id")
            .to_string();
        assert_eq!(created["data"]["name"], "production");

        let response = app
            .clone()
            .oneshot(with_admin_authorization(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/relay/groups")
                    .body(Body::empty())
                    .expect("list request should build"),
                &admin_token,
            ))
            .await
            .expect("list route should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let groups = json_body(response).await;
        assert_eq!(groups["data"].as_array().map(Vec::len), Some(1));
        assert_eq!(groups["data"][0]["id"], id);

        let response = app
            .clone()
            .oneshot(with_admin_authorization(
                json_request(
                    Method::PUT,
                    &format!("/api/relay/groups/{id}"),
                    json!({
                        "priority": 9,
                        "enabled": false,
                    }),
                ),
                &admin_token,
            ))
            .await
            .expect("update route should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let updated = json_body(response).await;
        assert_eq!(updated["data"]["priority"], 9);
        assert_eq!(updated["data"]["enabled"], false);

        let response = app
            .clone()
            .oneshot(with_admin_authorization(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/relay/groups/{id}"))
                    .body(Body::empty())
                    .expect("delete request should build"),
                &admin_token,
            ))
            .await
            .expect("delete route should respond");
        assert_eq!(response.status(), StatusCode::OK);

        let response = app
            .oneshot(with_admin_authorization(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/relay/groups")
                    .body(Body::empty())
                    .expect("post-delete list request should build"),
                &admin_token,
            ))
            .await
            .expect("post-delete list route should respond");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            json_body(response).await["data"].as_array().map(Vec::len),
            Some(0)
        );
    }

    #[tokio::test]
    async fn group_management_routes_fail_closed_without_a_database_store() {
        let (app, admin_token) = relay_enabled_router_without_database_store().await;

        let response = app
            .oneshot(with_admin_authorization(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/relay/groups")
                    .body(Body::empty())
                    .expect("database-less group list request should build"),
                &admin_token,
            ))
            .await
            .expect("database-less group list route should respond");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            json_body(response).await["code"],
            "database_backed_group_export_unavailable"
        );
    }

    #[tokio::test]
    async fn database_backed_group_management_routes_reject_unauthenticated_and_spoofed_admin_requests(
    ) {
        let (app, _) = group_management_router().await;

        let unauthenticated = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/relay/groups")
                    .body(Body::empty())
                    .expect("unauthenticated group list request should build"),
            )
            .await
            .expect("unauthenticated group list should respond");
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

        let spoofed = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/relay/groups")
                    .header(crate::constants::GATEWAY_HEADER, "rust-phase3b")
                    .header(crate::constants::TRUSTED_ADMIN_USER_ID_HEADER, "attacker")
                    .header(crate::constants::TRUSTED_ADMIN_USER_ROLE_HEADER, "admin")
                    .header(crate::constants::TRUSTED_ADMIN_SESSION_ID_HEADER, "forged")
                    .body(Body::empty())
                    .expect("spoofed group list request should build"),
            )
            .await
            .expect("spoofed group list should respond");
        assert_eq!(spoofed.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn every_mounted_relay_management_route_rejects_anonymous_service_and_spoofed_admin_requests(
    ) {
        let _env_lock = relay_api_env_lock()
            .lock()
            .expect("relay API environment lock should not be poisoned");
        let (app, _) = group_management_router().await;
        let service_control_secret = "relay-management-service-control";
        let _control_secret = EnvVarGuard::set("AETHER_CONTROL_SECRET", service_control_secret);
        let routes = [
            (Method::GET, "/api/relay/channels"),
            (Method::POST, "/api/relay/channels"),
            (Method::GET, "/api/relay/channels/channel-1"),
            (Method::PUT, "/api/relay/channels/channel-1"),
            (Method::DELETE, "/api/relay/channels/channel-1"),
            (Method::GET, "/api/relay/pricing"),
            (Method::POST, "/api/relay/pricing"),
            (Method::PUT, "/api/relay/pricing/rule-1"),
            (Method::DELETE, "/api/relay/pricing/rule-1"),
            (Method::GET, "/api/relay/downstream"),
            (Method::POST, "/api/relay/downstream"),
            (Method::DELETE, "/api/relay/downstream/downstream-1"),
            (Method::GET, "/api/relay/groups"),
            (Method::POST, "/api/relay/groups"),
            (Method::PUT, "/api/relay/groups/group-1"),
            (Method::DELETE, "/api/relay/groups/group-1"),
            (Method::GET, "/api/relay/dashboard"),
            (Method::POST, "/api/relay/sync/pricing"),
            (Method::POST, "/api/relay/sync/health"),
            (Method::GET, "/api/relay/settlements"),
            (Method::GET, "/api/relay/integration-status"),
        ];

        for (method, path) in routes {
            let anonymous = app
                .clone()
                .oneshot(relay_management_request(method.clone(), path))
                .await
                .expect("anonymous relay management request should respond");
            assert_eq!(
                anonymous.status(),
                StatusCode::UNAUTHORIZED,
                "anonymous request should be rejected for {method} {path}"
            );

            let service_credential = app
                .clone()
                .oneshot(management_token_request_at(
                    method.clone(),
                    path,
                    service_control_secret,
                ))
                .await
                .expect("service credential relay management request should respond");
            assert_eq!(
                service_credential.status(),
                StatusCode::UNAUTHORIZED,
                "service credential must not authorize {method} {path}"
            );

            let spoofed = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header(crate::constants::GATEWAY_HEADER, "rust-phase3b")
                        .header(crate::constants::TRUSTED_ADMIN_USER_ID_HEADER, "attacker")
                        .header(crate::constants::TRUSTED_ADMIN_USER_ROLE_HEADER, "admin")
                        .header(crate::constants::TRUSTED_ADMIN_SESSION_ID_HEADER, "forged")
                        .body(Body::empty())
                        .expect("spoofed relay management request should build"),
                )
                .await
                .expect("spoofed relay management request should respond");
            assert_eq!(
                spoofed.status(),
                StatusCode::UNAUTHORIZED,
                "forged trusted headers must not authorize {path}"
            );
        }
    }

    #[tokio::test]
    async fn full_router_relay_paths_do_not_overclassify_trailing_or_nested_integration_status() {
        let app = full_router_with_relay_engine();

        let trailing_slash = app
            .clone()
            .oneshot(relay_management_request(
                Method::GET,
                "/api/relay/integration-status/",
            ))
            .await
            .expect("trailing-slash integration status request should respond");
        assert_eq!(trailing_slash.status(), StatusCode::UNAUTHORIZED);

        let nested = app
            .oneshot(relay_management_request(
                Method::GET,
                "/api/relay/integration-status/nested",
            ))
            .await
            .expect("nested integration status request should respond");
        assert_eq!(nested.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn full_router_relay_management_tokens_enforce_relay_scope_permissions() {
        let (app, read_token, write_token, admin_token) =
            full_router_with_relay_management_tokens().await;

        let read_status = app
            .clone()
            .oneshot(management_token_request_at(
                Method::GET,
                "/api/relay/integration-status",
                &read_token,
            ))
            .await
            .expect("relay read token status request should respond");
        assert_eq!(read_status.status(), StatusCode::OK);
        let read_status = json_body(read_status).await;
        assert_eq!(read_status["data"]["relay_enabled"], true);

        let read_write = app
            .clone()
            .oneshot(management_token_request_at(
                Method::POST,
                "/api/relay/sync/health",
                &read_token,
            ))
            .await
            .expect("relay read token health reset request should respond");
        assert_eq!(read_write.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            json_body(read_write).await["required_permission"],
            "admin:relay:write"
        );

        let write = app
            .clone()
            .oneshot(management_token_request_at(
                Method::POST,
                "/api/relay/sync/health",
                &write_token,
            ))
            .await
            .expect("relay write token health reset request should respond");
        assert_ne!(write.status(), StatusCode::UNAUTHORIZED);
        assert_ne!(write.status(), StatusCode::FORBIDDEN);

        let admin_status = app
            .clone()
            .oneshot(management_token_request_at(
                Method::GET,
                "/api/relay/integration-status",
                &admin_token,
            ))
            .await
            .expect("relay admin token status request should respond");
        assert_eq!(admin_status.status(), StatusCode::OK);

        let admin_write = app
            .oneshot(management_token_request_at(
                Method::POST,
                "/api/relay/sync/health",
                &admin_token,
            ))
            .await
            .expect("relay admin token health reset request should respond");
        assert_ne!(admin_write.status(), StatusCode::UNAUTHORIZED);
        assert_ne!(admin_write.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn integration_status_is_admin_read_only_and_redacts_persisted_credentials() {
        let (app, state, admin_token) = relay_management_router_with_state().await;
        let instance_id = state
            .relay_engine()
            .expect("relay engine should be configured")
            .instance_id
            .clone();
        seed_relay_integration_status(&state, &instance_id).await;

        let configured = app
            .clone()
            .oneshot(with_admin_authorization(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/relay/integration-status")
                    .body(Body::empty())
                    .expect("configured integration status request should build"),
                &admin_token,
            ))
            .await
            .expect("configured integration status request should respond");
        assert_eq!(configured.status(), StatusCode::OK);
        let configured = json_body(configured).await;
        assert_eq!(configured["data"]["configured"], true);
        assert_eq!(configured["data"]["relay_enabled"], true);
        assert_eq!(
            configured["data"]["config"]["instance_id"],
            instance_id.as_str()
        );
        assert_eq!(configured["data"]["config"]["route_profile"], "balanced");
        assert_eq!(configured["data"]["credentials"]["credential_revision"], 1);
        assert_eq!(
            configured["data"]["credentials"]["rotation_id"],
            "status-rotation-1"
        );
        assert_eq!(
            configured["data"]["credentials"]["transition_expires_at_unix_ms"],
            Value::Null
        );
        assert_eq!(
            configured["data"]["credentials"]["transition_active"],
            false
        );
        let serialized = configured.to_string();
        for forbidden in [
            "status-current-control-ciphertext",
            "status-current-relay-ciphertext",
            "current_control_secret_ciphertext",
            "previous_control_secret_ciphertext",
            "current_relay_secret_ciphertext",
            "previous_relay_secret_ciphertext",
        ] {
            assert!(
                !serialized.contains(forbidden),
                "integration status must not expose {forbidden}"
            );
        }

        let query_override = app
            .clone()
            .oneshot(with_admin_authorization(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/relay/integration-status?instance_id=other-instance")
                    .body(Body::empty())
                    .expect("query override integration status request should build"),
                &admin_token,
            ))
            .await
            .expect("query override integration status request should respond");
        assert_eq!(query_override.status(), StatusCode::OK);
        let query_override = json_body(query_override).await;
        assert_eq!(query_override["data"]["configured"], true);
        assert_eq!(
            query_override["data"]["config"]["instance_id"],
            instance_id.as_str()
        );

        let (unconfigured_app, _, unconfigured_admin_token) =
            relay_management_router_with_state().await;
        let unconfigured = unconfigured_app
            .oneshot(with_admin_authorization(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/relay/integration-status")
                    .body(Body::empty())
                    .expect("unconfigured integration status request should build"),
                &unconfigured_admin_token,
            ))
            .await
            .expect("unconfigured integration status request should respond");
        assert_eq!(unconfigured.status(), StatusCode::OK);
        let unconfigured = json_body(unconfigured).await;
        assert_eq!(unconfigured["data"]["configured"], false);
        assert_eq!(unconfigured["data"]["relay_enabled"], true);
        assert!(unconfigured["data"]["config"].is_null());
        assert!(unconfigured["data"]["credentials"].is_null());

        let (without_store, without_store_admin_token) =
            relay_enabled_router_without_database_store().await;
        let unavailable = without_store
            .oneshot(with_admin_authorization(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/relay/integration-status")
                    .body(Body::empty())
                    .expect("store-unavailable integration status request should build"),
                &without_store_admin_token,
            ))
            .await
            .expect("store-unavailable integration status request should respond");
        assert_eq!(unavailable.status(), StatusCode::OK);
        let unavailable = json_body(unavailable).await;
        assert_eq!(unavailable["data"]["configured"], false);
        assert_eq!(unavailable["data"]["relay_enabled"], true);
        assert!(unavailable["data"]["config"].is_null());
        assert!(unavailable["data"]["credentials"].is_null());
    }

    #[tokio::test]
    async fn relay_group_routes_enforce_management_token_read_and_write_permissions() {
        let _env_lock = relay_api_env_lock()
            .lock()
            .expect("relay API environment lock should not be poisoned");
        let mut state = AppState::new().expect("gateway state should build");
        let user = state
            .create_local_auth_user_with_settings(
                Some("relay-group-token-admin@example.com".to_string()),
                true,
                "admin".to_string(),
                "hash".to_string(),
                "admin".to_string(),
                None,
                None,
                None,
                None,
            )
            .await
            .expect("management token admin should be created")
            .expect("management token admin should exist");
        let read_token = "ae-relay-groups-read";
        let write_token = "ae-relay-groups-write";
        let token_repository = Arc::new(InMemoryManagementTokenRepository::seed_with_hashes(
            vec![
                relay_group_management_token(
                    "relay-groups-read",
                    &user.id,
                    "relay-groups-read",
                    &["admin:routing_profiles:read"],
                ),
                relay_group_management_token(
                    "relay-groups-write",
                    &user.id,
                    "relay-groups-write",
                    &[
                        "admin:routing_profiles:read",
                        "admin:routing_profiles:write",
                    ],
                ),
            ],
            vec![
                (
                    hash_management_token(read_token),
                    "relay-groups-read".to_string(),
                ),
                (
                    hash_management_token(write_token),
                    "relay-groups-write".to_string(),
                ),
            ],
        ));
        state = state.with_data_state_for_tests(
            GatewayDataState::with_management_token_repository_for_tests(token_repository),
        );
        let app = Router::<AppState>::new()
            .route(
                "/api/relay/groups",
                axum::routing::get(relay_group_authorized_test_handler)
                    .post(relay_group_authorized_test_handler),
            )
            .route_layer(axum::middleware::from_fn_with_state(
                state.clone(),
                super::require_relay_admin,
            ))
            .with_state(state);

        let invalid = app
            .clone()
            .oneshot(management_token_request(
                Method::GET,
                "ae-relay-groups-missing",
            ))
            .await
            .expect("unknown management token request should respond");
        assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);

        let read = app
            .clone()
            .oneshot(management_token_request(Method::GET, read_token))
            .await
            .expect("read management token request should respond");
        assert_eq!(read.status(), StatusCode::NO_CONTENT);

        {
            let _control_secret = EnvVarGuard::set("AETHER_CONTROL_SECRET", read_token);
            let service_credential = app
                .clone()
                .oneshot(management_token_request(Method::GET, read_token))
                .await
                .expect("reused control credential request should respond");
            assert_eq!(service_credential.status(), StatusCode::UNAUTHORIZED);
        }

        let read_write = app
            .clone()
            .oneshot(management_token_request(Method::POST, read_token))
            .await
            .expect("read-only management token write request should respond");
        assert_eq!(read_write.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            json_body(read_write).await["required_permission"],
            "admin:routing_profiles:write"
        );

        let write = app
            .oneshot(management_token_request(Method::POST, write_token))
            .await
            .expect("write management token request should respond");
        assert_eq!(write.status(), StatusCode::NO_CONTENT);
    }

    async fn relay_group_authorized_test_handler() -> StatusCode {
        StatusCode::NO_CONTENT
    }

    async fn seed_relay_integration_status(state: &AppState, instance_id: &str) {
        let store = state
            .relay_integration_config_store()
            .expect("integration config store should be available");
        let update = IntegrationConfigUpdate {
            route_profile: "balanced".to_string(),
            execution_mode: "direct_channel".to_string(),
            enabled: true,
            capability_version: "0.1.0".to_string(),
            updated_at_unix_ms: 1_784_073_600_000,
        };
        let rotation = IntegrationCredentialRotation::new(
            OpaqueCredentialCiphertext::new("status-current-control-ciphertext")
                .expect("control ciphertext should be valid"),
            OpaqueCredentialCiphertext::new("status-current-relay-ciphertext")
                .expect("relay ciphertext should be valid"),
            None,
            false,
            "status-rotation-1",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .expect("credential rotation should be valid");
        store
            .compare_and_set_with_credential_rotation(instance_id, 0, &update, &rotation)
            .await
            .expect("integration config and credentials should persist");
    }

    fn management_token_request(method: Method, token: &str) -> Request<Body> {
        management_token_request_at(method, "/api/relay/groups", token)
    }

    fn management_token_request_at(method: Method, uri: &str, token: &str) -> Request<Body> {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .expect("management token request should build");
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:43123"
                .parse::<std::net::SocketAddr>()
                .expect("management token test address should parse"),
        ));
        request
    }

    fn relay_management_request(method: Method, uri: &str) -> Request<Body> {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .expect("relay management request should build");
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:43123"
                .parse::<std::net::SocketAddr>()
                .expect("relay management test address should parse"),
        ));
        request
    }

    fn relay_group_management_token(
        token_id: &str,
        user_id: &str,
        username: &str,
        permissions: &[&str],
    ) -> StoredManagementTokenWithUser {
        let mut token = StoredManagementToken::new(
            token_id.to_string(),
            user_id.to_string(),
            format!("{username}-token"),
        )
        .expect("relay group management token should build")
        .with_display_fields(
            Some(format!("{username} token")),
            Some("ae_test".to_string()),
            None,
        )
        .with_runtime_fields(
            Some(4_102_444_800),
            Some(1_711_000_000),
            Some("127.0.0.1".to_string()),
            7,
            true,
        )
        .with_timestamps(Some(1_710_000_000), Some(1_711_000_100));
        token.permissions = Some(json!(permissions));
        let user = StoredManagementTokenUserSummary::new(
            user_id.to_string(),
            Some(format!("{username}@example.com")),
            username.to_string(),
            "admin".to_string(),
        )
        .expect("relay group management token user should build");
        StoredManagementTokenWithUser::new(token, user)
    }

    fn hash_management_token(value: &str) -> String {
        let mut hasher = sha2::Sha256::new();
        hasher.update(value.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    fn profit_record(
        id: &str,
        request_id: &str,
        occurred_at_unix_ms: i64,
        upstream_cost_usd: Option<f64>,
        downstream_revenue_usd: Option<f64>,
    ) -> PersistedRelayProfitRecord {
        PersistedRelayProfitRecord {
            id: id.to_string(),
            instance_id: "aether-primary".to_string(),
            request_id: request_id.to_string(),
            channel_id: "41".to_string(),
            model_id: "gpt-5".to_string(),
            prompt_tokens: 10,
            completion_tokens: 5,
            charged_quota: "1250".to_string(),
            quota_per_unit: Some("500000".to_string()),
            upstream_cost_usd,
            downstream_revenue_usd,
            payment_fee_usd: downstream_revenue_usd.map(|revenue| revenue * 0.006),
            net_profit_usd: match (upstream_cost_usd, downstream_revenue_usd) {
                (Some(cost), Some(revenue)) => Some(revenue - cost - revenue * 0.006),
                _ => None,
            },
            margin_percent: None,
            cost_confidence: if upstream_cost_usd.is_some() {
                RelayCostConfidence::Known
            } else {
                RelayCostConfidence::Unknown
            },
            occurred_at_unix_ms,
        }
    }

    #[test]
    fn settlements_export_uses_profit_ledger_facts_without_zero_filling_unknown_values() {
        let settlements = settlements_from_profit_records(&[
            profit_record(
                "profit-known",
                "request-known",
                1_784_073_600_000,
                Some(0.001),
                Some(0.0025),
            ),
            profit_record(
                "profit-unknown",
                "request-unknown",
                1_784_073_601_000,
                None,
                None,
            ),
        ])
        .expect("persisted ledger facts should export");

        assert_eq!(settlements.len(), 2);
        assert_eq!(settlements[0].request_id, "request-unknown");
        assert_eq!(settlements[0].upstream_cost_usd, None);
        assert_eq!(settlements[0].downstream_revenue_usd, None);
        assert_eq!(settlements[0].net_profit_usd, None);
        assert_eq!(settlements[1].request_id, "request-known");
        assert_eq!(settlements[1].upstream_cost_usd, Some(0.001));
        assert_eq!(settlements[1].downstream_revenue_usd, Some(0.0025));
        assert!(chrono::DateTime::parse_from_rfc3339(&settlements[0].occurred_at).is_ok());
    }
}
