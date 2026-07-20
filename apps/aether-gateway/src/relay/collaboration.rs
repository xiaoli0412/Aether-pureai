//! New API 协同 — 入站渠道转发
//!
//! 接收 New API 以"Aether 渠道"身份转发的 OpenAI 兼容请求。
//! 使用 X-Aether-Relay-Context (base64url JSON) + X-Aether-Relay-Signature (HMAC-SHA256 hex)。
//!
//! 安全约束：
//! - 不记录、不依赖 New API 用户 API Key、真实用户资料、支付信息或余额
//! - 仅使用匿名 subject_id
//! - 绝不回写 New API 用户金融数据

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use aether_crypto::decrypt_python_fernet_ciphertext;
use aether_data::repository::integration_configs::PersistedIntegrationCredentials;
use aether_runtime_state::RuntimeState;
use async_trait::async_trait;
use axum::http::{Extensions, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::Utc;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use super::error::RelayError;

type HmacSha256 = Hmac<Sha256>;

// ==================== Header Constants ====================

pub const HEADER_INSTANCE_ID: &str = "x-aether-instance-id";
pub const HEADER_RELAY_CONTEXT: &str = "x-aether-relay-context";
pub const HEADER_RELAY_SIGNATURE: &str = "x-aether-relay-signature";
pub const HEADER_ONEAPI_REQUEST_ID: &str = "x-oneapi-request-id";
pub const HEADER_TRACEPARENT: &str = "traceparent";
pub(crate) const HEADER_CONTROL_SIGNATURE_VERSION: &str = "x-aether-signature-version";
pub(crate) const HEADER_CONTROL_TIMESTAMP: &str = "x-aether-timestamp";
pub(crate) const HEADER_CONTROL_NONCE: &str = "x-aether-nonce";
pub(crate) const HEADER_CONTROL_BODY_SHA256: &str = "x-aether-body-sha256";
pub(crate) const HEADER_CONTROL_SIGNATURE: &str = "x-aether-signature";
pub(crate) const USAGE_RELAY_METADATA_KEY: &str = "aether_relay";

/// 签名上下文过期时间窗口
const CONTEXT_MAX_AGE_SECS: i64 = 30;
pub(crate) const CONTROL_SIGNATURE_V2_VERSION: &str = "v2";
pub(crate) const CONTROL_SIGNATURE_V2_MAX_AGE_SECS: u64 = 300;
const CONTROL_SIGNATURE_V2_DOMAIN: &str = "AETHER-CONTROL-V2";
const CONTROL_SIGNATURE_V2_MIN_NONCE_BYTES: usize = 12;
const CONTROL_SIGNATURE_V2_MAX_NONCE_BYTES: usize = 256;
const CONTROL_SIGNATURE_V2_MAX_INSTANCE_ID_BYTES: usize = 128;

// ==================== Relay Context ====================

/// 入站 Relay 上下文（X-Aether-Relay-Context 解码后）
///
/// 不包含用户 API Key、真实姓名、支付信息或余额。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayContext {
    /// Aether 实例 ID
    pub instance_id: String,
    /// 请求 ID（由 New API 生成，用于日志关联）
    pub request_id: String,
    /// 匿名主体标识（不含真实用户信息）
    pub subject_id: String,
    /// Token 主体标识
    pub token_subject_id: String,
    /// 渠道 ID（New API 侧的 Aether 渠道 ID）
    pub channel_id: String,
    /// 用户分组
    pub group: String,
    /// 请求模型
    pub model: String,
    /// 转发格式标识
    pub relay_format: String,
    /// 配置版本号
    pub config_revision: u64,
    /// 过期时间（Unix 秒时间戳）
    pub expires_at: u64,
}

// ==================== Verification Results ====================

/// 签名验证结果
#[derive(Debug)]
pub enum RelayVerification {
    /// 验证通过
    Valid(RelayContext),
    /// 上下文字段无效
    InvalidContext(String),
    /// 签名无效
    InvalidSignature(String),
    /// 上下文已过期
    Expired { request_id: String, expired_at: u64 },
    /// 请求 ID 重放
    Replay { request_id: String },
    /// 重放存储不可用；必须失败关闭
    ReplayCheckFailed(String),
    /// 实例 ID 不匹配
    InstanceMismatch { expected: String, got: String },
    /// 缺少必要 Header
    MissingHeaders(String),
}

#[derive(Debug, Clone)]
pub struct TrustedRelayContext(pub RelayContext);

/// Add the verified New API request ID after provider header construction.
///
/// This keeps the signed correlation ID on every real upstream attempt without
/// allowing an inbound header to impersonate a trusted relay context.
pub(crate) fn apply_trusted_relay_request_id_to_provider_headers(
    parts: &axum::http::request::Parts,
    provider_headers: &mut BTreeMap<String, String>,
) {
    let Some(context) = parts.extensions.get::<TrustedRelayContext>() else {
        return;
    };

    provider_headers.insert(
        HEADER_ONEAPI_REQUEST_ID.to_string(),
        context.0.request_id.clone(),
    );
}

/// Add the authenticated relay correlation fields to the internal execution
/// report context. This is intentionally separate from provider headers so
/// anonymous relay data is available to usage settlement without reaching an
/// upstream provider.
pub(crate) fn apply_trusted_relay_usage_metadata_to_report_context(
    parts: &axum::http::request::Parts,
    report_context: &mut Option<serde_json::Value>,
) {
    let Some(context) = parts.extensions.get::<TrustedRelayContext>() else {
        return;
    };

    // The proxy inserts this extension only after the signed request has also
    // passed persisted integration/profile validation.
    if parts
        .extensions
        .get::<crate::routing::TrustedRelayRoutingProfile>()
        .is_none()
    {
        return;
    }

    let relay_metadata = serde_json::json!({
        "instance_id": context.0.instance_id.clone(),
        "request_id": context.0.request_id.clone(),
        "subject_id": context.0.subject_id.clone(),
        "token_subject_id": context.0.token_subject_id.clone(),
        "channel_id": context.0.channel_id.clone(),
        "group": context.0.group.clone(),
        "model": context.0.model.clone(),
        "relay_format": context.0.relay_format.clone(),
        "config_revision": context.0.config_revision,
    });

    let report_context =
        report_context.get_or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let Some(report_context) = report_context.as_object_mut() else {
        return;
    };
    report_context.insert(USAGE_RELAY_METADATA_KEY.to_string(), relay_metadata);
}

#[derive(Debug)]
pub struct RelayIngressRejection {
    pub status: StatusCode,
    pub message: String,
}

// ==================== Routing Mode ====================

/// 路由模式枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingMode {
    /// 接受转发 → 执行路由 → 返回结果（首期唯一允许真正执行的模式）
    DirectChannel,
    /// 显式停用：保留配置可见性，但绝不执行上游请求。
    Disabled,
    /// 仅做无副作用路由/成本评估，绝不复制真实生成请求
    ParallelShadow,
    /// 仅返回建议 + TTL，不自行执行
    AetherDecision,
}

impl RoutingMode {
    /// 是否允许真正执行上游请求
    pub fn allows_upstream_execution(&self) -> bool {
        matches!(self, RoutingMode::DirectChannel)
    }

    /// 从环境变量读取
    pub fn from_env() -> Self {
        match std::env::var("AETHER_ROUTING_MODE").as_deref() {
            Ok("disabled") => Self::Disabled,
            Ok("parallel_shadow") => Self::ParallelShadow,
            Ok("aether_decision") => Self::AetherDecision,
            _ => Self::DirectChannel,
        }
    }
}

/// 路由建议响应（parallel_shadow / aether_decision 模式）
#[derive(Debug, Clone, Serialize)]
pub struct RouteAdvice {
    pub decision_id: String,
    pub recommended_channel: String,
    pub composite_score: f64,
    pub estimated_cost_quota: String, // 字符串精确表示，不丢失精度
    pub fallback_channels: Vec<String>,
    pub ttl_secs: u64,
    pub issued_at: String,
}

// ==================== Inbound Verifier ====================

/// 入站请求验证器
#[derive(Clone)]
pub struct RelayVerifier {
	/// relay signing secret（预共享，用于 HMAC-SHA256）
	signing_secret: Arc<String>,
	/// 仅在明确的过渡截止时间前有效的上一把 relay signing secret。
	previous_signing_secret: Option<Arc<String>>,
	previous_signing_secret_expires_at: Option<u64>,
	/// 本实例 ID
	instance_id: Arc<String>,
    replay_guard: Arc<dyn ReplayGuard>,
}

#[async_trait]
pub trait ReplayGuard: Send + Sync {
    /// 原子占用重放键。true 表示首次占用，false 表示已存在。
    async fn claim(&self, key: &str, ttl: Duration) -> Result<bool, String>;
}

struct RuntimeReplayGuard {
    runtime_state: RuntimeState,
}

#[async_trait]
impl ReplayGuard for RuntimeReplayGuard {
    async fn claim(&self, key: &str, ttl: Duration) -> Result<bool, String> {
        self.runtime_state
            .lock_try_acquire(key, "relay-verifier", ttl)
            .await
            .map(|lease| lease.is_some())
            .map_err(|error| error.to_string())
    }
}

impl RelayVerifier {
	pub fn new(signing_secret: String, instance_id: String, runtime_state: RuntimeState) -> Self {
		Self::new_with_transition_secret(signing_secret, None, None, instance_id, runtime_state)
	}

    pub(crate) fn matches_instance_id(&self, instance_id: &str) -> bool {
        self.instance_id.as_str() == instance_id
    }

