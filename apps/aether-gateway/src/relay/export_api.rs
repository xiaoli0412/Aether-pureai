//! 数据导出接口
//!
//! 向 New API 暴露只读定价、用量和事件数据。
//! 使用独立的 Outbound_Export_Token Bearer 认证。

use aether_data_contracts::repository::global_models::{
    PublicCatalogModelListQuery, StoredPublicCatalogModel,
};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;

use super::error::RelayError;
use super::groups::FlatExportedGroup;
use crate::state::AppState;

const OUTBOUND_EXPORT_TOKEN_ENV: &str = "AETHER_OUTBOUND_EXPORT_TOKEN";

/// 定价快照导出
#[derive(Debug, Clone, Serialize)]
pub struct PricingExport {
    /// 导出时间
    pub exported_at: String,
    /// ETag (用于缓存验证)
    pub etag: String,
    /// 模型定价列表
    pub models: Vec<ModelPricingExport>,
    /// 分组列表
    pub groups: Vec<FlatExportedGroup>,
}

/// 单模型定价导出
#[derive(Debug, Clone, Serialize)]
pub struct ModelPricingExport {
    pub model_id: String,
    /// 输入 token 单价 (quota per token)
    pub input_quota_per_token: f64,
    /// 输出 token 单价 (quota per token)
    pub output_quota_per_token: f64,
    /// 能力标签
    pub capabilities: Vec<String>,
    /// 最低上游成本通道
    pub cheapest_channel_id: Option<String>,
    /// 可用通道数
    pub available_channels: usize,
}

/// 用量汇总导出
#[derive(Debug, Clone, Serialize)]
pub struct UsageSummaryExport {
    pub period_start: String,
    pub period_end: String,
    pub total_requests: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    pub total_charged_quota: f64,
    pub total_upstream_cost_usd: Option<f64>,
    pub total_downstream_revenue_usd: Option<f64>,
    pub unknown_upstream_cost_requests: u64,
    pub unknown_downstream_revenue_requests: u64,
    /// 按模型汇总
    pub by_model: Vec<ModelUsageSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelUsageSummary {
    pub model_id: String,
    pub requests: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub charged_quota: f64,
}

/// 增量事件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportEvent {
    pub event_id: String,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub created_at: String,
}

/// 事件查询参数
#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    pub cursor: Option<String>,
    pub limit: Option<u64>,
}

/// 用量查询参数
#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    pub start_date: Option<String>,
    pub end_date: Option<String>,
}

/// 挂载导出 API 路由
pub(crate) fn mount_export_routes(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/api/export/pricing", get(export_pricing))
        .route("/api/export/usage-summary", get(export_usage_summary))
        .route("/api/export/events", get(export_events))
}

/// GET /api/export/pricing
async fn export_pricing(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, RelayError> {
    if !is_authorized_export_request(&headers) {
        return Ok(unauthorized_export_response());
    }

    let relay = state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;

    if !state.has_global_model_data_reader() {
        return Err(RelayError::Internal(
            "provider catalog database is unavailable".to_string(),
        ));
    }
    let catalog_models = state
        .list_public_catalog_models(&PublicCatalogModelListQuery {
            provider_id: None,
            offset: 0,
            limit: 10_000,
        })
        .await
        .map_err(|error| {
            RelayError::Internal(format!("load provider catalog pricing: {error:?}"))
        })?;
    let models = pricing_exports_from_catalog_models(&catalog_models)?;

    // Get groups
    let group_store = state
        .data
        .relay_downstream_group_store()
        .ok_or(RelayError::DatabaseBackedGroupExportUnavailable)?;
    let group_mgr = super::groups::DownstreamGroupManager::with_store(
        Arc::clone(&relay.config),
        state.runtime_state().clone(),
        group_store,
    );
    let groups = group_mgr.export_flat_groups().await?;

    pricing_export_response(&headers, models, groups)
}

fn pricing_export_response(
    headers: &HeaderMap,
    models: Vec<ModelPricingExport>,
    groups: Vec<FlatExportedGroup>,
) -> Result<Response, RelayError> {
    let current_etag = pricing_etag(&models, &groups)?;
    if if_none_match_matches(headers, &current_etag) {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        set_pricing_cache_headers(&mut response, &current_etag);
        return Ok(response);
    }

    let export = PricingExport {
        exported_at: Utc::now().to_rfc3339(),
        etag: current_etag.clone(),
        models,
        groups,
    };
    let mut response = Json(export).into_response();
    set_pricing_cache_headers(&mut response, &current_etag);
    Ok(response)
}

fn pricing_etag(
    models: &[ModelPricingExport],
    groups: &[FlatExportedGroup],
) -> Result<String, RelayError> {
    #[derive(Serialize)]
    struct PricingEtagContent<'a> {
        models: &'a [ModelPricingExport],
        groups: &'a [FlatExportedGroup],
    }

    let content = serde_json::to_vec(&PricingEtagContent { models, groups }).map_err(|error| {
        RelayError::Internal(format!("serialize pricing etag content: {error}"))
    })?;
    let digest = Sha256::digest(content);
    let digest = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("\"pricing-{digest}\""))
}

