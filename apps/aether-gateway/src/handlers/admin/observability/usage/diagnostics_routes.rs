use super::diagnostics_summarizer::{
    build_diagnostics_summary_prompt, cached_diagnostics_summary, call_diagnostics_summarizer,
    diagnostics_summarizer_client, diagnostics_summary_now_unix_secs, merge_diagnostics_summary,
    parse_diagnostics_summarizer_config, ERROR_DIAGNOSTIC_SUMMARIZER_CONFIG_KEY,
};
use super::replay::{
    admin_usage_resolve_body_value, admin_usage_resolve_request_capture_body_for_item,
};
use crate::handlers::admin::request::{AdminAppState, AdminRequestContext};
use crate::handlers::admin::shared::{
    attach_admin_audit_response, query_param_bool, query_param_value,
};
use crate::GatewayError;
use aether_admin::observability::usage::{
    admin_diagnostics_id_from_action_path, admin_diagnostics_id_from_detail_path,
    admin_diagnostics_record_json, admin_usage_bad_request_response,
    admin_usage_data_unavailable_response, admin_usage_parse_limit, admin_usage_parse_offset,
    build_admin_diagnostics_detail_payload, ADMIN_USAGE_DATA_UNAVAILABLE_DETAIL,
};
use aether_data_contracts::repository::usage::{UsageAuditListQuery, UsageBodyField};
use axum::{
    body::Body,
    http,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

fn parse_diagnostics_unix_secs(query: Option<&str>, key: &str) -> Result<Option<u64>, String> {
    match query_param_value(query, key) {
        None => Ok(None),
        Some(value) => value
            .parse::<u64>()
            .map(Some)
            .map_err(|_| format!("{key} must be a non-negative integer")),
    }
}

pub(super) async fn maybe_build_local_admin_diagnostics_response(
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
                    "/api/admin/diagnostics" | "/api/admin/diagnostics/"
                ) =>
        {
            if !state.has_usage_data_reader() {
                return Ok(Some(admin_usage_data_unavailable_response(
                    ADMIN_USAGE_DATA_UNAVAILABLE_DETAIL,
                )));
            }

            let query = request_context.request_query_string.as_deref();
            let from = match parse_diagnostics_unix_secs(query, "from") {
                Ok(value) => value,
                Err(detail) => return Ok(Some(admin_usage_bad_request_response(detail))),
            };
            let to = match parse_diagnostics_unix_secs(query, "to") {
                Ok(value) => value,
                Err(detail) => return Ok(Some(admin_usage_bad_request_response(detail))),
            };
            if let (Some(from), Some(to)) = (from, to) {
                if from > to {
                    return Ok(Some(admin_usage_bad_request_response("from must be <= to")));
                }
            }
            let limit = match admin_usage_parse_limit(query) {
                Ok(value) => value,
                Err(detail) => return Ok(Some(admin_usage_bad_request_response(detail))),
            };
            let offset = match admin_usage_parse_offset(query) {
                Ok(value) => value,
                Err(detail) => return Ok(Some(admin_usage_bad_request_response(detail))),
            };
            let newest_first = query_param_bool(query, "newest_first", true);

            let list_query = UsageAuditListQuery {
                created_from_unix_secs: from,
                created_until_unix_secs: to,
                user_id: query_param_value(query, "user_id"),
                api_key_id: query_param_value(query, "api_key_id"),
                diagnostic_kind: query_param_value(query, "kind"),
                limit: Some(limit),
                offset: Some(offset),
                newest_first,
                ..UsageAuditListQuery::default()
            };
            let items = state.list_usage_audits(&list_query).await?;
            let mut count_query = list_query.clone();
            count_query.limit = None;
            count_query.offset = None;
            let total = usize::try_from(state.count_usage_audits(&count_query).await?)
                .unwrap_or(usize::MAX);
            let records: Vec<Value> = items.iter().map(admin_diagnostics_record_json).collect();

            return Ok(Some(
                Json(json!({
                    "records": records,
                    "total": total,
                    "limit": limit,
                    "offset": offset,
                }))
                .into_response(),
            ));
        }
        Some("detail")
            if request_context.request_method == http::Method::GET
                && request_context
                    .request_path
                    .starts_with("/api/admin/diagnostics/") =>
        {
            if !state.has_usage_data_reader() {
                return Ok(Some(admin_usage_data_unavailable_response(
                    ADMIN_USAGE_DATA_UNAVAILABLE_DETAIL,
                )));
            }

            let Some(request_id) =
                admin_diagnostics_id_from_detail_path(&request_context.request_path)
            else {
                return Ok(Some(admin_usage_bad_request_response("request_id 无效")));
            };
            let include_bodies = query_param_bool(
                request_context.request_query_string.as_deref(),
                "include_bodies",
                true,
            );

            let Some(item) = state.find_request_usage_by_request_id(&request_id).await? else {
                return Ok(Some(
                    (
                        http::StatusCode::NOT_FOUND,
                        Json(json!({ "detail": "Diagnostic event not found" })),
                    )
                        .into_response(),
                ));
            };

            let (request_body, provider_request_body, response_body, client_response_body) =
                if include_bodies {
                    tokio::try_join!(
                        admin_usage_resolve_request_capture_body_for_item(state, &item, None),
                        admin_usage_resolve_body_value(
                            state,
                            &item,
                            item.provider_request_body.as_ref(),
                            UsageBodyField::ProviderRequestBody,
                        ),
                        admin_usage_resolve_body_value(
                            state,
                            &item,
                            item.response_body.as_ref(),
                            UsageBodyField::ResponseBody,
                        ),
                        admin_usage_resolve_body_value(
                            state,
                            &item,
                            item.client_response_body.as_ref(),
                            UsageBodyField::ClientResponseBody,
                        ),
                    )?
                } else {
                    (None, None, None, None)
                };

            let payload = build_admin_diagnostics_detail_payload(
                &item,
                request_body,
                provider_request_body,
                response_body,
                client_response_body,
                include_bodies,
            );
            return Ok(Some(attach_admin_audit_response(
                Json(payload).into_response(),
                "admin_diagnostics_detail_viewed",
                "view_diagnostics_detail",
                "usage_record",
                &item.id,
            )));
        }
        Some("summarize")
            if request_context.request_method == http::Method::POST
                && request_context
                    .request_path
                    .starts_with("/api/admin/diagnostics/")
                && request_context.request_path.ends_with("/summarize") =>
        {
            let Some(request_id) =
                admin_diagnostics_id_from_action_path(&request_context.request_path, "/summarize")
            else {
                return Ok(Some(admin_usage_bad_request_response("request_id 无效")));
            };

            // The AI summarizer is a detachable module: when the system config
            // entry is absent or incomplete the feature is considered
            // uninstalled and the endpoint reports 503 instead of calling an
            // LLM.
            let config_value = state
                .read_system_config_json_value(ERROR_DIAGNOSTIC_SUMMARIZER_CONFIG_KEY)
                .await?;
            let Some(config) = parse_diagnostics_summarizer_config(config_value.as_ref()) else {
                return Ok(Some(
                    (
                        http::StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({
                            "detail": "AI 摘要器未配置：请先设置系统配置 error_diagnostic_summarizer（base_url / api_key / model）",
                            "request_id": request_id,
                        })),
                    )
                        .into_response(),
                ));
            };

            if !state.has_usage_data_reader() {
                return Ok(Some(admin_usage_data_unavailable_response(
                    ADMIN_USAGE_DATA_UNAVAILABLE_DETAIL,
                )));
            }

            let Some(item) = state.find_request_usage_by_request_id(&request_id).await? else {
                return Ok(Some(
                    (
                        http::StatusCode::NOT_FOUND,
                        Json(json!({ "detail": "Diagnostic event not found" })),
                    )
                        .into_response(),
                ));
            };

            let now = diagnostics_summary_now_unix_secs();
            let refresh = query_param_bool(
                request_context.request_query_string.as_deref(),
                "refresh",
                false,
            );

            if !refresh {
                if let Some((summary, summarized_at)) =
                    cached_diagnostics_summary(item.request_metadata.as_ref(), now)
                {
                    return Ok(Some(attach_admin_audit_response(
                        Json(json!({
                            "request_id": request_id,
                            "summary": summary,
                            "cached": true,
                            "persisted": true,
                            "summarized_at_unix_secs": summarized_at,
                        }))
                        .into_response(),
                        "admin_diagnostics_summary_generated",
                        "summarize_diagnostics",
                        "usage_record",
                        &item.id,
                    )));
                }
            }

            let response_body = admin_usage_resolve_body_value(
                state,
                &item,
                item.response_body.as_ref(),
                UsageBodyField::ResponseBody,
            )
            .await?;
            let prompt = build_diagnostics_summary_prompt(&item, response_body.as_ref());
            let client = diagnostics_summarizer_client(&config)?;
            let summary = match call_diagnostics_summarizer(&client, &config, &prompt).await {
                Ok(summary) => summary,
                Err(err) => {
                    return Ok(Some(
                        (
                            http::StatusCode::BAD_GATEWAY,
                            Json(json!({
                                "detail": format!("AI 摘要生成失败：{}", err.into_message()),
                                "request_id": request_id,
                            })),
                        )
                            .into_response(),
                    ));
                }
            };

            let merged_metadata =
                merge_diagnostics_summary(item.request_metadata.clone(), &summary, now);
            let persisted = state
                .update_usage_request_metadata(&request_id, merged_metadata)
                .await?;

            return Ok(Some(attach_admin_audit_response(
                Json(json!({
                    "request_id": request_id,
                    "summary": summary,
                    "cached": false,
                    "persisted": persisted,
                    "summarized_at_unix_secs": now,
                }))
                .into_response(),
                "admin_diagnostics_summary_generated",
                "summarize_diagnostics",
                "usage_record",
                &item.id,
            )));
        }
        _ => {}
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::parse_diagnostics_unix_secs;

    #[test]
    fn parses_unix_seconds_bounds() {
        assert_eq!(
            parse_diagnostics_unix_secs(Some("from=100"), "from"),
            Ok(Some(100))
        );
        assert_eq!(parse_diagnostics_unix_secs(None, "from"), Ok(None));
        assert_eq!(
            parse_diagnostics_unix_secs(Some("other=1"), "from"),
            Ok(None)
        );
        assert!(parse_diagnostics_unix_secs(Some("from=abc"), "from").is_err());
        assert!(parse_diagnostics_unix_secs(Some("from=-1"), "from").is_err());
    }
}
