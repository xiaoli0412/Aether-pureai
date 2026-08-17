use http::Uri;

use super::{classify_control_route, headers};

#[test]
fn classifies_admin_diagnostics_list_as_admin_proxy_route() {
    let headers = headers(&[]);
    let uri: Uri = "/api/admin/diagnostics".parse().expect("uri should parse");
    let decision =
        classify_control_route(&http::Method::GET, &uri, &headers).expect("route should classify");

    assert_eq!(decision.route_class.as_deref(), Some("admin_proxy"));
    assert_eq!(decision.route_family.as_deref(), Some("diagnostics_manage"));
    assert_eq!(decision.route_kind.as_deref(), Some("list"));
    assert_eq!(
        decision.auth_endpoint_signature.as_deref(),
        Some("admin:usage")
    );
    assert!(!decision.is_execution_runtime_candidate());
}

#[test]
fn classifies_admin_diagnostics_list_with_trailing_slash_as_admin_proxy_route() {
    let headers = headers(&[]);
    let uri: Uri = "/api/admin/diagnostics/".parse().expect("uri should parse");
    let decision =
        classify_control_route(&http::Method::GET, &uri, &headers).expect("route should classify");

    assert_eq!(decision.route_class.as_deref(), Some("admin_proxy"));
    assert_eq!(decision.route_family.as_deref(), Some("diagnostics_manage"));
    assert_eq!(decision.route_kind.as_deref(), Some("list"));
}

#[test]
fn classifies_admin_diagnostics_detail_as_admin_proxy_route() {
    let headers = headers(&[]);
    let uri: Uri = "/api/admin/diagnostics/req-123"
        .parse()
        .expect("uri should parse");
    let decision =
        classify_control_route(&http::Method::GET, &uri, &headers).expect("route should classify");

    assert_eq!(decision.route_class.as_deref(), Some("admin_proxy"));
    assert_eq!(decision.route_family.as_deref(), Some("diagnostics_manage"));
    assert_eq!(decision.route_kind.as_deref(), Some("detail"));
    assert_eq!(
        decision.auth_endpoint_signature.as_deref(),
        Some("admin:usage")
    );
    assert!(!decision.is_execution_runtime_candidate());
}

#[test]
fn classifies_admin_diagnostics_detail_with_trailing_slash_as_admin_proxy_route() {
    let headers = headers(&[]);
    let uri: Uri = "/api/admin/diagnostics/req-123/"
        .parse()
        .expect("uri should parse");
    let decision =
        classify_control_route(&http::Method::GET, &uri, &headers).expect("route should classify");

    assert_eq!(decision.route_family.as_deref(), Some("diagnostics_manage"));
    assert_eq!(decision.route_kind.as_deref(), Some("detail"));
}

#[test]
fn classifies_admin_diagnostics_summarize_as_admin_proxy_route() {
    let headers = headers(&[]);
    let uri: Uri = "/api/admin/diagnostics/req-123/summarize"
        .parse()
        .expect("uri should parse");
    let decision =
        classify_control_route(&http::Method::POST, &uri, &headers).expect("route should classify");

    assert_eq!(decision.route_class.as_deref(), Some("admin_proxy"));
    assert_eq!(decision.route_family.as_deref(), Some("diagnostics_manage"));
    assert_eq!(decision.route_kind.as_deref(), Some("summarize"));
    assert_eq!(
        decision.auth_endpoint_signature.as_deref(),
        Some("admin:usage")
    );
    assert!(!decision.is_execution_runtime_candidate());
}

#[test]
fn admin_diagnostics_list_does_not_classify_post() {
    let headers = headers(&[]);
    let uri: Uri = "/api/admin/diagnostics".parse().expect("uri should parse");
    let decision = classify_control_route(&http::Method::POST, &uri, &headers);

    assert!(
        decision
            .as_ref()
            .map(|value| value.route_family.as_deref() != Some("diagnostics_manage"))
            .unwrap_or(true),
        "POST on the diagnostics list path must not classify as diagnostics_manage"
    );
}

#[test]
fn admin_diagnostics_summarize_does_not_classify_get() {
    let headers = headers(&[]);
    let uri: Uri = "/api/admin/diagnostics/req-123/summarize"
        .parse()
        .expect("uri should parse");
    let decision = classify_control_route(&http::Method::GET, &uri, &headers);

    assert!(
        decision
            .as_ref()
            .map(|value| value.route_family.as_deref() != Some("diagnostics_manage"))
            .unwrap_or(true),
        "GET on the diagnostics summarize path must not classify as diagnostics_manage"
    );
}