    /// Builds an inbound verifier from the encrypted per-instance credentials
    /// that were already committed by the control plane. A malformed row is
    /// intentionally an error so callers cannot fall back to stale bootstrap
    /// credentials after durable credential state exists.
    pub(crate) fn from_persisted_credentials(
        credentials: &PersistedIntegrationCredentials,
        encryption_key: &str,
        runtime_state: RuntimeState,
        now_unix_secs: u64,
    ) -> Result<Self, ()> {
        let instance_id = credentials.instance_id.trim();
        if instance_id.is_empty() {
            return Err(());
        }
        let current = decrypt_python_fernet_ciphertext(
            encryption_key,
            credentials.current_relay_secret_ciphertext.as_ciphertext(),
        )
        .map_err(|_| ())?;
        if current.trim().is_empty() {
            return Err(());
        }

        let (previous, previous_expires_at) = match (
            credentials.previous_relay_secret_ciphertext.as_ref(),
            credentials.transition_expires_at_unix_ms,
        ) {
            (Some(ciphertext), Some(expires_at_unix_ms)) => {
                let expires_at_unix_ms = u64::try_from(expires_at_unix_ms).map_err(|_| ())?;
                let expires_at_unix_secs = expires_at_unix_ms / 1_000;
                if expires_at_unix_secs > now_unix_secs {
                    let previous = decrypt_python_fernet_ciphertext(
                        encryption_key,
                        ciphertext.as_ciphertext(),
                    )
                    .map_err(|_| ())?;
                    if previous.trim().is_empty() {
                        return Err(());
                    }
                    (Some(previous), Some(expires_at_unix_secs))
                } else {
                    (None, None)
                }
            }
            (None, None) => (None, None),
            _ => return Err(()),
        };

        Ok(Self::new_with_transition_secret(
            current,
            previous,
            previous_expires_at,
            instance_id.to_string(),
            runtime_state,
        ))
    }

	fn new_with_transition_secret(
		signing_secret: String,
		previous_signing_secret: Option<String>,
		previous_signing_secret_expires_at: Option<u64>,
		instance_id: String,
		runtime_state: RuntimeState,
	) -> Self {
		Self {
			signing_secret: Arc::new(signing_secret),
			previous_signing_secret: previous_signing_secret
				.filter(|secret| !secret.trim().is_empty())
				.map(Arc::new),
			previous_signing_secret_expires_at,
			instance_id: Arc::new(instance_id),
			replay_guard: Arc::new(RuntimeReplayGuard { runtime_state }),
		}
	}

	#[cfg(test)]
	pub fn new_with_previous(
		signing_secret: String,
		previous_signing_secret: Option<String>,
		instance_id: String,
		runtime_state: RuntimeState,
	) -> Self {
		Self::new_with_transition_secret(
			signing_secret,
			previous_signing_secret,
			Some(u64::MAX),
			instance_id,
			runtime_state,
		)
	}

    #[cfg(test)]
	pub fn new_with_replay_guard(
		signing_secret: String,
		instance_id: String,
		replay_guard: Arc<dyn ReplayGuard>,
	) -> Self {
		Self {
			signing_secret: Arc::new(signing_secret),
			previous_signing_secret: None,
			previous_signing_secret_expires_at: None,
			instance_id: Arc::new(instance_id),
			replay_guard,
		}
    }

    /// 从环境变量构造
	pub fn from_env(runtime_state: RuntimeState) -> Option<Self> {
		let secret = non_empty_environment_secret("AETHER_RELAY_SIGNING_SECRET")?;
		let previous_secret = non_empty_environment_secret("AETHER_RELAY_SIGNING_SECRET_PREVIOUS");
		let previous_expires_at = environment_expiry("AETHER_RELAY_SIGNING_SECRET_PREVIOUS_EXPIRES_AT");
		let control_secret = non_empty_environment_secret("AETHER_CONTROL_SECRET");
		let previous_control_secret = non_empty_environment_secret("AETHER_CONTROL_SECRET_PREVIOUS");
		let previous_control_expires_at = environment_expiry("AETHER_CONTROL_SECRET_PREVIOUS_EXPIRES_AT");
		if active_credential_domains_overlap(
			&secret,
			previous_secret.as_deref(),
			previous_expires_at,
			control_secret.as_deref(),
			previous_control_secret.as_deref(),
			previous_control_expires_at,
		) {
			return None;
		}
		if non_empty_environment_secret("AETHER_OUTBOUND_EXPORT_TOKEN")
			.is_some_and(|token| !outbound_export_token_is_isolated(&token))
		{
			return None;
		}
		let instance_id = non_empty_environment_secret("AETHER_INSTANCE_ID")?;
		Some(Self::new_with_transition_secret(
			secret,
			previous_secret,
			previous_expires_at,
			instance_id,
			runtime_state,
		))
    }

    /// 验证入站请求的完整性
    ///
    /// 步骤：
    /// 1. 提取 X-Aether-Instance-ID, X-Aether-Relay-Context, X-Aether-Relay-Signature
    /// 2. 验证 HMAC-SHA256(base64url_context_string) == signature
    /// 3. base64url decode -> JSON parse -> RelayContext
    /// 4. 校验 request_id 安全且可作为跟踪 Header
    /// 5. 校验 instance_id 一致
    /// 6. 校验 expires_at 在 30 秒内
    /// 7. 校验 request_id 未被重放
	pub async fn verify(&self, headers: &HeaderMap) -> RelayVerification {
		if self.signing_secret.trim().is_empty() {
			return RelayVerification::InvalidSignature("invalid signing secret".to_string());
		}
		let now_secs = Utc::now().timestamp() as u64;

        // 1. Extract headers
        let header_instance_id = match extract_header_str(headers, HEADER_INSTANCE_ID) {
            Some(v) => v,
            None => {
                return RelayVerification::MissingHeaders(format!("missing {}", HEADER_INSTANCE_ID))
            }
        };

        let context_b64 = match extract_header_str(headers, HEADER_RELAY_CONTEXT) {
            Some(v) => v,
            None => {
                return RelayVerification::MissingHeaders(format!(
                    "missing {}",
                    HEADER_RELAY_CONTEXT
                ))
            }
        };

        let signature_hex = match extract_header_str(headers, HEADER_RELAY_SIGNATURE) {
            Some(v) => v,
            None => {
                return RelayVerification::MissingHeaders(format!(
                    "missing {}",
                    HEADER_RELAY_SIGNATURE
                ))
            }
        };

        // 2. Verify HMAC-SHA256 of the base64url-encoded context string
		let transition_secret = self
			.previous_signing_secret
			.as_deref()
			.filter(|_| {
				self.previous_signing_secret_expires_at
					.is_some_and(|expires_at| expires_at > now_secs)
			})
			.map(String::as_str);
		let mut signature_matches = false;
		for signing_secret in std::iter::once(self.signing_secret.as_str()).chain(transition_secret) {
			let mut mac = match HmacSha256::new_from_slice(signing_secret.as_bytes()) {
				Ok(mac) => mac,
				Err(_) => continue,
			};
			mac.update(context_b64.as_bytes());
			let expected_sig = hex_encode(&mac.finalize().into_bytes());
			signature_matches |= constant_time_eq(signature_hex.as_bytes(), expected_sig.as_bytes());
		}

		if !signature_matches {
            warn!(
                header_instance_id = %header_instance_id,
                "relay signature verification failed"
            );
            return RelayVerification::InvalidSignature("signature mismatch".into());
        }

        // 3. Decode base64url -> JSON
        let context_json = match URL_SAFE_NO_PAD.decode(context_b64.as_bytes()) {
            Ok(bytes) => bytes,
            Err(e) => {
                return RelayVerification::InvalidSignature(format!(
                    "base64url decode failed: {}",
                    e
                ))
            }
        };

        let context: RelayContext = match serde_json::from_slice(&context_json) {
            Ok(c) => c,
            Err(e) => {
                return RelayVerification::InvalidSignature(format!(
                    "context JSON parse failed: {}",
                    e
                ))
            }
        };

        // 4. Validate request_id before it reaches expiry diagnostics, replay keys, or headers.
        if !is_valid_request_id(&context.request_id) {
            return RelayVerification::InvalidContext("invalid request_id".to_string());
        }
        if context.group.trim().is_empty() {
            return RelayVerification::InvalidContext("invalid group".to_string());
        }
        if context.model.trim().is_empty() {
            return RelayVerification::InvalidContext("invalid model".to_string());
        }
        if context.relay_format.trim().is_empty() {
            return RelayVerification::InvalidContext("invalid relay_format".to_string());
        }

        // 5. Verify instance_id
        if context.instance_id != *self.instance_id {
            return RelayVerification::InstanceMismatch {
                expected: self.instance_id.to_string(),
                got: context.instance_id.clone(),
            };
        }

        // Also verify header instance_id matches context
        if header_instance_id != context.instance_id {
            return RelayVerification::InstanceMismatch {
                expected: context.instance_id.clone(),
                got: header_instance_id,
            };
        }

        // 6. Check expiration (30 second window)
		if context.expires_at < now_secs {
            return RelayVerification::Expired {
                request_id: context.request_id.clone(),
                expired_at: context.expires_at,
            };
        }
        if context.expires_at > now_secs + CONTEXT_MAX_AGE_SECS as u64 {
            return RelayVerification::Expired {
                request_id: context.request_id.clone(),
                expired_at: context.expires_at,
            };
        }

        // 7. Replay detection
        let replay_key = format!(
            "relay:replay:{}:{}",
            context.instance_id, context.request_id
        );
        match self
            .replay_guard
            .claim(&replay_key, Duration::from_secs(60))
            .await
        {
            Ok(false) => {
                warn!(request_id = %context.request_id, "replay detected");
                return RelayVerification::Replay {
                    request_id: context.request_id.clone(),
                };
            }
            Ok(true) => {}
            Err(error) => {
                warn!(error = %error, "replay detection unavailable; rejecting request");
                return RelayVerification::ReplayCheckFailed(error);
            }
        }

        debug!(
            request_id = %context.request_id,
            model = %context.model,
            group = %context.group,
            subject_id = %context.subject_id,
            "relay context verified successfully"
        );

        RelayVerification::Valid(context)
    }
}

