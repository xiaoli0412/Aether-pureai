//! 对账任务
//!
//! 定期从下游 New API 实例拉取收入和用量数据，与本地利润记录比对生成对账记录。

use std::sync::Arc;
use std::time::Duration;

use aether_data::repository::relay_profit::{
    PersistedRelayProfitRecord, RelayCostConfidence, RelayProfitLedgerFilter,
    RelayProfitLedgerStore,
};
use aether_data::repository::relay_reconciliation::{
    PersistedRelayDownstreamInstance, PersistedRelaySettlement, RelayReconciliationStore,
};
use aether_relay_core::pricing::try_quota_to_usd;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{error, info, warn};
use uuid::Uuid;

use super::engine::RelayEngineConfig;
use super::error::RelayError;

/// 下游实例配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredDownstreamInstance {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    pub api_key: String,
    pub enabled: bool,
    pub last_sync_at: Option<String>,
    pub created_at: String,
}

/// 下游实例统计数据
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownstreamStats {
    pub total_revenue_quota: f64,
    pub total_tokens: u64,
    pub total_requests: u64,
    #[serde(default)]
    pub quota_per_unit: Option<String>,
}

/// 对账记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSettlement {
    pub id: String,
    pub downstream_id: String,
    pub period_start: String,
    pub period_end: String,
    pub downstream_revenue_usd: f64,
    pub upstream_cost_usd: f64,
    pub difference_usd: f64,
    pub difference_percent: f64,
    pub is_anomaly: bool,
    pub raw_data_json: Option<String>,
    pub created_at: String,
}

/// 创建下游实例输入
#[derive(Debug, Clone, Deserialize)]
pub struct CreateDownstreamInput {
    pub name: String,
    pub endpoint: String,
    pub api_key: String,
    pub enabled: Option<bool>,
}

/// 对账服务
#[derive(Clone)]
pub struct ReconciliationService {
    pub(crate) config: Arc<RelayEngineConfig>,
    pub(crate) reconciliation_store: Option<RelayReconciliationStore>,
    pub(crate) profit_store: Option<RelayProfitLedgerStore>,
    pub(crate) ledger_instance_id: String,
    pub(crate) http_client: reqwest::Client,
}

