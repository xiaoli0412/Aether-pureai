//! Admin surface for the detachable empty-response shield (F4): list the
//! currently blocked session/fingerprint keys, block keys manually, and
//! unblock them.

use crate::execution_runtime::empty_response_shield::{
    parse_empty_response_shield_config, EMPTY_RESPONSE_SHIELD_CONFIG_KEY,
};
use crate::handlers::admin::request::{AdminAppState, AdminRequestContext};
use crate::GatewayError;
use axum::{
    body::Body,
    http,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

const SHIELD_LIST_PATH: &str = "/api/admin/empty-response-shield";
const SHIELD_BLOCK_PATH: &str = "/api/admin/empty-response-shield/blocks";

fn shield_blocked_entry_kind(key: &str) -> &'static str {
    if key.starts_with("session:") {
        "session"
    } else {
        "fingerprint"
    }
}

pub(super) async fn maybe_build_local_admin_shield_response(
    state: &AdminAppState<'_>,
    request_context: &AdminRequestContext<'_>,
) -> Result<Option<Response<Body>>, GatewayError> {
    let route_kind = request_context
        .control_decision
        .as_ref()
        .and_then(|decision| decision.route_kind.as_deref());

    match route_kind {
        Some("list")
            if request_context.request_method == http::Method::GET
                && matches!(
                    request_context.request_path.as_str(),
                    SHIELD_LIST_PATH | "/api/admin/empty-response-shield/"
                ) =>
        {
            let app = state.app();
            let config_value = app
                .read_system_config_json_value(EMPTY_RESPONSE_SHIELD_CONFIG_KEY)
                .await?;
            let config = parse_empty_response_shield_config(config_value.as_ref());

            let blocked = match config.as_ref() {
                Some(config) => app
                    .empty_response_shield
                    .blocked_snapshot(config)
                    .into_iter()
                    .map(|entry| {
                        json!({
                            "key": entry.key,
                            "kind": shield_blocked_entry_kind(&entry.key),
                            "remaining_secs": entry.remaining_secs,
                            "strikes": entry.strikes,
                            "manual": entry.manual,
                        })
                    })
                    .collect::<Vec<_>>(),
                None => Vec::new(),
            };

            return Ok(Some(
                Json(json!({
                    "installed": config.is_some(),
                    "config": config.map(|config| json!({
                        "threshold": config.threshold,
                        "window_secs": config.window_secs,
                        "block_secs": config.block_secs,
                        "scope_by_client": config.scope_by_client,
                    })),
                    "blocked": blocked,
                    "total": blocked.len(),
                }))
                .into_response(),
            ));
        }
        Some("block")
            if request_context.request_method == http::Method::POST
                && matches!(
                    request_context.request_path.as_str(),
                    SHIELD_BLOCK_PATH | "/api/admin/empty-response-shield/blocks/"
                ) =>
        {
            let app = state.app();
            let config_value = app
                .read_system_config_json_value(EMPTY_RESPONSE_SHIELD_CONFIG_KEY)
                .await?;
            let Some(config) = parse_empty_response_shield_config(config_value.as_ref()) else {
                return Ok(Some(
                    (
                        http::StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({ "detail": "回空屏蔽未启用：请先在系统配置中开启 empty_response_shield" })),
                    )
                        .into_response(),
                ));
            };

            let query = request_context.request_query_string.as_deref();
            let block_secs = crate::handlers::admin::shared::query_param_value(query, "block_secs")
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(config.block_secs);

            // Accept either a full shield key (`key=`) or raw identity parts
            // (`session=` / `fingerprint=` with optional `api_key_id=`) so the
            // diagnostics page can ban a source without re-deriving the
            // client-scoped key format itself.
            let key = if let Some(key) = crate::handlers::admin::shared::query_param_value(
                query, "key",
            )
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
            {
                key
            } else {
                let scope = config
                    .scope_by_client
                    .then(|| {
                        crate::handlers::admin::shared::query_param_value(query, "api_key_id")
                            .map(|value| value.trim().to_string())
                            .filter(|value| !value.is_empty())
                    })
                    .flatten();
                if let Some(session) =
                    crate::handlers::admin::shared::query_param_value(query, "session")
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty())
                {
                    match scope.as_deref() {
                        Some(scope) => format!("session:{scope}:{session}"),
                        None => format!("session:{session}"),
                    }
                } else if let Some(fingerprint) =
                    crate::handlers::admin::shared::query_param_value(query, "fingerprint")
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty())
                {
                    match scope.as_deref() {
                        Some(scope) => format!("fp:{scope}:{fingerprint}"),
                        None => format!("fp:{fingerprint}"),
                    }
                } else {
                    return Ok(Some(
                        (
                            http::StatusCode::BAD_REQUEST,
                            Json(json!({ "detail": "缺少 key / session / fingerprint 参数" })),
                        )
                            .into_response(),
                    ));
                }
            };

            app.empty_response_shield.manual_block(&key, block_secs);
            tracing::info!(
                target: "aether_gateway::empty_response_shield",
                shield_key = %key,
                block_secs = block_secs,
                "empty-response shield key manually blocked from admin surface"
            );

            return Ok(Some(
                Json(json!({
                    "key": key,
                    "blocked": true,
                    "block_secs": block_secs,
                }))
                .into_response(),
            ));
        }
        Some("unblock")
            if request_context.request_method == http::Method::DELETE
                && request_context
                    .request_path
                    .starts_with("/api/admin/empty-response-shield/") =>
        {
            let Some(key) = shield_key_from_unblock_path(request_context.request_path.as_str())
            else {
                return Ok(Some(
                    (
                        http::StatusCode::BAD_REQUEST,
                        Json(json!({ "detail": "屏蔽键无效" })),
                    )
                        .into_response(),
                ));
            };

            let unblocked = state.app().empty_response_shield.unblock(&key);
            return Ok(Some(
                Json(json!({
                    "key": key,
                    "unblocked": unblocked,
                }))
                .into_response(),
            ));
        }
        _ => {}
    }

    Ok(None)
}

