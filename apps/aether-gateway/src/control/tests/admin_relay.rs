use http::Uri;

use super::{classify_control_route, headers};

#[test]
fn classifies_relay_group_routes_as_admin_routing_profile_routes() {
    let headers = headers(&[]);

    for (method, path, route_kind) in [
        (
            http::Method::GET,
            "/api/relay/groups",
            "list_downstream_groups",
        ),
        (
            http::Method::POST,
            "/api/relay/groups",
            "create_downstream_group",
        ),
        (
            http::Method::PUT,
            "/api/relay/groups/group-1",
            "update_downstream_group",
        ),
        (
            http::Method::DELETE,
            "/api/relay/groups/group-1",
            "delete_downstream_group",
        ),
    ] {
        let uri: Uri = path.parse().expect("relay group route URI should parse");
        let decision = classify_control_route(&method, &uri, &headers)
            .expect("relay group route should classify");

        assert_eq!(decision.route_class.as_deref(), Some("admin_proxy"));
        assert_eq!(
            decision.route_family.as_deref(),
            Some("relay_groups_manage")
        );
        assert_eq!(decision.route_kind.as_deref(), Some(route_kind));
        assert_eq!(
            decision.auth_endpoint_signature.as_deref(),
            Some("admin:routing_profiles")
        );
    }
}
