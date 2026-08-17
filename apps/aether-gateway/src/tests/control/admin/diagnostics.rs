use std::sync::Arc;

use aether_data::repository::usage::InMemoryUsageReadRepository;
use aether_data_contracts::repository::usage::StoredRequestUsageAudit;
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

const ADMIN_USAGE_DATA_UNAVAILABLE_DETAIL: &str = "Admin usage data unavailable";

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

async fn local_admin_diagnostics_response(
    state: &AppState,
    method: http::Method,
    uri: &str,
    body: Option<serde_json::Value>,
) -> axum::response::Response<Body> {
    let headers = trusted_admin_headers();
    let request_context = resolve_public_request_context(
        state,
        &method,
        &uri.parse().expect("uri should parse"),
        &headers,
        "trace-123",
    )
    .await
    .expect("request context should resolve");
    let body_bytes = body.map(|value| Bytes::from(value.to_string()));
    maybe_build_local_admin_usage_response(
        &AdminAppState::new(state),
        &AdminRequestContext::new(&request_context),
        body_bytes.as_ref(),
    )
    .await
    .expect("local diagnostics response should build")
    .expect("diagnostics route should resolve locally")
}

fn diagnostics_row(
    id: &str,
    request_id: &str,
    user_id: &str,
    api_key_id: &str,
    provider_name: &str,
    model: &str,
    status: &str,
    status_code: i32,
    created_at_unix_secs: i64,
    diagnostic: serde_json::Value,
) -> StoredRequestUsageAudit {
    let mut usage = StoredRequestUsageAudit::new(
        id.to_string(),
        request_id.to_string(),
        Some(user_id.to_string()),
        Some(api_key_id.to_string()),
        Some(format!("user-{user_id}")),
        Some(format!("key-{api_key_id}")),
        provider_name.to_string(),
        model.to_string(),
        Some(format!("{model}-target")),
        Some("provider-1".to_string()),
        Some("endpoint-1".to_string()),
        Some("provider-key-1".to_string()),
        Some("chat".to_string()),
        Some("openai:chat".to_string()),
        Some("openai".to_string()),
        Some("chat".to_string()),
        Some("openai:chat".to_string()),
        Some("openai".to_string()),
        Some("chat".to_string()),
        false,
        false,
        10,
        5,
        15,
        0.1,
        0.12,
        Some(status_code),
        (status == "failed").then(|| "request failed".to_string()),
        None,
        Some(420),
        Some(120),
        status.to_string(),
        "settled".to_string(),
        created_at_unix_secs,
        created_at_unix_secs + 1,
        Some(created_at_unix_secs + 2),
    )
    .expect("usage row should build");
    usage.request_metadata = Some(json!({ "error_diagnostic": diagnostic }));
    usage
}

fn app_state_with_rows(rows: Vec<StoredRequestUsageAudit>) -> AppState {
    let usage_repository = Arc::new(InMemoryUsageReadRepository::seed(rows));
    AppState::new()
        .expect("gateway should build")
        .with_data_state_for_tests(GatewayDataState::with_usage_reader_for_tests(
            usage_repository,
        ))
}

