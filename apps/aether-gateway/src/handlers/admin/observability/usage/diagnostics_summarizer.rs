//! Detachable AI summary module for error diagnostics (Plan B B4).
//!
//! The module is opt-in and controlled by the `error_diagnostic_summarizer`
//! system config entry. When that entry is absent the `/summarize` endpoint
//! responds with `503 Service Unavailable` — the gateway never calls an LLM.
//! When the entry is present, the gateway builds a compact prompt from the
//! forensic `error_diagnostic` marker plus the captured upstream response,
//! calls an OpenAI-compatible `chat/completions` endpoint, and writes the
//! generated summary back into `request_metadata.error_diagnostic.summary`.
//!
//! Everything here is written as small, pure functions plus a single outbound
//! HTTP call so the behavior stays unit-testable and the whole feature can be
//! removed by dropping the system config entry.

use crate::GatewayError;
use aether_data_contracts::repository::usage::StoredRequestUsageAudit;
use serde_json::{json, Map, Value};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// System config key that controls this detachable summarizer. When unset the
/// feature is considered uninstalled and summarize returns `503`.
pub(crate) const ERROR_DIAGNOSTIC_SUMMARIZER_CONFIG_KEY: &str = "error_diagnostic_summarizer";

/// How long a generated summary stays fresh before a new call regenerates it.
const DIAGNOSTICS_SUMMARY_TTL_SECS: u64 = 24 * 60 * 60;

/// Upper bound on the number of bytes of captured body text fed to the model.
const DIAGNOSTICS_SUMMARY_MAX_BODY_BYTES: usize = 4_096;

/// System prompt instructing the model how to summarize a diagnostic event.
const DIAGNOSTICS_SUMMARY_SYSTEM_PROMPT: &str = "You are an expert API gateway \
diagnostician. You receive a JSON record describing a failed upstream request \
(the diagnostic marker, the gateway's own error fields, and the captured \
upstream response body). Produce a concise Chinese summary (at most 5 short \
sentences) that explains: what went wrong, the most likely root cause, and what \
an operator should check or fix. Do not invent details that are not present in \
the record. Output only the summary text, no preamble.";

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DiagnosticsSummarizerConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_secs: u64,
}

fn non_empty_string(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.to_string())
}

/// Parses the `error_diagnostic_summarizer` system config value into a usable
/// config. Returns `None` when the value is missing or incomplete, which the
/// caller maps to the `503` "not installed" response.
pub(crate) fn parse_diagnostics_summarizer_config(
    value: Option<&Value>,
) -> Option<DiagnosticsSummarizerConfig> {
    let object = value?.as_object()?;
    let base_url = non_empty_string(object.get("base_url"))?;
    let api_key = non_empty_string(object.get("api_key"))?;
    let model = non_empty_string(object.get("model"))?;
    let timeout_secs = object
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .unwrap_or(30);
    Some(DiagnosticsSummarizerConfig {
        base_url,
        api_key,
        model,
        timeout_secs,
    })
}

pub(crate) fn diagnostics_summarizer_base_url(config: &DiagnosticsSummarizerConfig) -> String {
    format!("{}/chat/completions", config.base_url.trim_end_matches('/'))
}

/// Serializes a captured body value into compact text bounded by
/// `DIAGNOSTICS_SUMMARY_MAX_BODY_BYTES` so a huge payload cannot blow up the
/// prompt (and the cost).
fn diagnostics_summary_body_excerpt(body: &Value) -> String {
    let compact = serde_json::to_string(body).unwrap_or_else(|_| "<unserializable body>".into());
    if compact.len() <= DIAGNOSTICS_SUMMARY_MAX_BODY_BYTES {
        return compact;
    }
    let mut end = DIAGNOSTICS_SUMMARY_MAX_BODY_BYTES;
    while !compact.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…(truncated)", &compact[..end])
}