fn if_none_match_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get("if-none-match")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .map(str::trim)
                .any(|candidate| candidate == "*" || candidate == etag)
        })
}

fn set_pricing_cache_headers(response: &mut Response, etag: &str) {
    response.headers_mut().insert(
        "etag",
        etag.parse().expect("pricing etag should be a header value"),
    );
    response.headers_mut().insert(
        "cache-control",
        "max-age=300".parse().expect("cache-control is valid"),
    );
}

fn is_authorized_export_request(headers: &HeaderMap) -> bool {
    let Some(expected_token) = std::env::var(OUTBOUND_EXPORT_TOKEN_ENV)
        .ok()
        .filter(|token| !token.trim().is_empty())
    else {
        return false;
    };
    if !super::collaboration::outbound_export_token_is_isolated(&expected_token) {
        return false;
    }
    let Some(value) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(token.as_bytes(), expected_token.as_bytes())
}

fn unauthorized_export_response() -> Response {
    (StatusCode::UNAUTHORIZED, "invalid export token").into_response()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

const CATALOG_PRICE_PER_MILLION_TOKENS: f64 = 1_000_000.0;

#[derive(Default)]
struct CatalogProviderPricing {
    input_quota_per_token: f64,
    output_quota_per_token: f64,
    capabilities: std::collections::BTreeSet<String>,
}

fn pricing_exports_from_catalog_models(
    catalog_models: &[StoredPublicCatalogModel],
) -> Result<Vec<ModelPricingExport>, RelayError> {
    let mut providers_by_model =
        BTreeMap::<String, BTreeMap<String, CatalogProviderPricing>>::new();

    for catalog_model in catalog_models {
        if !catalog_model.is_active {
            continue;
        }
        let model_id = catalog_model.name.trim();
        if model_id.is_empty() {
            return Err(RelayError::Internal(
                "provider catalog model has an empty name".to_string(),
            ));
        }
        let provider_id = catalog_model.provider_id.trim();
        if provider_id.is_empty() {
            return Err(RelayError::Internal(format!(
                "provider catalog model {model_id} has an empty provider id"
            )));
        }

        let (Some(input_quota_per_token), Some(output_quota_per_token)) = (
            catalog_price_per_token(
                catalog_model.input_price_per_1m,
                "input_price_per_1m",
                model_id,
            )?,
            catalog_price_per_token(
                catalog_model.output_price_per_1m,
                "output_price_per_1m",
                model_id,
            )?,
        ) else {
            // An incomplete price is unknown, not a zero-priced route.
            continue;
        };

        let capabilities = catalog_model_capabilities(catalog_model);
        let provider_prices = providers_by_model.entry(model_id.to_string()).or_default();
        let provider_price = provider_prices
            .entry(provider_id.to_string())
            .or_insert_with(|| CatalogProviderPricing {
                input_quota_per_token,
                output_quota_per_token,
                capabilities: Default::default(),
            });
        provider_price.capabilities.extend(capabilities);

        let existing_total =
            provider_price.input_quota_per_token + provider_price.output_quota_per_token;
        let candidate_total = input_quota_per_token + output_quota_per_token;
        if candidate_total < existing_total {
            provider_price.input_quota_per_token = input_quota_per_token;
            provider_price.output_quota_per_token = output_quota_per_token;
        }
    }

    providers_by_model
        .into_iter()
        .map(|(model_id, providers)| {
            let mut capabilities = std::collections::BTreeSet::new();
            let mut cheapest: Option<(&str, &CatalogProviderPricing)> = None;

            for (provider_id, provider_price) in &providers {
                capabilities.extend(provider_price.capabilities.iter().cloned());
                let is_cheaper = cheapest.is_none_or(|(_, current)| {
                    provider_price.input_quota_per_token + provider_price.output_quota_per_token
                        < current.input_quota_per_token + current.output_quota_per_token
                });
                if is_cheaper {
                    cheapest = Some((provider_id, provider_price));
                }
            }

            let (cheapest_channel_id, price) = cheapest.ok_or_else(|| {
                RelayError::Internal(format!(
                    "provider catalog has no known active price for {model_id}"
                ))
            })?;
            Ok(ModelPricingExport {
                model_id,
                input_quota_per_token: price.input_quota_per_token,
                output_quota_per_token: price.output_quota_per_token,
                capabilities: capabilities.into_iter().collect(),
                cheapest_channel_id: Some(cheapest_channel_id.to_string()),
                available_channels: providers.len(),
            })
        })
        .collect()
}

fn catalog_price_per_token(
    price_per_1m: Option<f64>,
    field: &str,
    model_id: &str,
) -> Result<Option<f64>, RelayError> {
    let Some(price_per_1m) = price_per_1m else {
        return Ok(None);
    };
    if !price_per_1m.is_finite() || price_per_1m < 0.0 {
        return Err(RelayError::Internal(format!(
            "provider catalog {field} is invalid for {model_id}"
        )));
    }
    Ok(Some(price_per_1m / CATALOG_PRICE_PER_MILLION_TOKENS))
}

fn catalog_model_capabilities(model: &StoredPublicCatalogModel) -> Vec<String> {
    let mut capabilities = Vec::new();
    if model.supports_function_calling.unwrap_or(false) {
        capabilities.push("function_calling".to_string());
    }
    if model.supports_embedding.unwrap_or(false) {
        capabilities.push("embedding".to_string());
    }
    if model.supports_streaming.unwrap_or(false) {
        capabilities.push("streaming".to_string());
    }
    if model.supports_vision.unwrap_or(false) {
        capabilities.push("vision".to_string());
    }
    capabilities
}

/// GET /api/export/usage-summary
async fn export_usage_summary(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<UsageQuery>,
) -> Result<Response, RelayError> {
    if !is_authorized_export_request(&headers) {
        return Ok(unauthorized_export_response());
    }

    let relay = state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;

    let now = Utc::now();
    let start = query
        .start_date
        .unwrap_or_else(|| (now - chrono::Duration::days(1)).to_rfc3339());
    let end = query.end_date.unwrap_or_else(|| now.to_rfc3339());

    let start_unix_ms = parse_usage_period(&start, "start_date")?;
    let end_unix_ms = parse_usage_period(&end, "end_date")?;
    let store = relay
        .profit_store
        .as_ref()
        .ok_or_else(|| RelayError::Internal("profit ledger database is unavailable".to_string()))?;
    let records = store
        .list_for_filter(
            &aether_data::repository::relay_profit::RelayProfitLedgerFilter {
                instance_id: relay.instance_id.clone(),
                start_unix_ms,
                end_unix_ms,
            },
        )
        .await
        .map_err(|error| RelayError::Internal(format!("load profit ledger: {error}")))?;

    Ok(Json(usage_summary_from_records(start, end, &records)?).into_response())
}

fn parse_usage_period(value: &str, field: &str) -> Result<i64, RelayError> {
    DateTime::parse_from_rfc3339(value)
        .map_err(|_| RelayError::InvalidConfig(format!("invalid {field} timestamp")))
        .map(|value| value.timestamp_millis())
}

fn usage_summary_from_records(
    period_start: String,
    period_end: String,
    records: &[aether_data::repository::relay_profit::PersistedRelayProfitRecord],
) -> Result<UsageSummaryExport, RelayError> {
    let mut total_requests = 0_u64;
    let mut total_prompt_tokens = 0_u64;
    let mut total_completion_tokens = 0_u64;
    let mut total_charged_quota = 0.0;
    let mut known_upstream_cost = 0.0;
    let mut known_downstream_revenue = 0.0;
    let mut unknown_upstream_cost_requests = 0_u64;
    let mut unknown_downstream_revenue_requests = 0_u64;
    let mut by_model = BTreeMap::<String, ModelUsageSummary>::new();

    for record in records {
        let prompt_tokens = u64::try_from(record.prompt_tokens).map_err(|_| {
            RelayError::Internal("profit ledger contains negative prompt tokens".to_string())
        })?;
        let completion_tokens = u64::try_from(record.completion_tokens).map_err(|_| {
            RelayError::Internal("profit ledger contains negative completion tokens".to_string())
        })?;
        let charged_quota = record.charged_quota.parse::<f64>().map_err(|_| {
            RelayError::Internal("profit ledger contains an invalid charged quota".to_string())
        })?;
        if !charged_quota.is_finite() {
            return Err(RelayError::Internal(
                "profit ledger charged quota is outside export range".to_string(),
            ));
        }

        total_requests = total_requests.checked_add(1).ok_or_else(|| {
            RelayError::Internal("profit ledger request count overflows export range".to_string())
        })?;
        total_prompt_tokens = total_prompt_tokens
            .checked_add(prompt_tokens)
            .ok_or_else(|| {
                RelayError::Internal(
                    "profit ledger prompt tokens overflow export range".to_string(),
                )
            })?;
        total_completion_tokens = total_completion_tokens
            .checked_add(completion_tokens)
            .ok_or_else(|| {
                RelayError::Internal(
                    "profit ledger completion tokens overflow export range".to_string(),
                )
            })?;
        total_charged_quota += charged_quota;

        match record.upstream_cost_usd {
            Some(cost) if cost.is_finite() => known_upstream_cost += cost,
            Some(_) => {
                return Err(RelayError::Internal(
                    "profit ledger contains a non-finite upstream cost".to_string(),
                ))
            }
            None => unknown_upstream_cost_requests += 1,
        }
        match record.downstream_revenue_usd {
            Some(revenue) if revenue.is_finite() => known_downstream_revenue += revenue,
            Some(_) => {
                return Err(RelayError::Internal(
                    "profit ledger contains a non-finite downstream revenue".to_string(),
                ))
            }
            None => unknown_downstream_revenue_requests += 1,
        }

        let model = by_model
            .entry(record.model_id.clone())
            .or_insert_with(|| ModelUsageSummary {
                model_id: record.model_id.clone(),
                requests: 0,
                prompt_tokens: 0,
                completion_tokens: 0,
                charged_quota: 0.0,
            });
        model.requests = model.requests.checked_add(1).ok_or_else(|| {
            RelayError::Internal(
                "profit ledger model request count overflows export range".to_string(),
            )
        })?;
        model.prompt_tokens = model
            .prompt_tokens
            .checked_add(prompt_tokens)
            .ok_or_else(|| {
                RelayError::Internal(
                    "profit ledger model prompt tokens overflow export range".to_string(),
                )
            })?;
        model.completion_tokens = model
            .completion_tokens
            .checked_add(completion_tokens)
            .ok_or_else(|| {
                RelayError::Internal(
                    "profit ledger model completion tokens overflow export range".to_string(),
                )
            })?;
        model.charged_quota += charged_quota;
    }

    if !total_charged_quota.is_finite()
        || !known_upstream_cost.is_finite()
        || !known_downstream_revenue.is_finite()
    {
        return Err(RelayError::Internal(
            "profit ledger totals are outside export range".to_string(),
        ));
    }

    Ok(UsageSummaryExport {
        period_start,
        period_end,
        total_requests,
        total_prompt_tokens,
        total_completion_tokens,
        total_charged_quota,
        total_upstream_cost_usd: (unknown_upstream_cost_requests == 0)
            .then_some(known_upstream_cost),
        total_downstream_revenue_usd: (unknown_downstream_revenue_requests == 0)
            .then_some(known_downstream_revenue),
        unknown_upstream_cost_requests,
        unknown_downstream_revenue_requests,
        by_model: by_model.into_values().collect(),
    })
}

/// GET /api/export/events
async fn export_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<EventsQuery>,
) -> Result<Response, RelayError> {
    if !is_authorized_export_request(&headers) {
        return Ok(unauthorized_export_response());
    }

    let relay = state
        .relay_engine()
        .ok_or(RelayError::Internal("relay not enabled".into()))?;

    let limit = query.limit.unwrap_or(100).clamp(1, 1000) as usize;
    let cursor = query.cursor.unwrap_or_default();
    let outbox = relay
        .event_outbox
        .as_ref()
        .ok_or_else(|| RelayError::Internal("event outbox database is unavailable".into()))?;
    let (events, next_cursor, has_more) = outbox.query_events(&cursor, limit).await?;
    Ok(Json(ExportEventsResponse {
        events: events
            .into_iter()
            .map(|event| ExportEvent {
                event_id: event.id,
                event_type: event.event_type,
                payload: event.payload,
                created_at: event.created_at,
            })
            .collect(),
        next_cursor,
        has_more,
    })
    .into_response())
}

