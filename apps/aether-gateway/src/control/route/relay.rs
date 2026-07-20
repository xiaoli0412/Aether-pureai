use axum::http;

use super::{classified, ClassifiedRoute};

pub(super) fn classify_relay_route(
    method: &http::Method,
    normalized_path: &str,
) -> Option<ClassifiedRoute> {
    let path = normalized_path.trim_end_matches('/');
    let path = if path.is_empty() { "/" } else { path };

    let is_read = matches!(method, &http::Method::GET | &http::Method::HEAD);

    if is_read && path == "/api/relay/channels" {
        Some(relay_management_route("list_channels"))
    } else if method == http::Method::POST && path == "/api/relay/channels" {
        Some(relay_management_route("create_channel"))
    } else if is_read && is_relay_item_path(path, "/api/relay/channels/") {
        Some(relay_management_route("get_channel"))
    } else if method == http::Method::PUT && is_relay_item_path(path, "/api/relay/channels/") {
        Some(relay_management_route("update_channel"))
    } else if method == http::Method::DELETE && is_relay_item_path(path, "/api/relay/channels/") {
        Some(relay_management_route("delete_channel"))
    } else if is_read && path == "/api/relay/pricing" {
        Some(relay_management_route("list_pricing_rules"))
    } else if method == http::Method::POST && path == "/api/relay/pricing" {
        Some(relay_management_route("create_pricing_rule"))
    } else if method == http::Method::PUT && is_relay_item_path(path, "/api/relay/pricing/") {
        Some(relay_management_route("update_pricing_rule"))
    } else if method == http::Method::DELETE && is_relay_item_path(path, "/api/relay/pricing/") {
        Some(relay_management_route("delete_pricing_rule"))
    } else if is_read && path == "/api/relay/downstream" {
        Some(relay_management_route("list_downstream"))
    } else if method == http::Method::POST && path == "/api/relay/downstream" {
        Some(relay_management_route("create_downstream"))
    } else if method == http::Method::DELETE && is_relay_item_path(path, "/api/relay/downstream/") {
        Some(relay_management_route("delete_downstream"))
    } else if is_read && path == "/api/relay/groups" {
        Some(relay_group_route("list_downstream_groups"))
    } else if method == http::Method::POST && path == "/api/relay/groups" {
        Some(relay_group_route("create_downstream_group"))
    } else if method == http::Method::PUT && is_relay_group_item_path(path) {
        Some(relay_group_route("update_downstream_group"))
    } else if method == http::Method::DELETE && is_relay_group_item_path(path) {
        Some(relay_group_route("delete_downstream_group"))
    } else if is_read && path == "/api/relay/dashboard" {
        Some(relay_management_route("get_dashboard"))
    } else if method == http::Method::POST && path == "/api/relay/sync/pricing" {
        Some(relay_management_route("trigger_price_sync"))
    } else if method == http::Method::POST && path == "/api/relay/sync/health" {
        Some(relay_management_route("trigger_health_reset"))
    } else if is_read && path == "/api/relay/settlements" {
        Some(relay_management_route("list_settlements"))
    } else if is_read && path == "/api/relay/integration-status" {
        Some(relay_management_route("integration_status"))
    } else {
        None
    }
}

fn is_relay_group_item_path(path: &str) -> bool {
    is_relay_item_path(path, "/api/relay/groups/")
}

fn is_relay_item_path(path: &str, prefix: &str) -> bool {
    path.strip_prefix(prefix)
        .is_some_and(|id| !id.is_empty() && !id.contains('/'))
}

fn relay_management_route(route_kind: &'static str) -> ClassifiedRoute {
    classified(
        "admin_proxy",
        "relay_manage",
        route_kind,
        "admin:relay",
        false,
    )
}