fn diagnostics_summary_marker(item: &StoredRequestUsageAudit) -> Value {
    item.request_metadata
        .as_ref()
        .and_then(|metadata| metadata.get("error_diagnostic"))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Builds the user-facing prompt for the summarizer. It deliberately includes
/// the forensic marker, the gateway's own error fields, and the captured
/// upstream response body — but NOT the full downstream request body — both to
/// control cost and to avoid echoing secrets back to the model.
pub(crate) fn build_diagnostics_summary_prompt(
    item: &StoredRequestUsageAudit,
    response_body: Option<&Value>,
) -> String {
    let record = json!({
        "error_diagnostic": diagnostics_summary_marker(item),
        "status": item.status,
        "status_code": item.status_code,
        "error_message": item.error_message,
        "error_category": item.error_category,
        "model": item.model,
        "target_model": item.target_model,
        "provider_name": item.provider_name,
        "api_format": item.api_format,
        "is_stream": item.is_stream,
        "upstream_response_body": response_body
            .map(diagnostics_summary_body_excerpt)
            .unwrap_or_else(|| "<no upstream response body captured>".to_string()),
    });
    format!(
        "Please summarize the following failed upstream request record:\n{}",
        serde_json::to_string(&record).unwrap_or_default()
    )
}

/// Extracts the model's textual answer from an OpenAI-compatible
/// `chat/completions` response body.
pub(crate) fn parse_diagnostics_summary_response(value: &Value) -> Option<String> {
    let content = value
        .pointer("/choices/0/message/content")?
        .as_str()?
        .trim();
    if content.is_empty() {
        return None;
    }
    Some(content.to_string())
}

/// Returns a previously generated summary together with its generation time
/// when it is still fresh, letting the endpoint avoid paying for a redundant
/// model call.
pub(crate) fn cached_diagnostics_summary(
    metadata: Option<&Value>,
    now_unix_secs: u64,
) -> Option<(String, u64)> {
    let diagnostic = metadata?.get("error_diagnostic")?;
    let summary = diagnostic.get("summary")?.as_str()?.trim();
    if summary.is_empty() {
        return None;
    }
    let summarized_at = diagnostic.get("summarized_at_unix_secs")?.as_u64()?;
    if now_unix_secs.saturating_sub(summarized_at) >= DIAGNOSTICS_SUMMARY_TTL_SECS {
        return None;
    }
    Some((summary.to_string(), summarized_at))
}

/// Merges a freshly generated summary back into `request_metadata`, preserving
/// every pre-existing field (including the original `error_diagnostic` marker).
pub(crate) fn merge_diagnostics_summary(
    metadata: Option<Value>,
    summary: &str,
    now_unix_secs: u64,
) -> Value {
    let mut metadata = metadata
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    let diagnostic = metadata
        .entry("error_diagnostic")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(diagnostic) = diagnostic.as_object_mut() {
        diagnostic.insert("summary".to_string(), json!(summary));
        diagnostic.insert("summarized_at_unix_secs".to_string(), json!(now_unix_secs));
    }
    Value::Object(metadata)
}

/// Calls the configured OpenAI-compatible `chat/completions` endpoint and
/// returns the parsed summary text.
pub(crate) async fn call_diagnostics_summarizer(
    client: &reqwest::Client,
    config: &DiagnosticsSummarizerConfig,
    prompt: &str,
) -> Result<String, GatewayError> {
    let body = json!({
        "model": config.model,
        "messages": [
            { "role": "system", "content": DIAGNOSTICS_SUMMARY_SYSTEM_PROMPT },
            { "role": "user", "content": prompt },
        ],
        "temperature": 0.1,
    });
    let response = client
        .post(diagnostics_summarizer_base_url(config))
        .bearer_auth(&config.api_key)
        .json(&body)
        .send()
        .await
        .map_err(|err| {
            GatewayError::Internal(format!("diagnostics summarizer request failed: {err}"))
        })?;
    let status = response.status();
    let text = response.text().await.map_err(|err| {
        GatewayError::Internal(format!("diagnostics summarizer body read failed: {err}"))
    })?;
    if !status.is_success() {
        return Err(GatewayError::Internal(format!(
            "diagnostics summarizer returned HTTP {status}: {text}"
        )));
    }
    let parsed: Value = serde_json::from_str(&text).map_err(|err| {
        GatewayError::Internal(format!(
            "diagnostics summarizer returned invalid JSON: {err}"
        ))
    })?;
    parse_diagnostics_summary_response(&parsed).ok_or_else(|| {
        GatewayError::Internal(
            "diagnostics summarizer response did not contain message content".to_string(),
        )
    })
}

pub(crate) fn diagnostics_summary_now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Connectivity test for the configured summarizer, structured for the admin
/// UI: sends a minimal prompt and reports exactly where a failure happens
/// (transport, HTTP status, response parsing) instead of a bare 502.
pub(crate) async fn test_diagnostics_summarizer(
    client: &reqwest::Client,
    config: &DiagnosticsSummarizerConfig,
) -> Value {
    let endpoint = diagnostics_summarizer_base_url(config);
    let started = std::time::Instant::now();
    let body = json!({
        "model": config.model,
        "messages": [
            { "role": "user", "content": "Connectivity test. Reply with exactly: OK" },
        ],
        "temperature": 0.0,
    });
    let response = match client
        .post(&endpoint)
        .bearer_auth(&config.api_key)
        .json(&body)
        .send()
        .await
    {
        Ok(response) => response,
        Err(err) => {
            return json!({
                "ok": false,
                "stage": "transport",
                "endpoint": endpoint,
                "model": config.model,
                "detail": format!("无法连接到摘要器地址：{err}"),
            });
        }
    };
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if !status.is_success() {
        return json!({
            "ok": false,
            "stage": "http",
            "endpoint": endpoint,
            "model": config.model,
            "http_status": status.as_u16(),
            "elapsed_ms": elapsed_ms,
            "detail": format!("上游返回 HTTP {}", status.as_u16()),
            "upstream_excerpt": diagnostics_summary_text_excerpt(&text),
        });
    }
    let parsed: Value = match serde_json::from_str(&text) {
        Ok(parsed) => parsed,
        Err(err) => {
            return json!({
                "ok": false,
                "stage": "parse",
                "endpoint": endpoint,
                "model": config.model,
                "elapsed_ms": elapsed_ms,
                "detail": format!("上游返回的不是合法 JSON：{err}"),
                "upstream_excerpt": diagnostics_summary_text_excerpt(&text),
            });
        }
    };
    match parse_diagnostics_summary_response(&parsed) {
        Some(content) => json!({
            "ok": true,
            "endpoint": endpoint,
            "model": config.model,
            "elapsed_ms": elapsed_ms,
            "reply_excerpt": content.chars().take(120).collect::<String>(),
        }),
        None => json!({
            "ok": false,
            "stage": "parse",
            "endpoint": endpoint,
            "model": config.model,
            "elapsed_ms": elapsed_ms,
            "detail": "上游返回成功，但响应里没有 choices[0].message.content（模型名可能无效）",
            "upstream_excerpt": diagnostics_summary_text_excerpt(&text),
        }),
    }
}

fn diagnostics_summary_text_excerpt(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= 512 {
        return trimmed.to_string();
    }
    let mut end = 512;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &trimmed[..end])
}

