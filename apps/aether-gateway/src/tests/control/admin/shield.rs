//! Handler coverage for the empty-response shield admin surface (F4): listing
//! blocked keys and manually unblocking them.

use axum::body::{Body, Bytes};
use http::{HeaderMap, HeaderValue, StatusCode};
use serde_json::json;

use super::super::AppState;
use crate::admin_api::{
    maybe_build_local_admin_usage_response, AdminAppState, AdminRequestContext,
};
use crate::constants::{
    GATEWAY_HEADER, TRUSTED_ADMIN_MANAGEMENT_TOKEN_ID_HEADER, TRUSTED_ADMIN_SESSION_ID_HEADER,
    TRUSTED_ADMIN_USER_ID_HEADER, TRUSTED_ADMIN_USER_ROLE_HEADER,
};
use crate::control::resolve_public_request_context;
use crate::data::GatewayDataState;

fn trusted_admin_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(GATEWAY_HEADER, HeaderValue::from_static("rust-phase3b"));
    headers.insert(
        TRUSTED_ADMIN_USER_ID_HEADER,
        HeaderValue::from_static("admin-user-123"),
    );
    headers.insert(
        TRUSTED_ADMIN_USER_ROLE_HEADER,
        HeaderValue::from_static("admin"),
    );
    headers.insert(
        TRUSTED_ADMIN_SESSION_ID_HEADER,
        HeaderValue::from_static("session-123"),
    );
    headers.insert(
        TRUSTED_ADMIN_MANAGEMENT_TOKEN_ID_HEADER,
        HeaderValue::from_static("management-token-123"),
    );
    headers
}

fn shield_app_state(config: Option<serde_json::Value>) -> AppState {
    let mut entries: Vec<(String, serde_json::Value)> = Vec::new();
    if let Some(config) = config {
        entries.push(("empty_response_shield".to_string(), config));
    }
    let data_state = GatewayDataState::disabled().with_system_config_values_for_tests(entries);
    AppState::new()
        .expect("gateway should build")
        .with_data_state_for_tests(data_state)
}

async fn local_admin_shield_response(
    state: &AppState,
    method: http::Method,
    uri: &str,
) -> axum::response::Response<Body> {
    let headers = trusted_admin_headers();
    let request_context = resolve_public_request_context(
        state,
        &method,
        &uri.parse().expect("uri should parse"),
        &headers,
        "trace-shield-admin",
    )
    .await
    .expect("request context should resolve");
    let body_bytes: Option<Bytes> = None;
    maybe_build_local_admin_usage_response(
        &AdminAppState::new(state),
        &AdminRequestContext::new(&request_context),
        body_bytes.as_ref(),
    )
    .await
    .expect("local shield response should build")
    .expect("shield route should resolve locally")
}

async fn response_json(response: axum::response::Response<Body>) -> serde_json::Value {
    serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .expect("json should parse")
}

#[tokio::test]
async fn shield_list_reports_uninstalled_without_config() {
    let state = shield_app_state(None);
    let response = local_admin_shield_response(
        &state,
        http::Method::GET,
        "/api/admin/empty-response-shield",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload = response_json(response).await;
    assert_eq!(payload["installed"], false);
    assert_eq!(payload["total"], 0);
    assert!(payload["blocked"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn shield_list_reports_config_and_blocked_keys() {
    let state = shield_app_state(Some(json!({
        "enabled": true,
        "threshold": 1,
        "window_secs": 600,
        "block_secs": 300
    })));

    // Record strikes until the session key is blocked (threshold=1).
    state.empty_response_shield.record_strike("session:conv-1");
    state.empty_response_shield.record_strike("fp:abc123");
    state.empty_response_shield.record_strike("fp:abc123");

    let response = local_admin_shield_response(
        &state,
        http::Method::GET,
        "/api/admin/empty-response-shield",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload = response_json(response).await;
    assert_eq!(payload["installed"], true);
    assert_eq!(payload["config"]["threshold"], 1);
    assert_eq!(payload["config"]["block_secs"], 300);
    let blocked = payload["blocked"].as_array().expect("blocked array");
    assert_eq!(blocked.len(), 2);
    let kinds: Vec<&str> = blocked
        .iter()
        .map(|entry| entry["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"session"));
    assert!(kinds.contains(&"fingerprint"));
}

#[tokio::test]
async fn shield_unblock_clears_blocked_key() {
    let state = shield_app_state(Some(json!({
        "enabled": true,
        "threshold": 1,
        "window_secs": 600,
        "block_secs": 300
    })));
    state.empty_response_shield.record_strike("session:conv-9");

    let response = local_admin_shield_response(
        &state,
        http::Method::DELETE,
        "/api/admin/empty-response-shield/session%3Aconv-9",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload = response_json(response).await;
    assert_eq!(payload["key"], "session:conv-9");
    assert_eq!(payload["unblocked"], true);

    // Unblocking again reports nothing was removed.
    let response = local_admin_shield_response(
        &state,
        http::Method::DELETE,
        "/api/admin/empty-response-shield/session%3Aconv-9",
    )
    .await;
    let payload = response_json(response).await;
    assert_eq!(payload["unblocked"], false);
}
