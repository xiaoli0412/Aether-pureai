//! 利润记录写入
//!
//! 异步将每笔请求的利润数据写入数据库。支持写入失败时暂存到 RuntimeState 并重试。

use std::sync::Arc;

use aether_data::repository::relay_profit::{PersistedRelayProfitRecord, RelayCostConfidence};
use aether_data_contracts::repository::usage::StoredRequestUsageAudit;
use aether_relay_core::pricing::try_quota_to_usd;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, error, info};
use uuid::Uuid;

use super::engine::RelayEngineConfig;
use super::error::RelayError;
use crate::data::GatewayDataState;

// Keep derived ledger amounts stable when they cross the f64/SQL/JSON boundary.
// Twelve places preserves the existing micro-USD metric precision with headroom
// for sub-micro usage pricing, while preventing binary representation tails from
// becoming persisted or exported as financial facts.
const COMPUTED_MONEY_DECIMAL_SCALE: f64 = 1_000_000_000_000.0;

/// A verified New API `usage_settled` fact, scoped to the configured instance.
/// Monetary fields remain decimal strings until the ledger persists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewApiUsageSettlement {
    pub instance_id: String,
    pub request_id: String,
    pub channel_id: String,
    pub model_id: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub charged_quota: String,
    pub quota_per_unit: Option<String>,
    pub occurred_at_unix_ms: i64,
}

/// The only outcomes that an inbox worker may use to decide whether it can
/// advance a durable relay-event marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProfitPersistenceOutcome {
    /// A settled AETHER usage record was appended (or its idempotent duplicate exists).
    Recorded,
    /// AETHER usage is not settled yet, so the inbox event must be retried later.
    Pending,
}

pub(crate) fn build_persisted_profit_record(
    id: String,
    settlement: &NewApiUsageSettlement,
    upstream_usage: Option<&StoredRequestUsageAudit>,
    payment_fee_rate: f64,
) -> Result<PersistedRelayProfitRecord, RelayError> {
    if !payment_fee_rate.is_finite() || !(0.0..=1.0).contains(&payment_fee_rate) {
        return Err(RelayError::InvalidConfig(
            "relay payment fee rate must be finite and between zero and one".to_string(),
        ));
    }

    let charged_quota = parse_decimal(&settlement.charged_quota, "charged_quota")?;
    let quota_per_unit = settlement
        .quota_per_unit
        .as_deref()
        .map(|value| parse_decimal(value, "quota_per_unit"))
        .transpose()?;
    let downstream_revenue_usd = quota_per_unit
        .filter(|quota_per_unit| *quota_per_unit > 0.0)
        .and_then(|quota_per_unit| try_quota_to_usd(charged_quota, quota_per_unit))
        .map(canonicalize_computed_money);
    let upstream_cost_usd = settled_actual_cost(upstream_usage)?;
    let payment_fee_usd = downstream_revenue_usd
        .map(|revenue| canonicalize_computed_money(revenue * payment_fee_rate));
    let (net_profit_usd, margin_percent) = match (upstream_cost_usd, downstream_revenue_usd) {
        (Some(cost), Some(revenue)) => {
            let fee = payment_fee_usd.expect("revenue always has a payment fee");
            let profit = canonicalize_computed_money(revenue - cost - fee);
            let margin = if revenue > 0.0 {
                canonicalize_computed_money((profit / revenue) * 100.0)
            } else {
                0.0
            };
            (Some(profit), Some(margin))
        }
        _ => (None, None),
    };

    Ok(PersistedRelayProfitRecord {
        id,
        instance_id: settlement.instance_id.clone(),
        request_id: settlement.request_id.clone(),
        channel_id: settlement.channel_id.clone(),
        model_id: settlement.model_id.clone(),
        prompt_tokens: i64::try_from(settlement.prompt_tokens).map_err(|_| {
            RelayError::Internal("relay prompt token count exceeds database range".to_string())
        })?,
        completion_tokens: i64::try_from(settlement.completion_tokens).map_err(|_| {
            RelayError::Internal("relay completion token count exceeds database range".to_string())
        })?,
        charged_quota: settlement.charged_quota.clone(),
        quota_per_unit: settlement.quota_per_unit.clone(),
        upstream_cost_usd,
        downstream_revenue_usd,
        payment_fee_usd,
        net_profit_usd,
        margin_percent,
        cost_confidence: if upstream_cost_usd.is_some() {
            RelayCostConfidence::Known
        } else {
            RelayCostConfidence::Unknown
        },
        occurred_at_unix_ms: settlement.occurred_at_unix_ms,
    })
}