#[derive(Serialize)]
struct ExportEventsResponse {
    events: Vec<ExportEvent>,
    next_cursor: String,
    has_more: bool,
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::{Arc, Mutex, OnceLock};

    use aether_data::repository::relay_profit::{PersistedRelayProfitRecord, RelayCostConfidence};
    use aether_data::{DatabaseDriver, SqlDatabaseConfig, SqlPoolConfig};
    use aether_data_contracts::repository::global_models::{
        CreateAdminGlobalModelRecord, StoredPublicCatalogModel, UpsertAdminProviderModelRecord,
    };
    use aether_data_contracts::repository::provider_catalog::StoredProviderCatalogProvider;
    use aether_runtime_state::{MemoryRuntimeStateConfig, RuntimeState};
    use axum::body::{to_bytes, Body};
    use axum::http::{HeaderMap, Request, StatusCode};
    use axum::routing::get;
    use axum::Router;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::{
        mount_export_routes, pricing_etag, pricing_export_response,
        pricing_exports_from_catalog_models, usage_summary_from_records, ModelPricingExport,
    };
    use crate::data::GatewayDataConfig;
    use crate::relay::event_outbox::EventOutbox;
    use crate::relay::groups::{CreateGroupInput, DownstreamGroup, DownstreamGroupManager};
    use crate::relay::RelayEngineConfig;
    use crate::state::AppState;