/// Extracts the shield key from `/api/admin/empty-response-shield/{key}`,
/// percent-decoding the segment.
fn shield_key_from_unblock_path(path: &str) -> Option<String> {
    let segment = path.strip_prefix("/api/admin/empty-response-shield/")?;
    let segment = segment.trim_end_matches('/');
    if segment.is_empty() || segment.contains('/') {
        return None;
    }
    let decoded = percent_decode(segment);
    let decoded = decoded.trim();
    if decoded.is_empty() {
        return None;
    }
    Some(decoded.to_string())
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(hex) = hex {
                if let Ok(value) = u8::from_str_radix(hex, 16) {
                    out.push(value);
                    index += 3;
                    continue;
                }
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{percent_decode, shield_key_from_unblock_path};

    #[test]
    fn extracts_plain_session_key() {
        assert_eq!(
            shield_key_from_unblock_path("/api/admin/empty-response-shield/session:abc-123"),
            Some("session:abc-123".to_string())
        );
    }

    #[test]
    fn extracts_percent_encoded_key() {
        assert_eq!(
            shield_key_from_unblock_path("/api/admin/empty-response-shield/session%3Aabc%20def"),
            Some("session:abc def".to_string())
        );
    }

    #[test]
    fn rejects_nested_or_empty_segments() {
        assert_eq!(
            shield_key_from_unblock_path("/api/admin/empty-response-shield/"),
            None
        );
        assert_eq!(
            shield_key_from_unblock_path("/api/admin/empty-response-shield/a/b"),
            None
        );
    }

    #[test]
    fn percent_decode_handles_plain_and_encoded() {
        assert_eq!(percent_decode("abc"), "abc");
        assert_eq!(percent_decode("a%3Ab"), "a:b");
        assert_eq!(percent_decode("100%"), "100%");
    }
}