impl ReconciliationService {
    pub fn new(
        config: Arc<RelayEngineConfig>,
        reconciliation_store: Option<RelayReconciliationStore>,
        profit_store: Option<RelayProfitLedgerStore>,
        ledger_instance_id: String,
    ) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            config,
            reconciliation_store,
            profit_store,
            ledger_instance_id,
            http_client,
        }
    }

    /// 启动定期对账后台任务
    pub fn spawn_reconciliation_task(
        self: Arc<Self>,
        mut shutdown: watch::Receiver<()>,
    ) -> tokio::task::JoinHandle<()> {
        let interval = Duration::from_secs(self.config.reconciliation_interval_secs.max(1));
        tokio::spawn(async move {
            info!(
                interval_secs = self.config.reconciliation_interval_secs,
                "starting reconciliation background task"
            );

            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await; // Skip first immediate tick

            loop {
                tokio::select! {
                    _ = ticker.tick() => {
                        if let Err(e) = self.run_reconciliation().await {
                            error!(error = %e, "reconciliation task failed");
                        }
                    }
                    _ = shutdown.changed() => {
                        info!("reconciliation task shutting down");
                        break;
                    }
                }
            }
        })
    }

    /// 执行一次对账
    pub async fn run_reconciliation(&self) -> Result<Vec<StoredSettlement>, RelayError> {
        let instances = self.get_enabled_downstream_instances().await?;
        let mut settlements = Vec::new();

        for instance in &instances {
            match self.reconcile_instance(instance).await {
                Ok(settlement) => {
                    if settlement.is_anomaly {
                        warn!(
                            downstream_id = %instance.id,
                            difference_percent = settlement.difference_percent,
                            "settlement anomaly detected"
                        );
                    }
                    settlements.push(settlement);
                }
                Err(e) => {
                    error!(downstream_id = %instance.id, error = %e, "reconciliation failed for instance");
                }
            }
        }

        Ok(settlements)
    }

    /// 对账单个下游实例
    async fn reconcile_instance(
        &self,
        instance: &StoredDownstreamInstance,
    ) -> Result<StoredSettlement, RelayError> {
        let now = Utc::now();
        let period_end = now.to_rfc3339();
        let period_start = instance
            .last_sync_at
            .clone()
            .unwrap_or_else(|| (now - chrono::Duration::hours(1)).to_rfc3339());

        // Fetch downstream stats
        let stats = self.fetch_downstream_stats(instance).await?;
        let downstream_revenue_usd = downstream_revenue_usd(&stats)?;

        // Load settled upstream cost for this AETHER contract instance.
        let upstream_cost_usd = self
            .get_period_upstream_cost(&period_start, &period_end)
            .await?;

        let difference_usd = downstream_revenue_usd - upstream_cost_usd;
        let difference_percent = if upstream_cost_usd > 0.0 {
            (difference_usd / upstream_cost_usd) * 100.0
        } else {
            0.0
        };

        let is_anomaly =
            difference_percent.abs() > self.config.settlement_anomaly_threshold_percent;

        let settlement = StoredSettlement {
            id: Uuid::new_v4().to_string(),
            downstream_id: instance.id.clone(),
            period_start,
            period_end,
            downstream_revenue_usd,
            upstream_cost_usd,
            difference_usd,
            difference_percent,
            is_anomaly,
            raw_data_json: serde_json::to_string(&stats).ok(),
            created_at: now.to_rfc3339(),
        };

        let persisted_settlement = persisted_settlement(&settlement)?;
        self.reconciliation_store()?
            .record_settlement_and_advance_sync(&persisted_settlement)
            .await
            .map_err(|error| {
                RelayError::Reconciliation(format!("persist reconciliation settlement: {error}"))
            })?;

        info!(
            downstream_id = %instance.id,
            downstream_revenue = downstream_revenue_usd,
            upstream_cost = upstream_cost_usd,
            difference_percent = difference_percent,
            is_anomaly = is_anomaly,
            "reconciliation completed"
        );

        Ok(settlement)
    }

    /// 从下游实例获取统计数据
    async fn fetch_downstream_stats(
        &self,
        instance: &StoredDownstreamInstance,
    ) -> Result<DownstreamStats, RelayError> {
        // Call downstream New API dashboard/statistics endpoint
        let url = format!("{}/api/dashboard/", instance.endpoint.trim_end_matches('/'));

        let resp = self
            .http_client
            .get(&url)
            .header("Authorization", format!("Bearer {}", instance.api_key))
            .send()
            .await
            .map_err(|e| RelayError::Reconciliation(format!("fetch downstream stats: {}", e)))?;

        if !resp.status().is_success() {
            return Err(RelayError::Reconciliation(format!(
                "downstream API returned HTTP {}",
                resp.status()
            )));
        }

        let body = resp
            .text()
            .await
            .map_err(|e| RelayError::Reconciliation(format!("read response body: {}", e)))?;

        // Parse response — New API dashboard returns various formats
        // Try common structures
        if let Ok(stats) = serde_json::from_str::<DownstreamStats>(&body) {
            return Ok(stats);
        }

        // Try wrapped response
        #[derive(Deserialize)]
        struct Wrapped {
            data: Option<DownstreamStats>,
        }
        if let Ok(wrapped) = serde_json::from_str::<Wrapped>(&body) {
            if let Some(data) = wrapped.data {
                return Ok(data);
            }
        }

        Err(RelayError::Reconciliation(
            "failed to parse downstream stats".to_string(),
        ))
    }

    /// 获取时间段内已经结算的实际上游成本。
    ///
    /// A missing store, an empty period, or any unknown ledger cost is not a
    /// zero-cost period: reconciliation must fail closed rather than generate
    /// a misleading settlement.
    async fn get_period_upstream_cost(
        &self,
        period_start: &str,
        period_end: &str,
    ) -> Result<f64, RelayError> {
        let start_unix_ms = parse_reconciliation_period(period_start, "period_start")?;
        let end_unix_ms = parse_reconciliation_period(period_end, "period_end")?;
        if start_unix_ms >= end_unix_ms {
            return Err(RelayError::Reconciliation(
                "reconciliation period must have a positive duration".to_string(),
            ));
        }

        let store = self.profit_store.as_ref().ok_or_else(|| {
            RelayError::Reconciliation(
                "profit ledger database is unavailable; upstream cost is unknown".to_string(),
            )
        })?;
        let records = store
            .list_for_filter(&RelayProfitLedgerFilter {
                instance_id: self.ledger_instance_id.clone(),
                start_unix_ms,
                end_unix_ms,
            })
            .await
            .map_err(|error| {
                RelayError::Reconciliation(format!("load settled upstream costs: {error}"))
            })?;

        sum_settled_upstream_cost(&records)
    }

    /// 获取所有已启用的下游实例
    pub async fn get_enabled_downstream_instances(
        &self,
    ) -> Result<Vec<StoredDownstreamInstance>, RelayError> {
        self.reconciliation_store()?
            .list_enabled_downstream_instances()
            .await
            .map_err(|error| {
                RelayError::Reconciliation(format!("load downstream instances: {error}"))
            })?
            .into_iter()
            .map(stored_downstream_instance)
            .collect()
    }

    /// 验证下游实例连接有效性
    pub async fn validate_connection(
        &self,
        endpoint: &str,
        api_key: &str,
    ) -> Result<bool, RelayError> {
        let url = format!("{}/api/status", endpoint.trim_end_matches('/'));
        match self
            .http_client
            .get(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .send()
            .await
        {
            Ok(resp) => Ok(resp.status().is_success()),
            Err(_) => Ok(false),
        }
    }

    /// Saves a downstream instance to the durable reconciliation database.
    pub async fn save_downstream_instance(
        &self,
        instance: &StoredDownstreamInstance,
    ) -> Result<(), RelayError> {
        let persisted = persisted_downstream_instance(instance)?;
        self.reconciliation_store()?
            .insert_downstream_instance(&persisted)
            .await
            .map_err(|error| {
                RelayError::Reconciliation(format!("persist downstream instance: {error}"))
            })
    }

    pub async fn delete_downstream_instance(&self, id: &str) -> Result<bool, RelayError> {
        self.reconciliation_store()?
            .delete_downstream_instance(id)
            .await
            .map_err(|error| {
                RelayError::Reconciliation(format!("delete downstream instance: {error}"))
            })
    }

    fn reconciliation_store(&self) -> Result<&RelayReconciliationStore, RelayError> {
        self.reconciliation_store.as_ref().ok_or_else(|| {
            RelayError::Reconciliation(
                "reconciliation database is unavailable; durable downstream state is required"
                    .to_string(),
            )
        })
    }
}

