//! Detachable empty-response shield (Plan F4).
//!
//! When the upstream keeps returning "empty" success responses (HTTP 200 with
//! no visible model output — the classic Gemini risk-control block proxied
//! through a passthrough upstream), this module stops wasting quota on a
//! session that is already being intercepted upstream:
//!
//! 1. Every observed empty response records a *strike* against a shield key.
//!    The key is the request's session id when one is present (sticky session
//!    token) and otherwise a stable fingerprint of the request (method, path,
//!    headers minus credentials, canonical body).
//! 2. Before dispatching a chat request the gateway checks the shield. If the
//!    key collected `threshold` strikes within `window_secs`, the key is
//!    blocked for `block_secs` (default 300s / 5 minutes) and the request is
//!    answered locally with a Google-safety-review style response instead of
//!    reaching the upstream.
//!
//! The module is driven by the system configuration key
//! `empty_response_shield`; when that entry is absent or disabled the shield
//! is inert and adds no behavior (zero-impact default). Strikes are kept in
//! memory with TTL pruning — no schema changes are required.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{json, Map, Value};

use crate::ai_serving::api::{
    CLAUDE_CHAT_STREAM_PLAN_KIND, CLAUDE_CHAT_SYNC_PLAN_KIND, CLAUDE_CLI_STREAM_PLAN_KIND,
    CLAUDE_CLI_SYNC_PLAN_KIND, GEMINI_CHAT_STREAM_PLAN_KIND, GEMINI_CHAT_SYNC_PLAN_KIND,
    GEMINI_CLI_STREAM_PLAN_KIND, GEMINI_CLI_SYNC_PLAN_KIND, OPENAI_CHAT_STREAM_PLAN_KIND,
    OPENAI_CHAT_SYNC_PLAN_KIND, OPENAI_IMAGE_SYNC_PLAN_KIND,
};
use crate::ai_serving::extract_pool_sticky_session_token;
use crate::orchestration::{request_fingerprint_from_headers_body, session_token_from_headers};

/// System configuration key that installs the shield.
pub(crate) const EMPTY_RESPONSE_SHIELD_CONFIG_KEY: &str = "empty_response_shield";

/// Default number of empty responses inside the window that trigger a block.
const DEFAULT_EMPTY_SHIELD_THRESHOLD: u64 = 3;
/// Default rolling window for counting empty responses (seconds).
const DEFAULT_EMPTY_SHIELD_WINDOW_SECS: u64 = 600;
/// Default block duration once the threshold is reached (seconds).
const DEFAULT_EMPTY_SHIELD_BLOCK_SECS: u64 = 300;
/// Upper bound on tracked shield keys before stale pruning kicks in.
const EMPTY_SHIELD_MAX_ENTRIES: usize = 100_000;
/// Upper bound on retained strikes per key.
const EMPTY_SHIELD_MAX_STRIKES_PER_KEY: usize = 64;

/// Parsed shield configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EmptyResponseShieldConfig {
    pub(crate) threshold: u64,
    pub(crate) window_secs: u64,
    pub(crate) block_secs: u64,
    /// When true (default) shield keys are scoped by the client API key so
    /// one abusive client cannot cause another client's identical request to
    /// be blocked.
    pub(crate) scope_by_client: bool,
}

impl Default for EmptyResponseShieldConfig {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_EMPTY_SHIELD_THRESHOLD,
            window_secs: DEFAULT_EMPTY_SHIELD_WINDOW_SECS,
            block_secs: DEFAULT_EMPTY_SHIELD_BLOCK_SECS,
            scope_by_client: true,
        }
    }
}

/// Parses the shield configuration from the raw system config value. Returns
/// `None` when the module is not installed (absent entry or `enabled=false`),
/// which keeps the shield fully inert.
pub(crate) fn parse_empty_response_shield_config(
    value: Option<&Value>,
) -> Option<EmptyResponseShieldConfig> {
    let object = value?.as_object()?;
    let enabled = object
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !enabled {
        return None;
    }
    let read_u64 = |key: &str, default: u64| -> u64 {
        object
            .get(key)
            .and_then(Value::as_u64)
            .filter(|value| *value > 0)
            .unwrap_or(default)
    };
    Some(EmptyResponseShieldConfig {
        threshold: read_u64("threshold", DEFAULT_EMPTY_SHIELD_THRESHOLD),
        window_secs: read_u64("window_secs", DEFAULT_EMPTY_SHIELD_WINDOW_SECS),
        block_secs: read_u64("block_secs", DEFAULT_EMPTY_SHIELD_BLOCK_SECS),
        scope_by_client: object
            .get("scope_by_client")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    })
}

#[derive(Debug)]
struct EmptyShieldEntry {
    strikes: VecDeque<Instant>,
    touched_at: Instant,
    /// Gateway request id that recorded the last strike, so retries within a
    /// single request count at most one strike.
    last_request_id: Option<String>,
}

/// One blocked key as surfaced to the admin block list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShieldBlockedEntry {
    pub(crate) key: String,
    pub(crate) remaining_secs: u64,
    pub(crate) strikes: usize,
    pub(crate) manual: bool,
}

