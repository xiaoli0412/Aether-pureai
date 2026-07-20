//! 双边配置镜像接口
//!
//! 提供受限控制接口供 New API 管理 Aether 实例配置。
//! 使用独立控制凭据（与 relay signing secret 分离）。
//!
//! 不接收、不保存 New API 渠道 Key、用户余额或支付凭据。

use std::time::Duration;

use axum::body::to_bytes;
use axum::extract::{Json, Path, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::Router;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use aether_crypto::{decrypt_python_fernet_ciphertext, encrypt_python_fernet_plaintext};
use aether_data::repository::integration_configs::{
    BootstrapIntegrationCredentialRotation, IntegrationConfigCasResult, IntegrationConfigUpdate,
    IntegrationCredentialRotation, OpaqueCredentialCiphertext, PersistedIntegrationConfig,
    PersistedIntegrationCredentials,
};

use super::collaboration::{
    constant_time_eq, control_signature_v2_body_sha256_hex, has_control_signature_v2_headers,
    verify_control_signature_v2, ControlCredentialVerifier, ControlSignatureV2Secrets,
    ControlSignatureV2Verification, RoutingMode, CONTROL_SIGNATURE_V2_MAX_AGE_SECS,
    HEADER_INSTANCE_ID,
};
use super::error::RelayError;
use crate::state::AppState;

const MAX_CONTROL_REQUEST_BODY_BYTES: usize = 64 * 1024;
const CONTROL_NONCE_LOCK_OWNER: &str = "new-api-control-v2";

// ==================== Types ====================

/// 实例配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceConfig {
    pub instance_id: String,
    pub route_profile: String,
    pub execution_mode: RoutingMode,
    pub enabled: bool,
    pub capability_version: String,
    pub base_revision: u64,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credential_rotation_ack: Option<CredentialRotationAck>,
}

/// 更新实例配置输入
#[derive(Clone, Deserialize)]
pub struct UpdateInstanceInput {
    pub route_profile: Option<String>,
    pub execution_mode: Option<RoutingMode>,
    pub enabled: Option<bool>,
    /// 客户端必须提供当前 base_revision，冲突返回 409
    pub base_revision: u64,
    pub credential_rotation: Option<CredentialRotationInput>,
}

/// 明文只存在于本次受签名的控制请求中；不能实现 Debug，防止日志泄露。
#[derive(Clone, Deserialize)]
pub struct CredentialRotationInput {
    pub id: String,
    pub control_secret: String,
    pub relay_signing_secret: String,
    pub transition_expires_at: u64,
    pub revoke_previous: bool,
}

/// 轮换确认只返回不可逆的版本元数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialRotationAck {
    pub rotation_id: String,
    pub credential_revision: u64,
    pub transition_expires_at: u64,
    pub state: String,
}

/// 能力声明
#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    pub supported_modes: Vec<String>,
    pub supported_formats: Vec<String>,
    pub capability_version: String,
    pub max_concurrent_requests: u64,
    pub features: Vec<String>,
}

/// 实例状态
#[derive(Debug, Clone, Serialize)]
pub struct InstanceStatus {
    pub instance_id: String,
    /// Control-execution readiness only; this does not claim upstream or worker health.
    pub healthy: bool,
    /// Timestamp of the last persisted control configuration update, when available.
    pub last_sync_at: Option<String>,
    pub capability_version: String,
    pub base_revision: u64,
    /// No process-uptime source is wired into the control contract yet, so this is zero.
    pub uptime_secs: u64,
    pub active_channels: usize,
    pub routing_mode: RoutingMode,
}

/// 409 冲突响应
#[derive(Debug, Serialize)]
struct ConflictResponse {
    error: String,
    current_revision: u64,
    current_config: InstanceConfig,
    diff: Value,
}

// ==================== Routes ====================

/// 挂载集成 API 路由
pub(crate) fn mount_integrations_routes(router: Router<AppState>) -> Router<AppState> {
    router
        .route(
            "/api/integrations/new-api/v1/capabilities",
            get(get_capabilities),
        )
        .route(
            "/api/integrations/new-api/v1/instances/{instance_id}",
            put(update_instance),
        )
        .route(
            "/api/integrations/new-api/v1/instances/{instance_id}/status",
            get(get_instance_status),
        )
}

/// GET /api/integrations/new-api/v1/capabilities
async fn get_capabilities(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, RelayError> {
    let (parts, body) = request.into_parts();
    let raw_body = read_control_request_body(body).await?;
    if let Err(error) = verify_control_auth(
        &state,
        &parts.headers,
        &parts.method,
        &parts.uri,
        &raw_body,
        None,
    )
    .await
    {
        return Ok(error.into_response());
    }

    Ok(Json(contract_capabilities(&state)).into_response())
}

/// PUT /api/integrations/new-api/v1/instances/{instance_id}
async fn update_instance(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    request: Request,
) -> Result<Response, RelayError> {
    let (parts, body) = request.into_parts();
    let raw_body = read_control_request_body(body).await?;
    let authentication = match verify_control_auth(
        &state,
        &parts.headers,
        &parts.method,
        &parts.uri,
        &raw_body,
        Some(&instance_id),
    )
    .await
    {
        Ok(authentication) => authentication,
        Err(error) => return Ok(error.into_response()),
    };
    let input: UpdateInstanceInput = serde_json::from_slice(&raw_body)
        .map_err(|_| RelayError::InvalidConfig("invalid integration update JSON".into()))?;
    if input.credential_rotation.is_some() && authentication.version != ControlAuthVersion::V2 {
        return Ok(ControlAuthFailure::Unauthorized.into_response());
    }

    state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;
    let store = state
        .relay_integration_config_store()
        .ok_or_else(|| RelayError::Internal("integration config database is unavailable".into()))?;
    let current = store
        .get(&instance_id)
        .await
        .map_err(|error| RelayError::Internal(format!("load instance config: {error}")))?
        .map(instance_config_from_persisted)
        .transpose()?
        .unwrap_or_else(|| default_instance_config(&instance_id));
    let requested = apply_instance_update(current, &input)?;
    let expected_revision = i64::try_from(input.base_revision)
        .map_err(|_| RelayError::InvalidConfig("base revision is too large".into()))?;
    let updated_at_unix_ms = Utc::now().timestamp_millis();
    let update = IntegrationConfigUpdate {
        route_profile: requested.route_profile.clone(),
        execution_mode: routing_mode_name(requested.execution_mode).to_string(),
        enabled: requested.enabled,
        capability_version: requested.capability_version.clone(),
        updated_at_unix_ms,
    };
    let rotation = match input.credential_rotation.as_ref() {
        Some(rotation) => match authentication.source {
            ControlCredentialSource::Persisted => Some(CredentialRotationOperation::Persisted(
                build_persisted_credential_rotation(
                    &state,
                    rotation,
                    &raw_body,
                    updated_at_unix_ms,
                )?,
            )),
            ControlCredentialSource::BootstrapV2(bootstrap) => Some(
                CredentialRotationOperation::Bootstrap(build_bootstrap_credential_rotation(
                    &state,
                    &bootstrap,
                    rotation,
                    &raw_body,
                    updated_at_unix_ms,
                )?),
            ),
            ControlCredentialSource::BootstrapLegacy => {
                return Ok(ControlAuthFailure::Unauthorized.into_response());
            }
        },
        None => None,
    };
    let outcome = match rotation.as_ref() {
        Some(CredentialRotationOperation::Persisted(rotation)) => {
            store
                .compare_and_set_with_route_profile_outbox_and_credential_rotation(
                    &instance_id,
                    expected_revision,
                    &update,
                    rotation,
                )
                .await
        }
        Some(CredentialRotationOperation::Bootstrap(bootstrap)) => {
            store
                .compare_and_set_with_route_profile_outbox_and_bootstrap_credential_rotation(
                    &instance_id,
                    expected_revision,
                    &update,
                    bootstrap,
                )
                .await
        }
        None => {
            store
                .compare_and_set_with_route_profile_outbox(&instance_id, expected_revision, &update)
                .await
        }
    }
    .map_err(|error| RelayError::Internal(format!("persist instance config: {error}")))?;
    match outcome {
        IntegrationConfigCasResult::Applied(config) => {
            let credential_rotation_ack = match input.credential_rotation.as_ref() {
                Some(rotation) => {
                    let credentials = store
                        .get_credentials(&instance_id)
                        .await
                        .map_err(|error| {
                            RelayError::Internal(format!("load rotated credentials: {error}"))
                        })?
                        .ok_or_else(|| {
                            RelayError::Internal(
                                "rotation applied without persisted credentials".into(),
                            )
                        })?;
                    Some(CredentialRotationAck {
                        rotation_id: rotation.id.clone(),
                        credential_revision: u64::try_from(credentials.credential_revision)
                            .map_err(|_| {
                                RelayError::Internal("invalid credential revision".into())
                            })?,
                        transition_expires_at: rotation.transition_expires_at,
                        state: "applied".to_string(),
                    })
                }
                None => None,
            };
            let mut response = instance_config_from_persisted(config)?;
            response.credential_rotation_ack = credential_rotation_ack;
            Ok(Json(response).into_response())
        }
        IntegrationConfigCasResult::Conflict(current) => {
            let current = current
                .map(instance_config_from_persisted)
                .transpose()?
                .unwrap_or_else(|| default_instance_config(&instance_id));
            let conflict = ConflictResponse {
                error: format!(
                    "revision conflict: expected {}, got {}",
                    current.base_revision, expected_revision
                ),
                current_revision: current.base_revision,
                diff: instance_config_diff(&requested, &current),
                current_config: current,
            };
            Ok((StatusCode::CONFLICT, Json(conflict)).into_response())
        }
    }
}

/// GET /api/integrations/new-api/v1/instances/{instance_id}/status
async fn get_instance_status(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
    request: Request,
) -> Result<Response, RelayError> {
    let (parts, body) = request.into_parts();
    let raw_body = read_control_request_body(body).await?;
    if let Err(error) = verify_control_auth(
        &state,
        &parts.headers,
        &parts.method,
        &parts.uri,
        &raw_body,
        Some(&instance_id),
    )
    .await
    {
        return Ok(error.into_response());
    }

    let store = match state.relay_integration_config_store() {
        Some(store) => store,
        None => return Ok(integration_config_store_unavailable_response()),
    };
    let persisted_config = match store.get(&instance_id).await {
        Ok(config) => config,
        Err(_) => return Ok(integration_config_store_unavailable_response()),
    };
    let config = persisted_config
        .map(instance_config_from_persisted)
        .transpose()?;
    let relay = state.relay_engine();
    let relay_enabled = relay.is_some_and(|relay| relay.is_enabled());
    let active_channels = match relay {
        Some(relay) => relay.config_store.get_enabled_channel_ids().await?.len(),
        None => 0,
    };
    let (last_sync_at, base_revision, routing_mode, config_ready) = match config.as_ref() {
        Some(config) => (
            Some(config.updated_at.clone()),
            config.base_revision,
            config.execution_mode,
            config.enabled && config.execution_mode == RoutingMode::DirectChannel,
        ),
        None => (None, 0, RoutingMode::Disabled, false),
    };

    let status = InstanceStatus {
        instance_id: instance_id.clone(),
        healthy: relay_enabled && config_ready,
        last_sync_at,
        capability_version: env!("CARGO_PKG_VERSION").to_string(),
        base_revision,
        uptime_secs: 0,
        active_channels,
        routing_mode,
    };

    Ok(Json(status).into_response())
}

fn integration_config_store_unavailable_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": true,
            "message": "integration config database is unavailable",
        })),
    )
        .into_response()
}