fn stored_downstream_instance(
    instance: PersistedRelayDownstreamInstance,
) -> Result<StoredDownstreamInstance, RelayError> {
    Ok(StoredDownstreamInstance {
        id: instance.id,
        name: instance.name,
        endpoint: instance.endpoint,
        api_key: instance.api_key,
        enabled: instance.enabled,
        last_sync_at: instance
            .last_sync_at_unix_ms
            .map(format_reconciliation_timestamp)
            .transpose()?,
        created_at: format_reconciliation_timestamp(instance.created_at_unix_ms)?,
    })
}

fn persisted_downstream_instance(
    instance: &StoredDownstreamInstance,
) -> Result<PersistedRelayDownstreamInstance, RelayError> {
    Ok(PersistedRelayDownstreamInstance {
        id: instance.id.clone(),
        name: instance.name.clone(),
        endpoint: instance.endpoint.clone(),
        api_key: instance.api_key.clone(),
        enabled: instance.enabled,
        last_sync_at_unix_ms: instance
            .last_sync_at
            .as_deref()
            .map(|value| parse_reconciliation_period(value, "last_sync_at"))
            .transpose()?,
        created_at_unix_ms: parse_reconciliation_period(&instance.created_at, "created_at")?,
    })
}

fn persisted_settlement(
    settlement: &StoredSettlement,
) -> Result<PersistedRelaySettlement, RelayError> {
    Ok(PersistedRelaySettlement {
        id: settlement.id.clone(),
        downstream_id: settlement.downstream_id.clone(),
        period_start_unix_ms: parse_reconciliation_period(
            &settlement.period_start,
            "period_start",
        )?,
        period_end_unix_ms: parse_reconciliation_period(&settlement.period_end, "period_end")?,
        downstream_revenue_usd: settlement.downstream_revenue_usd,
        upstream_cost_usd: settlement.upstream_cost_usd,
        difference_usd: settlement.difference_usd,
        difference_percent: settlement.difference_percent,
        is_anomaly: settlement.is_anomaly,
        raw_data_json: settlement.raw_data_json.clone(),
        created_at_unix_ms: parse_reconciliation_period(&settlement.created_at, "created_at")?,
    })
}