/// In-memory shield state: per-key empty-response strikes. The block window
/// is evaluated lazily from the strike history so a key stays blocked for
/// `block_secs` after its most recent qualifying strike.
///
/// The tracker stays unarmed until a pre-dispatch gate observes a valid
/// configuration, so an uninstalled shield never accumulates state.
#[derive(Debug, Default)]
pub(crate) struct EmptyResponseShieldTracker {
    entries: Mutex<HashMap<String, EmptyShieldEntry>>,
    /// Manual blocks created from the admin surface (key -> block expiry).
    manual_blocks: Mutex<HashMap<String, Instant>>,
    armed: std::sync::atomic::AtomicBool,
    scope_by_client: std::sync::atomic::AtomicBool,
    last_prune_at: Mutex<Option<Instant>>,
}

/// Minimum interval between background prune sweeps.
const EMPTY_SHIELD_PRUNE_INTERVAL: Duration = Duration::from_secs(60);

impl EmptyResponseShieldTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Arms strike recording. Called by the pre-dispatch gate whenever a
    /// valid shield configuration is present; remembers the configured key
    /// scoping so strike sites (which run without the config) derive keys
    /// the same way the gate does.
    pub(crate) fn arm(&self, config: &EmptyResponseShieldConfig) {
        self.armed.store(true, std::sync::atomic::Ordering::Relaxed);
        self.scope_by_client
            .store(config.scope_by_client, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn is_armed(&self) -> bool {
        self.armed.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn scope_by_client(&self) -> bool {
        self.scope_by_client.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Records one empty-response strike for the key. No-op while the shield
    /// is unarmed (module uninstalled). Retries of the same gateway request
    /// (`request_id`) count at most one strike.
    pub(crate) fn record_strike(&self, key: &str, request_id: &str) {
        if !self.is_armed() {
            return;
        }
        let mut entries = self.entries.lock();
        if entries.len() >= EMPTY_SHIELD_MAX_ENTRIES {
            prune_locked_entries(&mut entries, DEFAULT_EMPTY_SHIELD_WINDOW_SECS * 4);
        }
        let entry = entries
            .entry(key.to_string())
            .or_insert_with(|| EmptyShieldEntry {
                strikes: VecDeque::new(),
                touched_at: Instant::now(),
                last_request_id: None,
            });
        if entry
            .last_request_id
            .as_deref()
            .is_some_and(|last| !request_id.is_empty() && last == request_id)
        {
            entry.touched_at = Instant::now();
            return;
        }
        entry.strikes.push_back(Instant::now());
        if entry.strikes.len() > EMPTY_SHIELD_MAX_STRIKES_PER_KEY {
            entry.strikes.pop_front();
        }
        entry.last_request_id = Some(request_id.to_string());
        entry.touched_at = Instant::now();
    }

    /// Manually blocks a key for `block_secs` (admin ban action).
    pub(crate) fn manual_block(&self, key: &str, block_secs: u64) {
        let until = Instant::now() + Duration::from_secs(block_secs.max(1));
        self.manual_blocks.lock().insert(key.to_string(), until);
    }

    /// Throttled prune sweep honoring the configured window/block horizon.
    /// Called from the pre-dispatch gate; runs at most once per
    /// `EMPTY_SHIELD_PRUNE_INTERVAL`.
    pub(crate) fn maybe_prune(&self, config: &EmptyResponseShieldConfig) {
        let now = Instant::now();
        {
            let mut last = self.last_prune_at.lock();
            if last.is_some_and(|instant| now.duration_since(instant) < EMPTY_SHIELD_PRUNE_INTERVAL)
            {
                return;
            }
            *last = Some(now);
        }
        let horizon = config.window_secs + config.block_secs + 300;
        prune_locked_entries(&mut self.entries.lock(), horizon);
        self.manual_blocks.lock().retain(|_, until| *until > now);
    }

    /// Returns how long (seconds) the key remains blocked, or `None` when it
    /// is not currently blocked under `config`. Manual blocks take priority
    /// over strike-derived blocks.
    pub(crate) fn blocked_remaining_secs(
        &self,
        key: &str,
        config: &EmptyResponseShieldConfig,
    ) -> Option<u64> {
        let now = Instant::now();
        if let Some(until) = self.manual_blocks.lock().get(key).copied() {
            if until > now {
                return Some(until.duration_since(now).as_secs().max(1));
            }
        }
        let mut entries = self.entries.lock();
        let entry = entries.get_mut(key)?;
        let window = Duration::from_secs(config.window_secs);
        entry
            .strikes
            .retain(|strike| now.duration_since(*strike) <= window);
        if (entry.strikes.len() as u64) < config.threshold {
            return None;
        }
        let latest = *entry.strikes.back()?;
        let block_until = latest + Duration::from_secs(config.block_secs);
        if now >= block_until {
            entry.strikes.clear();
            return None;
        }
        entry.touched_at = now;
        Some(block_until.duration_since(now).as_secs().max(1))
    }

    /// Clears all strikes and manual blocks for a key (manual unblock).
    /// Returns whether anything existed for the key.
    pub(crate) fn unblock(&self, key: &str) -> bool {
        let removed_manual = self.manual_blocks.lock().remove(key).is_some();
        let removed_entry = self.entries.lock().remove(key).is_some();
        removed_manual || removed_entry
    }

    /// Snapshot of currently blocked keys (strike-derived and manual) with
    /// their remaining block seconds, sorted by remaining time.
    pub(crate) fn blocked_snapshot(&self, config: &EmptyResponseShieldConfig) -> Vec<ShieldBlockedEntry> {
        let strike_keys: Vec<String> = self.entries.lock().keys().cloned().collect();
        let manual_keys: Vec<String> = self.manual_blocks.lock().keys().cloned().collect();
        let mut blocked = Vec::new();
        for key in strike_keys {
            let strikes = self
                .entries
                .lock()
                .get(&key)
                .map(|entry| entry.strikes.len())
                .unwrap_or(0);
            if let Some(remaining) = self.blocked_remaining_secs(&key, config) {
                let manual = manual_keys.contains(&key);
                blocked.push(ShieldBlockedEntry {
                    key,
                    remaining_secs: remaining,
                    strikes,
                    manual,
                });
            }
        }
        for key in manual_keys {
            if blocked.iter().any(|entry| entry.key == key) {
                continue;
            }
            if let Some(remaining) = self.blocked_remaining_secs(&key, config) {
                blocked.push(ShieldBlockedEntry {
                    key,
                    remaining_secs: remaining,
                    strikes: 0,
                    manual: true,
                });
            }
        }
        blocked.sort_by(|a, b| {
            b.remaining_secs
                .cmp(&a.remaining_secs)
                .then_with(|| a.key.cmp(&b.key))
        });
        blocked
    }

    pub(crate) fn prune_stale(&self) {
        prune_locked_entries(
            &mut self.entries.lock(),
            DEFAULT_EMPTY_SHIELD_WINDOW_SECS * 4,
        );
    }

    #[cfg(test)]
    pub(crate) fn strike_count(&self, key: &str) -> usize {
        self.entries
            .lock()
            .get(key)
            .map(|entry| entry.strikes.len())
            .unwrap_or(0)
    }
}

fn prune_locked_entries(entries: &mut HashMap<String, EmptyShieldEntry>, max_age_secs: u64) {
    let now = Instant::now();
    let max_age = Duration::from_secs(max_age_secs.max(1));
    entries.retain(|_, entry| now.duration_since(entry.touched_at) < max_age);
}

/// Formats a session shield key, optionally scoped by the client API key so
/// different clients never share block state.
fn format_session_key(scope: Option<&str>, token: &str) -> String {
    match scope {
        Some(scope) => format!("session:{scope}:{token}"),
        None => format!("session:{token}"),
    }
}

/// Formats a fingerprint shield key, optionally scoped by the client API key.
fn format_fingerprint_key(scope: Option<&str>, fingerprint: &str) -> String {
    match scope {
        Some(scope) => format!("fp:{scope}:{fingerprint}"),
        None => format!("fp:{fingerprint}"),
    }
}

fn client_scope_segment(scope_by_client: bool, client_api_key_id: Option<&str>) -> Option<String> {
    scope_by_client
        .then(|| {
            client_api_key_id
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
        .flatten()
}

/// Derives the shield key for a request from the session id when present
/// (request body first, then recognized session headers), falling back to
/// the request fingerprint. Kept in sync with the report-context injection.
pub(crate) fn shield_key_for_request(
    parts: &http::request::Parts,
    body_json: &Value,
    scope_by_client: bool,
    client_api_key_id: Option<&str>,
) -> Option<String> {
    let scope = client_scope_segment(scope_by_client, client_api_key_id);
    if let Some(token) = extract_pool_sticky_session_token(body_json) {
        return Some(format_session_key(scope.as_deref(), &token));
    }
    if let Some(token) = session_token_from_headers(&parts.headers) {
        return Some(format_session_key(scope.as_deref(), &token));
    }
    Some(format_fingerprint_key(
        scope.as_deref(),
        &request_fingerprint_from_headers_body(&parts.headers, body_json),
    ))
}

/// Reads the shield key back out of a report context (which carries the
/// injected `session_id` / `request_fingerprint` fields plus the client
/// `api_key_id` used for scoping).
pub(crate) fn shield_key_from_report_context(
    report_context: Option<&Value>,
    scope_by_client: bool,
) -> Option<String> {
    let context = report_context?;
    let scope = client_scope_segment(
        scope_by_client,
        context
            .get("api_key_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty()),
    );
    if let Some(session) = context
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(format_session_key(scope.as_deref(), session));
    }
    context
        .get("request_fingerprint")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|fingerprint| format_fingerprint_key(scope.as_deref(), fingerprint))
}

/// Builds a local "Google safety review" style response for a blocked chat
/// request, matching the client's API format. Returns `None` for plan kinds
/// the shield does not synthesize responses for (non-chat families), letting
/// those requests proceed normally.
pub(crate) fn build_shield_local_response(
    plan_kind: &str,
    body_json: &Value,
    is_stream: bool,
) -> Option<(u16, std::collections::BTreeMap<String, String>, Vec<u8>)> {
    let model = body_json
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();

    match plan_kind {
        OPENAI_CHAT_SYNC_PLAN_KIND => {
            let body = build_openai_chat_safety_body(&model);
            Some(json_response(&body))
        }
        OPENAI_CHAT_STREAM_PLAN_KIND if !is_stream => {
            let body = build_openai_chat_safety_body(&model);
            Some(json_response(&body))
        }
        OPENAI_CHAT_STREAM_PLAN_KIND => {
            let body = build_openai_chat_safety_sse(&model);
            Some(sse_response(body))
        }
        GEMINI_CHAT_SYNC_PLAN_KIND | GEMINI_CLI_SYNC_PLAN_KIND => {
            let body = build_gemini_safety_body();
            Some(json_response(&body))
        }
        GEMINI_CHAT_STREAM_PLAN_KIND | GEMINI_CLI_STREAM_PLAN_KIND => {
            let frame = build_gemini_safety_body();
            let body = format!("data: {}\n\n", frame);
            Some(sse_response(body))
        }
        CLAUDE_CHAT_SYNC_PLAN_KIND | CLAUDE_CLI_SYNC_PLAN_KIND => {
            let body = build_claude_safety_body(&model);
            Some(json_response(&body))
        }
        CLAUDE_CHAT_STREAM_PLAN_KIND | CLAUDE_CLI_STREAM_PLAN_KIND => {
            let body = build_claude_safety_sse(&model);
            Some(sse_response(body))
        }
        _ => None,
    }
}

fn json_response(body: &Value) -> (u16, std::collections::BTreeMap<String, String>, Vec<u8>) {
    let mut headers = std::collections::BTreeMap::new();
    headers.insert("content-type".to_string(), "application/json".to_string());
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    (200, headers, bytes)
}

fn sse_response(body: String) -> (u16, std::collections::BTreeMap<String, String>, Vec<u8>) {
    let mut headers = std::collections::BTreeMap::new();
    headers.insert("content-type".to_string(), "text/event-stream".to_string());
    headers.insert("cache-control".to_string(), "no-cache".to_string());
    (200, headers, body.into_bytes())
}

/// Pre-dispatch shield gate for local execution paths. Returns a locally built
/// "Google safety review" style response when the request's shield key is
/// currently blocked; returns `None` (proceed normally) when the module is
/// uninstalled, the key is not blocked, or the plan kind has no synthetic
/// response shape.
pub(crate) async fn maybe_build_shielded_local_response(
    state: &crate::AppState,
    parts: &http::request::Parts,
    body_json: &Value,
    trace_id: &str,
    decision: &crate::control::GatewayControlDecision,
    plan_kind: &str,
    is_stream: bool,
) -> Result<Option<axum::response::Response<axum::body::Body>>, crate::GatewayError> {
    let config_value = state
        .read_system_config_json_value(EMPTY_RESPONSE_SHIELD_CONFIG_KEY)
        .await?;
    let Some(config) = parse_empty_response_shield_config(config_value.as_ref()) else {
        return Ok(None);
    };

    // The shield is installed: arm strike recording and run a throttled
    // prune sweep honoring the configured horizon.
    state.empty_response_shield.arm(&config);
    state.empty_response_shield.maybe_prune(&config);

    let client_api_key_id = decision
        .auth_context
        .as_ref()
        .map(|context| context.api_key_id.as_str());
    let Some(key) = shield_key_for_request(
        parts,
        body_json,
        config.scope_by_client,
        client_api_key_id,
    ) else {
        return Ok(None);
    };
    let Some(remaining_secs) = state
        .empty_response_shield
        .blocked_remaining_secs(&key, &config)
    else {
        return Ok(None);
    };

    let Some((status_code, headers, bytes)) =
        build_shield_local_response(plan_kind, body_json, is_stream)
    else {
        // No synthetic response shape for this family: let it through.
        return Ok(None);
    };

    tracing::info!(
        target: "aether_gateway::empty_response_shield",
        shield_key = %key,
        remaining_secs = remaining_secs,
        plan_kind = %plan_kind,
        "empty-response shield blocked request locally"
    );

    record_shield_blocked_usage(
        state,
        parts,
        body_json,
        trace_id,
        decision,
        plan_kind,
        is_stream,
        &key,
        remaining_secs,
        status_code,
        &headers,
        &bytes,
    )
    .await;

    let response = crate::api::response::build_client_response_from_parts(
        status_code,
        &headers,
        axum::body::Body::from(bytes),
        trace_id,
        Some(decision),
    )?;
    Ok(Some(response))
}

/// Records a shield strike from an observed empty response. Called from the
/// sync/stream empty-detection sites; the shield key is read from the report
/// context (session id preferred, request fingerprint fallback). Retries of
/// the same gateway request count at most one strike.
pub(crate) fn record_shield_strike_from_report_context(
    state: &crate::AppState,
    report_context: Option<&Value>,
    request_id: &str,
) {
    if !state.empty_response_shield.is_armed() {
        return;
    }
    let scope_by_client = state.empty_response_shield.scope_by_client();
    let Some(key) = shield_key_from_report_context(report_context, scope_by_client) else {
        return;
    };
    state.empty_response_shield.record_strike(&key, request_id);
}

/// Client API format recorded for a blocked request, derived from the plan
/// kind.
fn shield_api_format_for_plan_kind(plan_kind: &str) -> &'static str {
    match plan_kind {
        OPENAI_CHAT_SYNC_PLAN_KIND | OPENAI_CHAT_STREAM_PLAN_KIND => "openai:chat",
        GEMINI_CHAT_SYNC_PLAN_KIND
        | GEMINI_CHAT_STREAM_PLAN_KIND
        | GEMINI_CLI_SYNC_PLAN_KIND
        | GEMINI_CLI_STREAM_PLAN_KIND => "gemini:content",
        CLAUDE_CHAT_SYNC_PLAN_KIND
        | CLAUDE_CHAT_STREAM_PLAN_KIND
        | CLAUDE_CLI_SYNC_PLAN_KIND
        | CLAUDE_CLI_STREAM_PLAN_KIND => "claude:messages",
        _ => "unknown",
    }
}

/// Diagnostic kind marker persisted on blocked usage records so the admin
/// diagnostics surface can list shield blocks.
pub(crate) const EMPTY_RESPONSE_SHIELD_DIAGNOSTIC_KIND: &str = "empty_response_shield";

#[allow(clippy::too_many_arguments)]
async fn record_shield_blocked_usage(
    state: &crate::AppState,
    parts: &http::request::Parts,
    body_json: &Value,
    trace_id: &str,
    decision: &crate::control::GatewayControlDecision,
    plan_kind: &str,
    is_stream: bool,
    shield_key: &str,
    remaining_secs: u64,
    status_code: u16,
    response_headers: &std::collections::BTreeMap<String, String>,
    response_bytes: &[u8],
) {
    if !state.usage_runtime.is_enabled() {
        return;
    }

    let auth_context = crate::control::resolve_execution_runtime_auth_context(
        state,
        decision,
        &parts.headers,
        &parts.uri,
        trace_id,
    )
    .await
    .ok()
    .flatten();

    let model = body_json
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .trim()
        .to_string();
    let api_format = shield_api_format_for_plan_kind(plan_kind);
    let message = format!(
        "empty-response shield: blocked locally for ~{remaining_secs}s after repeated empty upstream responses"
    );

    let client_body: Value = serde_json::from_slice(response_bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(response_bytes).into_owned()));

    let mut request_metadata = Map::new();
    request_metadata.insert("trace_id".to_string(), Value::String(trace_id.to_string()));
    let session_identity = extract_pool_sticky_session_token(body_json)
        .or_else(|| session_token_from_headers(&parts.headers));
    if let Some(session) = session_identity {
        request_metadata.insert("session_id".to_string(), Value::String(session));
    } else {
        request_metadata.insert(
            "request_fingerprint".to_string(),
            Value::String(request_fingerprint_from_headers_body(
                &parts.headers,
                body_json,
            )),
        );
    }
    request_metadata.insert(
        "error_diagnostic".to_string(),
        json!({
            "kind": EMPTY_RESPONSE_SHIELD_DIAGNOSTIC_KIND,
            "upstream_status": null,
            "classification": "empty_response_shield",
            "decision": "blocked_locally",
            "message": message,
            "shield_key": shield_key,
            "remaining_secs": remaining_secs,
        }),
    );

    let mut response_header_map = Map::new();
    for (name, value) in response_headers {
        response_header_map.insert(name.clone(), Value::String(value.clone()));
    }

    let data = aether_usage_runtime::UsageEventData {
        user_id: auth_context.as_ref().map(|context| context.user_id.clone()),
        api_key_id: auth_context
            .as_ref()
            .map(|context| context.api_key_id.clone()),
        username: auth_context
            .as_ref()
            .and_then(|context| context.username.clone()),
        api_key_name: auth_context
            .as_ref()
            .and_then(|context| context.api_key_name.clone()),
        provider_name: "empty-response-shield".to_string(),
        model,
        request_type: Some("chat".to_string()),
        api_format: Some(api_format.to_string()),
        api_family: api_format
            .split_once(':')
            .map(|(family, _)| family.to_string()),
        endpoint_kind: api_format.split_once(':').map(|(_, kind)| kind.to_string()),
        is_stream: Some(is_stream),
        status_code: Some(status_code),
        error_message: Some(message),
        request_body: Some(body_json.clone()),
        client_response_headers: Some(Value::Object(response_header_map)),
        client_response_body: Some(client_body),
        route_family: decision.route_family.clone(),
        route_kind: decision.route_kind.clone(),
        request_metadata: Some(Value::Object(request_metadata)),
        ..aether_usage_runtime::UsageEventData::default()
    };

    state
        .usage_runtime
        .record_terminal_event_direct(
            state.usage_lifecycle_data_state().as_ref(),
            aether_usage_runtime::UsageEvent::new(
                aether_usage_runtime::UsageEventType::Failed,
                trace_id,
                data,
            ),
        )
        .await;
}

/// OpenAI chat-completion shaped safety response. Uses `finish_reason =
/// content_filter` with an empty assistant message, the standard signal that
/// a provider safety system withheld the output.
fn build_openai_chat_safety_body(model: &str) -> Value {
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    json!({
        "id": format!("chatcmpl-shield-{}", uuid::Uuid::now_v7()),
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "",
            },
            "finish_reason": "content_filter",
        }],
        "usage": {
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "total_tokens": 0,
        },
    })
}