fn settled_actual_cost(
    upstream_usage: Option<&StoredRequestUsageAudit>,
) -> Result<Option<f64>, RelayError> {
    let Some(usage) = upstream_usage else {
        return Ok(None);
    };
    if usage.status != "completed" || usage.billing_status != "settled" {
        return Ok(None);
    }
    if !has_known_upstream_cost_snapshot(usage) {
        return Ok(None);
    }
    if !usage.actual_total_cost_usd.is_finite() || usage.actual_total_cost_usd < 0.0 {
        return Err(RelayError::Internal(
            "settled usage contains an invalid actual upstream cost".to_string(),
        ));
    }
    Ok(Some(usage.actual_total_cost_usd))
}

/// The persisted numeric field uses zero for unresolved billing states, so a
/// cost is settled only when its immutable pricing snapshot has a known status.
fn has_known_upstream_cost_snapshot(usage: &StoredRequestUsageAudit) -> bool {
    is_known_upstream_cost_status(usage.settlement_billing_snapshot_status())
        || ["settlement_snapshot", "billing_snapshot"]
            .into_iter()
            .any(|snapshot_key| {
                is_known_upstream_cost_status(
                    usage
                        .request_metadata
                        .as_ref()
                        .and_then(|metadata| metadata.get(snapshot_key))
                        .and_then(|snapshot| snapshot.get("status"))
                        .and_then(|status| status.as_str()),
                )
            })
}

fn is_known_upstream_cost_status(status: Option<&str>) -> bool {
    matches!(status, Some("complete" | "resolved"))
}

fn parse_decimal(value: &str, field: &str) -> Result<f64, RelayError> {
    if !is_base10_decimal(value) {
        return Err(RelayError::Internal(format!(
            "New API usage_settled {field} is not a non-negative base-10 decimal"
        )));
    }
    let parsed = value.parse::<f64>().map_err(|_| {
        RelayError::Internal(format!(
            "New API usage_settled {field} cannot be represented"
        ))
    })?;
    if !parsed.is_finite() {
        return Err(RelayError::Internal(format!(
            "New API usage_settled {field} cannot be represented"
        )));
    }
    Ok(parsed)
}

fn canonicalize_computed_money(value: f64) -> f64 {
    if !value.is_finite() || value.abs() > f64::MAX / COMPUTED_MONEY_DECIMAL_SCALE {
        return value;
    }

    let canonical = (value * COMPUTED_MONEY_DECIMAL_SCALE).round() / COMPUTED_MONEY_DECIMAL_SCALE;
    if canonical == 0.0 {
        0.0
    } else {
        canonical
    }
}