fn format_reconciliation_timestamp(value: i64) -> Result<String, RelayError> {
    DateTime::<Utc>::from_timestamp_millis(value)
        .ok_or_else(|| {
            RelayError::Reconciliation("invalid durable reconciliation timestamp".to_string())
        })
        .map(|timestamp| timestamp.to_rfc3339())
}

fn parse_reconciliation_period(value: &str, field: &str) -> Result<i64, RelayError> {
    DateTime::parse_from_rfc3339(value)
        .map_err(|_| RelayError::Reconciliation(format!("invalid {field} timestamp")))
        .map(|value| value.timestamp_millis())
}

fn sum_settled_upstream_cost(records: &[PersistedRelayProfitRecord]) -> Result<f64, RelayError> {
    if records.is_empty() {
        return Err(RelayError::Reconciliation(
            "no settled upstream cost records are available for this period".to_string(),
        ));
    }

    let mut total_cost_usd = 0.0;
    for record in records {
        if record.cost_confidence != RelayCostConfidence::Known {
            return Err(RelayError::Reconciliation(
                "upstream cost is unknown for a ledger entry".to_string(),
            ));
        }
        let Some(cost) = record.upstream_cost_usd else {
            return Err(RelayError::Reconciliation(
                "upstream cost is unknown for a ledger entry".to_string(),
            ));
        };
        if !cost.is_finite() || cost < 0.0 {
            return Err(RelayError::Reconciliation(
                "upstream cost is invalid in a ledger entry".to_string(),
            ));
        }
        total_cost_usd += cost;
        if !total_cost_usd.is_finite() {
            return Err(RelayError::Reconciliation(
                "settled upstream costs exceed the supported range".to_string(),
            ));
        }
    }

    Ok(total_cost_usd)
}

fn downstream_revenue_usd(stats: &DownstreamStats) -> Result<f64, RelayError> {
    let quota_per_unit = stats
        .quota_per_unit
        .as_deref()
        .ok_or_else(|| {
            RelayError::Reconciliation(
                "downstream quota_per_unit is unavailable; revenue is unknown".to_string(),
            )
        })?
        .parse::<f64>()
        .map_err(|_| {
            RelayError::Reconciliation(
                "downstream quota_per_unit is invalid; revenue is unknown".to_string(),
            )
        })?;
    try_quota_to_usd(stats.total_revenue_quota, quota_per_unit).ok_or_else(|| {
        RelayError::Reconciliation(
            "downstream quota_per_unit is invalid; revenue is unknown".to_string(),
        )
    })
}