    struct EnvVarGuard {
        key: &'static str,
        original: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = self.original.take() {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    fn export_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn export_test_router() -> Router {
        mount_export_routes(Router::<AppState>::new())
            .with_state(AppState::new().expect("gateway state should build"))
    }

    fn pricing_model(model_id: &str, input_quota_per_token: f64) -> ModelPricingExport {
        ModelPricingExport {
            model_id: model_id.to_string(),
            input_quota_per_token,
            output_quota_per_token: input_quota_per_token * 2.0,
            capabilities: vec!["streaming".to_string()],
            cheapest_channel_id: Some("provider-a".to_string()),
            available_channels: 1,
        }
    }

    async fn database_export_test_state() -> AppState {
        let mut pool = SqlPoolConfig::default();
        pool.min_connections = 0;
        pool.max_connections = 1;
        let database = SqlDatabaseConfig::new(DatabaseDriver::Sqlite, "sqlite::memory:", pool)
            .expect("sqlite test database configuration should be valid");
        let mut state = AppState::new()
            .expect("gateway state should build")
            .with_data_config(GatewayDataConfig::from_database_config(database))
            .expect("gateway data state should build");
        state
            .run_database_migrations()
            .await
            .expect("sqlite relay migrations should run");

        let mut relay_config = RelayEngineConfig::default();
        relay_config.enabled = true;
        state.configure_relay_engine_with_config(relay_config);
        state
    }

    async fn seed_database_export_truth(state: &AppState) -> (String, String) {
        let provider = StoredProviderCatalogProvider::new(
            "provider-db".to_string(),
            "Database Provider".to_string(),
            Some("https://example.invalid".to_string()),
            "openai".to_string(),
        )
        .expect("provider fixture should build");
        state
            .create_provider_catalog_provider(&provider, None)
            .await
            .expect("provider fixture should persist")
            .expect("provider fixture should return");

        let global_model = CreateAdminGlobalModelRecord::new(
            "global-db".to_string(),
            "gpt-db".to_string(),
            "GPT Database".to_string(),
            true,
            None,
            None,
            None,
            None,
        )
        .expect("global model fixture should build");
        state
            .create_admin_global_model(&global_model)
            .await
            .expect("global model fixture should persist")
            .expect("global model fixture should return");
        let provider_model = UpsertAdminProviderModelRecord::new(
            "model-db".to_string(),
            "provider-db".to_string(),
            "global-db".to_string(),
            "gpt-db-upstream".to_string(),
            None,
            None,
            Some(json!({
                "tiers": [{
                    "input_price_per_1m": 2.0,
                    "output_price_per_1m": 8.0
                }]
            })),
            None,
            None,
            Some(true),
            None,
            None,
            true,
            true,
            None,
        )
        .expect("provider model fixture should build");
        state
            .create_admin_provider_model(&provider_model)
            .await
            .expect("provider model fixture should persist")
            .expect("provider model fixture should return");

        // Seed the database through a separate runtime so the state under test remains empty.
        let database_group_manager = DownstreamGroupManager::with_store(
            Arc::new(RelayEngineConfig::default()),
            RuntimeState::memory(MemoryRuntimeStateConfig::default()),
            state
                .data
                .relay_downstream_group_store()
                .expect("downstream group store should be available"),
        );
        let database_group = database_group_manager
            .create_group(CreateGroupInput {
                name: "database-group".to_string(),
                description: None,
                parent_id: None,
                global_ratio_multiplier: Some(1.25),
                model_whitelist: Some(vec!["gpt-db".to_string()]),
                model_blacklist: None,
                model_ratio_overrides: None,
                priority: Some(7),
                requests_per_minute: None,
                requests_per_day: None,
                daily_quota_limit: None,
                monthly_quota_limit: None,
                time_rules: None,
            })
            .await
            .expect("database group fixture should persist");

        let occurred_at = super::parse_usage_period("2026-07-17T00:30:00Z", "fixture")
            .expect("fixture timestamp should parse");
        state
            .data
            .relay_profit_ledger_store()
            .expect("profit ledger store should be available")
            .append(&PersistedRelayProfitRecord {
                id: "profit-db".to_string(),
                instance_id: "export-db-truth".to_string(),
                request_id: "request-db".to_string(),
                channel_id: "channel-db".to_string(),
                model_id: "gpt-db".to_string(),
                prompt_tokens: 17,
                completion_tokens: 9,
                charged_quota: "42".to_string(),
                quota_per_unit: Some("500000".to_string()),
                upstream_cost_usd: Some(0.1),
                downstream_revenue_usd: Some(0.2),
                payment_fee_usd: Some(0.0012),
                net_profit_usd: Some(0.0988),
                margin_percent: Some(98.8),
                cost_confidence: RelayCostConfidence::Known,
                occurred_at_unix_ms: occurred_at,
            })
            .await
            .expect("profit fixture should persist");

        let event_id = EventOutbox::new(
            state
                .data
                .relay_event_outbox_store()
                .expect("event outbox store should be available"),
            "export-db-truth".to_string(),
        )
        .publish("database_truth", json!({"source": "database"}))
        .await
        .expect("event fixture should persist");

        (database_group.id, event_id)
    }

    async fn authorized_export_json(router: &Router, uri: &str) -> Value {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("authorization", "Bearer export-db-truth-token")
                    .body(Body::empty())
                    .expect("export request should build"),
            )
            .await
            .expect("export route should respond");
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("export response body should read");
        serde_json::from_slice(&body).expect("export response should be JSON")
    }