fn build_openai_chat_safety_sse(model: &str) -> String {
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let id = format!("chatcmpl-shield-{}", uuid::Uuid::now_v7());
    let first = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "content": "" },
            "finish_reason": null,
        }],
    });
    let last = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "content_filter",
        }],
    });
    format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n")
}

/// Gemini native shaped safety response. Faithfully mirrors a real Gemini
/// prompt-level safety block: HTTP 200 with a `promptFeedback` carrying
/// `blockReason = SAFETY` and NO candidates (the shape verified against the
/// official API — real responses never combine a prompt blockReason with
/// candidates). Clients treat this as terminal and do not retry.
fn build_gemini_safety_body() -> Value {
    json!({
        "promptFeedback": {
            "blockReason": "SAFETY",
            "safetyRatings": [
                {
                    "category": "HARM_CATEGORY_HARASSMENT",
                    "probability": "HIGH",
                    "blocked": true,
                },
                {
                    "category": "HARM_CATEGORY_HATE_SPEECH",
                    "probability": "NEGLIGIBLE",
                    "blocked": false,
                },
                {
                    "category": "HARM_CATEGORY_SEXUALLY_EXPLICIT",
                    "probability": "NEGLIGIBLE",
                    "blocked": false,
                },
                {
                    "category": "HARM_CATEGORY_DANGEROUS_CONTENT",
                    "probability": "NEGLIGIBLE",
                    "blocked": false,
                },
            ],
        },
    })
}