pub fn has_relay_headers(headers: &HeaderMap) -> bool {
    [
        HEADER_INSTANCE_ID,
        HEADER_RELAY_CONTEXT,
        HEADER_RELAY_SIGNATURE,
    ]
    .iter()
    .any(|name| headers.contains_key(*name))
}

pub async fn authenticate_relay_request(
    verifier: Option<&RelayVerifier>,
    routing_mode: RoutingMode,
    headers: &mut HeaderMap,
    extensions: &mut Extensions,
) -> Result<(), RelayIngressRejection> {
    let Some(verifier) = verifier else {
        return Err(RelayIngressRejection {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "Aether relay verifier is not configured".to_string(),
        });
    };

    let context = match verifier.verify(headers).await {
        RelayVerification::Valid(context) => context,
        RelayVerification::InvalidContext(_) => {
            return Err(RelayIngressRejection {
                status: StatusCode::BAD_REQUEST,
                message: "invalid Aether relay context".to_string(),
            });
        }
        RelayVerification::InvalidSignature(_) => {
            return Err(RelayIngressRejection {
                status: StatusCode::UNAUTHORIZED,
                message: "invalid Aether relay signature".to_string(),
            });
        }
        RelayVerification::Expired { .. } => {
            return Err(RelayIngressRejection {
                status: StatusCode::UNAUTHORIZED,
                message: "expired Aether relay context".to_string(),
            });
        }
        RelayVerification::Replay { .. } => {
            return Err(RelayIngressRejection {
                status: StatusCode::UNAUTHORIZED,
                message: "replayed Aether relay request".to_string(),
            });
        }
        RelayVerification::ReplayCheckFailed(_) => {
            return Err(RelayIngressRejection {
                status: StatusCode::UNAUTHORIZED,
                message: "Aether relay replay check unavailable".to_string(),
            });
        }
        RelayVerification::InstanceMismatch { .. } => {
            return Err(RelayIngressRejection {
                status: StatusCode::UNAUTHORIZED,
                message: "Aether relay instance mismatch".to_string(),
            });
        }
        RelayVerification::MissingHeaders(_) => {
            return Err(RelayIngressRejection {
                status: StatusCode::UNAUTHORIZED,
                message: "incomplete Aether relay headers".to_string(),
            });
        }
    };

    if !routing_mode.allows_upstream_execution() {
        return Err(RelayIngressRejection {
            status: StatusCode::CONFLICT,
            message: format!(
                "Aether routing mode {} is reserved and cannot execute upstream requests",
                match routing_mode {
                    RoutingMode::DirectChannel => "direct_channel",
                    RoutingMode::Disabled => "disabled",
                    RoutingMode::ParallelShadow => "parallel_shadow",
                    RoutingMode::AetherDecision => "aether_decision",
                }
            ),
        });
    }

    extensions.insert(TrustedRelayContext(context));
    headers.remove(HEADER_INSTANCE_ID);
    headers.remove(HEADER_RELAY_CONTEXT);
    headers.remove(HEADER_RELAY_SIGNATURE);
    headers.remove(crate::routing::ROUTING_GROUP_HEADER);
    Ok(())
}

/// Bind a verified New API relay context to the AETHER request that will enter
/// the normal API-key, provider-pool, and settlement pipeline.
///
/// The relay context never replaces an AETHER API key or routing identity. It
/// only attests that the signed model and protocol describe this request.
pub fn validate_trusted_relay_request(
    context: Option<&TrustedRelayContext>,
    actual_model: Option<&str>,
    endpoint_signature: Option<&str>,
) -> Result<(), RelayIngressRejection> {
    let Some(context) = context else {
        return Ok(());
    };

    let signed_model = context.0.model.trim();
    let actual_model = actual_model
        .map(str::trim)
        .filter(|model| !model.is_empty());
    let matches_model = actual_model.is_some_and(|model| model == signed_model);
    let matches_format = context.0.relay_format == "openai"
        && endpoint_signature.is_some_and(|signature| signature.starts_with("openai:"));

    if matches_model && matches_format {
        return Ok(());
    }

    Err(RelayIngressRejection {
        status: StatusCode::UNAUTHORIZED,
        message: "invalid Aether relay context binding".to_string(),
    })
}

// ==================== Control Signature V2 ====================

/// Explicit control secrets supplied by the credential lifecycle layer.
///
/// This type deliberately has no Debug implementation because its fields are
/// raw shared secrets.
#[derive(Clone, Copy)]
pub(crate) struct ControlSignatureV2Secrets<'a> {
    pub current: &'a str,
    pub previous: Option<(&'a str, u64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlSignatureV2Failure {
    MissingHeader(&'static str),
    InvalidHeaderValue(&'static str),
    InvalidVersion,
    InvalidMethod,
    InvalidPath,
    InvalidInstanceId,
    InvalidTimestamp,
    ExpiredTimestamp,
    InvalidNonce,
    InvalidBodyDigest,
    InvalidSignature,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlSignatureV2Verification {
    /// No v2-specific header was supplied, so a caller may apply a separately
    /// configured legacy migration policy.
    NotPresent,
    Valid,
    /// A partial or invalid v2 attempt must be rejected rather than downgraded.
    Invalid(ControlSignatureV2Failure),
}

/// Returns true only for headers that opt a request into control signature v2.
///
/// X-Aether-Instance-ID is intentionally not a signal by itself because it is
/// shared by other collaboration flows.
pub(crate) fn has_control_signature_v2_headers(headers: &HeaderMap) -> bool {
    [
        HEADER_CONTROL_SIGNATURE_VERSION,
        HEADER_CONTROL_TIMESTAMP,
        HEADER_CONTROL_NONCE,
        HEADER_CONTROL_BODY_SHA256,
        HEADER_CONTROL_SIGNATURE,
    ]
    .iter()
    .any(|header| headers.contains_key(*header))
}

pub(crate) fn control_signature_v2_body_sha256_hex(raw_body: &[u8]) -> String {
    hex_encode(&Sha256::digest(raw_body))
}

/// Builds the exact v2 HMAC message. Path and query are intentionally taken
/// from the HTTP Uri without decoding or query normalization.
pub(crate) fn control_signature_v2_canonical_payload(
    method: &Method,
    uri: &Uri,
    instance_id: &str,
    timestamp: &str,
    nonce: &str,
    body_sha256_hex: &str,
) -> String {
    let path = if uri.path().is_empty() {
        "/"
    } else {
        uri.path()
    };
    format!(
        "{CONTROL_SIGNATURE_V2_DOMAIN}\n{}\n{path}\n{}\n{instance_id}\n{timestamp}\n{nonce}\n{body_sha256_hex}",
        method.as_str(),
        uri.query().unwrap_or_default(),
    )
}

/// Verifies only the cryptographic and canonical request shape of control
/// signature v2. It intentionally has no environment, database, replay-store,
/// or routing dependency; callers must atomically claim the nonce after this
/// succeeds.
pub(crate) fn verify_control_signature_v2(
    headers: &HeaderMap,
    method: &Method,
    uri: &Uri,
    raw_body: &[u8],
    secrets: ControlSignatureV2Secrets<'_>,
    now_unix_secs: u64,
) -> ControlSignatureV2Verification {
    if !has_control_signature_v2_headers(headers) {
        return ControlSignatureV2Verification::NotPresent;
    }

    let version = match required_control_signature_v2_header(
        headers,
        HEADER_CONTROL_SIGNATURE_VERSION,
    ) {
        Ok(value) => value,
        Err(error) => return ControlSignatureV2Verification::Invalid(error),
    };
    if version != CONTROL_SIGNATURE_V2_VERSION {
        return ControlSignatureV2Verification::Invalid(
            ControlSignatureV2Failure::InvalidVersion,
        );
    }

    let instance_id = match required_control_signature_v2_header(headers, HEADER_INSTANCE_ID) {
        Ok(value) => value,
        Err(error) => return ControlSignatureV2Verification::Invalid(error),
    };
    let timestamp = match required_control_signature_v2_header(headers, HEADER_CONTROL_TIMESTAMP) {
        Ok(value) => value,
        Err(error) => return ControlSignatureV2Verification::Invalid(error),
    };
    let nonce = match required_control_signature_v2_header(headers, HEADER_CONTROL_NONCE) {
        Ok(value) => value,
        Err(error) => return ControlSignatureV2Verification::Invalid(error),
    };
    let body_digest =
        match required_control_signature_v2_header(headers, HEADER_CONTROL_BODY_SHA256) {
            Ok(value) => value,
            Err(error) => return ControlSignatureV2Verification::Invalid(error),
        };
    let signature = match required_control_signature_v2_header(headers, HEADER_CONTROL_SIGNATURE) {
        Ok(value) => value,
        Err(error) => return ControlSignatureV2Verification::Invalid(error),
    };

    if !is_canonical_control_signature_v2_method(method.as_str()) {
        return ControlSignatureV2Verification::Invalid(
            ControlSignatureV2Failure::InvalidMethod,
        );
    }
    if uri.path().is_empty() || !uri.path().starts_with('/') {
        return ControlSignatureV2Verification::Invalid(ControlSignatureV2Failure::InvalidPath);
    }
    if !is_valid_control_signature_v2_instance_id(instance_id) {
        return ControlSignatureV2Verification::Invalid(
            ControlSignatureV2Failure::InvalidInstanceId,
        );
    }
    let Some(timestamp_secs) = parse_control_signature_v2_timestamp(timestamp) else {
        return ControlSignatureV2Verification::Invalid(
            ControlSignatureV2Failure::InvalidTimestamp,
        );
    };
    if timestamp_secs.abs_diff(now_unix_secs) > CONTROL_SIGNATURE_V2_MAX_AGE_SECS {
        return ControlSignatureV2Verification::Invalid(
            ControlSignatureV2Failure::ExpiredTimestamp,
        );
    }
    if !is_valid_control_signature_v2_nonce(nonce) {
        return ControlSignatureV2Verification::Invalid(
            ControlSignatureV2Failure::InvalidNonce,
        );
    }
    if decode_lower_hex_32(body_digest).is_none()
        || !constant_time_eq(
            body_digest.as_bytes(),
            control_signature_v2_body_sha256_hex(raw_body).as_bytes(),
        )
    {
        return ControlSignatureV2Verification::Invalid(
            ControlSignatureV2Failure::InvalidBodyDigest,
        );
    }
    let Some(signature_bytes) = decode_lower_hex_32(signature) else {
        return ControlSignatureV2Verification::Invalid(
            ControlSignatureV2Failure::InvalidSignature,
        );
    };

    let canonical = control_signature_v2_canonical_payload(
        method,
        uri,
        instance_id,
        timestamp,
        nonce,
        body_digest,
    );
    let active_previous = secrets
        .previous
        .filter(|(_, expires_at)| *expires_at > now_unix_secs)
        .map(|(secret, _)| secret);
    let mut signature_matches = false;
    for secret in std::iter::once(secrets.current).chain(active_previous) {
        if secret.trim().is_empty() {
            continue;
        }
        let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
            Ok(mac) => mac,
            Err(_) => continue,
        };
        mac.update(canonical.as_bytes());
        // verify_slice performs a constant-time MAC comparison. Keep checking
        // all active keys so current/previous choice is not an early exit.
        signature_matches |= mac.verify_slice(&signature_bytes).is_ok();
    }

    if signature_matches {
        ControlSignatureV2Verification::Valid
    } else {
        ControlSignatureV2Verification::Invalid(ControlSignatureV2Failure::InvalidSignature)
    }
}

fn required_control_signature_v2_header<'a>(
    headers: &'a HeaderMap,
    name: &'static str,
) -> Result<&'a str, ControlSignatureV2Failure> {
    let value = headers
        .get(name)
        .ok_or(ControlSignatureV2Failure::MissingHeader(name))?;
    value
        .to_str()
        .map_err(|_| ControlSignatureV2Failure::InvalidHeaderValue(name))
}

