use crate::handlers::admin::model::ADMIN_EXTERNAL_MODELS_PROXY_NODE_CONFIG_KEY;
use crate::handlers::admin::request::AdminAppState;
use crate::handlers::shared::unix_secs_to_rfc3339;
use crate::GatewayError;
use aether_admin::system::{
    admin_system_config_default_value as admin_system_config_default_value_pure,
    admin_system_config_delete_keys as admin_system_config_delete_keys_pure,
    build_admin_system_config_deleted_payload,
    build_admin_system_config_detail_payload as build_admin_system_config_detail_payload_pure,
    build_admin_system_config_updated_payload,
    build_admin_system_configs_payload as build_admin_system_configs_payload_pure,
    is_sensitive_admin_system_config_key as is_sensitive_admin_system_config_key_pure,
    normalize_admin_system_config_key as normalize_admin_system_config_key_pure,
    parse_admin_system_config_update,
};
use aether_crypto::encrypt_python_fernet_plaintext;
use axum::body::Bytes;
use axum::http;
use serde_json::json;

const ADMIN_EXTERNAL_MODELS_CONFIG_ROUTE: &str = "/api/admin/models/external/config";

/// System config key holding the AI diagnostics summarizer settings. Its
/// nested `api_key` is masked on read and preserved on empty write.
const DIAGNOSTICS_SUMMARIZER_CONFIG_KEY: &str = "error_diagnostic_summarizer";

fn is_external_models_proxy_node_config_key(key: &str) -> bool {
    key.trim()
        .eq_ignore_ascii_case(ADMIN_EXTERNAL_MODELS_PROXY_NODE_CONFIG_KEY)
}

fn external_models_proxy_node_config_owner_error(
    key: &str,
) -> Option<(http::StatusCode, serde_json::Value)> {
    is_external_models_proxy_node_config_key(key).then(|| {
        (
            http::StatusCode::BAD_REQUEST,
            json!({
                "detail": format!(
                    "配置项 '{}' 由模型目录管理，请使用 {}",
                    ADMIN_EXTERNAL_MODELS_PROXY_NODE_CONFIG_KEY,
                    ADMIN_EXTERNAL_MODELS_CONFIG_ROUTE,
                )
            }),
        )
    })
}

fn normalize_admin_system_config_key(requested_key: &str) -> String {
    normalize_admin_system_config_key_pure(requested_key)
}

fn admin_system_config_delete_keys(requested_key: &str) -> Vec<String> {
    admin_system_config_delete_keys_pure(requested_key)
}

pub(crate) fn is_sensitive_admin_system_config_key(key: &str) -> bool {
    is_sensitive_admin_system_config_key_pure(key)
}

fn admin_system_config_default_value(key: &str) -> Option<serde_json::Value> {
    admin_system_config_default_value_pure(key)
}

fn legacy_admin_system_config_fallback_key(normalized_key: &str) -> Option<&'static str> {
    match normalized_key {
        "module.server_chan_push.enabled" => {
            Some("module.important_notification.server_chan_enabled")
        }
        "module.server_chan_push.send_key" => {
            Some("module.important_notification.server_chan_send_key")
        }
        "module.server_chan_push.template" => {
            Some("module.important_notification.server_chan_template")
        }
        _ => None,
    }
}

pub(crate) fn build_admin_system_configs_payload(
    entries: &[aether_data::repository::system::StoredSystemConfigEntry],
) -> serde_json::Value {
    let visible_entries = entries
        .iter()
        .filter(|entry| !is_external_models_proxy_node_config_key(&entry.key))
        .cloned()
        .collect::<Vec<_>>();
    build_admin_system_configs_payload_pure(&visible_entries)
}

pub(crate) async fn build_admin_system_config_detail_payload(
    state: &AdminAppState<'_>,
    requested_key: &str,
) -> Result<Result<serde_json::Value, (http::StatusCode, serde_json::Value)>, GatewayError> {
    let requested_key = requested_key.trim();
    if let Some(error) = external_models_proxy_node_config_owner_error(requested_key) {
        return Ok(Err(error));
    }
    let normalized_key = normalize_admin_system_config_key(requested_key);
    let mut value = state.read_system_config_json_value(&normalized_key).await?;
    if value.is_none() {
        if let Some(legacy_key) = legacy_admin_system_config_fallback_key(&normalized_key) {
            value = state.read_system_config_json_value(legacy_key).await?;
        }
    }
    let value = value.or_else(|| admin_system_config_default_value(&normalized_key));
    if normalized_key == DIAGNOSTICS_SUMMARIZER_CONFIG_KEY {
        // Mask the nested api_key secret; expose only an is_set flag.
        return Ok(Ok(build_summarizer_config_detail_payload(
            requested_key,
            value,
        )));
    }
    Ok(build_admin_system_config_detail_payload_pure(
        requested_key,
        value,
    ))
}