fn relay_group_route(route_kind: &'static str) -> ClassifiedRoute {
    classified(
        "admin_proxy",
        "relay_groups_manage",
        route_kind,
        "admin:routing_profiles",
        false,
    )
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, Method, Uri};

    use super::super::classify_control_route;

    #[test]
    fn classifies_every_mounted_relay_management_route_with_relay_permissions() {
        let cases = [
            (
                Method::GET,
                "/api/relay/channels",
                "list_channels",
                "relay_manage",
                "admin:relay",
            ),
            (
                Method::HEAD,
                "/api/relay/channels",
                "list_channels",
                "relay_manage",
                "admin:relay",
            ),
            (
                Method::POST,
                "/api/relay/channels",
                "create_channel",
                "relay_manage",
                "admin:relay",
            ),
            (
                Method::GET,
                "/api/relay/channels/channel-1",
                "get_channel",
                "relay_manage",
                "admin:relay",
            ),
            (
                Method::PUT,
                "/api/relay/channels/channel-1",
                "update_channel",
                "relay_manage",
                "admin:relay",
            ),
            (
                Method::DELETE,
                "/api/relay/channels/channel-1",
                "delete_channel",
                "relay_manage",
                "admin:relay",
            ),
            (
                Method::GET,
                "/api/relay/pricing",
                "list_pricing_rules",
                "relay_manage",
                "admin:relay",
            ),
            (
                Method::POST,
                "/api/relay/pricing",
                "create_pricing_rule",
                "relay_manage",
                "admin:relay",
            ),
            (
                Method::PUT,
                "/api/relay/pricing/rule-1",
                "update_pricing_rule",
                "relay_manage",
                "admin:relay",
            ),
            (
                Method::DELETE,
                "/api/relay/pricing/rule-1",
                "delete_pricing_rule",
                "relay_manage",
                "admin:relay",
            ),
            (
                Method::GET,
                "/api/relay/downstream",
                "list_downstream",
                "relay_manage",
                "admin:relay",
            ),
            (
                Method::POST,
                "/api/relay/downstream",
                "create_downstream",
                "relay_manage",
                "admin:relay",
            ),
            (
                Method::DELETE,
                "/api/relay/downstream/downstream-1",
                "delete_downstream",
                "relay_manage",
                "admin:relay",
            ),
            (
                Method::GET,
                "/api/relay/groups",
                "list_downstream_groups",
                "relay_groups_manage",
                "admin:routing_profiles",
            ),
            (
                Method::POST,
                "/api/relay/groups",
                "create_downstream_group",
                "relay_groups_manage",
                "admin:routing_profiles",
            ),
            (
                Method::PUT,
                "/api/relay/groups/group-1",
                "update_downstream_group",
                "relay_groups_manage",
                "admin:routing_profiles",
            ),
            (
                Method::DELETE,
                "/api/relay/groups/group-1",
                "delete_downstream_group",
                "relay_groups_manage",
                "admin:routing_profiles",
            ),
            (
                Method::GET,
                "/api/relay/dashboard",
                "get_dashboard",
                "relay_manage",
                "admin:relay",
            ),
            (
                Method::POST,
                "/api/relay/sync/pricing",
                "trigger_price_sync",
                "relay_manage",
                "admin:relay",
            ),
            (
                Method::POST,
                "/api/relay/sync/health",
                "trigger_health_reset",
                "relay_manage",
                "admin:relay",
            ),
            (
                Method::GET,
                "/api/relay/settlements",
                "list_settlements",
                "relay_manage",
                "admin:relay",
            ),
            (
                Method::GET,
                "/api/relay/integration-status",
                "integration_status",
                "relay_manage",
                "admin:relay",
            ),
        ];

        for (method, path, route_kind, route_family, auth_signature) in cases {
            let uri: Uri = path.parse().expect("relay route URI should parse");
            let decision = classify_control_route(&method, &uri, &HeaderMap::new())
                .expect("mounted relay route should be classified");
            assert_eq!(decision.route_class.as_deref(), Some("admin_proxy"));
            assert_eq!(decision.route_family.as_deref(), Some(route_family));
            assert_eq!(decision.route_kind.as_deref(), Some(route_kind));
            assert_eq!(
                decision.auth_endpoint_signature.as_deref(),
                Some(auth_signature)
            );
        }
    }

    #[test]
    fn does_not_treat_unmounted_or_nested_relay_paths_as_management_routes() {
        for path in [
            "/api/relay/channels/channel-1/nested",
            "/api/relay/groups/group-1/nested",
            "/api/relay/integration-status/nested",
            "/api/relay/not-mounted",
        ] {
            let uri: Uri = path.parse().expect("relay route URI should parse");
            assert!(classify_control_route(&Method::GET, &uri, &HeaderMap::new()).is_none());
        }
    }
}