fn is_canonical_control_signature_v2_method(method: &str) -> bool {
    !method.is_empty()
        && method
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'-')
}

fn is_valid_control_signature_v2_instance_id(instance_id: &str) -> bool {
    !instance_id.is_empty()
        && instance_id.len() <= CONTROL_SIGNATURE_V2_MAX_INSTANCE_ID_BYTES
        && instance_id
            .bytes()
            .all(|byte| (0x21..=0x7e).contains(&byte))
}

fn parse_control_signature_v2_timestamp(timestamp: &str) -> Option<u64> {
    if timestamp.is_empty()
        || (timestamp.len() > 1 && timestamp.starts_with('0'))
        || !timestamp.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    timestamp.parse().ok()
}

fn is_valid_control_signature_v2_nonce(nonce: &str) -> bool {
    (CONTROL_SIGNATURE_V2_MIN_NONCE_BYTES..=CONTROL_SIGNATURE_V2_MAX_NONCE_BYTES)
        .contains(&nonce.len())
        && nonce.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

fn decode_lower_hex_32(value: &str) -> Option<[u8; 32]> {
    let bytes = value.as_bytes();
    if bytes.len() != 64 {
        return None;
    }

    let mut decoded = [0u8; 32];
    for (index, pair) in bytes.chunks_exact(2).enumerate() {
        let high = lower_hex_value(pair[0])?;
        let low = lower_hex_value(pair[1])?;
        decoded[index] = (high << 4) | low;
    }
    Some(decoded)
}

fn lower_hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

// ==================== Control Credentials (separate from relay signing) ====================

/// 控制面凭据验证器（用于 /api/integrations/ 端点）
#[derive(Clone)]
pub struct ControlCredentialVerifier {
	/// 控制凭据（独立于 relay signing secret）
	control_secret: Arc<String>,
	previous_control_secret: Option<Arc<String>>,
	previous_control_secret_expires_at: Option<u64>,
}

impl ControlCredentialVerifier {
	pub fn new(control_secret: String) -> Self {
		Self::new_with_transition_secret(control_secret, None, None)
	}

	fn new_with_transition_secret(
		control_secret: String,
		previous_control_secret: Option<String>,
		previous_control_secret_expires_at: Option<u64>,
	) -> Self {
		Self {
			control_secret: Arc::new(control_secret),
			previous_control_secret: previous_control_secret
				.filter(|secret| !secret.trim().is_empty())
				.map(Arc::new),
			previous_control_secret_expires_at,
		}
	}

	#[cfg(test)]
	pub fn new_with_previous(control_secret: String, previous_control_secret: Option<String>) -> Self {
		Self::new_with_transition_secret(control_secret, previous_control_secret, Some(u64::MAX))
	}

	pub fn from_env() -> Option<Self> {
		let secret = non_empty_environment_secret("AETHER_CONTROL_SECRET")?;
		let previous_secret = non_empty_environment_secret("AETHER_CONTROL_SECRET_PREVIOUS");
		let previous_expires_at = environment_expiry("AETHER_CONTROL_SECRET_PREVIOUS_EXPIRES_AT");
		let relay_secret = non_empty_environment_secret("AETHER_RELAY_SIGNING_SECRET");
		let previous_relay_secret = non_empty_environment_secret("AETHER_RELAY_SIGNING_SECRET_PREVIOUS");
		let previous_relay_expires_at = environment_expiry("AETHER_RELAY_SIGNING_SECRET_PREVIOUS_EXPIRES_AT");
		if active_credential_domains_overlap(
			&secret,
			previous_secret.as_deref(),
			previous_expires_at,
			relay_secret.as_deref(),
			previous_relay_secret.as_deref(),
			previous_relay_expires_at,
		) {
			return None;
		}
		if non_empty_environment_secret("AETHER_OUTBOUND_EXPORT_TOKEN")
			.is_some_and(|token| !outbound_export_token_is_isolated(&token))
		{
			return None;
		}
		Some(Self::new_with_transition_secret(
			secret,
			previous_secret,
			previous_expires_at,
		))
    }

    /// 验证控制面请求签名（与 relay 签名算法相同但使用不同密钥）
	pub fn verify_control_request(&self, headers: &HeaderMap) -> bool {
		// Control API uses Bearer token for simplicity
		match headers.get("authorization") {
			Some(v) => {
				let val = v.to_str().unwrap_or("");
				val.strip_prefix("Bearer ").is_some_and(|token| {
					let current_matches =
						constant_time_eq(token.as_bytes(), self.control_secret.as_bytes());
					let transition_matches = self
						.previous_control_secret
						.as_deref()
						.filter(|_| {
							self.previous_control_secret_expires_at
								.is_some_and(|expires_at| expires_at > Utc::now().timestamp() as u64)
						})
						.is_some_and(|secret| constant_time_eq(token.as_bytes(), secret.as_bytes()));
					current_matches | transition_matches
				})
			}
            None => false,
        }
    }
}

// ==================== Helpers ====================

fn non_empty_environment_secret(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn environment_expiry(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
}

/// Reject an export credential that could also authenticate control or relay traffic.
pub(crate) fn outbound_export_token_is_isolated(export_token: &str) -> bool {
    let control_secret = non_empty_environment_secret("AETHER_CONTROL_SECRET");
    let previous_control_secret = non_empty_environment_secret("AETHER_CONTROL_SECRET_PREVIOUS");
    let previous_control_expires_at =
        environment_expiry("AETHER_CONTROL_SECRET_PREVIOUS_EXPIRES_AT");
    let relay_secret = non_empty_environment_secret("AETHER_RELAY_SIGNING_SECRET");
    let previous_relay_secret = non_empty_environment_secret("AETHER_RELAY_SIGNING_SECRET_PREVIOUS");
    let previous_relay_expires_at =
        environment_expiry("AETHER_RELAY_SIGNING_SECRET_PREVIOUS_EXPIRES_AT");

    !active_credential_domains_overlap(
        export_token,
        None,
        None,
        control_secret.as_deref(),
        previous_control_secret.as_deref(),
        previous_control_expires_at,
    ) && !active_credential_domains_overlap(
        export_token,
        None,
        None,
        relay_secret.as_deref(),
        previous_relay_secret.as_deref(),
        previous_relay_expires_at,
    )
}

/// Match a presented bearer value against every active service credential domain.
pub(crate) fn is_active_service_credential(credential: &str) -> bool {
    let control_secret = non_empty_environment_secret("AETHER_CONTROL_SECRET");
    let previous_control_secret = non_empty_environment_secret("AETHER_CONTROL_SECRET_PREVIOUS");
    let previous_control_expires_at =
        environment_expiry("AETHER_CONTROL_SECRET_PREVIOUS_EXPIRES_AT");
    let relay_secret = non_empty_environment_secret("AETHER_RELAY_SIGNING_SECRET");
    let previous_relay_secret = non_empty_environment_secret("AETHER_RELAY_SIGNING_SECRET_PREVIOUS");
    let previous_relay_expires_at =
        environment_expiry("AETHER_RELAY_SIGNING_SECRET_PREVIOUS_EXPIRES_AT");
    let outbound_export_token = non_empty_environment_secret("AETHER_OUTBOUND_EXPORT_TOKEN");

    active_credential_domains_overlap(
        credential,
        None,
        None,
        control_secret.as_deref(),
        previous_control_secret.as_deref(),
        previous_control_expires_at,
    ) || active_credential_domains_overlap(
        credential,
        None,
        None,
        relay_secret.as_deref(),
        previous_relay_secret.as_deref(),
        previous_relay_expires_at,
    ) || active_credential_domains_overlap(
        credential,
        None,
        None,
        outbound_export_token.as_deref(),
        None,
        None,
    )
}

/// Reject an active credential that could authenticate both control and relay traffic.
fn active_credential_domains_overlap(
    first_current: &str,
    first_previous: Option<&str>,
    first_previous_expires_at: Option<u64>,
    second_current: Option<&str>,
    second_previous: Option<&str>,
    second_previous_expires_at: Option<u64>,
) -> bool {
    let now_secs = Utc::now().timestamp().max(0) as u64;
    let first_active = [
        Some(first_current),
        first_previous.filter(|_| {
            first_previous_expires_at.is_some_and(|expires_at| expires_at > now_secs)
        }),
    ];
    let second_active = [
        second_current,
        second_previous.filter(|_| {
            second_previous_expires_at.is_some_and(|expires_at| expires_at > now_secs)
        }),
    ];

    first_active.iter().flatten().any(|first| {
        second_active.iter().flatten().any(|second| {
            constant_time_eq(first.as_bytes(), second.as_bytes())
        })
    })
}

fn is_valid_request_id(request_id: &str) -> bool {
    !request_id.is_empty()
        && request_id.len() <= 100
        && request_id.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        && HeaderValue::from_str(request_id).is_ok()
}

fn extract_header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name)?.to_str().ok().map(|s| s.to_string())
}