#[tokio::test]
async fn diagnostics_list_returns_only_rows_matching_kind() {
    let state = app_state_with_rows(vec![
        diagnostics_row(
            "usage-1",
            "req-empty",
            "user-1",
            "key-1",
            "Gemini",
            "gemini-2.5-pro",
            "failed",
            200,
            1_700_000_100,
            json!({
                "kind": "empty_response",
                "upstream_status": 200,
                "classification": "stop_status_code",
                "decision": "stop_local_failover",
                "message": "empty upstream response",
            }),
        ),
        diagnostics_row(
            "usage-2",
            "req-4xx",
            "user-1",
            "key-1",
            "Gemini",
            "gemini-2.5-pro",
            "failed",
            400,
            1_700_000_200,
            json!({
                "kind": "upstream_4xx",
                "upstream_status": 400,
            }),
        ),
    ]);

    let response = local_admin_diagnostics_response(
        &state,
        http::Method::GET,
        "/api/admin/diagnostics?kind=empty_response",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .expect("json should parse");
    assert_eq!(payload["total"], 1);
    assert_eq!(payload["records"][0]["request_id"], "req-empty");
    assert_eq!(payload["records"][0]["kind"], "empty_response");
    assert_eq!(payload["records"][0]["upstream_status"], 200);
    assert_eq!(payload["records"][0]["user_id"], "user-1");
    assert_eq!(payload["records"][0]["api_key_id"], "key-1");
    assert_eq!(payload["records"][0]["model"], "gemini-2.5-pro");
    assert_eq!(payload["records"][0]["status_code"], 200);
}

#[tokio::test]
async fn diagnostics_list_filters_by_api_key_id_and_user_id() {
    let state = app_state_with_rows(vec![
        diagnostics_row(
            "usage-1",
            "req-a",
            "user-1",
            "key-1",
            "Gemini",
            "gemini-2.5-pro",
            "failed",
            200,
            1_700_000_100,
            json!({ "kind": "empty_response", "upstream_status": 200 }),
        ),
        diagnostics_row(
            "usage-2",
            "req-b",
            "user-2",
            "key-2",
            "Gemini",
            "gemini-2.5-pro",
            "failed",
            200,
            1_700_000_200,
            json!({ "kind": "empty_response", "upstream_status": 200 }),
        ),
    ]);

    let response = local_admin_diagnostics_response(
        &state,
        http::Method::GET,
        "/api/admin/diagnostics?kind=empty_response&api_key_id=key-2",
        None,
    )
    .await;
    let payload: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .expect("json should parse");
    assert_eq!(payload["total"], 1);
    assert_eq!(payload["records"][0]["request_id"], "req-b");

    let response = local_admin_diagnostics_response(
        &state,
        http::Method::GET,
        "/api/admin/diagnostics?kind=empty_response&user_id=user-1",
        None,
    )
    .await;
    let payload: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .expect("json should parse");
    assert_eq!(payload["total"], 1);
    assert_eq!(payload["records"][0]["request_id"], "req-a");
}

#[tokio::test]
async fn diagnostics_list_rejects_invalid_from_param() {
    let state = app_state_with_rows(vec![]);
    let response = local_admin_diagnostics_response(
        &state,
        http::Method::GET,
        "/api/admin/diagnostics?from=not-a-number",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn diagnostics_detail_returns_forensic_payload() {
    let state = app_state_with_rows(vec![diagnostics_row(
        "usage-1",
        "req-empty",
        "user-1",
        "key-1",
        "Gemini",
        "gemini-2.5-pro",
        "failed",
        200,
        1_700_000_100,
        json!({
            "kind": "empty_response",
            "upstream_status": 200,
            "message": "empty upstream response",
        }),
    )]);

    let response = local_admin_diagnostics_response(
        &state,
        http::Method::GET,
        "/api/admin/diagnostics/req-empty",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .expect("json should parse");
    assert_eq!(payload["request_id"], "req-empty");
    assert_eq!(payload["error_diagnostic"]["kind"], "empty_response");
    assert_eq!(payload["error_diagnostic"]["upstream_status"], 200);
    assert_eq!(payload["status_code"], 200);
    assert_eq!(payload["user_id"], "user-1");
    assert_eq!(payload["api_key_id"], "key-1");
}

#[tokio::test]
async fn diagnostics_detail_unknown_request_id_returns_404() {
    let state = app_state_with_rows(vec![]);
    let response = local_admin_diagnostics_response(
        &state,
        http::Method::GET,
        "/api/admin/diagnostics/missing-request",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn diagnostics_list_returns_503_without_usage_reader() {
    let state = AppState::new()
        .expect("gateway should build")
        .with_data_state_for_tests(GatewayDataState::disabled());
    let response =
        local_admin_diagnostics_response(&state, http::Method::GET, "/api/admin/diagnostics", None)
            .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let payload: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .expect("json should parse");
    assert_eq!(payload["detail"], ADMIN_USAGE_DATA_UNAVAILABLE_DETAIL);
}

#[tokio::test]
async fn diagnostics_summarize_stub_returns_501() {
    let state = app_state_with_rows(vec![]);
    let response = local_admin_diagnostics_response(
        &state,
        http::Method::POST,
        "/api/admin/diagnostics/req-empty/summarize",
        Some(json!({})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    let payload: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .expect("json should parse");
    assert_eq!(payload["request_id"], "req-empty");
}