fn build_summarizer_config_detail_payload(
    requested_key: &str,
    value: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut payload = json!({
        "key": requested_key,
        "value": serde_json::Value::Null,
        "is_set": false,
    });
    let Some(mut object) = value.and_then(|value| value.as_object().map(ToOwned::to_owned)) else {
        return payload;
    };
    let api_key_set = object
        .get("api_key")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());
    if object.contains_key("api_key") {
        object.insert("api_key".to_string(), serde_json::Value::Null);
    }
    payload["value"] = serde_json::Value::Object(object);
    payload["is_set"] = json!(api_key_set);
    payload
}

async fn merge_summarizer_api_key(
    state: &AdminAppState<'_>,
    value: serde_json::Value,
) -> Result<serde_json::Value, GatewayError> {
    let Some(mut object) = value.as_object().map(ToOwned::to_owned) else {
        return Ok(value);
    };
    let incoming_provided = object
        .get("api_key")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some();
    if !incoming_provided {
        // Preserve the previously stored api_key when the client sends none.
        if let Some(existing) = state
            .read_system_config_json_value(DIAGNOSTICS_SUMMARIZER_CONFIG_KEY)
            .await?
        {
            if let Some(existing_key) = existing.get("api_key").cloned() {
                object.insert("api_key".to_string(), existing_key);
            }
        }
    }
    Ok(serde_json::Value::Object(object))
}

pub(crate) async fn apply_admin_system_config_update(
    state: &AdminAppState<'_>,
    requested_key: &str,
    request_body: &Bytes,
) -> Result<Result<serde_json::Value, (http::StatusCode, serde_json::Value)>, GatewayError> {
    if let Some(error) = external_models_proxy_node_config_owner_error(requested_key) {
        return Ok(Err(error));
    }
    let update = match parse_admin_system_config_update(requested_key, request_body) {
        Ok(update) => update,
        Err(err) => return Ok(Err(err)),
    };
    let mut value = update.value;
    let normalized_key = update.normalized_key;
    let description = update.description;

    if normalized_key == DIAGNOSTICS_SUMMARIZER_CONFIG_KEY {
        value = merge_summarizer_api_key(state, value).await?;
    }

    if is_sensitive_admin_system_config_key(&normalized_key)
        && value.as_str().is_some_and(|raw| !raw.is_empty())
    {
        let Some(encryption_key) = state
            .encryption_key()
            .filter(|value| !value.trim().is_empty())
        else {
            return Ok(Err((
                http::StatusCode::SERVICE_UNAVAILABLE,
                json!({ "detail": "系统配置写入需要可用的加密密钥" }),
            )));
        };
        let plaintext = value.as_str().unwrap();
        value = json!(encrypt_python_fernet_plaintext(encryption_key, plaintext)
            .map_err(|err| GatewayError::Internal(err.to_string()))?);
    }

    let updated = state
        .upsert_system_config_entry(&normalized_key, &value, description.as_deref())
        .await?;
    let display_value = if is_sensitive_admin_system_config_key(&normalized_key) {
        json!("********")
    } else {
        updated.value.clone()
    };
    Ok(Ok(build_admin_system_config_updated_payload(
        updated.key,
        display_value,
        updated.description,
        updated.updated_at_unix_secs,
    )))
}

pub(crate) async fn delete_admin_system_config(
    state: &AdminAppState<'_>,
    requested_key: &str,
) -> Result<Result<serde_json::Value, (http::StatusCode, serde_json::Value)>, GatewayError> {
    if let Some(error) = external_models_proxy_node_config_owner_error(requested_key) {
        return Ok(Err(error));
    }
    let delete_keys = admin_system_config_delete_keys(requested_key);
    let mut deleted = false;
    for key in &delete_keys {
        deleted |= state.delete_system_config_value(key).await?;
    }
    if !deleted {
        return Ok(Err((
            http::StatusCode::NOT_FOUND,
            json!({ "detail": format!("配置项 '{requested_key}' 不存在") }),
        )));
    }
    Ok(Ok(build_admin_system_config_deleted_payload(requested_key)))
}