fn contract_capabilities(state: &AppState) -> Capabilities {
    let credential_rotation_available = state.relay_integration_config_store().is_some()
        && state
            .data
            .encryption_key()
            .is_some_and(|key| !key.trim().is_empty());
    let mut features = vec!["relay_routing".to_string()];
    if credential_rotation_available {
        features.push("credential_rotation_v1".to_string());
        features.push("control_hmac_v2".to_string());
    }
    Capabilities {
        supported_modes: vec!["direct_channel".to_string()],
        supported_formats: vec!["openai".to_string()],
        capability_version: env!("CARGO_PKG_VERSION").to_string(),
        max_concurrent_requests: 10_000,
        features,
    }
}

fn validate_requested_execution_mode(mode: RoutingMode) -> Result<(), RelayError> {
    if matches!(mode, RoutingMode::DirectChannel | RoutingMode::Disabled) {
        return Ok(());
    }
    Err(RelayError::InvalidConfig(format!(
        "routing mode {} is reserved and cannot be activated",
        routing_mode_name(mode)
    )))
}

fn routing_mode_name(mode: RoutingMode) -> &'static str {
    match mode {
        RoutingMode::DirectChannel => "direct_channel",
        RoutingMode::Disabled => "disabled",
        RoutingMode::ParallelShadow => "parallel_shadow",
        RoutingMode::AetherDecision => "aether_decision",
    }
}

fn default_instance_config(instance_id: &str) -> InstanceConfig {
    InstanceConfig {
        instance_id: instance_id.to_string(),
        route_profile: "default".to_string(),
        execution_mode: RoutingMode::DirectChannel,
        enabled: false,
        capability_version: env!("CARGO_PKG_VERSION").to_string(),
        base_revision: 0,
        updated_at: Utc::now().to_rfc3339(),
        credential_rotation_ack: None,
    }
}

fn apply_instance_update(
    current: InstanceConfig,
    input: &UpdateInstanceInput,
) -> Result<InstanceConfig, RelayError> {
    if input.base_revision != current.base_revision {
        let mut requested = current;
        requested.base_revision = input.base_revision.saturating_add(1);
        if let Some(route_profile) = input.route_profile.as_deref() {
            requested.route_profile = route_profile.to_string();
        }
        if let Some(execution_mode) = input.execution_mode {
            validate_requested_execution_mode(execution_mode)?;
            requested.execution_mode = execution_mode;
        }
        if let Some(enabled) = input.enabled {
            requested.enabled = enabled;
        }
        return Ok(requested);
    }

    let mut requested = current;
    if let Some(route_profile) = input.route_profile.as_deref() {
        let route_profile = route_profile.trim();
        if route_profile.is_empty() {
            return Err(RelayError::InvalidConfig(
                "route profile must not be empty".into(),
            ));
        }
        requested.route_profile = route_profile.to_string();
    }
    if let Some(execution_mode) = input.execution_mode {
        validate_requested_execution_mode(execution_mode)?;
        requested.execution_mode = execution_mode;
    }
    if let Some(enabled) = input.enabled {
        requested.enabled = enabled;
    }
    requested.base_revision = requested.base_revision.saturating_add(1);
    requested.updated_at = Utc::now().to_rfc3339();
    Ok(requested)
}

fn instance_config_from_persisted(
    config: PersistedIntegrationConfig,
) -> Result<InstanceConfig, RelayError> {
    let execution_mode = match config.execution_mode.as_str() {
        "direct_channel" => RoutingMode::DirectChannel,
        "disabled" => RoutingMode::Disabled,
        other => {
            return Err(RelayError::Internal(format!(
                "unsupported persisted integration mode: {other}"
            )));
        }
    };
    let base_revision = u64::try_from(config.revision)
        .map_err(|_| RelayError::Internal("invalid persisted integration revision".into()))?;
    let updated_at = chrono::DateTime::<Utc>::from_timestamp_millis(config.updated_at_unix_ms)
        .ok_or_else(|| RelayError::Internal("invalid persisted integration timestamp".into()))?
        .to_rfc3339();
    Ok(InstanceConfig {
        instance_id: config.instance_id,
        route_profile: config.route_profile,
        execution_mode,
        enabled: config.enabled,
        capability_version: config.capability_version,
        base_revision,
        updated_at,
        credential_rotation_ack: None,
    })
}

fn instance_config_diff(requested: &InstanceConfig, current: &InstanceConfig) -> Value {
    let mut diff = Map::new();
    if requested.route_profile != current.route_profile {
        diff.insert(
            "route_profile".to_string(),
            serde_json::json!({
                "requested": requested.route_profile,
                "current": current.route_profile,
            }),
        );
    }
    if requested.execution_mode != current.execution_mode {
        diff.insert(
            "execution_mode".to_string(),
            serde_json::json!({
                "requested": routing_mode_name(requested.execution_mode),
                "current": routing_mode_name(current.execution_mode),
            }),
        );
    }
    if requested.enabled != current.enabled {
        diff.insert(
            "enabled".to_string(),
            serde_json::json!({
                "requested": requested.enabled,
                "current": current.enabled,
            }),
        );
    }
    Value::Object(diff)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ControlAuthVersion {
    LegacyBearer,
    V2,
}

enum ControlCredentialSource {
    Persisted,
    BootstrapLegacy,
    BootstrapV2(BootstrapCredentialMaterial),
}

struct ControlAuthentication {
    version: ControlAuthVersion,
    source: ControlCredentialSource,
}

#[derive(Clone, Copy)]
enum ControlAuthFailure {
    Unauthorized,
    Unavailable,
}

impl ControlAuthFailure {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "control authentication unavailable",
            ),
        };
        (
            status,
            Json(serde_json::json!({ "error": true, "message": message })),
        )
            .into_response()
    }
}