#[cfg(test)]
mod tests {
    use aether_data::repository::relay_profit::{
        PersistedRelayProfitRecord, RelayCostConfidence, RelayProfitLedgerStore,
    };
    use sqlx::sqlite::SqlitePoolOptions;

    use super::{
        downstream_revenue_usd, sum_settled_upstream_cost, DownstreamStats, ReconciliationService,
        RelayEngineConfig,
    };

    const PERIOD_START: &str = "1970-01-01T00:00:01Z";
    const PERIOD_END: &str = "1970-01-01T00:00:10Z";

    async fn test_profit_store() -> RelayProfitLedgerStore {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite test pool should connect");
        sqlx::query(
            "CREATE TABLE relay_profit_ledger (
                id TEXT PRIMARY KEY,
                instance_id TEXT NOT NULL,
                request_id TEXT NOT NULL,
                channel_id TEXT NOT NULL,
                model_id TEXT NOT NULL,
                prompt_tokens INTEGER NOT NULL,
                completion_tokens INTEGER NOT NULL,
                charged_quota TEXT NOT NULL,
                quota_per_unit TEXT,
                upstream_cost_usd REAL,
                downstream_revenue_usd REAL,
                payment_fee_usd REAL,
                net_profit_usd REAL,
                margin_percent REAL,
                cost_confidence TEXT NOT NULL,
                occurred_at_unix_ms INTEGER NOT NULL,
                UNIQUE (instance_id, request_id)
            )",
        )
        .execute(&pool)
        .await
        .expect("relay profit table should be created");
        RelayProfitLedgerStore::sqlite(pool)
    }

    fn settled_record(
        id: &str,
        instance_id: &str,
        request_id: &str,
        occurred_at_unix_ms: i64,
        upstream_cost_usd: f64,
    ) -> PersistedRelayProfitRecord {
        PersistedRelayProfitRecord {
            id: id.to_string(),
            instance_id: instance_id.to_string(),
            request_id: request_id.to_string(),
            channel_id: "41".to_string(),
            model_id: "gpt-5".to_string(),
            prompt_tokens: 10,
            completion_tokens: 5,
            charged_quota: "1250".to_string(),
            quota_per_unit: Some("500000".to_string()),
            upstream_cost_usd: Some(upstream_cost_usd),
            downstream_revenue_usd: Some(0.0025),
            payment_fee_usd: Some(0.000015),
            net_profit_usd: Some(0.001485),
            margin_percent: Some(59.4),
            cost_confidence: RelayCostConfidence::Known,
            occurred_at_unix_ms,
        }
    }

    fn unknown_cost_record(
        id: &str,
        instance_id: &str,
        request_id: &str,
        occurred_at_unix_ms: i64,
    ) -> PersistedRelayProfitRecord {
        PersistedRelayProfitRecord {
            id: id.to_string(),
            instance_id: instance_id.to_string(),
            request_id: request_id.to_string(),
            channel_id: "41".to_string(),
            model_id: "gpt-5".to_string(),
            prompt_tokens: 10,
            completion_tokens: 5,
            charged_quota: "1250".to_string(),
            quota_per_unit: None,
            upstream_cost_usd: None,
            downstream_revenue_usd: None,
            payment_fee_usd: None,
            net_profit_usd: None,
            margin_percent: None,
            cost_confidence: RelayCostConfidence::Unknown,
            occurred_at_unix_ms,
        }
    }

    fn test_service(
        store: RelayProfitLedgerStore,
        ledger_instance_id: &str,
    ) -> ReconciliationService {
        ReconciliationService::new(
            std::sync::Arc::new(RelayEngineConfig::default()),
            None,
            Some(store),
            ledger_instance_id.to_string(),
        )
    }

    #[tokio::test]
    async fn reconciliation_sums_settled_upstream_costs_from_sql_ledger() {
        let store = test_profit_store().await;
        store
            .append(&settled_record(
                "profit-1",
                "instance-a",
                "request-1",
                2_000,
                0.25,
            ))
            .await
            .expect("first settled profit should persist");
        store
            .append(&settled_record(
                "profit-2",
                "instance-a",
                "request-2",
                3_000,
                0.75,
            ))
            .await
            .expect("second settled profit should persist");

        let cost = test_service(store, "instance-a")
            .get_period_upstream_cost(PERIOD_START, PERIOD_END)
            .await
            .expect("settled SQL costs should reconcile");

        assert!((cost - 1.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn reconciliation_refuses_unknown_upstream_cost_instead_of_substituting_zero() {
        let store = test_profit_store().await;
        store
            .append(&unknown_cost_record(
                "profit-unknown",
                "instance-a",
                "request-unknown",
                2_000,
            ))
            .await
            .expect("unknown-cost profit should persist");

        let error = test_service(store, "instance-a")
            .get_period_upstream_cost(PERIOD_START, PERIOD_END)
            .await
            .expect_err("unknown upstream cost cannot reconcile as zero");

        assert!(error.to_string().contains("upstream cost is unknown"));
    }

    #[test]
    fn reconciliation_refuses_unknown_cost_confidence_even_when_a_value_is_present() {
        let mut record = settled_record(
            "profit-inconsistent",
            "instance-a",
            "request-a",
            2_000,
            0.25,
        );
        record.cost_confidence = RelayCostConfidence::Unknown;

        let error = sum_settled_upstream_cost(&[record])
            .expect_err("unknown cost confidence must not become a settled cost");

        assert!(error.to_string().contains("upstream cost is unknown"));
    }

    #[tokio::test]
    async fn reconciliation_refuses_an_empty_ledger_instead_of_assuming_zero_cost() {
        let error = test_service(test_profit_store().await, "instance-a")
            .get_period_upstream_cost(PERIOD_START, PERIOD_END)
            .await
            .expect_err("an empty ledger cannot prove zero upstream cost");

        assert!(error
            .to_string()
            .contains("no settled upstream cost records"));
    }

    #[tokio::test]
    async fn reconciliation_costs_are_scoped_to_the_configured_aether_instance() {
        let store = test_profit_store().await;
        store
            .append(&settled_record(
                "profit-a",
                "instance-a",
                "request-a",
                2_000,
                0.25,
            ))
            .await
            .expect("first instance profit should persist");
        store
            .append(&settled_record(
                "profit-b",
                "instance-b",
                "request-b",
                2_000,
                9.0,
            ))
            .await
            .expect("second instance profit should persist");

        let cost = test_service(store, "instance-a")
            .get_period_upstream_cost(PERIOD_START, PERIOD_END)
            .await
            .expect("only matching-instance costs should reconcile");

        assert!((cost - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn reconciliation_uses_the_reported_quota_per_unit_for_downstream_revenue() {
        let revenue = downstream_revenue_usd(&DownstreamStats {
            total_revenue_quota: 1_250.0,
            total_tokens: 0,
            total_requests: 0,
            quota_per_unit: Some("250000".to_string()),
        })
        .expect("a reported positive quota rate should produce a settlement revenue");

        assert!(
            (revenue - 0.005).abs() < f64::EPSILON,
            "the settlement must use the reported quota_per_unit, not a fixed 500000 rate"
        );
    }

    #[test]
    fn reconciliation_refuses_to_fabricate_revenue_without_quota_per_unit() {
        let error = downstream_revenue_usd(&DownstreamStats {
            total_revenue_quota: 1_250.0,
            total_tokens: 0,
            total_requests: 0,
            quota_per_unit: None,
        })
        .expect_err("an absent quota rate makes downstream revenue unknown");

        assert!(error
            .to_string()
            .contains("quota_per_unit is unavailable; revenue is unknown"));
    }
}
