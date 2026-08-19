//! Request identity derivation shared by the planner and the execution
//! runtime: a stable credential-free fingerprint of a client request used by
//! the empty-response shield (F4) when the request carries no session id.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// Headers excluded from the request fingerprint: credentials and per-request
/// transport identifiers would make identical retried requests hash apart.
const FINGERPRINT_HEADER_DENYLIST: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "api-key",
    "x-goog-api-key",
    "x-request-id",
    "request-id",
    "x-trace-id",
    "x-aether-trace-id",
    "content-length",
    "host",
    "connection",
    "accept-encoding",
    "transfer-encoding",
    "date",
    "user-agent",
];

/// Stable fingerprint of a request from its headers and body: SHA-256 over a
/// credential-free sorted header view and the canonical request body. The
/// method/path are intentionally excluded so the fingerprint computed at the
/// pre-dispatch shield gate and the one injected into the report context (at
/// planning time) always agree.
pub(crate) fn request_fingerprint_from_headers_body(
    headers: &http::HeaderMap,
    body_json: &Value,
) -> String {
    let mut hasher = Sha256::new();

    let mut header_pairs: Vec<(String, String)> = Vec::new();
    for (name, value) in headers.iter() {
        let lowered = name.as_str().to_ascii_lowercase();
        if FINGERPRINT_HEADER_DENYLIST.contains(&lowered.as_str()) {
            continue;
        }
        if let Ok(value) = value.to_str() {
            header_pairs.push((lowered, value.trim().to_string()));
        }
    }
    header_pairs.sort();
    for (name, value) in &header_pairs {
        hasher.update(name.as_bytes());
        hasher.update(b":");
        hasher.update(value.as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(b"\n");

    let normalized = normalize_value_for_fingerprint(body_json);
    if let Ok(canonical) = serde_json::to_vec(&normalized) {
        hasher.update(&canonical);
    }
    format!("{:x}", hasher.finalize())
}

fn normalize_value_for_fingerprint(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries: Vec<(&String, &Value)> = object.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let mut sorted = Map::new();
            for (key, value) in entries {
                sorted.insert(key.clone(), normalize_value_for_fingerprint(value));
            }
            Value::Object(sorted)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(normalize_value_for_fingerprint).collect())
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::request_fingerprint_from_headers_body;
    use serde_json::json;

    fn test_headers() -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        headers
    }

    #[test]
    fn fingerprint_is_stable_across_key_order() {
        let headers = test_headers();
        let a = json!({ "model": "m", "messages": [] });
        let b = json!({ "messages": [], "model": "m" });
        assert_eq!(
            request_fingerprint_from_headers_body(&headers, &a),
            request_fingerprint_from_headers_body(&headers, &b)
        );
    }

    #[test]
    fn fingerprint_ignores_credentials_but_tracks_body_changes() {
        let mut headers = test_headers();
        let body = json!({ "model": "m" });
        let base = request_fingerprint_from_headers_body(&headers, &body);
        // Rotating the bearer token does not change the fingerprint.
        headers.insert("authorization", "Bearer other-secret".parse().unwrap());
        assert_eq!(base, request_fingerprint_from_headers_body(&headers, &body));
        // A different body does.
        let other_body = json!({ "model": "other" });
        assert_ne!(
            base,
            request_fingerprint_from_headers_body(&headers, &other_body)
        );
    }
}
