//! Admin surface for the detachable empty-response shield (F4): list the
//! currently blocked session/fingerprint keys and unblock them manually.

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
                    .map(|(key, remaining_secs)| {
                        let kind = if key.starts_with("session:") {
                            "session"
                        } else {
                            "fingerprint"
                        };
                        json!({
                            "key": key,
                            "kind": kind,
                            "remaining_secs": remaining_secs,
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
                    })),
                    "blocked": blocked,
                    "total": blocked.len(),
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