/// 恒定时间比较（防止时序攻击）
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Hex 编码
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use axum::http::{HeaderMap, HeaderValue, Method, Uri};
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use chrono::Utc;
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};

    use crate::routing::ROUTING_GROUP_HEADER;

    use super::{
        authenticate_relay_request, hex_encode, is_active_service_credential,
        outbound_export_token_is_isolated, control_signature_v2_body_sha256_hex,
        control_signature_v2_canonical_payload, verify_control_signature_v2,
        ControlCredentialVerifier, ControlSignatureV2Secrets, ControlSignatureV2Verification,
        RelayContext, RelayVerification, RelayVerifier, ReplayGuard, RoutingMode,
        TrustedRelayContext, HEADER_CONTROL_BODY_SHA256, HEADER_CONTROL_NONCE,
        HEADER_CONTROL_SIGNATURE, HEADER_CONTROL_SIGNATURE_VERSION, HEADER_CONTROL_TIMESTAMP,
        HEADER_INSTANCE_ID, HEADER_RELAY_CONTEXT, HEADER_RELAY_SIGNATURE,
    };

    struct FailingReplayGuard;

    static RELAY_ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.previous.as_ref() {
                std::env::set_var(self.key, previous);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[async_trait]
    impl ReplayGuard for FailingReplayGuard {
        async fn claim(&self, _key: &str, _ttl: std::time::Duration) -> Result<bool, String> {
            Err("replay store unavailable".to_string())
        }
    }

    #[derive(Default)]
    struct CountingReplayGuard {
        claims: AtomicUsize,
    }

    impl CountingReplayGuard {
        fn claim_count(&self) -> usize {
            self.claims.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ReplayGuard for CountingReplayGuard {
        async fn claim(&self, _key: &str, _ttl: std::time::Duration) -> Result<bool, String> {
            self.claims.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        }
    }

    #[test]
    fn bilateral_contract_bundle_matches_the_audited_baseline() {
        const BASELINE_NAME: &str = "aether-newapi-v1.baseline.json";
        const TRACKED_FILES: [&str; 3] = [
            "aether-newapi-v1.json",
            "aether-newapi-v1.schema.json",
            "aether-newapi-v1.examples.json",
        ];

        let local_contract_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("docs")
            .join("contracts");
        let peer_root = std::env::var_os("AETHER_NEWAPI_CONTRACT_PEER_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../..")
                    .join("new-api")
            });
        let peer_contract_dir = peer_root.join("docs").join("contracts");

        for name in TRACKED_FILES {
            let local = fs::read(local_contract_dir.join(name))
                .unwrap_or_else(|error| panic!("read local contract {name}: {error}"));
            let peer = fs::read(peer_contract_dir.join(name))
                .unwrap_or_else(|error| panic!("read peer contract {name}: {error}"));
            assert_eq!(local, peer, "{name} must be byte-identical in both repositories");
        }

        let local_baseline = fs::read(local_contract_dir.join(BASELINE_NAME))
            .unwrap_or_else(|error| panic!("read local baseline: {error}"));
        let peer_baseline = fs::read(peer_contract_dir.join(BASELINE_NAME))
            .unwrap_or_else(|error| panic!("read peer baseline: {error}"));
        assert_eq!(local_baseline, peer_baseline, "audited baselines must match");

        let baseline: serde_json::Value =
            serde_json::from_slice(&local_baseline).expect("baseline must be JSON");
        assert_eq!(
            baseline["baseline_format"],
            "aether-newapi-contract-baseline/v1"
        );
        assert!(baseline["baseline_revision"].as_u64().is_some());
        assert_eq!(
            baseline["tracked_files"],
            serde_json::json!(TRACKED_FILES)
        );
        let file_sha256 = baseline["file_sha256"]
            .as_object()
            .expect("baseline must include per-file SHA-256 values");
        assert_eq!(file_sha256.len(), TRACKED_FILES.len());
        assert_eq!(
            baseline["change_control"]["review_policy"],
            "cross-repository-review-required"
        );
        assert!(baseline["change_control"]["change_id"]
            .as_str()
            .is_some_and(|value| !value.is_empty()));

        let schema = fs::read(local_contract_dir.join(TRACKED_FILES[1]))
            .expect("read contract schema");
        let examples = fs::read(local_contract_dir.join(TRACKED_FILES[2]))
            .expect("read contract examples");
        let mut hasher = Sha256::new();
        hasher.update(schema);
        hasher.update([0]);
        hasher.update(examples);
        let bundle_sha256 = hex_encode(&hasher.finalize());

        let manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(local_contract_dir.join(TRACKED_FILES[0])).expect("read contract manifest"),
        )
        .expect("manifest must be JSON");
        assert_eq!(manifest["bundle_sha256"], bundle_sha256);
        assert_eq!(baseline["bundle_sha256"], bundle_sha256);
        assert_eq!(baseline["contract_version"], manifest["contract_version"]);
        assert_eq!(baseline["schema_revision"], manifest["schema_revision"]);
        for name in TRACKED_FILES {
            let contents = fs::read(local_contract_dir.join(name))
                .unwrap_or_else(|error| panic!("read local contract {name}: {error}"));
            let mut hasher = Sha256::new();
            hasher.update(contents);
            assert_eq!(file_sha256[name], hex_encode(&hasher.finalize()));
        }
    }

    fn relay_context(request_id: &str) -> RelayContext {
        RelayContext {
            instance_id: "aether-primary".to_string(),
            request_id: request_id.to_string(),
            subject_id: "subject-1".to_string(),
            token_subject_id: "token-subject-1".to_string(),
            channel_id: "41".to_string(),
            group: "pro".to_string(),
            model: "gpt-5".to_string(),
            relay_format: "openai".to_string(),
            config_revision: 7,
            expires_at: (Utc::now().timestamp() + 30) as u64,
        }
    }

    fn signed_headers_for_context(secret: &str, context: RelayContext) -> HeaderMap {
        let encoded = URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&context).expect("relay context should serialize"));
        let mut mac =
            Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC secret should be valid");
        mac.update(encoded.as_bytes());
        let signature = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();

        let mut headers = HeaderMap::new();
        headers.insert(
            HEADER_INSTANCE_ID,
            HeaderValue::from_static("aether-primary"),
        );
        headers.insert(
            HEADER_RELAY_CONTEXT,
            HeaderValue::from_str(&encoded).expect("encoded context should be a header"),
        );
        headers.insert(
            HEADER_RELAY_SIGNATURE,
            HeaderValue::from_str(&signature).expect("signature should be a header"),
        );
        headers
    }

    fn signed_headers(secret: &str, request_id: &str) -> HeaderMap {
        signed_headers_for_context(secret, relay_context(request_id))
    }

    fn signed_control_signature_v2_headers(
        secret: &str,
        method: &Method,
        uri: &Uri,
        instance_id: &str,
        timestamp: &str,
        nonce: &str,
        raw_body: &[u8],
    ) -> HeaderMap {
        let body_sha256 = control_signature_v2_body_sha256_hex(raw_body);
        let canonical = control_signature_v2_canonical_payload(
            method,
            uri,
            instance_id,
            timestamp,
            nonce,
            &body_sha256,
        );
        let mut mac =
            Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC secret should be valid");
        mac.update(canonical.as_bytes());
        let signature = hex_encode(&mac.finalize().into_bytes());

        let mut headers = HeaderMap::new();
        headers.insert(
            HEADER_INSTANCE_ID,
            HeaderValue::from_str(instance_id).expect("instance ID should be a header"),
        );
        headers.insert(
            HEADER_CONTROL_SIGNATURE_VERSION,
            HeaderValue::from_static("v2"),
        );
        headers.insert(
            HEADER_CONTROL_TIMESTAMP,
            HeaderValue::from_str(timestamp).expect("timestamp should be a header"),
        );
        headers.insert(
            HEADER_CONTROL_NONCE,
            HeaderValue::from_str(nonce).expect("nonce should be a header"),
        );
        headers.insert(
            HEADER_CONTROL_BODY_SHA256,
            HeaderValue::from_str(&body_sha256).expect("body digest should be a header"),
        );
        headers.insert(
            HEADER_CONTROL_SIGNATURE,
            HeaderValue::from_str(&signature).expect("signature should be a header"),
        );
        headers
    }

    fn blank_required_relay_contexts() -> Vec<(&'static str, &'static str, RelayContext)> {
        let mut contexts = Vec::new();
        for (field, value) in [
            ("group", ""),
            ("group", " \t "),
            ("model", ""),
            ("model", " \t "),
            ("relay_format", ""),
            ("relay_format", " \t "),
        ] {
            let mut context = relay_context(&format!("request-blank-{field}-{}", contexts.len()));
            match field {
                "group" => context.group = value.to_string(),
                "model" => context.model = value.to_string(),
                "relay_format" => context.relay_format = value.to_string(),
                _ => unreachable!("test cases only use required relay context fields"),
            }
            contexts.push((field, value, context));
        }
        contexts
    }

    fn invalid_request_ids() -> Vec<(&'static str, String)> {
        vec![
            ("empty", String::new()),
            ("whitespace only", "   ".to_string()),
            ("leading whitespace", " request-123".to_string()),
            ("trailing whitespace", "request-123 ".to_string()),
            ("CRLF", "request\r\n123".to_string()),
            ("non-ASCII", "request-请求".to_string()),
            ("101 bytes", "x".repeat(101)),
        ]
    }

    #[test]
    fn relay_context_accepts_canonical_string_ids_and_numeric_revision() {
        let context: RelayContext = serde_json::from_str(
            r#"{
                "instance_id":"aether-primary",
                "request_id":"req_123",
                "subject_id":"u_abc",
                "token_subject_id":"t_def",
                "channel_id":"41",
                "group":"pro",
                "model":"gpt-5",
                "relay_format":"openai",
                "config_revision":7,
                "expires_at":1784073630
            }"#,
        )
        .expect("canonical relay context should deserialize");

        assert_eq!(context.channel_id, "41");
        assert_eq!(context.config_revision, 7);
    }

    #[tokio::test]
    async fn verifier_rejects_empty_or_whitespace_signing_secrets_before_replay_claim() {
        for (case, signing_secret) in [("empty", ""), ("whitespace", " \t ")] {
            let replay_guard = Arc::new(CountingReplayGuard::default());
            let verifier = RelayVerifier::new_with_replay_guard(
                signing_secret.to_string(),
                "aether-primary".to_string(),
                replay_guard.clone(),
            );

            let result = verifier
                .verify(&signed_headers(
                    signing_secret,
                    &format!("request-{case}-secret"),
                ))
                .await;

            assert!(
                matches!(result, RelayVerification::InvalidSignature(_)),
                "{case} signing secret must fail closed"
            );
            assert_eq!(
                replay_guard.claim_count(),
                0,
                "{case} signing secret must fail before replay claim"
            );
        }
    }

    #[test]
    fn from_env_rejects_empty_or_whitespace_signing_secrets() {
        let _lock = RELAY_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _instance_guard = EnvVarGuard::set("AETHER_INSTANCE_ID", "");
        let _secret_guard = EnvVarGuard::set("AETHER_RELAY_SIGNING_SECRET", "relay-secret");

        let runtime = aether_runtime_state::RuntimeState::memory(Default::default());
        assert!(RelayVerifier::from_env(runtime).is_none());

        for signing_secret in ["", " \t "] {
            std::env::set_var("AETHER_RELAY_SIGNING_SECRET", signing_secret);
            let runtime = aether_runtime_state::RuntimeState::memory(Default::default());

            assert!(RelayVerifier::from_env(runtime).is_none());
        }
    }

    #[test]
    fn environment_credentials_reject_cross_domain_secret_reuse() {
        let credentials_are_isolated = {
            let _lock = RELAY_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let _instance_id = EnvVarGuard::set("AETHER_INSTANCE_ID", "aether-primary");
            let _control_current = EnvVarGuard::set("AETHER_CONTROL_SECRET", "control-v2");
            let _control_previous =
                EnvVarGuard::set("AETHER_CONTROL_SECRET_PREVIOUS", "control-v1");
            let _control_previous_expires = EnvVarGuard::set(
                "AETHER_CONTROL_SECRET_PREVIOUS_EXPIRES_AT",
                "4102444800",
            );
            let _relay_current =
                EnvVarGuard::set("AETHER_RELAY_SIGNING_SECRET", "relay-v2");
            let _relay_previous =
                EnvVarGuard::set("AETHER_RELAY_SIGNING_SECRET_PREVIOUS", "relay-v1");
            let _relay_previous_expires = EnvVarGuard::set(
                "AETHER_RELAY_SIGNING_SECRET_PREVIOUS_EXPIRES_AT",
                "4102444800",
            );
            let _outbound_export_token =
                EnvVarGuard::set("AETHER_OUTBOUND_EXPORT_TOKEN", "export-v2");

            let distinct_current_credentials = ControlCredentialVerifier::from_env().is_some()
                && RelayVerifier::from_env(aether_runtime_state::RuntimeState::memory(
                    Default::default(),
                ))
                .is_some()
                && outbound_export_token_is_isolated("export-v2");

            std::env::set_var("AETHER_CONTROL_SECRET", "relay-v2");
            let current_collision_rejected = ControlCredentialVerifier::from_env().is_none()
                && RelayVerifier::from_env(aether_runtime_state::RuntimeState::memory(
                    Default::default(),
                ))
                .is_none();

            std::env::set_var("AETHER_CONTROL_SECRET", "control-v2");
            std::env::set_var("AETHER_CONTROL_SECRET_PREVIOUS", "relay-v1");
            let transition_collision_rejected = ControlCredentialVerifier::from_env().is_none()
                && RelayVerifier::from_env(aether_runtime_state::RuntimeState::memory(
                    Default::default(),
                ))
                .is_none();

            std::env::set_var("AETHER_CONTROL_SECRET_PREVIOUS", "control-v1");
            std::env::set_var("AETHER_OUTBOUND_EXPORT_TOKEN", "relay-v2");
            let export_relay_collision_rejected = ControlCredentialVerifier::from_env().is_none()
                && RelayVerifier::from_env(aether_runtime_state::RuntimeState::memory(
                    Default::default(),
                ))
                .is_none()
                && !outbound_export_token_is_isolated("relay-v2");

            std::env::set_var("AETHER_OUTBOUND_EXPORT_TOKEN", "control-v1");
            let export_transition_collision_rejected =
                ControlCredentialVerifier::from_env().is_none()
                    && RelayVerifier::from_env(aether_runtime_state::RuntimeState::memory(
                        Default::default(),
                    ))
                    .is_none()
                    && !outbound_export_token_is_isolated("control-v1");

            std::env::set_var("AETHER_CONTROL_SECRET_PREVIOUS_EXPIRES_AT", "1");
            let expired_transition_does_not_block = ControlCredentialVerifier::from_env().is_some()
                && RelayVerifier::from_env(aether_runtime_state::RuntimeState::memory(
                    Default::default(),
                ))
                .is_some()
                && outbound_export_token_is_isolated("control-v1");

            distinct_current_credentials
                && current_collision_rejected
                && transition_collision_rejected
                && export_relay_collision_rejected
                && export_transition_collision_rejected
                && expired_transition_does_not_block
        };

        assert!(credentials_are_isolated);
    }

    #[test]
    fn active_service_credential_detection_includes_current_and_active_transition_values() {
        let _lock = RELAY_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _control_current = EnvVarGuard::set("AETHER_CONTROL_SECRET", "control-current");
        let _control_previous =
            EnvVarGuard::set("AETHER_CONTROL_SECRET_PREVIOUS", "control-previous");
        let _control_previous_expires = EnvVarGuard::set(
            "AETHER_CONTROL_SECRET_PREVIOUS_EXPIRES_AT",
            "4102444800",
        );
        let _relay_current = EnvVarGuard::set("AETHER_RELAY_SIGNING_SECRET", "relay-current");
        let _relay_previous =
            EnvVarGuard::set("AETHER_RELAY_SIGNING_SECRET_PREVIOUS", "relay-previous");
        let _relay_previous_expires = EnvVarGuard::set(
            "AETHER_RELAY_SIGNING_SECRET_PREVIOUS_EXPIRES_AT",
            "4102444800",
        );
        let _outbound_export_token =
            EnvVarGuard::set("AETHER_OUTBOUND_EXPORT_TOKEN", "export-current");

        for credential in [
            "control-current",
            "control-previous",
            "relay-current",
            "relay-previous",
            "export-current",
        ] {
            assert!(
                is_active_service_credential(credential),
                "{credential} should be recognized as an active service credential"
            );
        }
        assert!(!is_active_service_credential("unrelated-management-token"));

        std::env::set_var("AETHER_CONTROL_SECRET_PREVIOUS_EXPIRES_AT", "1");
        std::env::set_var("AETHER_RELAY_SIGNING_SECRET_PREVIOUS_EXPIRES_AT", "1");
        assert!(!is_active_service_credential("control-previous"));
        assert!(!is_active_service_credential("relay-previous"));
    }

    #[test]
    fn disabled_routing_mode_is_deserializable_and_fails_closed() {
        let mode: RoutingMode =
            serde_json::from_str("\"disabled\"").expect("disabled is part of the integration contract");

        let environment_mode = {
            let _lock = RELAY_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let _mode = EnvVarGuard::set("AETHER_ROUTING_MODE", "disabled");
            RoutingMode::from_env()
        };

        assert!(!mode.allows_upstream_execution());
        assert_eq!(environment_mode, RoutingMode::Disabled);
    }

    #[tokio::test]
    async fn verifier_rejects_invalid_request_ids_before_claiming_replay_keys() {
        for (case, request_id) in invalid_request_ids() {
            let replay_guard = Arc::new(CountingReplayGuard::default());
            let verifier = RelayVerifier::new_with_replay_guard(
                "relay-secret".to_string(),
                "aether-primary".to_string(),
                replay_guard.clone(),
            );

            let result = verifier
                .verify(&signed_headers("relay-secret", &request_id))
                .await;

            assert!(
                !matches!(result, RelayVerification::Valid(_)),
                "{case} request_id must be rejected"
            );
            assert_eq!(
                replay_guard.claim_count(),
                0,
                "{case} request_id must be rejected before replay claim"
            );
        }
    }

    #[tokio::test]
    async fn verifier_rejects_blank_required_relay_context_fields_before_claiming_replay_keys() {
        for (field, value, context) in blank_required_relay_contexts() {
            let replay_guard = Arc::new(CountingReplayGuard::default());
            let verifier = RelayVerifier::new_with_replay_guard(
                "relay-secret".to_string(),
                "aether-primary".to_string(),
                replay_guard.clone(),
            );

            let result = verifier
                .verify(&signed_headers_for_context("relay-secret", context))
                .await;

            assert!(
                matches!(result, RelayVerification::InvalidContext(_)),
                "{field} value {value:?} must be rejected as an invalid context"
            );
            assert_eq!(
                replay_guard.claim_count(),
                0,
                "{field} value {value:?} must be rejected before replay claim"
            );
        }
    }

    #[tokio::test]
    async fn authentication_maps_invalid_request_ids_to_a_stable_bad_request() {
        for (case, request_id) in invalid_request_ids() {
            let replay_guard = Arc::new(CountingReplayGuard::default());
            let verifier = RelayVerifier::new_with_replay_guard(
                "relay-secret".to_string(),
                "aether-primary".to_string(),
                replay_guard.clone(),
            );
            let mut headers = signed_headers("relay-secret", &request_id);
            let mut extensions = axum::http::Extensions::new();

            let rejection = match authenticate_relay_request(
                Some(&verifier),
                RoutingMode::DirectChannel,
                &mut headers,
                &mut extensions,
            )
            .await
            {
                Ok(()) => panic!("{case} request_id must be rejected"),
                Err(rejection) => rejection,
            };

            assert_eq!(rejection.status, axum::http::StatusCode::BAD_REQUEST);
            assert_eq!(rejection.message, "invalid Aether relay context");
            assert_eq!(
                replay_guard.claim_count(),
                0,
                "{case} request_id must be rejected before replay claim"
            );
            assert!(extensions.get::<TrustedRelayContext>().is_none());
        }
    }

    #[tokio::test]
    async fn authentication_rejects_blank_required_relay_context_fields_without_trusting_them() {
        for (field, value, context) in blank_required_relay_contexts() {
            let replay_guard = Arc::new(CountingReplayGuard::default());
            let verifier = RelayVerifier::new_with_replay_guard(
                "relay-secret".to_string(),
                "aether-primary".to_string(),
                replay_guard.clone(),
            );
            let mut headers = signed_headers_for_context("relay-secret", context);
            let mut extensions = axum::http::Extensions::new();

            let rejection = match authenticate_relay_request(
                Some(&verifier),
                RoutingMode::DirectChannel,
                &mut headers,
                &mut extensions,
            )
            .await
            {
                Ok(()) => panic!("{field} value {value:?} must be rejected"),
                Err(rejection) => rejection,
            };

            assert_eq!(rejection.status, axum::http::StatusCode::BAD_REQUEST);
            assert_eq!(rejection.message, "invalid Aether relay context");
            assert_eq!(
                replay_guard.claim_count(),
                0,
                "{field} value {value:?} must be rejected before replay claim"
            );
            assert!(
                extensions.get::<TrustedRelayContext>().is_none(),
                "{field} value {value:?} must not become a trusted relay context"
            );
        }
    }

    #[tokio::test]
    async fn authentication_accepts_a_100_byte_ascii_graphic_request_id() {
        let request_id = "x".repeat(100);
        let replay_guard = Arc::new(CountingReplayGuard::default());
        let verifier = RelayVerifier::new_with_replay_guard(
            "relay-secret".to_string(),
            "aether-primary".to_string(),
            replay_guard.clone(),
        );
        let mut headers = signed_headers("relay-secret", &request_id);
        let mut extensions = axum::http::Extensions::new();

        authenticate_relay_request(
            Some(&verifier),
            RoutingMode::DirectChannel,
            &mut headers,
            &mut extensions,
        )
        .await
        .expect("100-byte ASCII graphic request_id should authenticate");

        assert_eq!(replay_guard.claim_count(), 1);
        assert_eq!(
            extensions
                .get::<TrustedRelayContext>()
                .expect("valid request_id should be promoted to trusted context")
                .0
                .request_id,
            request_id
        );
    }

    #[tokio::test]
    async fn replay_store_failure_rejects_the_relay_request() {
        let verifier = RelayVerifier::new_with_replay_guard(
            "relay-secret".to_string(),
            "aether-primary".to_string(),
            Arc::new(FailingReplayGuard),
        );

        let result = verifier
            .verify(&signed_headers("relay-secret", "request-fail-closed"))
            .await;

        assert!(matches!(result, RelayVerification::ReplayCheckFailed(_)));
    }

    #[tokio::test]
    async fn replay_claim_is_atomic_and_only_one_concurrent_request_succeeds() {
        let runtime = aether_runtime_state::RuntimeState::memory(Default::default());
        let verifier = Arc::new(RelayVerifier::new(
            "relay-secret".to_string(),
            "aether-primary".to_string(),
            runtime,
        ));
        let first = {
            let verifier = Arc::clone(&verifier);
            let headers = signed_headers("relay-secret", "request-atomic");
            tokio::spawn(async move { verifier.verify(&headers).await })
        };
        let second = {
            let verifier = Arc::clone(&verifier);
            let headers = signed_headers("relay-secret", "request-atomic");
            tokio::spawn(async move { verifier.verify(&headers).await })
        };

        let results = [
            first.await.expect("first verifier task should join"),
            second.await.expect("second verifier task should join"),
        ];

        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, RelayVerification::Valid(_)))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, RelayVerification::Replay { .. }))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn relay_verifier_accepts_current_and_transition_signing_secrets() {
        let runtime = aether_runtime_state::RuntimeState::memory(Default::default());
        let verifier = RelayVerifier::new_with_previous(
            "relay-v2".to_string(),
            Some("relay-v1".to_string()),
            "aether-primary".to_string(),
            runtime,
        );

        assert!(matches!(
            verifier
                .verify(&signed_headers("relay-v2", "request-current-secret"))
                .await,
            RelayVerification::Valid(_)
        ));
        assert!(matches!(
            verifier
                .verify(&signed_headers("relay-v1", "request-transition-secret"))
                .await,
            RelayVerification::Valid(_)
        ));
        assert!(matches!(
            verifier
                .verify(&signed_headers("relay-invalid", "request-invalid-secret"))
                .await,
            RelayVerification::InvalidSignature(_)
        ));
    }

    #[test]
    fn control_credential_verifier_accepts_current_and_transition_secrets() {
        let verifier = ControlCredentialVerifier::new_with_previous(
            "control-v2".to_string(),
            Some("control-v1".to_string()),
        );
        let mut headers = HeaderMap::new();

        headers.insert("authorization", HeaderValue::from_static("Bearer control-v2"));
        assert!(verifier.verify_control_request(&headers));

        headers.insert("authorization", HeaderValue::from_static("Bearer control-v1"));
        assert!(verifier.verify_control_request(&headers));

        headers.insert("authorization", HeaderValue::from_static("Bearer control-invalid"));
        assert!(!verifier.verify_control_request(&headers));
    }

    #[test]
    fn control_signature_v2_canonical_payload_binds_raw_request_identity_and_body_digest() {
        let method = Method::PUT;
        let uri: Uri = "/api/integrations/new-api/v1/instances/aether-primary?group=%E4%B8%AD%E6%96%87+pro&cursor=a%2Fb%3F"
            .parse()
            .expect("URI should parse");
        let body = b"hello";
        let digest = control_signature_v2_body_sha256_hex(body);

        assert_eq!(
            digest,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(
            control_signature_v2_canonical_payload(
                &method,
                &uri,
                "aether-primary",
                "1784073600",
                "nonce-1234567890",
                &digest,
            ),
            "AETHER-CONTROL-V2\nPUT\n/api/integrations/new-api/v1/instances/aether-primary\ngroup=%E4%B8%AD%E6%96%87+pro&cursor=a%2Fb%3F\naether-primary\n1784073600\nnonce-1234567890\n2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn control_signature_v2_rejects_tampered_request_components_and_accepts_active_secrets() {
        let method = Method::PUT;
        let uri: Uri = "/api/integrations/new-api/v1/instances/aether-primary?cursor=a%2Fb%3F"
            .parse()
            .expect("URI should parse");
        let body = br#"{"route_profile":"balanced","base_revision":7}"#;
        let now_unix_secs = 1_784_073_600;
        let headers = signed_control_signature_v2_headers(
            "control-v2",
            &method,
            &uri,
            "aether-primary",
            "1784073600",
            "nonce-1234567890",
            body,
        );
        let secrets = ControlSignatureV2Secrets {
            current: "control-v2",
            previous: Some(("control-v1", now_unix_secs + 1)),
        };

        assert_eq!(
            verify_control_signature_v2(&headers, &method, &uri, body, secrets, now_unix_secs),
            ControlSignatureV2Verification::Valid
        );

        let changed_method = Method::GET;
        assert!(matches!(
            verify_control_signature_v2(
                &headers,
                &changed_method,
                &uri,
                body,
                secrets,
                now_unix_secs,
            ),
            ControlSignatureV2Verification::Invalid(_)
        ));

        let changed_uri: Uri = "/api/integrations/new-api/v1/instances/aether-primary?cursor=other"
            .parse()
            .expect("URI should parse");
        assert!(matches!(
            verify_control_signature_v2(
                &headers,
                &method,
                &changed_uri,
                body,
                secrets,
                now_unix_secs,
            ),
            ControlSignatureV2Verification::Invalid(_)
        ));

        let mut changed_instance_headers = headers.clone();
        changed_instance_headers.insert(
            HEADER_INSTANCE_ID,
            HeaderValue::from_static("aether-secondary"),
        );
        assert!(matches!(
            verify_control_signature_v2(
                &changed_instance_headers,
                &method,
                &uri,
                body,
                secrets,
                now_unix_secs,
            ),
            ControlSignatureV2Verification::Invalid(_)
        ));

        assert!(matches!(
            verify_control_signature_v2(
                &headers,
                &method,
                &uri,
                b"{\"route_profile\":\"tampered\"}",
                secrets,
                now_unix_secs,
            ),
            ControlSignatureV2Verification::Invalid(_)
        ));

        let previous_headers = signed_control_signature_v2_headers(
            "control-v1",
            &method,
            &uri,
            "aether-primary",
            "1784073600",
            "nonce-previous-123456",
            body,
        );
        assert_eq!(
            verify_control_signature_v2(
                &previous_headers,
                &method,
                &uri,
                body,
                secrets,
                now_unix_secs,
            ),
            ControlSignatureV2Verification::Valid
        );
        assert!(matches!(
            verify_control_signature_v2(
                &previous_headers,
                &method,
                &uri,
                body,
                ControlSignatureV2Secrets {
                    current: "control-v2",
                    previous: Some(("control-v1", now_unix_secs)),
                },
                now_unix_secs,
            ),
            ControlSignatureV2Verification::Invalid(_)
        ));
    }

    #[test]
    fn malformed_or_partial_control_signature_v2_never_reports_not_present() {
        let method = Method::GET;
        let uri: Uri = "/api/integrations/new-api/v1/capabilities"
            .parse()
            .expect("URI should parse");
        let mut headers = HeaderMap::new();
        headers.insert(
            HEADER_CONTROL_SIGNATURE_VERSION,
            HeaderValue::from_static("v2"),
        );

        assert!(matches!(
            verify_control_signature_v2(
                &headers,
                &method,
                &uri,
                b"",
                ControlSignatureV2Secrets {
                    current: "control-v2",
                    previous: None,
                },
                1_784_073_600,
            ),
            ControlSignatureV2Verification::Invalid(_)
        ));

        let mut invalid_headers = signed_control_signature_v2_headers(
            "control-v2",
            &method,
            &uri,
            "aether-primary",
            "1784073600",
            "nonce-invalid-123456",
            b"",
        );
        invalid_headers.insert(
            HEADER_CONTROL_SIGNATURE,
            HeaderValue::from_static("not-a-valid-v2-signature"),
        );
        assert!(matches!(
            verify_control_signature_v2(
                &invalid_headers,
                &method,
                &uri,
                b"",
                ControlSignatureV2Secrets {
                    current: "control-v2",
                    previous: None,
                },
                1_784_073_600,
            ),
            ControlSignatureV2Verification::Invalid(_)
        ));
    }

    #[tokio::test]
    async fn environment_transition_secrets_require_a_future_deadline() {
        let _lock = RELAY_ENV_LOCK.lock().expect("relay env lock should not be poisoned");
        let _instance_id = EnvVarGuard::set("AETHER_INSTANCE_ID", "aether-primary");
        let _relay_current = EnvVarGuard::set("AETHER_RELAY_SIGNING_SECRET", "relay-v2");
        let _relay_previous = EnvVarGuard::set("AETHER_RELAY_SIGNING_SECRET_PREVIOUS", "relay-v1");
        let _relay_expiry = EnvVarGuard::set(
            "AETHER_RELAY_SIGNING_SECRET_PREVIOUS_EXPIRES_AT",
            &(Utc::now().timestamp() + 60).to_string(),
        );
        let _control_current = EnvVarGuard::set("AETHER_CONTROL_SECRET", "control-v2");
        let _control_previous = EnvVarGuard::set("AETHER_CONTROL_SECRET_PREVIOUS", "control-v1");
        let _control_expiry = EnvVarGuard::set(
            "AETHER_CONTROL_SECRET_PREVIOUS_EXPIRES_AT",
            &(Utc::now().timestamp() + 60).to_string(),
        );

        let relay = RelayVerifier::from_env(aether_runtime_state::RuntimeState::memory(
            Default::default(),
        ))
        .expect("current relay secret should configure the verifier");
        assert!(matches!(
            relay
                .verify(&signed_headers("relay-v1", "request-env-transition-secret"))
                .await,
            RelayVerification::Valid(_)
        ));

        let control = ControlCredentialVerifier::from_env()
            .expect("current control secret should configure the verifier");
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer control-v1"));
        assert!(control.verify_control_request(&headers));
    }

    #[tokio::test]
    async fn verified_context_is_internal_and_relay_headers_are_not_forwarded() {
        let runtime = aether_runtime_state::RuntimeState::memory(Default::default());
        let verifier = RelayVerifier::new(
            "relay-secret".to_string(),
            "aether-primary".to_string(),
            runtime,
        );
        let mut headers = signed_headers("relay-secret", "request-trusted-extension");
        headers.insert(
            "x-aether-trusted-relay-context",
            HeaderValue::from_static("client-forged"),
        );
        let mut extensions = axum::http::Extensions::new();

        authenticate_relay_request(
            Some(&verifier),
            RoutingMode::DirectChannel,
            &mut headers,
            &mut extensions,
        )
        .await
        .expect("valid relay request should authenticate");

        assert_eq!(
            extensions
                .get::<TrustedRelayContext>()
                .expect("trusted context should be inserted internally")
                .0
                .request_id,
            "request-trusted-extension"
        );
        assert!(!headers.contains_key(HEADER_INSTANCE_ID));
        assert!(!headers.contains_key(HEADER_RELAY_CONTEXT));
        assert!(!headers.contains_key(HEADER_RELAY_SIGNATURE));
    }

    #[tokio::test]
    async fn verified_relay_context_strips_client_scheduler_group_header() {
        let runtime = aether_runtime_state::RuntimeState::memory(Default::default());
        let verifier = RelayVerifier::new(
            "relay-secret".to_string(),
            "aether-primary".to_string(),
            runtime,
        );
        let mut headers = signed_headers("relay-secret", "request-trusted-routing-group");
        headers.insert(
            ROUTING_GROUP_HEADER,
            HeaderValue::from_static("client-forged-group"),
        );
        let mut extensions = axum::http::Extensions::new();

        authenticate_relay_request(
            Some(&verifier),
            RoutingMode::DirectChannel,
            &mut headers,
            &mut extensions,
        )
        .await
        .expect("valid relay request should authenticate");

        assert!(!headers.contains_key(ROUTING_GROUP_HEADER));
        assert_eq!(
            extensions
                .get::<TrustedRelayContext>()
                .expect("trusted context should be inserted internally")
                .0
                .group,
            "pro"
        );
    }

    #[tokio::test]
    async fn reserved_routing_modes_explicitly_reject_upstream_execution() {
        let runtime = aether_runtime_state::RuntimeState::memory(Default::default());
        let verifier = RelayVerifier::new(
            "relay-secret".to_string(),
            "aether-primary".to_string(),
            runtime,
        );
        let mut headers = signed_headers("relay-secret", "request-reserved-mode");
        let mut extensions = axum::http::Extensions::new();

        let rejection = authenticate_relay_request(
            Some(&verifier),
            RoutingMode::ParallelShadow,
            &mut headers,
            &mut extensions,
        )
        .await
        .expect_err("reserved mode must not execute upstream requests");

        assert_eq!(rejection.status, axum::http::StatusCode::CONFLICT);
        assert!(rejection.message.contains("parallel_shadow"));
        assert!(extensions.get::<TrustedRelayContext>().is_none());
    }
}