    #[test]
    fn outbound_export_token_is_independent_from_active_control_and_relay_credentials() {
        let _lock = export_env_lock()
            .lock()
            .expect("export env lock should acquire");
        let _outbound_token = EnvVarGuard::set("AETHER_OUTBOUND_EXPORT_TOKEN", "export-token");
        let _control_secret = EnvVarGuard::set("AETHER_CONTROL_SECRET", "control-only-token");
        let _control_previous =
            EnvVarGuard::set("AETHER_CONTROL_SECRET_PREVIOUS", "control-previous-token");
        let _control_previous_expires =
            EnvVarGuard::set("AETHER_CONTROL_SECRET_PREVIOUS_EXPIRES_AT", "1");
        let _relay_secret = EnvVarGuard::set("AETHER_RELAY_SIGNING_SECRET", "relay-only-token");
        let _relay_previous =
            EnvVarGuard::set("AETHER_RELAY_SIGNING_SECRET_PREVIOUS", "relay-previous-token");
        let _relay_previous_expires =
            EnvVarGuard::set("AETHER_RELAY_SIGNING_SECRET_PREVIOUS_EXPIRES_AT", "1");
        let mut headers = HeaderMap::new();

        headers.insert("authorization", "Bearer export-token".parse().unwrap());
        assert!(super::is_authorized_export_request(&headers));

        headers.insert(
            "authorization",
            "Bearer control-only-token".parse().unwrap(),
        );
        assert!(!super::is_authorized_export_request(&headers));

        headers.insert(
            "authorization",
            "Bearer relay-only-token".parse().unwrap(),
        );
        assert!(!super::is_authorized_export_request(&headers));

        headers.insert("authorization", "Bearer export-token".parse().unwrap());
        std::env::set_var("AETHER_CONTROL_SECRET", "export-token");
        assert!(
            !super::is_authorized_export_request(&headers),
            "an export token that overlaps an active control credential must fail closed"
        );

        std::env::set_var("AETHER_CONTROL_SECRET", "control-only-token");
        std::env::set_var("AETHER_RELAY_SIGNING_SECRET", "export-token");
        assert!(
            !super::is_authorized_export_request(&headers),
            "an export token that overlaps an active relay credential must fail closed"
        );

        std::env::set_var("AETHER_RELAY_SIGNING_SECRET", "relay-only-token");
        std::env::set_var("AETHER_CONTROL_SECRET_PREVIOUS", "export-token");
        std::env::set_var("AETHER_CONTROL_SECRET_PREVIOUS_EXPIRES_AT", "4102444800");
        assert!(
            !super::is_authorized_export_request(&headers),
            "an export token that overlaps an active transition credential must fail closed"
        );

        std::env::set_var("AETHER_CONTROL_SECRET_PREVIOUS_EXPIRES_AT", "1");
        assert!(
            super::is_authorized_export_request(&headers),
            "an expired transition credential must not block the independent export token"
        );
    }

