use crate::audit::attach_admin_audit_event;
use crate::control::{
    audit_admin_read_only_management_token_permissions,
    management_token_permission_keys_from_value, validate_management_token_admin_route_permission,
    GatewayAdminPrincipalContext, GatewayPublicRequestContext,
};
use crate::{AppState, GatewayError};
use axum::{
    body::Body,
    http,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::warn;

use super::normalize::json_ip_rules_allow;

const MANAGEMENT_TOKEN_PREFIX: &str = "ae-";
const LEGACY_MANAGEMENT_TOKEN_PREFIX: &str = "ae_";

pub(crate) fn build_unhandled_admin_proxy_response(
    request_context: &GatewayPublicRequestContext,
) -> Response<Body> {
    let decision = request_context.control_decision.as_ref();
    (
        http::StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "detail": "admin proxy route not implemented in rust frontdoor",
            "route_family": decision.and_then(|value| value.route_family.as_deref()),
            "route_kind": decision.and_then(|value| value.route_kind.as_deref()),
            "request_path": request_context.request_path,
        })),
    )
        .into_response()
}

pub(crate) fn build_admin_proxy_auth_required_response(
    request_context: &GatewayPublicRequestContext,
) -> Response<Body> {
    let decision = request_context.control_decision.as_ref();
    (
        http::StatusCode::UNAUTHORIZED,
        Json(json!({
            "detail": "admin authentication required",
            "route_family": decision.and_then(|value| value.route_family.as_deref()),
            "route_kind": decision.and_then(|value| value.route_kind.as_deref()),
            "request_path": request_context.request_path,
        })),
    )
        .into_response()
}

pub(crate) async fn promote_management_token_admin_principal(
    state: &AppState,
    client_ip: std::net::IpAddr,
    headers: &http::HeaderMap,
    trace_id: &str,
    request_context: &mut GatewayPublicRequestContext,
) -> Result<(), GatewayError> {
    let Some(decision) = request_context.control_decision.as_mut() else {
        return Ok(());
    };
    if decision.route_class.as_deref() != Some("admin_proxy") || decision.admin_principal.is_some()
    {
        return Ok(());
    }

    let Some(token) = extract_management_token_bearer(headers) else {
        return Ok(());
    };
    let token_hash = hash_management_token(&token);
    let Some(token_with_user) = state
        .get_management_token_with_user_by_hash(&token_hash)
        .await?
    else {
        return Ok(());
    };

    if !token_with_user.token.is_active {
        return Ok(());
    }
    if token_with_user
        .token
        .expires_at_unix_secs
        .is_some_and(|value| value <= chrono::Utc::now().timestamp().max(0) as u64)
    {
        return Ok(());
    }
    if !json_ip_rules_allow(token_with_user.token.allowed_ips.as_ref(), client_ip) {
        return Ok(());
    }
    let Some(user) = state.find_user_auth_by_id(&token_with_user.user.id).await? else {
        return Ok(());
    };
    if !user.is_active || user.is_deleted || !crate::roles::can_access_admin_console(&user.role) {
        return Ok(());
    }
    let management_token_permissions = match management_token_permission_keys_from_value(
        token_with_user.token.permissions.as_ref(),
    ) {
        Ok(value) => value,
        Err(error) => {
            warn!(
                trace_id = %trace_id,
                token_id = %token_with_user.token.id,
                error = %error,
                "gateway rejected management token with invalid permissions"
            );
            return Ok(());
        }
    };

    decision.admin_principal = Some(GatewayAdminPrincipalContext {
        user_id: user.id.clone(),
        user_role: user.role.clone(),
        session_id: None,
        management_token_id: Some(token_with_user.token.id.clone()),
        management_token_permissions,
    });

    let remote_ip = client_ip.to_string();
    if let Err(error) = state
        .record_management_token_usage(&token_with_user.token.id, Some(remote_ip.as_str()))
        .await
    {
        warn!(
            trace_id = %trace_id,
            token_id = %token_with_user.token.id,
            error = ?error,
            "gateway failed to record management token usage"
        );
    }
    Ok(())
}

pub(crate) fn management_token_permission_denied_response(
    request_context: &GatewayPublicRequestContext,
) -> Option<Response<Body>> {
    let decision = request_context.control_decision.as_ref()?;
    let admin_principal = decision.admin_principal.as_ref()?;
    let audit_admin_read_only_permissions;
    let token_permissions = if crate::roles::can_write_admin_console(&admin_principal.user_role) {
        admin_principal.management_token_permissions.as_deref()
    } else {
        audit_admin_read_only_permissions = audit_admin_read_only_management_token_permissions();
        Some(audit_admin_read_only_permissions.as_slice())
    };
    let denied = validate_management_token_admin_route_permission(
        &request_context.request_method,
        decision,
        token_permissions,
    )
    .err()?;
    let actor_id = admin_principal
        .management_token_id
        .as_deref()
        .unwrap_or(admin_principal.user_id.as_str());

    warn!(
        trace_id = %request_context.trace_id,
        admin_actor_id = %actor_id,
        admin_user_role = %admin_principal.user_role,
        route_family = decision.route_family.as_deref().unwrap_or("unknown"),
        route_kind = decision.route_kind.as_deref().unwrap_or("unknown"),
        required_permission = %denied.required_permission,
        "admin route permission denied"
    );

    let mut response = (
        http::StatusCode::FORBIDDEN,
        Json(json!({
            "detail": "management token permission denied",
            "required_permission": denied.required_permission,
            "route_family": decision.route_family.as_deref(),
            "route_kind": decision.route_kind.as_deref(),
            "request_path": request_context.request_path,
        })),
    )
        .into_response();
    attach_admin_audit_event(
        &mut response,
        "admin_route_permission_denied",
        "permission_denied",
        "admin_route_permission",
        actor_id,
    );
    Some(response)
}

fn extract_management_token_bearer(headers: &http::HeaderMap) -> Option<String> {
    let header = crate::headers::header_value_str(headers, http::header::AUTHORIZATION.as_str())?;
    let token = header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))?
        .trim()
        .to_string();
    (!token.is_empty()
        && (token.starts_with(MANAGEMENT_TOKEN_PREFIX)
            || token.starts_with(LEGACY_MANAGEMENT_TOKEN_PREFIX)))
    .then_some(token)
}

fn hash_management_token(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub(crate) fn attach_admin_audit_response(
    mut response: Response<Body>,
    event_name: &'static str,
    action: &'static str,
    target_type: &'static str,
    target_id: &str,
) -> Response<Body> {
    attach_admin_audit_event(&mut response, event_name, action, target_type, target_id);
    response
}