/// The resolved values are intentionally not Debug so a caller cannot expose
/// plaintext control credentials through routine diagnostics.
struct ResolvedControlCredentials {
    current: String,
    previous: Option<(String, u64)>,
}

/// Bootstrap material is captured only after a bound environment credential
/// has authenticated the request. It is never deserialized from the request.
struct BootstrapCredentialMaterial {
    control_secret: String,
    relay_secret: String,
}

enum CredentialRotationOperation {
    Persisted(IntegrationCredentialRotation),
    Bootstrap(BootstrapIntegrationCredentialRotation),
}

async fn read_control_request_body(body: axum::body::Body) -> Result<Vec<u8>, RelayError> {
    to_bytes(body, MAX_CONTROL_REQUEST_BODY_BYTES)
        .await
        .map(|body| body.to_vec())
        .map_err(|_| {
            RelayError::InvalidConfig("control request body is invalid or too large".into())
        })
}

async fn verify_control_auth(
    state: &AppState,
    headers: &HeaderMap,
    method: &Method,
    uri: &Uri,
    raw_body: &[u8],
    expected_instance_id: Option<&str>,
) -> Result<ControlAuthentication, ControlAuthFailure> {
    let instance_id = control_header_instance_id(headers)?;
    if expected_instance_id.is_some_and(|expected| expected != instance_id) {
        return Err(ControlAuthFailure::Unauthorized);
    }

    let store = state
        .relay_integration_config_store()
        .ok_or(ControlAuthFailure::Unavailable)?;
    let persisted = store
        .get_credentials(instance_id)
        .await
        .map_err(|_| ControlAuthFailure::Unavailable)?;

    if let Some(persisted) = persisted {
        let credentials = resolve_persisted_control_credentials(state, &persisted)?;
        return authenticate_with_control_credentials(
            state,
            headers,
            method,
            uri,
            raw_body,
            instance_id,
            &credentials,
            ControlCredentialSource::Persisted,
        )
        .await;
    }

    authenticate_bootstrap_control_request(state, headers, method, uri, raw_body, instance_id).await
}

fn control_header_instance_id(headers: &HeaderMap) -> Result<&str, ControlAuthFailure> {
    headers
        .get(HEADER_INSTANCE_ID)
        .and_then(|value| value.to_str().ok())
        .filter(|instance_id| !instance_id.trim().is_empty())
        .ok_or(ControlAuthFailure::Unauthorized)
}

fn resolve_persisted_control_credentials(
    state: &AppState,
    credentials: &PersistedIntegrationCredentials,
) -> Result<ResolvedControlCredentials, ControlAuthFailure> {
    let encryption_key = state
        .data
        .encryption_key()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .ok_or(ControlAuthFailure::Unavailable)?;
    let current = decrypt_python_fernet_ciphertext(
        encryption_key,
        credentials
            .current_control_secret_ciphertext
            .as_ciphertext(),
    )
    .map_err(|_| ControlAuthFailure::Unavailable)?;
    if current.trim().is_empty() {
        return Err(ControlAuthFailure::Unavailable);
    }

    let previous = match credentials.previous_control_secret_ciphertext.as_ref() {
        Some(ciphertext) => {
            let expires_at_unix_ms = credentials
                .transition_expires_at_unix_ms
                .ok_or(ControlAuthFailure::Unavailable)?;
            let expires_at_unix_ms =
                u64::try_from(expires_at_unix_ms).map_err(|_| ControlAuthFailure::Unavailable)?;
            let expires_at_unix_secs = expires_at_unix_ms / 1_000;
            if expires_at_unix_secs > now_unix_secs() {
                let previous =
                    decrypt_python_fernet_ciphertext(encryption_key, ciphertext.as_ciphertext())
                        .map_err(|_| ControlAuthFailure::Unavailable)?;
                if previous.trim().is_empty() {
                    return Err(ControlAuthFailure::Unavailable);
                }
                Some((previous, expires_at_unix_secs))
            } else {
                None
            }
        }
        None => None,
    };

    Ok(ResolvedControlCredentials { current, previous })
}

async fn authenticate_bootstrap_control_request(
    state: &AppState,
    headers: &HeaderMap,
    method: &Method,
    uri: &Uri,
    raw_body: &[u8],
    instance_id: &str,
) -> Result<ControlAuthentication, ControlAuthFailure> {
    let configured_instance_id =
        non_empty_environment_value("AETHER_INSTANCE_ID").ok_or(ControlAuthFailure::Unavailable)?;
    if configured_instance_id != instance_id {
        return Err(ControlAuthFailure::Unauthorized);
    }
    let verifier = ControlCredentialVerifier::from_env().ok_or(ControlAuthFailure::Unavailable)?;

    if has_control_signature_v2_headers(headers) {
        let (credentials, bootstrap) = resolve_bootstrap_control_credentials()?;
        return authenticate_with_control_credentials(
            state,
            headers,
            method,
            uri,
            raw_body,
            instance_id,
            &credentials,
            ControlCredentialSource::BootstrapV2(bootstrap),
        )
        .await;
    }

    if verifier.verify_control_request(headers) {
        Ok(ControlAuthentication {
            version: ControlAuthVersion::LegacyBearer,
            source: ControlCredentialSource::BootstrapLegacy,
        })
    } else {
        Err(ControlAuthFailure::Unauthorized)
    }
}

fn resolve_bootstrap_control_credentials(
) -> Result<(ResolvedControlCredentials, BootstrapCredentialMaterial), ControlAuthFailure> {
    let current = non_empty_environment_value("AETHER_CONTROL_SECRET")
        .ok_or(ControlAuthFailure::Unavailable)?;
    let relay_secret = non_empty_environment_value("AETHER_RELAY_SIGNING_SECRET")
        .ok_or(ControlAuthFailure::Unavailable)?;
    let previous = match (
        non_empty_environment_value("AETHER_CONTROL_SECRET_PREVIOUS"),
        std::env::var("AETHER_CONTROL_SECRET_PREVIOUS_EXPIRES_AT")
            .ok()
            .and_then(|value| value.parse::<u64>().ok()),
    ) {
        (Some(secret), Some(expires_at)) if expires_at > now_unix_secs() => {
            Some((secret, expires_at))
        }
        _ => None,
    };
    let material = BootstrapCredentialMaterial {
        control_secret: current.clone(),
        relay_secret,
    };
    Ok((ResolvedControlCredentials { current, previous }, material))
}

async fn authenticate_with_control_credentials(
    state: &AppState,
    headers: &HeaderMap,
    method: &Method,
    uri: &Uri,
    raw_body: &[u8],
    instance_id: &str,
    credentials: &ResolvedControlCredentials,
    source: ControlCredentialSource,
) -> Result<ControlAuthentication, ControlAuthFailure> {
    if has_control_signature_v2_headers(headers) {
        let previous = credentials
            .previous
            .as_ref()
            .map(|(secret, expires_at)| (secret.as_str(), *expires_at));
        match verify_control_signature_v2(
            headers,
            method,
            uri,
            raw_body,
            ControlSignatureV2Secrets {
                current: &credentials.current,
                previous,
            },
            now_unix_secs(),
        ) {
            ControlSignatureV2Verification::Valid => {
                let nonce = headers
                    .get(super::collaboration::HEADER_CONTROL_NONCE)
                    .and_then(|value| value.to_str().ok())
                    .ok_or(ControlAuthFailure::Unauthorized)?;
                claim_control_nonce(state, instance_id, nonce).await?;
                return Ok(ControlAuthentication {
                    version: ControlAuthVersion::V2,
                    source,
                });
            }
            ControlSignatureV2Verification::Invalid(_) => {
                return Err(ControlAuthFailure::Unauthorized);
            }
            ControlSignatureV2Verification::NotPresent => {
                return Err(ControlAuthFailure::Unauthorized);
            }
        }
    }

    if bearer_matches_control_credentials(headers, credentials) {
        Ok(ControlAuthentication {
            version: ControlAuthVersion::LegacyBearer,
            source,
        })
    } else {
        Err(ControlAuthFailure::Unauthorized)
    }
}