    #[tokio::test]
    async fn export_routes_reject_control_credentials_and_return_unauthorized() {
        let _lock = export_env_lock()
            .lock()
            .expect("export env lock should acquire");
        let _outbound_token = EnvVarGuard::set("AETHER_OUTBOUND_EXPORT_TOKEN", "export-token");
        let _control_secret = EnvVarGuard::set("AETHER_CONTROL_SECRET", "control-only-token");
        let router = export_test_router();

        for uri in [
            "/api/export/pricing",
            "/api/export/usage-summary",
            "/api/export/events",
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header("authorization", "Bearer control-only-token")
                        .body(Body::empty())
                        .expect("export request should build"),
                )
                .await
                .expect("export router should respond");
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        }
    }

    #[tokio::test]
    async fn pricing_route_uses_content_etags_for_conditional_requests() {
        let before = vec![pricing_model("gpt-5", 0.000_002)];
        let after = vec![pricing_model("gpt-5", 0.000_003)];
        let groups = Vec::new();
        let old_etag = pricing_etag(&before, &groups).expect("initial etag should build");
        let route_models = after.clone();
        let router = Router::new().route(
            "/api/export/pricing",
            get(move |headers: HeaderMap| {
                let models = route_models.clone();
                async move { pricing_export_response(&headers, models, Vec::new()) }
            }),
        );

        let changed_response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/export/pricing")
                    .header("if-none-match", &old_etag)
                    .body(Body::empty())
                    .expect("conditional request should build"),
            )
            .await
            .expect("pricing route should respond");
        assert_eq!(changed_response.status(), StatusCode::OK);
        let current_etag = changed_response
            .headers()
            .get("etag")
            .expect("changed response should include etag")
            .to_str()
            .expect("etag should be text")
            .to_string();
        assert_ne!(current_etag, old_etag);

