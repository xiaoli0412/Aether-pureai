//! 请求追踪传播
//!
//! 统一传播 X-Oneapi-Request-Id 和 W3C traceparent，
//! 确保 New API 和 Aether 的日志可按请求完整关联。

use axum::http::{HeaderMap, HeaderValue};
use serde::Serialize;
use uuid::Uuid;

/// 追踪上下文 Header 名称
pub const HEADER_ONEAPI_REQUEST_ID: &str = "x-oneapi-request-id";
pub const HEADER_TRACEPARENT: &str = "traceparent";
pub const HEADER_AETHER_UPSTREAM_REQUEST_ID: &str = "x-aether-upstream-request-id";
pub const HEADER_AETHER_DECISION_ID: &str = "x-aether-decision-id";

/// 请求追踪上下文
#[derive(Debug, Clone, Serialize)]
pub struct RequestTraceContext {
    /// 来自 New API 的请求 ID
    pub request_id: String,
    /// Aether 内部决策 ID
    pub decision_id: String,
    /// 上游实际使用的请求 ID
    pub upstream_request_id: Option<String>,
    /// 选中的通道
    pub selected_channel: Option<String>,
    /// 综合评分
    pub composite_score: Option<f64>,
    /// 路由决策耗时 (微秒)
    pub decision_latency_us: Option<u64>,
}

impl RequestTraceContext {
    /// 从入站 headers 创建追踪上下文
    pub fn from_inbound_headers(headers: &HeaderMap) -> Self {
        let request_id = headers
            .get(HEADER_ONEAPI_REQUEST_ID)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        Self {
            request_id,
            decision_id: Uuid::new_v4().to_string(),
            upstream_request_id: None,
            selected_channel: None,
            composite_score: None,
            decision_latency_us: None,
        }
    }

    /// 将追踪信息附加到上游请求 headers
    pub fn apply_to_upstream_headers(&self, headers: &mut HeaderMap) {
        if let Ok(val) = HeaderValue::from_str(&self.request_id) {
            headers.insert(HEADER_ONEAPI_REQUEST_ID, val);
        }
        // Generate W3C traceparent if not already present
        if !headers.contains_key(HEADER_TRACEPARENT) {
            let traceparent = generate_traceparent(&self.request_id);
            if let Ok(val) = HeaderValue::from_str(&traceparent) {
                headers.insert(HEADER_TRACEPARENT, val);
            }
        }
    }

    /// 将追踪信息附加到下游响应 headers
    pub fn apply_to_response_headers(&self, headers: &mut HeaderMap) {
        if let Some(ref upstream_id) = self.upstream_request_id {
            if let Ok(val) = HeaderValue::from_str(upstream_id) {
                headers.insert(HEADER_AETHER_UPSTREAM_REQUEST_ID, val);
            }
        }
        if let Ok(val) = HeaderValue::from_str(&self.decision_id) {
            headers.insert(HEADER_AETHER_DECISION_ID, val);
        }
    }

    /// 生成结构化日志字段
    pub fn log_fields(&self) -> Vec<(&'static str, String)> {
        let mut fields = vec![
            ("request_id", self.request_id.clone()),
            ("decision_id", self.decision_id.clone()),
        ];
        if let Some(ref ch) = self.selected_channel {
            fields.push(("selected_channel", ch.clone()));
        }
        if let Some(score) = self.composite_score {
            fields.push(("composite_score", format!("{:.4}", score)));
        }
        if let Some(lat) = self.decision_latency_us {
            fields.push(("decision_latency_us", lat.to_string()));
        }
        if let Some(ref uid) = self.upstream_request_id {
            fields.push(("upstream_request_id", uid.clone()));
        }
        fields
    }
}

/// 生成 W3C traceparent header
/// Format: {version}-{trace-id}-{parent-id}-{trace-flags}
fn generate_traceparent(request_id: &str) -> String {
    // Use request_id bytes to derive trace-id (pad/truncate to 32 hex chars)
    let trace_id: String = request_id
        .replace('-', "")
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .chain(std::iter::repeat('0'))
        .take(32)
        .collect();
    let parent_id = &Uuid::new_v4().to_string().replace('-', "")[..16];
    format!("00-{}-{}-01", trace_id, parent_id)
}