fn bearer_matches_control_credentials(
    headers: &HeaderMap,
    credentials: &ResolvedControlCredentials,
) -> bool {
    let Some(token) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return false;
    };
    let current_matches = constant_time_eq(token.as_bytes(), credentials.current.as_bytes());
    let previous_matches = credentials
        .previous
        .as_ref()
        .filter(|(_, expires_at)| *expires_at > now_unix_secs())
        .is_some_and(|(secret, _)| constant_time_eq(token.as_bytes(), secret.as_bytes()));
    current_matches | previous_matches
}

async fn claim_control_nonce(
    state: &AppState,
    instance_id: &str,
    nonce: &str,
) -> Result<(), ControlAuthFailure> {
    let key = format!(
        "aether-control-v2:{}:{instance_id}:{}:{nonce}",
        instance_id.len(),
        nonce.len()
    );
    let claimed = state
        .runtime_state
        .lock_try_acquire(
            &key,
            CONTROL_NONCE_LOCK_OWNER,
            Duration::from_secs(CONTROL_SIGNATURE_V2_MAX_AGE_SECS),
        )
        .await
        .map_err(|_| ControlAuthFailure::Unavailable)?;
    if claimed.is_some() {
        Ok(())
    } else {
        Err(ControlAuthFailure::Unauthorized)
    }
}

fn build_persisted_credential_rotation(
    state: &AppState,
    input: &CredentialRotationInput,
    raw_body: &[u8],
    updated_at_unix_ms: i64,
) -> Result<IntegrationCredentialRotation, RelayError> {
    if input.control_secret.trim().is_empty() || input.relay_signing_secret.trim().is_empty() {
        return Err(RelayError::InvalidConfig(
            "control and relay credentials must be non-empty".into(),
        ));
    }
    if constant_time_eq(
        input.control_secret.as_bytes(),
        input.relay_signing_secret.as_bytes(),
    ) {
        return Err(RelayError::InvalidConfig(
            "control and relay credentials must differ".into(),
        ));
    }
    let transition_expires_at_unix_ms = i64::try_from(input.transition_expires_at)
        .ok()
        .and_then(|value| value.checked_mul(1_000))
        .ok_or_else(|| RelayError::InvalidConfig("invalid credential transition expiry".into()))?;
    if transition_expires_at_unix_ms <= updated_at_unix_ms {
        return Err(RelayError::InvalidConfig(
            "credential transition expiry must be in the future".into(),
        ));
    }
    let encryption_key = state
        .data
        .encryption_key()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| RelayError::Internal("data encryption key is unavailable".into()))?;
    let control_secret_ciphertext =
        encrypt_python_fernet_plaintext(encryption_key, &input.control_secret)
            .map_err(|_| RelayError::Internal("encrypt control credential".into()))?;
    let relay_secret_ciphertext =
        encrypt_python_fernet_plaintext(encryption_key, &input.relay_signing_secret)
            .map_err(|_| RelayError::Internal("encrypt relay credential".into()))?;
    let stable_payload_sha256 = control_signature_v2_body_sha256_hex(raw_body);
    IntegrationCredentialRotation::new(
        OpaqueCredentialCiphertext::new(control_secret_ciphertext)
            .map_err(|_| RelayError::InvalidConfig("invalid control credential".into()))?,
        OpaqueCredentialCiphertext::new(relay_secret_ciphertext)
            .map_err(|_| RelayError::InvalidConfig("invalid relay credential".into()))?,
        Some(transition_expires_at_unix_ms),
        input.revoke_previous,
        input.id.clone(),
        stable_payload_sha256,
    )
    .map_err(|error| RelayError::InvalidConfig(error.to_string()))
}

fn build_bootstrap_credential_rotation(
    state: &AppState,
    bootstrap: &BootstrapCredentialMaterial,
    input: &CredentialRotationInput,
    raw_body: &[u8],
    updated_at_unix_ms: i64,
) -> Result<BootstrapIntegrationCredentialRotation, RelayError> {
    if input.revoke_previous {
        return Err(RelayError::InvalidConfig(
            "bootstrap credential rotation cannot revoke previous credentials".into(),
        ));
    }
    let rotation = build_persisted_credential_rotation(state, input, raw_body, updated_at_unix_ms)?;
    let encryption_key = state
        .data
        .encryption_key()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| RelayError::Internal("data encryption key is unavailable".into()))?;
    let bootstrap_control_ciphertext =
        encrypt_python_fernet_plaintext(encryption_key, &bootstrap.control_secret)
            .map_err(|_| RelayError::Internal("encrypt bootstrap control credential".into()))?;
    let bootstrap_relay_ciphertext =
        encrypt_python_fernet_plaintext(encryption_key, &bootstrap.relay_secret)
            .map_err(|_| RelayError::Internal("encrypt bootstrap relay credential".into()))?;
    BootstrapIntegrationCredentialRotation::new(
        OpaqueCredentialCiphertext::new(bootstrap_control_ciphertext)
            .map_err(|_| RelayError::Internal("invalid bootstrap control credential".into()))?,
        OpaqueCredentialCiphertext::new(bootstrap_relay_ciphertext)
            .map_err(|_| RelayError::Internal("invalid bootstrap relay credential".into()))?,
        rotation,
    )
    .map_err(|error| RelayError::InvalidConfig(error.to_string()))
}

