use super::super::internal;
use crate::admin_api;
use crate::control::GatewayPublicRequestContext;
use crate::handlers::shared::management_token_permission_denied_response;
use crate::{AppState, GatewayError};
use axum::body::{Body, Bytes};
use axum::http::{self, Response};

pub(super) async fn maybe_build_local_internal_proxy_response(
    state: &AppState,
    request_context: &GatewayPublicRequestContext,
    remote_addr: &std::net::SocketAddr,
    request_body: Option<&Bytes>,
) -> Result<Option<Response<Body>>, GatewayError> {
    internal::maybe_build_local_internal_proxy_response_impl(
        state,
        request_context,
        remote_addr,
        request_body,
    )
    .await
}

pub(super) async fn maybe_build_local_admin_proxy_response(
    state: &AppState,
    request_context: &GatewayPublicRequestContext,
    request_headers: &http::HeaderMap,
    request_body: Option<&Bytes>,
) -> Result<Option<Response<Body>>, GatewayError> {
    let Some(decision) = request_context.control_decision.as_ref() else {
        return Ok(None);
    };
    if decision.route_class.as_deref() != Some("admin_proxy") {
        return Ok(None);
    }
    if decision.admin_principal.is_none() {
        return Ok(None);
    }
    if let Some(response) = management_token_permission_denied_response(request_context) {
        return Ok(Some(response));
    }

    admin_api::maybe_build_local_admin_response(admin_api::AdminRouteRequest::new(
        state,
        request_context,
        request_headers,
        request_body,
    ))
    .await
}