/// Claude messages shaped safety response. Uses an empty content array with
/// `stop_reason = refusal`.
fn build_claude_safety_body(model: &str) -> Value {
    json!({
        "id": format!("msg_shield_{}", uuid::Uuid::now_v7().simple()),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [],
        "stop_reason": "refusal",
        "stop_sequence": null,
        "usage": { "input_tokens": 0, "output_tokens": 0 },
    })
}

/// Claude streaming safety response following the Anthropic SSE protocol:
/// `message_start` (message envelope) -> `message_delta` (stop_reason) ->
/// `message_stop`. Strict SDKs parse events as a discriminated union, so a
/// bare `message_stop` carrying a full message object would be rejected.
fn build_claude_safety_sse(model: &str) -> String {
    let message = build_claude_safety_body(model);
    let mut envelope = message.clone();
    if let Some(object) = envelope.as_object_mut() {
        object.insert("stop_reason".to_string(), Value::Null);
    }
    let message_start = json!({
        "type": "message_start",
        "message": envelope,
    });
    let message_delta = json!({
        "type": "message_delta",
        "delta": { "stop_reason": "refusal", "stop_sequence": null },
        "usage": { "output_tokens": 0 },
    });
    let message_stop = json!({ "type": "message_stop" });
    format!(
        "event: message_start\ndata: {message_start}\n\n\
         event: message_delta\ndata: {message_delta}\n\n\
         event: message_stop\ndata: {message_stop}\n\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(threshold: u64, window_secs: u64, block_secs: u64) -> EmptyResponseShieldConfig {
        EmptyResponseShieldConfig {
            threshold,
            window_secs,
            block_secs,
            scope_by_client: true,
        }
    }

    #[test]
    fn parses_config_with_defaults() {
        let value = json!({ "enabled": true, "threshold": 5 });
        let parsed = parse_empty_response_shield_config(Some(&value)).expect("config should parse");
        assert_eq!(parsed.threshold, 5);
        assert_eq!(parsed.window_secs, DEFAULT_EMPTY_SHIELD_WINDOW_SECS);
        assert_eq!(parsed.block_secs, DEFAULT_EMPTY_SHIELD_BLOCK_SECS);
        assert!(parsed.scope_by_client);
    }

    #[test]
    fn parses_scope_by_client_override() {
        let value = json!({ "enabled": true, "scope_by_client": false });
        let parsed = parse_empty_response_shield_config(Some(&value)).expect("config should parse");
        assert!(!parsed.scope_by_client);
    }

    #[test]
    fn absent_or_disabled_config_is_inert() {
        assert!(parse_empty_response_shield_config(None).is_none());
        let disabled = json!({ "enabled": false });
        assert!(parse_empty_response_shield_config(Some(&disabled)).is_none());
        let not_object = json!("on");
        assert!(parse_empty_response_shield_config(Some(&not_object)).is_none());
    }

    #[test]
    fn blocks_after_threshold_strikes_then_expires() {
        let tracker = EmptyResponseShieldTracker::new();
        let cfg = config(2, 600, 300);
        tracker.arm(&cfg);
        tracker.record_strike("session:a", "req-1");
        assert!(tracker.blocked_remaining_secs("session:a", &cfg).is_none());
        tracker.record_strike("session:a", "req-2");
        assert!(tracker.blocked_remaining_secs("session:a", &cfg).is_some());
        // A different key is unaffected.
        assert!(tracker.blocked_remaining_secs("session:b", &cfg).is_none());
    }

    #[test]
    fn retries_of_the_same_request_count_one_strike() {
        let tracker = EmptyResponseShieldTracker::new();
        let cfg = config(2, 600, 300);
        tracker.arm(&cfg);
        tracker.record_strike("session:a", "req-1");
        tracker.record_strike("session:a", "req-1");
        tracker.record_strike("session:a", "req-1");
        assert_eq!(tracker.strike_count("session:a"), 1);
        assert!(tracker.blocked_remaining_secs("session:a", &cfg).is_none());
    }

    #[test]
    fn manual_block_blocks_and_unblocks() {
        let tracker = EmptyResponseShieldTracker::new();
        let cfg = config(3, 600, 300);
        tracker.arm(&cfg);
        tracker.manual_block("session:bad", 120);
        let remaining = tracker
            .blocked_remaining_secs("session:bad", &cfg)
            .expect("manual block should block");
        assert!(remaining <= 120 && remaining >= 1);
        let snapshot = tracker.blocked_snapshot(&cfg);
        assert_eq!(snapshot.len(), 1);
        assert!(snapshot[0].manual);
        assert!(tracker.unblock("session:bad"));
        assert!(tracker.blocked_remaining_secs("session:bad", &cfg).is_none());
    }

    #[test]
    fn unblock_clears_strikes() {
        let tracker = EmptyResponseShieldTracker::new();
        let cfg = config(1, 600, 300);
        tracker.arm(&cfg);
        tracker.record_strike("fp:x", "req-1");
        assert!(tracker.blocked_remaining_secs("fp:x", &cfg).is_some());
        assert!(tracker.unblock("fp:x"));
        assert!(tracker.blocked_remaining_secs("fp:x", &cfg).is_none());
        assert!(!tracker.unblock("fp:x"));
    }

    #[test]
    fn blocked_snapshot_lists_blocked_keys() {
        let tracker = EmptyResponseShieldTracker::new();
        let cfg = config(1, 600, 300);
        tracker.arm(&cfg);
        tracker.record_strike("session:a", "req-1");
        tracker.record_strike("session:b", "req-2");
        let snapshot = tracker.blocked_snapshot(&cfg);
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot.iter().all(|entry| !entry.manual && entry.strikes == 1));
    }

    #[test]
    fn unarmed_tracker_ignores_strikes() {
        let tracker = EmptyResponseShieldTracker::new();
        let cfg = config(1, 600, 300);
        tracker.record_strike("session:a", "req-1");
        assert!(tracker.blocked_remaining_secs("session:a", &cfg).is_none());
        assert_eq!(tracker.strike_count("session:a"), 0);
        // Arming enables recording.
        tracker.arm(&cfg);
        tracker.record_strike("session:a", "req-2");
        assert_eq!(tracker.strike_count("session:a"), 1);
    }

    #[test]
    fn shield_key_prefers_session_over_fingerprint() {
        let body = json!({ "session_id": "sess-1", "messages": [] });
        let parts = test_parts();
        let key = shield_key_for_request(&parts, &body, false, None).expect("key should derive");
        assert_eq!(key, "session:sess-1");
    }

    #[test]
    fn shield_key_scopes_by_client_api_key() {
        let body = json!({ "session_id": "sess-1", "messages": [] });
        let parts = test_parts();
        let scoped =
            shield_key_for_request(&parts, &body, true, Some("key-1")).expect("key should derive");
        assert_eq!(scoped, "session:key-1:sess-1");
        // Without a resolvable client key the key stays unscoped rather than
        // colliding under an empty scope segment.
        let unscoped =
            shield_key_for_request(&parts, &body, true, None).expect("key should derive");
        assert_eq!(unscoped, "session:sess-1");
    }

    #[test]
    fn shield_key_uses_session_header_when_body_has_none() {
        let body = json!({ "messages": [] });
        let request = http::Request::builder()
            .method(http::Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("session-id", "hdr-sess-7")
            .body(())
            .expect("request should build");
        let (parts, _) = request.into_parts();
        let key = shield_key_for_request(&parts, &body, false, None).expect("key should derive");
        assert_eq!(key, "session:hdr-sess-7");
    }

    #[test]
    fn shield_key_falls_back_to_fingerprint() {
        let body = json!({ "messages": [{ "role": "user", "content": "hi" }] });
        let parts = test_parts();
        let key = shield_key_for_request(&parts, &body, false, None).expect("key should derive");
        assert!(key.starts_with("fp:"));
        assert!(key.len() > 3);
    }

    #[test]
    fn fingerprint_is_stable_across_key_order() {
        let parts = test_parts();
        let a = json!({ "model": "m", "messages": [] });
        let b = json!({ "messages": [], "model": "m" });
        assert_eq!(
            request_fingerprint_from_headers_body(&parts.headers, &a),
            request_fingerprint_from_headers_body(&parts.headers, &b)
        );
    }

    #[test]
    fn report_context_key_matches_request_key() {
        let body = json!({ "session_id": "sess-9", "messages": [] });
        let parts = test_parts();
        let request_key = shield_key_for_request(&parts, &body, true, Some("key-9")).unwrap();
        let context = json!({ "session_id": "sess-9", "api_key_id": "key-9" });
        let context_key = shield_key_from_report_context(Some(&context), true).unwrap();
        assert_eq!(request_key, context_key);
    }

    #[test]
    fn builds_openai_chat_safety_response() {
        let body = json!({ "model": "gemini-2.5-pro" });
        let (status, headers, bytes) =
            build_shield_local_response(OPENAI_CHAT_SYNC_PLAN_KIND, &body, false)
                .expect("openai chat sync should build");
        assert_eq!(status, 200);
        assert_eq!(
            headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
        let parsed: Value = serde_json::from_slice(&bytes).expect("valid json");
        assert_eq!(parsed["choices"][0]["finish_reason"], "content_filter");
        assert_eq!(parsed["model"], "gemini-2.5-pro");
        assert!(parsed.get("aether_shield").is_none());
    }

    #[test]
    fn builds_gemini_safety_response_as_prompt_level_block() {
        let body = json!({ "model": "gemini-2.5-pro" });
        let (status, _headers, bytes) =
            build_shield_local_response(GEMINI_CHAT_SYNC_PLAN_KIND, &body, false)
                .expect("gemini chat sync should build");
        assert_eq!(status, 200);
        let parsed: Value = serde_json::from_slice(&bytes).expect("valid json");
        // Real Gemini prompt-level blocks carry promptFeedback with no
        // candidates; the mimicked response must match that shape exactly.
        assert_eq!(parsed["promptFeedback"]["blockReason"], "SAFETY");
        assert!(parsed.get("candidates").is_none());
        assert!(parsed.get("aetherShield").is_none());
        let ratings = parsed["promptFeedback"]["safetyRatings"]
            .as_array()
            .expect("safety ratings present");
        assert!(ratings.iter().any(|rating| rating["blocked"] == json!(true)));
    }

    #[test]
    fn builds_gemini_safety_stream_without_done_sentinel() {
        let body = json!({ "model": "gemini-2.5-pro" });
        let (_status, headers, bytes) =
            build_shield_local_response(GEMINI_CHAT_STREAM_PLAN_KIND, &body, true)
                .expect("gemini chat stream should build");
        assert_eq!(
            headers.get("content-type").map(String::as_str),
            Some("text/event-stream")
        );
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.starts_with("data: "));
        // Gemini SSE streams end without a [DONE] sentinel.
        assert!(!text.contains("[DONE]"));
        assert!(text.contains("\"blockReason\":\"SAFETY\""));
    }

    #[test]
    fn builds_openai_chat_safety_stream() {
        let body = json!({ "model": "gemini-2.5-pro" });
        let (status, headers, bytes) =
            build_shield_local_response(OPENAI_CHAT_STREAM_PLAN_KIND, &body, true)
                .expect("openai chat stream should build");
        assert_eq!(status, 200);
        assert_eq!(
            headers.get("content-type").map(String::as_str),
            Some("text/event-stream")
        );
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.contains("content_filter"));
        assert!(text.contains("data: [DONE]"));
    }

    #[test]
    fn builds_claude_safety_stream_with_valid_event_sequence() {
        let body = json!({ "model": "claude-sonnet-4-5" });
        let (status, headers, bytes) =
            build_shield_local_response(CLAUDE_CHAT_STREAM_PLAN_KIND, &body, true)
                .expect("claude chat stream should build");
        assert_eq!(status, 200);
        assert_eq!(
            headers.get("content-type").map(String::as_str),
            Some("text/event-stream")
        );
        let text = String::from_utf8(bytes).expect("utf8");
        // The Anthropic SSE protocol requires message_start -> message_delta
        // -> message_stop in that order.
        let start = text
            .find("event: message_start")
            .expect("message_start event");
        let delta = text
            .find("event: message_delta")
            .expect("message_delta event");
        let stop = text
            .find("event: message_stop")
            .expect("message_stop event");
        assert!(
            start < delta && delta < stop,
            "event order must be start->delta->stop"
        );
        // message_stop carries only its marker object.
        let stop_frame = &text[stop..];
        assert!(stop_frame.contains(r#"{"type":"message_stop"}"#));
        // message_delta carries the refusal stop_reason.
        let delta_frame = &text[delta..stop];
        assert!(delta_frame.contains(r#""stop_reason":"refusal""#));
        // message_start carries the message envelope.
        let start_frame = &text[start..delta];
        assert!(start_frame.contains(r#""type":"message_start""#));
        assert!(start_frame.contains(r#""role":"assistant""#));
    }

    #[test]
    fn unsupported_plan_kind_returns_none() {
        let body = json!({ "model": "m" });
        assert!(build_shield_local_response(OPENAI_IMAGE_SYNC_PLAN_KIND, &body, false).is_none());
    }

    fn test_parts() -> http::request::Parts {
        let request = http::Request::builder()
            .method(http::Method::POST)
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", "Bearer secret")
            .body(())
            .expect("request should build");
        let (parts, _) = request.into_parts();
        parts
    }
}