fn non_empty_environment_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn now_unix_secs() -> u64 {
    Utc::now().timestamp().max(0) as u64
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::{Mutex, OnceLock};

    use aether_crypto::{
        decrypt_python_fernet_ciphertext, encrypt_python_fernet_plaintext,
        DEVELOPMENT_ENCRYPTION_KEY,
    };
    use aether_data::repository::integration_configs::{
        IntegrationConfigCasResult, IntegrationConfigUpdate, IntegrationCredentialRotation,
        OpaqueCredentialCiphertext, PersistedIntegrationConfig,
    };
    use aether_data::{DatabaseDriver, SqlDatabaseConfig, SqlPoolConfig};
    use axum::body::{to_bytes, Body};
    use axum::http::{header::CONTENT_TYPE, Method, Request, StatusCode, Uri};
    use axum::Router;
    use chrono::Utc;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use tower::ServiceExt;

    use super::{
        contract_capabilities, instance_config_from_persisted, mount_integrations_routes,
        validate_requested_execution_mode, RoutingMode,
    };
    use crate::data::GatewayDataConfig;
    use crate::relay::collaboration::{
        control_signature_v2_body_sha256_hex, control_signature_v2_canonical_payload,
        HEADER_CONTROL_BODY_SHA256, HEADER_CONTROL_NONCE, HEADER_CONTROL_SIGNATURE,
        HEADER_CONTROL_SIGNATURE_VERSION, HEADER_CONTROL_TIMESTAMP, HEADER_INSTANCE_ID,
    };
    use crate::relay::RelayEngineConfig;
    use crate::state::AppState;

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

    fn integrations_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    async fn integration_test_state(encryption_key: Option<&str>) -> AppState {
        let mut pool = SqlPoolConfig::default();
        pool.min_connections = 0;
        pool.max_connections = 1;
        let database = SqlDatabaseConfig::new(DatabaseDriver::Sqlite, "sqlite::memory:", pool)
            .expect("sqlite test database configuration should be valid");
        let mut data_config = GatewayDataConfig::from_database_config(database);
        if let Some(encryption_key) = encryption_key {
            data_config = data_config.with_encryption_key(encryption_key);
        }
        let mut state = AppState::new()
            .expect("gateway state should build")
            .with_data_config(data_config)
            .expect("gateway data state should build");
        state
            .run_database_migrations()
            .await
            .expect("sqlite relay migrations should run");
        let mut relay_config = RelayEngineConfig::default();
        relay_config.enabled = true;
        state.configure_relay_engine_with_config(relay_config);

        state
    }

    async fn integration_export_router() -> Router {
        let state = integration_test_state(None).await;

        crate::relay::export_api::mount_export_routes(mount_integrations_routes(
            Router::<AppState>::new(),
        ))
        .with_state(state)
    }

    async fn seed_persisted_credentials(
        state: &AppState,
        instance_id: &str,
        control_secret: &str,
        relay_secret: &str,
        encryption_key: &str,
    ) {
        let store = state
            .relay_integration_config_store()
            .expect("integration config store should be available");
        let control_ciphertext = encrypt_python_fernet_plaintext(encryption_key, control_secret)
            .expect("control secret should encrypt");
        let relay_ciphertext = encrypt_python_fernet_plaintext(encryption_key, relay_secret)
            .expect("relay secret should encrypt");
        let rotation = IntegrationCredentialRotation::new(
            OpaqueCredentialCiphertext::new(control_ciphertext)
                .expect("control ciphertext should be accepted"),
            OpaqueCredentialCiphertext::new(relay_ciphertext)
                .expect("relay ciphertext should be accepted"),
            None,
            false,
            format!("seed-{instance_id}"),
            format!("{:064x}", 1),
        )
        .expect("seed rotation should be valid");
        let update = IntegrationConfigUpdate {
            route_profile: "default".to_string(),
            execution_mode: "direct_channel".to_string(),
            enabled: true,
            capability_version: "0.1.0".to_string(),
            updated_at_unix_ms: Utc::now().timestamp_millis(),
        };
        let outcome = store
            .compare_and_set_with_route_profile_outbox_and_credential_rotation(
                instance_id,
                0,
                &update,
                &rotation,
            )
            .await
            .expect("seed credentials should persist");
        assert!(matches!(outcome, IntegrationConfigCasResult::Applied(_)));
    }

    fn signed_v2_update_request(
        path: &str,
        instance_id: &str,
        control_secret: &str,
        raw_body: &[u8],
        nonce: &str,
    ) -> Request<Body> {
        let method = Method::PUT;
        let uri: Uri = path.parse().expect("control URI should parse");
        let timestamp = Utc::now().timestamp().max(0).to_string();
        let body_sha256 = control_signature_v2_body_sha256_hex(raw_body);
        let canonical = control_signature_v2_canonical_payload(
            &method,
            &uri,
            instance_id,
            &timestamp,
            nonce,
            &body_sha256,
        );
        let mut mac = Hmac::<Sha256>::new_from_slice(control_secret.as_bytes())
            .expect("control secret should initialize HMAC");
        mac.update(canonical.as_bytes());
        let signature = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();

        Request::builder()
            .method(method)
            .uri(uri)
            .header(CONTENT_TYPE, "application/json")
            .header(HEADER_INSTANCE_ID, instance_id)
            .header(HEADER_CONTROL_SIGNATURE_VERSION, "v2")
            .header(HEADER_CONTROL_TIMESTAMP, timestamp)
            .header(HEADER_CONTROL_NONCE, nonce)
            .header(HEADER_CONTROL_BODY_SHA256, body_sha256)
            .header(HEADER_CONTROL_SIGNATURE, signature)
            .body(Body::from(raw_body.to_vec()))
            .expect("v2 control request should build")
    }

    fn instance_status_request(instance_id: &str, control_secret: &str) -> Request<Body> {
        Request::builder()
            .method(Method::GET)
            .uri(format!(
                "/api/integrations/new-api/v1/instances/{instance_id}/status"
            ))
            .header(HEADER_INSTANCE_ID, instance_id)
            .header("authorization", format!("Bearer {control_secret}"))
            .body(Body::empty())
            .expect("instance status request should build")
    }

    #[tokio::test]
    async fn capabilities_only_advertise_active_contract_mode_and_format() {
        let state = integration_test_state(Some(DEVELOPMENT_ENCRYPTION_KEY)).await;
        let capabilities = contract_capabilities(&state);

        assert_eq!(capabilities.supported_modes, vec!["direct_channel"]);
        assert_eq!(capabilities.supported_formats, vec!["openai"]);
        assert!(capabilities
            .features
            .iter()
            .any(|feature| feature == "credential_rotation_v1"));
        assert!(capabilities
            .features
            .iter()
            .any(|feature| feature == "control_hmac_v2"));
    }

    #[test]
    fn reserved_execution_modes_fail_closed() {
        assert!(validate_requested_execution_mode(RoutingMode::DirectChannel).is_ok());
        assert!(validate_requested_execution_mode(RoutingMode::Disabled).is_ok());
        assert!(validate_requested_execution_mode(RoutingMode::ParallelShadow).is_err());
        assert!(validate_requested_execution_mode(RoutingMode::AetherDecision).is_err());
    }

    #[test]
    fn disabled_execution_mode_is_a_valid_nonexecuting_configuration() {
        let disabled: RoutingMode = serde_json::from_str("\"disabled\"")
            .expect("disabled is part of the integration contract");

        assert!(validate_requested_execution_mode(disabled).is_ok());
        assert!(!disabled.allows_upstream_execution());
    }

    #[test]
    fn persisted_disabled_execution_mode_round_trips_as_a_nonexecuting_configuration() {
        let config = instance_config_from_persisted(PersistedIntegrationConfig {
            instance_id: "aether-primary".to_string(),
            route_profile: "balanced".to_string(),
            execution_mode: "disabled".to_string(),
            enabled: false,
            capability_version: "0.1.0".to_string(),
            revision: 3,
            updated_at_unix_ms: 1_784_073_600_000,
        })
        .expect("disabled config should remain readable");

        assert_eq!(config.execution_mode, RoutingMode::Disabled);
        assert!(!config.execution_mode.allows_upstream_execution());
    }

    #[tokio::test]
    async fn instance_status_marks_missing_config_unhealthy_without_fabricating_direct_mode() {
        let _lock = integrations_env_lock()
            .lock()
            .expect("integration env lock should acquire");
        let _instance_id = EnvVarGuard::set("AETHER_INSTANCE_ID", "aether-primary");
        let _control_secret = EnvVarGuard::set("AETHER_CONTROL_SECRET", "bootstrap-control");
        let _relay_secret = EnvVarGuard::set("AETHER_RELAY_SIGNING_SECRET", "bootstrap-relay");
        let state = integration_test_state(Some(DEVELOPMENT_ENCRYPTION_KEY)).await;
        let app = mount_integrations_routes(Router::<AppState>::new()).with_state(state);

        let response = app
            .oneshot(instance_status_request(
                "aether-primary",
                "bootstrap-control",
            ))
            .await
            .expect("missing-config instance status should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let status: serde_json::Value = serde_json::from_slice(
            &to_bytes(response.into_body(), 1024 * 1024)
                .await
                .expect("missing-config status body should read"),
        )
        .expect("missing-config status should be JSON");

        assert_eq!(status["healthy"], false);
        assert!(status["last_sync_at"].is_null());
        assert_eq!(status["base_revision"], 0);
        assert_eq!(status["uptime_secs"], 0);
        assert_eq!(status["active_channels"], 0);
        assert_eq!(status["routing_mode"], "disabled");
    }

    #[tokio::test]
    async fn instance_status_requires_enabled_direct_channel_configuration() {
        let _lock = integrations_env_lock()
            .lock()
            .expect("integration env lock should acquire");
        let _instance_id = EnvVarGuard::set("AETHER_INSTANCE_ID", "aether-primary");
        let _control_secret = EnvVarGuard::set("AETHER_CONTROL_SECRET", "bootstrap-control");
        let _relay_secret = EnvVarGuard::set("AETHER_RELAY_SIGNING_SECRET", "bootstrap-relay");
        let state = integration_test_state(Some(DEVELOPMENT_ENCRYPTION_KEY)).await;
        seed_persisted_credentials(
            &state,
            "aether-primary",
            "persisted-control",
            "persisted-relay",
            DEVELOPMENT_ENCRYPTION_KEY,
        )
        .await;
        let store = state
            .relay_integration_config_store()
            .expect("integration config store should be available");
        let app = mount_integrations_routes(Router::<AppState>::new()).with_state(state);

        let ready = app
            .clone()
            .oneshot(instance_status_request(
                "aether-primary",
                "persisted-control",
            ))
            .await
            .expect("ready instance status should respond");
        assert_eq!(ready.status(), StatusCode::OK);
        let ready: serde_json::Value = serde_json::from_slice(
            &to_bytes(ready.into_body(), 1024 * 1024)
                .await
                .expect("ready status body should read"),
        )
        .expect("ready status should be JSON");
        assert_eq!(ready["healthy"], true);
        assert_eq!(ready["routing_mode"], "direct_channel");

        let disabled_at_unix_ms = 1_784_073_600_000;
        let disabled_outcome = store
            .compare_and_set(
                "aether-primary",
                1,
                &IntegrationConfigUpdate {
                    route_profile: "default".to_string(),
                    execution_mode: "direct_channel".to_string(),
                    enabled: false,
                    capability_version: "0.1.0".to_string(),
                    updated_at_unix_ms: disabled_at_unix_ms,
                },
            )
            .await
            .expect("disabled config should persist");
        assert!(matches!(
            disabled_outcome,
            IntegrationConfigCasResult::Applied(_)
        ));

        let disabled = app
            .clone()
            .oneshot(instance_status_request(
                "aether-primary",
                "persisted-control",
            ))
            .await
            .expect("disabled instance status should respond");
        assert_eq!(disabled.status(), StatusCode::OK);
        let disabled: serde_json::Value = serde_json::from_slice(
            &to_bytes(disabled.into_body(), 1024 * 1024)
                .await
                .expect("disabled status body should read"),
        )
        .expect("disabled status should be JSON");
        assert_eq!(disabled["healthy"], false);
        assert_eq!(disabled["base_revision"], 2);
        assert_eq!(disabled["routing_mode"], "direct_channel");
        let expected_last_sync =
            chrono::DateTime::<Utc>::from_timestamp_millis(disabled_at_unix_ms)
                .expect("disabled config timestamp should be valid")
                .to_rfc3339();
        assert_eq!(
            disabled["last_sync_at"].as_str(),
            Some(expected_last_sync.as_str()),
        );

        let inactive_mode_outcome = store
            .compare_and_set(
                "aether-primary",
                2,
                &IntegrationConfigUpdate {
                    route_profile: "default".to_string(),
                    execution_mode: "disabled".to_string(),
                    enabled: true,
                    capability_version: "0.1.0".to_string(),
                    updated_at_unix_ms: disabled_at_unix_ms.saturating_add(1),
                },
            )
            .await
            .expect("disabled-mode config should persist");
        assert!(matches!(
            inactive_mode_outcome,
            IntegrationConfigCasResult::Applied(_)
        ));

        let inactive_mode = app
            .oneshot(instance_status_request(
                "aether-primary",
                "persisted-control",
            ))
            .await
            .expect("inactive-mode instance status should respond");
        assert_eq!(inactive_mode.status(), StatusCode::OK);
        let inactive_mode: serde_json::Value = serde_json::from_slice(
            &to_bytes(inactive_mode.into_body(), 1024 * 1024)
                .await
                .expect("inactive-mode status body should read"),
        )
        .expect("inactive-mode status should be JSON");
        assert_eq!(inactive_mode["healthy"], false);
        assert_eq!(inactive_mode["routing_mode"], "disabled");
    }

    #[tokio::test]
    async fn instance_status_marks_a_disabled_relay_engine_unhealthy() {
        let _lock = integrations_env_lock()
            .lock()
            .expect("integration env lock should acquire");
        let _instance_id = EnvVarGuard::set("AETHER_INSTANCE_ID", "aether-primary");
        let _control_secret = EnvVarGuard::set("AETHER_CONTROL_SECRET", "bootstrap-control");
        let _relay_secret = EnvVarGuard::set("AETHER_RELAY_SIGNING_SECRET", "bootstrap-relay");
        let mut state = integration_test_state(Some(DEVELOPMENT_ENCRYPTION_KEY)).await;
        seed_persisted_credentials(
            &state,
            "aether-primary",
            "persisted-control",
            "persisted-relay",
            DEVELOPMENT_ENCRYPTION_KEY,
        )
        .await;
        state.configure_relay_engine_with_config(RelayEngineConfig::default());
        let app = mount_integrations_routes(Router::<AppState>::new()).with_state(state);

        let response = app
            .oneshot(instance_status_request(
                "aether-primary",
                "persisted-control",
            ))
            .await
            .expect("disabled-engine instance status should respond");
        assert_eq!(response.status(), StatusCode::OK);
        let status: serde_json::Value = serde_json::from_slice(
            &to_bytes(response.into_body(), 1024 * 1024)
                .await
                .expect("disabled-engine status body should read"),
        )
        .expect("disabled-engine status should be JSON");

        assert_eq!(status["healthy"], false);
        assert_eq!(status["active_channels"], 0);
        assert_eq!(status["routing_mode"], "direct_channel");
    }

    #[tokio::test]
    async fn authenticated_instance_update_publishes_the_route_transition_to_the_export_outbox() {
        let _lock = integrations_env_lock()
            .lock()
            .expect("integration env lock should acquire");
        let _instance_id = EnvVarGuard::set("AETHER_INSTANCE_ID", "aether-primary");
        let _control_secret = EnvVarGuard::set("AETHER_CONTROL_SECRET", "control-token");
        let _relay_secret = EnvVarGuard::set("AETHER_RELAY_SIGNING_SECRET", "relay-token");
        let _export_token = EnvVarGuard::set("AETHER_OUTBOUND_EXPORT_TOKEN", "export-token");
        let app = integration_export_router().await;

        let update = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/integrations/new-api/v1/instances/aether-primary")
                    .header("authorization", "Bearer control-token")
                    .header(HEADER_INSTANCE_ID, "aether-primary")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"route_profile":"balanced","base_revision":0}"#,
                    ))
                    .expect("instance update request should build"),
            )
            .await
            .expect("instance update should respond");
        assert_eq!(update.status(), StatusCode::OK);

        let export = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/export/events")
                    .header("authorization", "Bearer export-token")
                    .body(Body::empty())
                    .expect("export request should build"),
            )
            .await
            .expect("export request should respond");
        assert_eq!(export.status(), StatusCode::OK);
        let payload: serde_json::Value = serde_json::from_slice(
            &to_bytes(export.into_body(), 1024 * 1024)
                .await
                .expect("export response body should read"),
        )
        .expect("export response should be JSON");
        assert_eq!(
            payload["events"].as_array().map(|events| events.len()),
            Some(1)
        );
        assert_eq!(payload["next_cursor"], "1");
        assert_eq!(payload["has_more"], false);
        assert_eq!(
            payload["events"][0]["event_id"],
            "route-profile:aether-primary:1"
        );
        assert_eq!(payload["events"][0]["event_type"], "route_decision_changed");
        assert_eq!(
            payload["events"][0]["payload"]["previous_route_profile"],
            "default"
        );
        assert_eq!(payload["events"][0]["payload"]["route_profile"], "balanced");
        assert_eq!(payload["events"][0]["payload"]["revision"], 1);

        let conflict = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/integrations/new-api/v1/instances/aether-primary")
                    .header("authorization", "Bearer control-token")
                    .header(HEADER_INSTANCE_ID, "aether-primary")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"route_profile":"cost-first","base_revision":0}"#,
                    ))
                    .expect("stale instance update request should build"),
            )
            .await
            .expect("stale instance update should respond");
        assert_eq!(conflict.status(), StatusCode::CONFLICT);

        let export_after_conflict = app
            .oneshot(
                Request::builder()
                    .uri("/api/export/events")
                    .header("authorization", "Bearer export-token")
                    .body(Body::empty())
                    .expect("post-conflict export request should build"),
            )
            .await
            .expect("post-conflict export request should respond");
        assert_eq!(export_after_conflict.status(), StatusCode::OK);
        let after_conflict_payload: serde_json::Value = serde_json::from_slice(
            &to_bytes(export_after_conflict.into_body(), 1024 * 1024)
                .await
                .expect("post-conflict export body should read"),
        )
        .expect("post-conflict export response should be JSON");
        assert_eq!(
            after_conflict_payload["events"]
                .as_array()
                .map(|events| events.len()),
            Some(1)
        );
        assert_eq!(
            after_conflict_payload["events"][0]["event_id"],
            "route-profile:aether-primary:1"
        );
    }

    #[tokio::test]
    async fn v2_credential_rotation_uses_persisted_credentials_rejects_replay_and_keeps_conflicts_out_of_the_outbox(
    ) {
        let _lock = integrations_env_lock()
            .lock()
            .expect("integration env lock should acquire");
        let _instance_id = EnvVarGuard::set("AETHER_INSTANCE_ID", "aether-primary");
        let _control_secret = EnvVarGuard::set("AETHER_CONTROL_SECRET", "bootstrap-control");
        let _relay_secret = EnvVarGuard::set("AETHER_RELAY_SIGNING_SECRET", "bootstrap-relay");
        let _export_token = EnvVarGuard::set("AETHER_OUTBOUND_EXPORT_TOKEN", "export-token");
        let state = integration_test_state(Some(DEVELOPMENT_ENCRYPTION_KEY)).await;
        seed_persisted_credentials(
            &state,
            "aether-primary",
            "persisted-control-v1",
            "persisted-relay-v1",
            DEVELOPMENT_ENCRYPTION_KEY,
        )
        .await;
        let store = state
            .relay_integration_config_store()
            .expect("integration config store should remain available");
        let app =
            crate::relay::export_api::mount_export_routes(mount_integrations_routes(Router::<
                AppState,
            >::new(
            )))
            .with_state(state);
        let path = "/api/integrations/new-api/v1/instances/aether-primary";
        let transition_expires_at = Utc::now().timestamp().max(0) + 600;
        let body = format!(
            r#"{{"route_profile":"balanced","base_revision":1,"credential_rotation":{{"id":"rotation-v2","control_secret":"persisted-control-v2","relay_signing_secret":"persisted-relay-v2","transition_expires_at":{transition_expires_at},"revoke_previous":false}}}}"#,
        )
        .into_bytes();

        let update = app
            .clone()
            .oneshot(signed_v2_update_request(
                path,
                "aether-primary",
                "persisted-control-v1",
                &body,
                "nonce-control-rotation-0001",
            ))
            .await
            .expect("rotation update should respond");
        assert_eq!(update.status(), StatusCode::OK);
        let update_body = to_bytes(update.into_body(), 1024 * 1024)
            .await
            .expect("rotation response body should read");
        let update_text = String::from_utf8(update_body.to_vec())
            .expect("rotation response should be UTF-8 JSON");
        assert!(!update_text.contains("persisted-control-v2"));
        assert!(!update_text.contains("persisted-relay-v2"));
        let update_payload: serde_json::Value =
            serde_json::from_str(&update_text).expect("rotation response should be JSON");
        assert_eq!(update_payload["base_revision"], 2);
        assert_eq!(
            update_payload["credential_rotation_ack"]["rotation_id"],
            "rotation-v2"
        );
        assert_eq!(
            update_payload["credential_rotation_ack"]["credential_revision"],
            2
        );
        assert_eq!(
            update_payload["credential_rotation_ack"]["transition_expires_at"],
            transition_expires_at
        );
        assert_eq!(
            update_payload["credential_rotation_ack"]["state"],
            "applied"
        );

        let replay = app
            .clone()
            .oneshot(signed_v2_update_request(
                path,
                "aether-primary",
                "persisted-control-v1",
                &body,
                "nonce-control-rotation-0001",
            ))
            .await
            .expect("replayed update should respond");
        assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);

        let stale_body = format!(
            r#"{{"route_profile":"cost-first","base_revision":1,"credential_rotation":{{"id":"rotation-v3","control_secret":"persisted-control-v3","relay_signing_secret":"persisted-relay-v3","transition_expires_at":{transition_expires_at},"revoke_previous":false}}}}"#,
        )
        .into_bytes();
        let conflict = app
            .clone()
            .oneshot(signed_v2_update_request(
                path,
                "aether-primary",
                "persisted-control-v1",
                &stale_body,
                "nonce-control-rotation-0002",
            ))
            .await
            .expect("stale rotation should respond");
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        let persisted = store
            .get_credentials("aether-primary")
            .await
            .expect("persisted credentials should read")
            .expect("persisted credentials should exist");
        assert_eq!(persisted.rotation_id, "rotation-v2");
        assert_eq!(persisted.credential_revision, 2);

        let export = app
            .oneshot(
                Request::builder()
                    .uri("/api/export/events")
                    .header("authorization", "Bearer export-token")
                    .body(Body::empty())
                    .expect("export request should build"),
            )
            .await
            .expect("export request should respond");
        assert_eq!(export.status(), StatusCode::OK);
        let export_payload: serde_json::Value = serde_json::from_slice(
            &to_bytes(export.into_body(), 1024 * 1024)
                .await
                .expect("export body should read"),
        )
        .expect("export response should be JSON");
        assert_eq!(export_payload["events"].as_array().map(Vec::len), Some(1));
        assert_eq!(
            export_payload["events"][0]["event_id"],
            "route-profile:aether-primary:2"
        );
    }

    #[tokio::test]
    async fn bootstrap_v2_rotation_persists_the_verified_environment_pair_for_retry() {
        let _lock = integrations_env_lock()
            .lock()
            .expect("integration env lock should acquire");
        let _instance_id = EnvVarGuard::set("AETHER_INSTANCE_ID", "aether-primary");
        let _control_secret = EnvVarGuard::set("AETHER_CONTROL_SECRET", "bootstrap-control-v1");
        let _relay_secret = EnvVarGuard::set("AETHER_RELAY_SIGNING_SECRET", "bootstrap-relay-v1");
        let state = integration_test_state(Some(DEVELOPMENT_ENCRYPTION_KEY)).await;
        let store = state
            .relay_integration_config_store()
            .expect("integration config store should be available");
        let app = mount_integrations_routes(Router::<AppState>::new()).with_state(state);
        let path = "/api/integrations/new-api/v1/instances/aether-primary";
        let transition_expires_at = Utc::now().timestamp().max(0) + 600;
        let body = format!(
            r#"{{"base_revision":0,"credential_rotation":{{"id":"bootstrap-rotation-v2","control_secret":"persisted-control-v2","relay_signing_secret":"persisted-relay-v2","transition_expires_at":{transition_expires_at},"revoke_previous":false}}}}"#,
        )
        .into_bytes();

        let applied = app
            .clone()
            .oneshot(signed_v2_update_request(
                path,
                "aether-primary",
                "bootstrap-control-v1",
                &body,
                "nonce-bootstrap-rotation-0001",
            ))
            .await
            .expect("bootstrap rotation should respond");
        assert_eq!(applied.status(), StatusCode::OK);
        let response_body = to_bytes(applied.into_body(), 1024 * 1024)
            .await
            .expect("bootstrap response should read");
        let response_text = String::from_utf8(response_body.to_vec())
            .expect("bootstrap response should be UTF-8 JSON");
        assert!(!response_text.contains("bootstrap-control-v1"));
        assert!(!response_text.contains("bootstrap-relay-v1"));
        assert!(!response_text.contains("persisted-control-v2"));
        assert!(!response_text.contains("persisted-relay-v2"));
        let response: serde_json::Value =
            serde_json::from_str(&response_text).expect("bootstrap response should be JSON");
        assert_eq!(response["base_revision"], 1);
        assert_eq!(
            response["credential_rotation_ack"]["rotation_id"],
            "bootstrap-rotation-v2"
        );
        assert_eq!(
            response["credential_rotation_ack"]["credential_revision"],
            1
        );

        let credentials = store
            .get_credentials("aether-primary")
            .await
            .expect("bootstrap credentials should load")
            .expect("bootstrap credentials should persist");
        assert_eq!(credentials.credential_revision, 1);
        assert_eq!(
            decrypt_python_fernet_ciphertext(
                DEVELOPMENT_ENCRYPTION_KEY,
                credentials
                    .current_control_secret_ciphertext
                    .as_ciphertext(),
            )
            .expect("current control credential should decrypt"),
            "persisted-control-v2"
        );
        assert_eq!(
            decrypt_python_fernet_ciphertext(
                DEVELOPMENT_ENCRYPTION_KEY,
                credentials
                    .previous_control_secret_ciphertext
                    .as_ref()
                    .expect("bootstrap control predecessor should persist")
                    .as_ciphertext(),
            )
            .expect("previous control credential should decrypt"),
            "bootstrap-control-v1"
        );
        assert_eq!(
            decrypt_python_fernet_ciphertext(
                DEVELOPMENT_ENCRYPTION_KEY,
                credentials
                    .previous_relay_secret_ciphertext
                    .as_ref()
                    .expect("bootstrap relay predecessor should persist")
                    .as_ciphertext(),
            )
            .expect("previous relay credential should decrypt"),
            "bootstrap-relay-v1"
        );

        let retry = app
            .oneshot(signed_v2_update_request(
                path,
                "aether-primary",
                "bootstrap-control-v1",
                &body,
                "nonce-bootstrap-rotation-0002",
            ))
            .await
            .expect("bootstrap retry should respond");
        assert_eq!(retry.status(), StatusCode::OK);
        let after_retry = store
            .get_credentials("aether-primary")
            .await
            .expect("credentials should load after retry")
            .expect("credentials should still exist after retry");
        assert_eq!(after_retry.rotation_id, "bootstrap-rotation-v2");
        assert_eq!(after_retry.credential_revision, 1);
    }

    #[tokio::test]
    async fn rotation_ack_reads_the_persisted_credential_revision_after_a_config_update() {
        let _lock = integrations_env_lock()
            .lock()
            .expect("integration env lock should acquire");
        let _instance_id = EnvVarGuard::set("AETHER_INSTANCE_ID", "aether-primary");
        let _control_secret = EnvVarGuard::set("AETHER_CONTROL_SECRET", "environment-control");
        let _relay_secret = EnvVarGuard::set("AETHER_RELAY_SIGNING_SECRET", "environment-relay");
        let state = integration_test_state(Some(DEVELOPMENT_ENCRYPTION_KEY)).await;
        seed_persisted_credentials(
            &state,
            "aether-primary",
            "persisted-control-v1",
            "persisted-relay-v1",
            DEVELOPMENT_ENCRYPTION_KEY,
        )
        .await;
        let store = state
            .relay_integration_config_store()
            .expect("integration config store should be available");
        let app = mount_integrations_routes(Router::<AppState>::new()).with_state(state);
        let path = "/api/integrations/new-api/v1/instances/aether-primary";

        let configuration_update = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(path)
                    .header(HEADER_INSTANCE_ID, "aether-primary")
                    .header("authorization", "Bearer persisted-control-v1")
                    .header(CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"route_profile":"balanced","base_revision":1}"#,
                    ))
                    .expect("configuration update should build"),
            )
            .await
            .expect("configuration update should respond");
        assert_eq!(configuration_update.status(), StatusCode::OK);

        let transition_expires_at = Utc::now().timestamp().max(0) + 600;
        let rotation_body = format!(
            r#"{{"base_revision":2,"credential_rotation":{{"id":"rotation-after-config","control_secret":"persisted-control-v2","relay_signing_secret":"persisted-relay-v2","transition_expires_at":{transition_expires_at},"revoke_previous":false}}}}"#,
        )
        .into_bytes();
        let rotation_response = app
            .oneshot(signed_v2_update_request(
                path,
                "aether-primary",
                "persisted-control-v1",
                &rotation_body,
                "nonce-credential-ack-0001",
            ))
            .await
            .expect("rotation should respond");
        assert_eq!(rotation_response.status(), StatusCode::OK);
        let response: serde_json::Value = serde_json::from_slice(
            &to_bytes(rotation_response.into_body(), 1024 * 1024)
                .await
                .expect("rotation response should read"),
        )
        .expect("rotation response should be JSON");
        let credentials = store
            .get_credentials("aether-primary")
            .await
            .expect("credentials should load")
            .expect("credentials should exist");
        assert_eq!(response["base_revision"], 3);
        assert_eq!(
            response["credential_rotation_ack"]["credential_revision"],
            credentials.credential_revision
        );
    }

    #[tokio::test]
    async fn persistent_control_credentials_override_environment_and_partial_v2_cannot_downgrade() {
        let _lock = integrations_env_lock()
            .lock()
            .expect("integration env lock should acquire");
        let _instance_id = EnvVarGuard::set("AETHER_INSTANCE_ID", "aether-primary");
        let _control_secret = EnvVarGuard::set("AETHER_CONTROL_SECRET", "environment-control");
        let _relay_secret = EnvVarGuard::set("AETHER_RELAY_SIGNING_SECRET", "environment-relay");
        let state = integration_test_state(Some(DEVELOPMENT_ENCRYPTION_KEY)).await;
        seed_persisted_credentials(
            &state,
            "aether-primary",
            "persisted-control",
            "persisted-relay",
            DEVELOPMENT_ENCRYPTION_KEY,
        )
        .await;
        let app = mount_integrations_routes(Router::<AppState>::new()).with_state(state);

        let environment_fallback = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/integrations/new-api/v1/capabilities")
                    .header(HEADER_INSTANCE_ID, "aether-primary")
                    .header("authorization", "Bearer environment-control")
                    .body(Body::empty())
                    .expect("environment fallback request should build"),
            )
            .await
            .expect("environment fallback request should respond");
        assert_eq!(environment_fallback.status(), StatusCode::UNAUTHORIZED);

        let persistent = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/integrations/new-api/v1/capabilities")
                    .header(HEADER_INSTANCE_ID, "aether-primary")
                    .header("authorization", "Bearer persisted-control")
                    .body(Body::empty())
                    .expect("persistent credential request should build"),
            )
            .await
            .expect("persistent credential request should respond");
        assert_eq!(persistent.status(), StatusCode::OK);
        let capabilities: serde_json::Value = serde_json::from_slice(
            &to_bytes(persistent.into_body(), 1024 * 1024)
                .await
                .expect("capabilities body should read"),
        )
        .expect("capabilities should be JSON");
        assert!(capabilities["features"]
            .as_array()
            .expect("features should be an array")
            .iter()
            .any(|value| value == "credential_rotation_v1"));
        assert!(capabilities["features"]
            .as_array()
            .expect("features should be an array")
            .iter()
            .any(|value| value == "control_hmac_v2"));

        let partial_v2 = app
            .oneshot(
                Request::builder()
                    .uri("/api/integrations/new-api/v1/capabilities")
                    .header(HEADER_INSTANCE_ID, "aether-primary")
                    .header("authorization", "Bearer persisted-control")
                    .header(HEADER_CONTROL_SIGNATURE_VERSION, "v2")
                    .body(Body::empty())
                    .expect("partial v2 request should build"),
            )
            .await
            .expect("partial v2 request should respond");
        assert_eq!(partial_v2.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn control_v2_binds_the_path_and_exact_raw_body() {
        let _lock = integrations_env_lock()
            .lock()
            .expect("integration env lock should acquire");
        let _instance_id = EnvVarGuard::set("AETHER_INSTANCE_ID", "aether-primary");
        let _control_secret = EnvVarGuard::set("AETHER_CONTROL_SECRET", "environment-control");
        let _relay_secret = EnvVarGuard::set("AETHER_RELAY_SIGNING_SECRET", "environment-relay");
        let state = integration_test_state(Some(DEVELOPMENT_ENCRYPTION_KEY)).await;
        seed_persisted_credentials(
            &state,
            "aether-primary",
            "persisted-primary-control",
            "persisted-primary-relay",
            DEVELOPMENT_ENCRYPTION_KEY,
        )
        .await;
        seed_persisted_credentials(
            &state,
            "aether-other",
            "persisted-other-control",
            "persisted-other-relay",
            DEVELOPMENT_ENCRYPTION_KEY,
        )
        .await;
        let app = mount_integrations_routes(Router::<AppState>::new()).with_state(state);
        let path = "/api/integrations/new-api/v1/instances/aether-primary";
        let signed_body = br#"{"base_revision":1}"#;
        let tampered_body = br#"{"base_revision":1,"enabled":false}"#;
        let signed = signed_v2_update_request(
            path,
            "aether-primary",
            "persisted-primary-control",
            signed_body,
            "nonce-control-body-tamper-0001",
        );
        let (parts, _) = signed.into_parts();
        let tampered = Request::from_parts(parts, Body::from(tampered_body.to_vec()));
        let tampered_response = app
            .clone()
            .oneshot(tampered)
            .await
            .expect("tampered v2 request should respond");
        assert_eq!(tampered_response.status(), StatusCode::UNAUTHORIZED);

        let mismatched_instance = app
            .oneshot(signed_v2_update_request(
                path,
                "aether-other",
                "persisted-other-control",
                signed_body,
                "nonce-control-path-binding-0001",
            ))
            .await
            .expect("instance-mismatched v2 request should respond");
        assert_eq!(mismatched_instance.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn persisted_control_credentials_fail_closed_without_a_data_encryption_key() {
        let _lock = integrations_env_lock()
            .lock()
            .expect("integration env lock should acquire");
        let _instance_id = EnvVarGuard::set("AETHER_INSTANCE_ID", "aether-primary");
        let _control_secret = EnvVarGuard::set("AETHER_CONTROL_SECRET", "environment-control");
        let _relay_secret = EnvVarGuard::set("AETHER_RELAY_SIGNING_SECRET", "environment-relay");
        let state = integration_test_state(None).await;
        seed_persisted_credentials(
            &state,
            "aether-primary",
            "persisted-control",
            "persisted-relay",
            DEVELOPMENT_ENCRYPTION_KEY,
        )
        .await;
        let app = mount_integrations_routes(Router::<AppState>::new()).with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/integrations/new-api/v1/capabilities")
                    .header(HEADER_INSTANCE_ID, "aether-primary")
                    .header("authorization", "Bearer environment-control")
                    .body(Body::empty())
                    .expect("missing-key request should build"),
            )
            .await
            .expect("missing-key request should respond");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