pub(crate) fn is_base10_decimal(value: &str) -> bool {
    if value.is_empty() || value.trim() != value {
        return false;
    }
    let mut parts = value.split('.');
    let Some(whole) = parts.next() else {
        return false;
    };
    let fraction = parts.next();
    let whole_is_valid = whole == "0"
        || whole.as_bytes().split_first().is_some_and(|(first, rest)| {
            matches!(first, b'1'..=b'9') && rest.iter().all(|byte| byte.is_ascii_digit())
        });
    parts.next().is_none()
        && whole_is_valid
        && !fraction
            .is_some_and(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Queues settlement facts for the SQL-backed profit worker.
#[derive(Clone)]
pub struct ProfitLedgerWriter {
    sender: mpsc::Sender<ProfitLedgerWork>,
}

pub(crate) struct ProfitLedgerWork {
    settlement: NewApiUsageSettlement,
    completion: oneshot::Sender<Result<ProfitPersistenceOutcome, RelayError>>,
}

impl ProfitLedgerWork {
    pub(crate) fn settlement(&self) -> &NewApiUsageSettlement {
        &self.settlement
    }

    pub(crate) fn complete(self, result: Result<ProfitPersistenceOutcome, RelayError>) {
        let _ = self.completion.send(result);
    }
}

impl ProfitLedgerWriter {
    pub fn new(_config: Arc<RelayEngineConfig>) -> (Self, mpsc::Receiver<ProfitLedgerWork>) {
        let (sender, receiver) = mpsc::channel(4096);
        let writer = Self { sender };
        (writer, receiver)
    }

    pub async fn record_new_api_usage_settlement(
        &self,
        settlement: NewApiUsageSettlement,
    ) -> Result<ProfitPersistenceOutcome, RelayError> {
        let (completion, completed) = oneshot::channel();
        self.sender
            .send(ProfitLedgerWork {
                settlement,
                completion,
            })
            .await
            .map_err(|_| {
                RelayError::Internal("relay profit database worker is unavailable".to_string())
            })?;
        completed.await.map_err(|_| {
            RelayError::Internal("relay profit database worker stopped before append".to_string())
        })?
    }
}

/// Consumes New API settlement facts and persists a SQL-backed profit record.
pub async fn spawn_profit_writer_task(
    mut receiver: mpsc::Receiver<ProfitLedgerWork>,
    store: aether_data::repository::relay_profit::RelayProfitLedgerStore,
    data: Arc<GatewayDataState>,
    payment_fee_rate: f64,
    mut shutdown: watch::Receiver<()>,
) {
    info!("profit ledger writer task started");

    loop {
        tokio::select! {
            Some(work) = receiver.recv() => {
                let request_id = work.settlement().request_id.clone();
                let result = persist_profit_record(&store, data.as_ref(), work.settlement(), payment_fee_rate).await;
                if let Err(error) = &result {
                    error!(request_id = %request_id, error = %error, "failed to persist relay profit record");
                }
                work.complete(result);
            }
            _ = shutdown.changed() => {
                info!("profit ledger writer task shutting down");
                while let Ok(work) = receiver.try_recv() {
                    let request_id = work.settlement().request_id.clone();
                    let result = persist_profit_record(&store, data.as_ref(), work.settlement(), payment_fee_rate).await;
                    if let Err(error) = &result {
                        error!(request_id = %request_id, error = %error, "failed to drain relay profit record");
                    }
                    work.complete(result);
                }
                break;
            }
        }
    }
}

async fn persist_profit_record(
    store: &aether_data::repository::relay_profit::RelayProfitLedgerStore,
    data: &GatewayDataState,
    settlement: &NewApiUsageSettlement,
    payment_fee_rate: f64,
) -> Result<ProfitPersistenceOutcome, RelayError> {
    let upstream_usage = data
        .find_request_usage_by_request_id(&settlement.request_id)
        .await
        .map_err(|error| RelayError::Internal(format!("read settled AETHER usage: {error}")))?;
    let Some(upstream_usage) = upstream_usage else {
        return Ok(ProfitPersistenceOutcome::Pending);
    };
    if upstream_usage.status != "completed" || upstream_usage.billing_status != "settled" {
        return Ok(ProfitPersistenceOutcome::Pending);
    }
    let record = build_persisted_profit_record(
        Uuid::new_v4().to_string(),
        settlement,
        Some(&upstream_usage),
        payment_fee_rate,
    )?;
    let inserted = store
        .append(&record)
        .await
        .map_err(|error| RelayError::Internal(format!("append relay profit ledger: {error}")))?;
    if inserted {
        debug!(
            request_id = %record.request_id,
            cost_confidence = ?record.cost_confidence,
            "relay profit record persisted"
        );
    }
    Ok(ProfitPersistenceOutcome::Recorded)
}

#[cfg(test)]
mod tests {
    use super::{build_persisted_profit_record, NewApiUsageSettlement, ProfitPersistenceOutcome};
    use aether_data::repository::relay_profit::RelayCostConfidence;
    use aether_data_contracts::repository::usage::StoredRequestUsageAudit;
    use std::time::Duration;
    use tokio::time::timeout;

    fn settled_usage(actual_total_cost_usd: f64) -> StoredRequestUsageAudit {
        let mut usage = StoredRequestUsageAudit::new(
            "usage-1".to_string(),
            "req-1".to_string(),
            None,
            None,
            None,
            None,
            "AETHER".to_string(),
            "gpt-5".to_string(),
            None,
            Some("provider-1".to_string()),
            None,
            None,
            Some("chat".to_string()),
            Some("openai:chat".to_string()),
            None,
            None,
            Some("openai:chat".to_string()),
            None,
            None,
            false,
            false,
            120,
            30,
            150,
            0.9,
            actual_total_cost_usd,
            Some(200),
            None,
            None,
            None,
            None,
            "completed".to_string(),
            "settled".to_string(),
            1_784_073_600_000,
            1_784_073_601,
            Some(1_784_073_601),
        )
        .expect("test usage should build");
        usage.request_metadata = Some(serde_json::json!({
            "billing_snapshot": { "status": "complete" }
        }));
        usage
    }

    fn settlement(quota_per_unit: Option<&str>) -> NewApiUsageSettlement {
        NewApiUsageSettlement {
            instance_id: "aether-primary".to_string(),
            request_id: "req-1".to_string(),
            channel_id: "41".to_string(),
            model_id: "gpt-5".to_string(),
            prompt_tokens: 120,
            completion_tokens: 30,
            charged_quota: "1250".to_string(),
            quota_per_unit: quota_per_unit.map(ToOwned::to_owned),
            occurred_at_unix_ms: 1_784_073_600_000,
        }
    }

    #[test]
    fn profit_record_uses_settled_actual_cost_and_dynamic_downstream_revenue() {
        let record = build_persisted_profit_record(
            "profit-1".to_string(),
            &settlement(Some("500000")),
            Some(&settled_usage(0.001)),
            0.006,
        )
        .expect("bilateral settlement facts should build a profit record");

        assert_eq!(record.cost_confidence, RelayCostConfidence::Known);
        assert_eq!(record.upstream_cost_usd, Some(0.001));
        assert_eq!(record.downstream_revenue_usd, Some(0.0025));
        assert_eq!(record.payment_fee_usd, Some(0.000015));
        assert_eq!(record.net_profit_usd, Some(0.001485));
        assert_eq!(record.charged_quota, "1250");
    }

    #[test]
    fn profit_record_does_not_fabricate_revenue_or_profit_when_quota_rate_is_unknown() {
        let record = build_persisted_profit_record(
            "profit-unknown-revenue".to_string(),
            &settlement(None),
            Some(&settled_usage(0.001)),
            0.006,
        )
        .expect("known upstream cost with unknown revenue should be representable");

        assert_eq!(record.cost_confidence, RelayCostConfidence::Known);
        assert_eq!(record.upstream_cost_usd, Some(0.001));
        assert_eq!(record.downstream_revenue_usd, None);
        assert_eq!(record.payment_fee_usd, None);
        assert_eq!(record.net_profit_usd, None);
        assert_eq!(record.margin_percent, None);
    }

    #[test]
    fn profit_record_does_not_treat_unpriced_zero_cost_as_known_or_profitable() {
        let mut unpriced_usage = settled_usage(0.0);
        unpriced_usage.request_metadata = Some(serde_json::json!({
            "billing_snapshot": { "status": "no_rule" }
        }));

        let record = build_persisted_profit_record(
            "profit-unpriced-zero".to_string(),
            &settlement(Some("500000")),
            Some(&unpriced_usage),
            0.006,
        )
        .expect("an unknown upstream cost should remain representable");

        assert_eq!(record.cost_confidence, RelayCostConfidence::Unknown);
        assert_eq!(record.upstream_cost_usd, None);
        assert_eq!(record.net_profit_usd, None);
        assert_eq!(record.margin_percent, None);
    }

    #[test]
    fn profit_record_preserves_complete_zero_cost_as_known() {
        let record = build_persisted_profit_record(
            "profit-resolved-zero".to_string(),
            &settlement(Some("500000")),
            Some(&settled_usage(0.0)),
            0.006,
        )
        .expect("a complete pricing snapshot may prove a zero upstream cost");

        assert_eq!(record.cost_confidence, RelayCostConfidence::Known);
        assert_eq!(record.upstream_cost_usd, Some(0.0));
        assert_eq!(record.net_profit_usd, Some(0.002485));
    }

    #[test]
    fn profit_record_accepts_resolved_typed_billing_status_as_known_cost() {
        let mut resolved_usage = settled_usage(0.001);
        resolved_usage.request_metadata = Some(serde_json::json!({
            "billing_snapshot_schema_version": "v2",
            "billing_snapshot_status": "resolved"
        }));

        let record = build_persisted_profit_record(
            "profit-typed-resolved".to_string(),
            &settlement(Some("500000")),
            Some(&resolved_usage),
            0.006,
        )
        .expect("a resolved typed billing snapshot should establish upstream cost");

        assert_eq!(record.cost_confidence, RelayCostConfidence::Known);
        assert_eq!(record.upstream_cost_usd, Some(0.001));
        assert_eq!(record.net_profit_usd, Some(0.001485));
    }

    #[tokio::test]
    async fn profit_writer_queues_new_api_settlement_facts_for_the_database_worker() {
        let (writer, mut receiver) = super::ProfitLedgerWriter::new(std::sync::Arc::new(
            super::RelayEngineConfig::default(),
        ));
        let expected = settlement(Some("500000"));

        let record = tokio::spawn({
            let writer = writer.clone();
            let expected = expected.clone();
            async move { writer.record_new_api_usage_settlement(expected).await }
        });

        let work = receiver
            .recv()
            .await
            .expect("the database worker must receive the settlement work");

        assert_eq!(
            work.settlement(),
            &expected,
            "the database worker must receive the exact New API fact"
        );
        work.complete(Ok(ProfitPersistenceOutcome::Recorded));
        assert_eq!(
            record
                .await
                .expect("writer task should join")
                .expect("worker acknowledgement should succeed"),
            ProfitPersistenceOutcome::Recorded
        );
    }

    #[tokio::test]
    async fn profit_writer_does_not_acknowledge_until_the_ledger_worker_reports_an_append_result() {
        let (writer, _receiver) = super::ProfitLedgerWriter::new(std::sync::Arc::new(
            super::RelayEngineConfig::default(),
        ));

        let acknowledgement = timeout(
            Duration::from_millis(20),
            writer.record_new_api_usage_settlement(settlement(Some("500000"))),
        )
        .await;

        assert!(
            acknowledgement.is_err(),
            "the inbox marker cannot become recorded merely because a settlement entered an in-memory queue"
        );
    }
}