/// Builds the outbound client for a summarizer call, honoring the configured
/// timeout. Kept separate so tests can substitute their own client.
pub(crate) fn diagnostics_summarizer_client(
    config: &DiagnosticsSummarizerConfig,
) -> Result<reqwest::Client, GatewayError> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(config.timeout_secs.max(1)))
        .build()
        .map_err(|err| {
            GatewayError::Internal(format!(
                "failed to build diagnostics summarizer client: {err}"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summarizer_config_value() -> Value {
        json!({
            "base_url": "https://llm.example.com/v1/",
            "api_key": "sk-test",
            "model": "gpt-test",
            "timeout_secs": 12,
        })
    }

    fn audit_with_marker(marker: Option<Value>) -> StoredRequestUsageAudit {
        let mut item = StoredRequestUsageAudit::new(
            "usage-1".to_string(),
            "req-1".to_string(),
            Some("user-1".to_string()),
            Some("key-1".to_string()),
            Some("alice".to_string()),
            Some("key-name".to_string()),
            "newapi".to_string(),
            "gemini-2.5-pro".to_string(),
            None,
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
            Some(400),
            Some("upstream rejected request".to_string()),
            Some("upstream_error".to_string()),
            Some(120),
            Some(30),
            "failed".to_string(),
            "settled".to_string(),
            1_700_000_100,
            1_700_000_101,
            Some(1_700_000_102),
        )
        .expect("usage row should build");
        item.request_metadata = marker.map(|marker| json!({ "error_diagnostic": marker }));
        item
    }

    #[test]
    fn parses_config_when_complete() {
        let config = parse_diagnostics_summarizer_config(Some(&summarizer_config_value()))
            .expect("config should parse");
        assert_eq!(config.base_url, "https://llm.example.com/v1/");
        assert_eq!(config.api_key, "sk-test");
        assert_eq!(config.model, "gpt-test");
        assert_eq!(config.timeout_secs, 12);
        assert_eq!(
            diagnostics_summarizer_base_url(&config),
            "https://llm.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn parse_config_rejects_missing_and_incomplete() {
        assert!(parse_diagnostics_summarizer_config(None).is_none());
        assert!(parse_diagnostics_summarizer_config(Some(&json!({}))).is_none());
        assert!(parse_diagnostics_summarizer_config(Some(&json!({
            "base_url": "https://x", "api_key": "", "model": "m"
        })))
        .is_none());
        assert!(parse_diagnostics_summarizer_config(Some(&json!("not-an-object"))).is_none());
    }

    #[test]
    fn parse_config_defaults_timeout() {
        let mut value = summarizer_config_value();
        value.as_object_mut().unwrap().remove("timeout_secs");
        let config = parse_diagnostics_summarizer_config(Some(&value)).expect("config");
        assert_eq!(config.timeout_secs, 30);
    }

    #[test]
    fn builds_prompt_with_marker_and_body() {
        let item = audit_with_marker(Some(json!({
            "kind": "upstream_4xx",
            "upstream_status": 400,
            "message": "invalid schema",
        })));
        let body = json!({ "error": { "code": 400, "message": "invalid field" } });
        let prompt = build_diagnostics_summary_prompt(&item, Some(&body));
        assert!(prompt.contains("upstream_4xx"));
        assert!(prompt.contains("invalid schema"));
        assert!(prompt.contains("invalid field"));
        assert!(prompt.contains("gemini-2.5-pro"));
    }

    #[test]
    fn builds_prompt_without_body() {
        let item = audit_with_marker(None);
        let prompt = build_diagnostics_summary_prompt(&item, None);
        assert!(prompt.contains("no upstream response body captured"));
    }

    #[test]
    fn truncates_large_body() {
        let huge = "x".repeat(DIAGNOSTICS_SUMMARY_MAX_BODY_BYTES * 2);
        let excerpt = diagnostics_summary_body_excerpt(&json!(huge));
        assert!(excerpt.ends_with("(truncated)"));
        assert!(excerpt.len() < huge.len());
    }

    #[test]
    fn parses_chat_completion_response() {
        let response = json!({
            "choices": [{ "message": { "content": "  上游 400，字段格式错误。 " } }],
        });
        assert_eq!(
            parse_diagnostics_summary_response(&response),
            Some("上游 400，字段格式错误。".to_string())
        );
        assert_eq!(parse_diagnostics_summary_response(&json!({})), None);
        assert_eq!(
            parse_diagnostics_summary_response(
                &json!({"choices": [{"message": {"content": "  "}}]})
            ),
            None
        );
    }

    #[test]
    fn cached_summary_honors_ttl() {
        let now = 1_000_000;
        let fresh = json!({
            "error_diagnostic": { "summary": "old summary", "summarized_at_unix_secs": now - 10 },
        });
        let stale = json!({
            "error_diagnostic": {
                "summary": "old summary",
                "summarized_at_unix_secs": now - DIAGNOSTICS_SUMMARY_TTL_SECS - 1,
            },
        });
        let missing = json!({ "error_diagnostic": { "kind": "upstream_4xx" } });
        assert_eq!(
            cached_diagnostics_summary(Some(&fresh), now),
            Some(("old summary".to_string(), now - 10))
        );
        assert!(cached_diagnostics_summary(Some(&stale), now).is_none());
        assert!(cached_diagnostics_summary(Some(&missing), now).is_none());
        assert!(cached_diagnostics_summary(None, now).is_none());
    }

    #[test]
    fn merge_summary_preserves_existing_metadata() {
        let metadata = json!({
            "error_diagnostic": { "kind": "upstream_4xx", "upstream_status": 400 },
            "other_field": "keep-me",
        });
        let merged = merge_diagnostics_summary(Some(metadata), "摘要内容", 42);
        assert_eq!(merged["other_field"], json!("keep-me"));
        assert_eq!(merged["error_diagnostic"]["kind"], json!("upstream_4xx"));
        assert_eq!(merged["error_diagnostic"]["summary"], json!("摘要内容"));
        assert_eq!(
            merged["error_diagnostic"]["summarized_at_unix_secs"],
            json!(42)
        );
    }

    #[test]
    fn merge_summary_handles_missing_metadata() {
        let merged = merge_diagnostics_summary(None, "摘要内容", 7);
        assert_eq!(merged["error_diagnostic"]["summary"], json!("摘要内容"));
        assert_eq!(
            merged["error_diagnostic"]["summarized_at_unix_secs"],
            json!(7)
        );
    }
}
