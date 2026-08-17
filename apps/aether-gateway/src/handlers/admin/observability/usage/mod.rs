use crate::handlers::admin::request::{AdminAppState, AdminRequestContext};
use crate::GatewayError;
use axum::{body::Body, response::Response};

mod analytics;
mod analytics_routes;
mod detail_routes;
mod diagnostics_routes;
mod replay;
mod summary_routes;

pub(crate) async fn maybe_build_local_admin_usage_response(
    state: &AdminAppState<'_>,
    request_context: &AdminRequestContext<'_>,
    request_body: Option<&axum::body::Bytes>,
) -> Result<Option<Response<Body>>, GatewayError> {
    let Some(decision) = request_context.decision() else {
        return Ok(None);
    };

    let route_family = decision.route_family.as_deref();
    if !matches!(
        route_family,
        Some("usage_manage") | Some("diagnostics_manage")
    ) {
        return Ok(None);
    }

    if route_family == Some("diagnostics_manage") {
        return diagnostics_routes::maybe_build_local_admin_diagnostics_response(
            state,
            request_context,
        )
        .await;
    }

    if let Some(response) = detail_routes::maybe_build_local_admin_usage_detail_response(
        state,
        request_context,
        request_body,
    )
    .await?
    {
        return Ok(Some(response));
    }

    if let Some(response) =
        summary_routes::maybe_build_local_admin_usage_summary_response(state, request_context)
            .await?
    {
        return Ok(Some(response));
    }

    if let Some(response) =
        analytics_routes::maybe_build_local_admin_usage_analytics_response(state, request_context)
            .await?
    {
        return Ok(Some(response));
    }

    Ok(None)
}