        let unchanged_response = router
            .oneshot(
                Request::builder()
                    .uri("/api/export/pricing")
                    .header("if-none-match", &current_etag)
                    .body(Body::empty())
                    .expect("conditional request should build"),
            )
            .await
            .expect("pricing route should respond");
        assert_eq!(unchanged_response.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn export_routes_ignore_empty_and_misleading_runtime_state_for_database_truth() {
        let _lock = export_env_lock()
            .lock()
            .expect("export env lock should acquire");
        let _outbound_token =
            EnvVarGuard::set("AETHER_OUTBOUND_EXPORT_TOKEN", "export-db-truth-token");
        let _instance_id = EnvVarGuard::set("AETHER_INSTANCE_ID", "export-db-truth");
        let state = database_export_test_state().await;
        let (database_group_id, database_event_id) = seed_database_export_truth(&state).await;
        let runtime_state = state.runtime_state.clone();
        assert_eq!(
            runtime_state
                .kv_get("relay:dgroup:ids")
                .await
                .expect("target runtime state should be readable"),
            None,
            "the target state must start without a group cache"
        );
        let router = mount_export_routes(Router::<AppState>::new()).with_state(state);

        let pricing_before = authorized_export_json(&router, "/api/export/pricing").await;
        assert_eq!(pricing_before["models"].as_array().map(Vec::len), Some(1));
        assert_eq!(pricing_before["models"][0]["model_id"], "gpt-db");
        assert_eq!(
            pricing_before["models"][0]["input_quota_per_token"],
            json!(0.000_002)
        );
        assert_eq!(
            pricing_before["models"][0]["output_quota_per_token"],
            json!(0.000_008)
        );
        assert_eq!(pricing_before["groups"].as_array().map(Vec::len), Some(1));
        assert_eq!(pricing_before["groups"][0]["id"], database_group_id);
        assert_eq!(pricing_before["groups"][0]["name"], "database-group");

        let usage_before = authorized_export_json(
            &router,
            "/api/export/usage-summary?start_date=2026-07-17T00:00:00Z&end_date=2026-07-18T00:00:00Z",
        )
        .await;
        assert_eq!(usage_before["total_requests"], 1);
        assert_eq!(usage_before["total_prompt_tokens"], 17);
        assert_eq!(usage_before["total_completion_tokens"], 9);
        assert_eq!(usage_before["total_charged_quota"], 42.0);
        assert_eq!(usage_before["by_model"][0]["model_id"], "gpt-db");

        let events_before = authorized_export_json(&router, "/api/export/events").await;
        assert_eq!(events_before["events"].as_array().map(Vec::len), Some(1));
        assert_eq!(events_before["events"][0]["event_id"], database_event_id);
        assert_eq!(
            events_before["events"][0]["payload"],
            json!({"source": "database"})
        );

        let runtime_only_group = DownstreamGroup {
            id: "runtime-only".to_string(),
            name: "runtime-only".to_string(),
            description: None,
            parent_id: None,
            global_ratio_multiplier: 99.0,
            model_whitelist: vec!["runtime-model".to_string()],
            model_blacklist: Vec::new(),
            model_ratio_overrides: Default::default(),
            priority: 99,
            requests_per_minute: 0,
            requests_per_day: 0,
            daily_quota_limit: 0.0,
            monthly_quota_limit: 0.0,
            time_rules: Vec::new(),
            enabled: true,
            created_at: "2026-07-17T00:00:00Z".to_string(),
            updated_at: "2026-07-17T00:00:00Z".to_string(),
        };
        runtime_state
            .kv_set(
                "relay:dgroup:runtime-only",
                serde_json::to_string(&runtime_only_group)
                    .expect("runtime-only group should serialize"),
                None,
            )
            .await
            .expect("runtime-only group should cache");
        runtime_state
            .set_add("relay:dgroup:ids", "runtime-only")
            .await
            .expect("runtime-only group id should cache");
        for (key, value) in [
            ("relay:pricing:gpt-db", "runtime-price"),
            ("relay:usage:request-db", "runtime-usage"),
            ("relay:events:database_truth", "runtime-event"),
        ] {
            runtime_state
                .kv_set(key, value, None)
                .await
                .expect("misleading runtime entry should cache");
        }

        let pricing_after = authorized_export_json(&router, "/api/export/pricing").await;
        assert_eq!(pricing_after["etag"], pricing_before["etag"]);
        assert_eq!(pricing_after["models"], pricing_before["models"]);
        assert_eq!(pricing_after["groups"], pricing_before["groups"]);
        assert!(pricing_after["groups"]
            .as_array()
            .expect("groups should be an array")
            .iter()
            .all(|group| group["id"] != "runtime-only"));

        assert_eq!(
            authorized_export_json(
                &router,
                "/api/export/usage-summary?start_date=2026-07-17T00:00:00Z&end_date=2026-07-18T00:00:00Z",
            )
            .await,
            usage_before
        );
        assert_eq!(
            authorized_export_json(&router, "/api/export/events").await,
            events_before
        );
    }

    fn catalog_model(
        provider_id: &str,
        provider_name: &str,
        model_name: &str,
        input_price_per_1m: Option<f64>,
        output_price_per_1m: Option<f64>,
        is_active: bool,
    ) -> StoredPublicCatalogModel {
        StoredPublicCatalogModel {
            id: format!("{provider_id}-{model_name}"),
            provider_id: provider_id.to_string(),
            provider_name: provider_name.to_string(),
            provider_model_name: model_name.to_string(),
            name: model_name.to_string(),
            display_name: model_name.to_string(),
            description: None,
            icon_url: None,
            input_price_per_1m,
            output_price_per_1m,
            cache_creation_price_per_1m: None,
            cache_read_price_per_1m: None,
            supports_vision: None,
            supports_function_calling: None,
            supports_streaming: None,
            supports_embedding: None,
            is_active,
        }
    }

    fn record(
        request_id: &str,
        model_id: &str,
        charged_quota: &str,
        upstream_cost_usd: Option<f64>,
        downstream_revenue_usd: Option<f64>,
    ) -> PersistedRelayProfitRecord {
        PersistedRelayProfitRecord {
            id: format!("profit-{request_id}"),
            instance_id: "aether-primary".to_string(),
            request_id: request_id.to_string(),
            channel_id: "41".to_string(),
            model_id: model_id.to_string(),
            prompt_tokens: 10,
            completion_tokens: 5,
            charged_quota: charged_quota.to_string(),
            quota_per_unit: Some("500000".to_string()),
            upstream_cost_usd,
            downstream_revenue_usd,
            payment_fee_usd: downstream_revenue_usd.map(|revenue| revenue * 0.006),
            net_profit_usd: None,
            margin_percent: None,
            cost_confidence: if upstream_cost_usd.is_some() {
                RelayCostConfidence::Known
            } else {
                RelayCostConfidence::Unknown
            },
            occurred_at_unix_ms: 1,
        }
    }

    #[test]
    fn usage_summary_does_not_replace_unknown_financial_totals_with_zero() {
        let records = vec![
            record("known", "gpt-5", "1250", Some(0.001), Some(0.0025)),
            record("unknown", "gpt-4.1", "500", None, None),
        ];

        let summary = usage_summary_from_records(
            "2026-07-16T00:00:00Z".to_string(),
            "2026-07-17T00:00:00Z".to_string(),
            &records,
        )
        .expect("valid persisted ledger rows should aggregate");

        assert_eq!(summary.total_requests, 2);
        assert_eq!(summary.total_prompt_tokens, 20);
        assert_eq!(summary.total_completion_tokens, 10);
        assert_eq!(summary.total_charged_quota, 1750.0);
        assert_eq!(summary.total_upstream_cost_usd, None);
        assert_eq!(summary.total_downstream_revenue_usd, None);
        assert_eq!(summary.unknown_upstream_cost_requests, 1);
        assert_eq!(summary.unknown_downstream_revenue_requests, 1);
        assert_eq!(
            summary
                .by_model
                .iter()
                .map(|row| row.model_id.as_str())
                .collect::<Vec<_>>(),
            vec!["gpt-4.1", "gpt-5"]
        );
    }

    #[test]
    fn pricing_export_uses_known_active_catalog_models_without_fabricating_unknown_prices() {
        let mut expensive = catalog_model(
            "provider-expensive",
            "Expensive",
            "gpt-5",
            Some(4.0),
            Some(12.0),
            true,
        );
        expensive.supports_streaming = Some(true);
        let mut cheapest = catalog_model(
            "provider-cheap",
            "Cheap",
            "gpt-5",
            Some(2.0),
            Some(8.0),
            true,
        );
        cheapest.supports_vision = Some(true);

        let models = pricing_exports_from_catalog_models(&[
            expensive,
            cheapest,
            catalog_model(
                "provider-unknown",
                "Unknown",
                "gpt-4.1",
                None,
                Some(5.0),
                true,
            ),
            catalog_model(
                "provider-inactive",
                "Inactive",
                "gpt-5",
                Some(1.0),
                Some(1.0),
                false,
            ),
        ])
        .expect("known catalog prices should export");

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_id, "gpt-5");
        assert_eq!(models[0].input_quota_per_token, 0.000_002);
        assert_eq!(models[0].output_quota_per_token, 0.000_008);
        assert_eq!(
            models[0].cheapest_channel_id.as_deref(),
            Some("provider-cheap")
        );
        assert_eq!(models[0].available_channels, 2);
        assert_eq!(models[0].capabilities, vec!["streaming", "vision"]);
    }
}
