//! 高级下游分组管理
//!
//! 支持多级分组、分组继承、时间段定价、模型级倍率覆盖、
//! 并发限制和成本上限。比 New API 的单级分组更灵活。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use aether_data::repository::relay_groups::{
    PersistedRelayDownstreamGroup, RelayDownstreamGroupStore,
};
use aether_runtime_state::RuntimeState;
use chrono::{DateTime, Datelike, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use uuid::Uuid;

use super::engine::RelayEngineConfig;
use super::error::RelayError;

/// 下游分组定义
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownstreamGroup {
    pub id: String,
    /// 分组名称（唯一）
    pub name: String,
    /// 描述
    pub description: Option<String>,
    /// 父分组 ID（用于继承）
    pub parent_id: Option<String>,
    /// 全局倍率乘数（应用于该分组所有模型）
    pub global_ratio_multiplier: f64,
    /// 模型白名单（为空表示允许所有）
    pub model_whitelist: Vec<String>,
    /// 模型黑名单
    pub model_blacklist: Vec<String>,
    /// 模型级倍率覆盖：model_id -> ratio_multiplier
    pub model_ratio_overrides: HashMap<String, f64>,
    /// 优先级（数字越大优先级越高）
    pub priority: i32,
    /// 每分钟请求限制（0=不限）
    pub requests_per_minute: u32,
    /// 每日请求限制（0=不限）
    pub requests_per_day: u32,
    /// 每日 quota 上限（0.0=不限）
    pub daily_quota_limit: f64,
    /// 每月 quota 上限（0.0=不限）
    pub monthly_quota_limit: f64,
    /// 时间段定价规则
    pub time_rules: Vec<TimeBasedPricingRule>,
    /// 是否启用
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// 时间段定价规则
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeBasedPricingRule {
    /// 开始小时（0-23，本地时间）
    pub start_hour: u8,
    /// 结束小时（0-23，本地时间）
    pub end_hour: u8,
    /// 时区（如 "Asia/Shanghai"）
    pub timezone: String,
    /// 该时段的倍率乘数
    pub ratio_multiplier: f64,
    /// 适用的星期几（1=周一..7=周日，空=每天）
    pub weekdays: Vec<u8>,
}

/// 创建分组输入
#[derive(Debug, Clone, Deserialize)]
pub struct CreateGroupInput {
    pub name: String,
    pub description: Option<String>,
    pub parent_id: Option<String>,
    pub global_ratio_multiplier: Option<f64>,
    pub model_whitelist: Option<Vec<String>>,
    pub model_blacklist: Option<Vec<String>>,
    pub model_ratio_overrides: Option<HashMap<String, f64>>,
    pub priority: Option<i32>,
    pub requests_per_minute: Option<u32>,
    pub requests_per_day: Option<u32>,
    pub daily_quota_limit: Option<f64>,
    pub monthly_quota_limit: Option<f64>,
    pub time_rules: Option<Vec<TimeBasedPricingRule>>,
}

/// 更新分组输入
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateGroupInput {
    pub name: Option<String>,
    pub description: Option<String>,
    pub parent_id: Option<String>,
    pub global_ratio_multiplier: Option<f64>,
    pub model_whitelist: Option<Vec<String>>,
    pub model_blacklist: Option<Vec<String>>,
    pub model_ratio_overrides: Option<HashMap<String, f64>>,
    pub priority: Option<i32>,
    pub requests_per_minute: Option<u32>,
    pub requests_per_day: Option<u32>,
    pub daily_quota_limit: Option<f64>,
    pub monthly_quota_limit: Option<f64>,
    pub time_rules: Option<Vec<TimeBasedPricingRule>>,
    pub enabled: Option<bool>,
}

/// 分组的有效倍率（解析继承和时间段后的最终值）
#[derive(Debug, Clone, Serialize)]
pub struct EffectiveGroupRatio {
    pub group_id: String,
    pub group_name: String,
    pub model_id: String,
    /// 最终有效倍率乘数
    pub effective_multiplier: f64,
    /// 倍率来源说明
    pub source: String,
}

/// The fully resolved policy for a root-to-leaf group lineage. Runtime pricing
/// and the database-backed export both use this representation so inherited
/// access control cannot drift between the two paths.
#[derive(Debug, Clone)]
struct ResolvedGroupLineage {
    groups: Vec<DownstreamGroup>,
    model_whitelist: Option<Vec<String>>,
    model_blacklist: Vec<String>,
    enabled: bool,
}

impl ResolvedGroupLineage {
    fn from_groups(groups: Vec<DownstreamGroup>) -> Self {
        let mut model_whitelist = None;
        let mut model_blacklist = Vec::new();
        let mut enabled = true;

        for group in &groups {
            enabled &= group.enabled;
            model_whitelist =
                intersect_model_whitelists(model_whitelist, &group.model_whitelist);
            model_blacklist.extend(group.model_blacklist.clone());
        }

        Self {
            groups,
            model_whitelist,
            model_blacklist,
            enabled,
        }
    }

    fn allows_model(&self, model_id: &str) -> bool {
        if self
            .model_blacklist
            .iter()
            .any(|model| model_matches(model, model_id))
        {
            return false;
        }

        match &self.model_whitelist {
            None => true,
            Some(whitelist) => whitelist
                .iter()
                .any(|model| model_matches(model, model_id)),
        }
    }

    fn effective_multiplier_at(
        &self,
        model_id: &str,
        now: DateTime<Utc>,
    ) -> Result<f64, RelayError> {
        if !self.enabled {
            return Err(RelayError::InvalidConfig("group is disabled".into()));
        }
        if !self.allows_model(model_id) {
            return Err(RelayError::InvalidConfig(format!(
                "model {model_id} is not allowed by the group lineage"
            )));
        }

        let mut multiplier = 1.0;
        for group in &self.groups {
            let group_multiplier = group
                .model_ratio_overrides
                .get(model_id)
                .copied()
                .unwrap_or(group.global_ratio_multiplier);
            if !group_multiplier.is_finite() {
                return Err(RelayError::InvalidConfig(format!(
                    "group {} has a non-finite pricing multiplier",
                    group.id
                )));
            }
            multiplier *= group_multiplier;
            multiplier *= active_time_multiplier(&group.time_rules, now)?;
            if !multiplier.is_finite() {
                return Err(RelayError::InvalidConfig(format!(
                    "group {} produces a non-finite inherited pricing multiplier",
                    group.id
                )));
            }
        }

        Ok(multiplier)
    }

    fn flat_export_ratio(&self, group_id: &str) -> Result<f64, RelayError> {
        let mut ratio = 1.0;
        for group in &self.groups {
            if !group.model_ratio_overrides.is_empty() {
                return Err(RelayError::InvalidConfig(format!(
                    "group {} has model ratio overrides that cannot be represented by the flat pricing export",
                    group.id
                )));
            }
            if !group.time_rules.is_empty() {
                return Err(RelayError::InvalidConfig(format!(
                    "group {} has time-based pricing rules that cannot be represented by the flat pricing export",
                    group.id
                )));
            }
            ratio *= group.global_ratio_multiplier;
            if !ratio.is_finite() {
                return Err(RelayError::InvalidConfig(format!(
                    "group {group_id} produces a non-finite inherited pricing multiplier"
                )));
            }
        }
        Ok(ratio)
    }
}

/// 下游分组管理器
#[derive(Clone)]
pub struct DownstreamGroupManager {
    pub(crate) config: Arc<RelayEngineConfig>,
    pub(crate) runtime_state: RuntimeState,
    group_store: Option<RelayDownstreamGroupStore>,
}

impl DownstreamGroupManager {
    pub fn new(config: Arc<RelayEngineConfig>, runtime_state: RuntimeState) -> Self {
        Self {
            config,
            runtime_state,
            group_store: None,
        }
    }

    pub fn with_store(
        config: Arc<RelayEngineConfig>,
        runtime_state: RuntimeState,
        group_store: RelayDownstreamGroupStore,
    ) -> Self {
        Self {
            config,
            runtime_state,
            group_store: Some(group_store),
        }
    }

    // ========== CRUD ==========

    /// 创建分组
    pub async fn create_group(
        &self,
        input: CreateGroupInput,
    ) -> Result<DownstreamGroup, RelayError> {
        // Validate name uniqueness
        if self.find_group_by_name(&input.name).await?.is_some() {
            return Err(RelayError::InvalidConfig(format!(
                "group name '{}' already exists",
                input.name
            )));
        }

        // Validate parent exists if specified
        if let Some(ref parent_id) = input.parent_id {
            if self.get_group(parent_id).await?.is_none() {
                return Err(RelayError::NotFound(format!("parent group {}", parent_id)));
            }
        }

        let group = DownstreamGroup {
            id: Uuid::new_v4().to_string(),
            name: input.name,
            description: input.description,
            parent_id: input.parent_id,
            global_ratio_multiplier: input.global_ratio_multiplier.unwrap_or(1.0),
            model_whitelist: input.model_whitelist.unwrap_or_default(),
            model_blacklist: input.model_blacklist.unwrap_or_default(),
            model_ratio_overrides: input.model_ratio_overrides.unwrap_or_default(),
            priority: input.priority.unwrap_or(0),
            requests_per_minute: input.requests_per_minute.unwrap_or(0),
            requests_per_day: input.requests_per_day.unwrap_or(0),
            daily_quota_limit: input.daily_quota_limit.unwrap_or(0.0),
            monthly_quota_limit: input.monthly_quota_limit.unwrap_or(0.0),
            time_rules: input.time_rules.unwrap_or_default(),
            enabled: true,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        };

        self.insert_group(&group).await?;
        info!(group_id = %group.id, name = %group.name, "downstream group created");
        Ok(group)
    }

    /// 获取分组
    pub async fn get_group(&self, group_id: &str) -> Result<Option<DownstreamGroup>, RelayError> {
        let persisted = self
            .group_store()?
            .get(group_id)
            .await
            .map_err(|error| RelayError::Internal(format!("load downstream group: {error}")))?;
        match persisted {
            Some(persisted) => {
                let group = DownstreamGroup::try_from(persisted)?;
                self.cache_group(&group).await;
                Ok(Some(group))
            }
            None => Ok(None),
        }
    }

    /// 按名称查找分组
    pub async fn find_group_by_name(
        &self,
        name: &str,
    ) -> Result<Option<DownstreamGroup>, RelayError> {
        let groups = self.list_groups().await?;
        Ok(groups.into_iter().find(|g| g.name == name))
    }

    /// 列出所有分组
    pub async fn list_groups(&self) -> Result<Vec<DownstreamGroup>, RelayError> {
        let persisted =
            self.group_store()?.list().await.map_err(|error| {
                RelayError::Internal(format!("list downstream groups: {error}"))
            })?;
        let groups = persisted
            .into_iter()
            .map(DownstreamGroup::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        for group in &groups {
            self.cache_group(group).await;
        }
        Ok(groups)
    }

    /// 更新分组
    pub async fn update_group(
        &self,
        group_id: &str,
        input: UpdateGroupInput,
    ) -> Result<DownstreamGroup, RelayError> {
        let mut group = self
            .get_group(group_id)
            .await?
            .ok_or_else(|| RelayError::NotFound(format!("group {}", group_id)))?;

        if let Some(v) = input.name {
            if v != group.name && self.find_group_by_name(&v).await?.is_some() {
                return Err(RelayError::InvalidConfig(format!(
                    "group name '{}' already exists",
                    v
                )));
            }
            group.name = v;
        }
        if let Some(v) = input.description {
            group.description = Some(v);
        }
        if let Some(v) = input.parent_id {
            self.validate_parent_assignment(group_id, &v).await?;
            group.parent_id = Some(v);
        }
        if let Some(v) = input.global_ratio_multiplier {
            group.global_ratio_multiplier = v;
        }
        if let Some(v) = input.model_whitelist {
            group.model_whitelist = v;
        }
        if let Some(v) = input.model_blacklist {
            group.model_blacklist = v;
        }
        if let Some(v) = input.model_ratio_overrides {
            group.model_ratio_overrides = v;
        }
        if let Some(v) = input.priority {
            group.priority = v;
        }
        if let Some(v) = input.requests_per_minute {
            group.requests_per_minute = v;
        }
        if let Some(v) = input.requests_per_day {
            group.requests_per_day = v;
        }
        if let Some(v) = input.daily_quota_limit {
            group.daily_quota_limit = v;
        }
        if let Some(v) = input.monthly_quota_limit {
            group.monthly_quota_limit = v;
        }
        if let Some(v) = input.time_rules {
            group.time_rules = v;
        }
        if let Some(v) = input.enabled {
            group.enabled = v;
        }
        group.updated_at = Utc::now().to_rfc3339();

        self.replace_group(&group).await?;
        Ok(group)
    }

    /// 删除分组
    pub async fn delete_group(&self, group_id: &str) -> Result<(), RelayError> {
        let children = self
            .list_groups()
            .await?
            .into_iter()
            .filter(|group| group.parent_id.as_deref() == Some(group_id))
            .map(|group| group.id)
            .collect::<Vec<_>>();
        if !children.is_empty() {
            return Err(RelayError::InvalidConfig(format!(
                "cannot delete group {group_id} while child groups exist: {}",
                children.join(", ")
            )));
        }

        let _ = self
            .group_store()?
            .delete(group_id)
            .await
            .map_err(|error| RelayError::Internal(format!("delete downstream group: {error}")))?;
        self.remove_cached_group(group_id).await;
        info!(group_id = %group_id, "downstream group deleted");
        Ok(())
    }

    // ========== 倍率计算 ==========

    /// 计算某分组某模型的有效倍率（含继承 + 时间段）
    pub async fn get_effective_multiplier(
        &self,
        group_id: &str,
        model_id: &str,
    ) -> Result<f64, RelayError> {
        self.get_effective_multiplier_at(group_id, model_id, Utc::now())
            .await
    }

    async fn get_effective_multiplier_at(
        &self,
        group_id: &str,
        model_id: &str,
        now: DateTime<Utc>,
    ) -> Result<f64, RelayError> {
        let group = self
            .get_group(group_id)
            .await?
            .ok_or_else(|| RelayError::NotFound(format!("group {}", group_id)))?;
        self.resolve_group_lineage(&group)
            .await?
            .effective_multiplier_at(model_id, now)
    }

    /// 为多个分组取最优（最低）倍率（当 key 属于多个分组时）
    pub async fn get_best_multiplier_for_groups(
        &self,
        group_ids: &[String],
        model_id: &str,
    ) -> Result<f64, RelayError> {
        let mut best = f64::MAX;
        for group_id in group_ids {
            match self.get_effective_multiplier(group_id, model_id).await {
                Ok(m) => {
                    if m < best {
                        best = m;
                    }
                }
                Err(_) => continue, // Skip groups that don't allow this model
            }
        }
        if best == f64::MAX {
            return Err(RelayError::InvalidConfig(format!(
                "no group allows model {}",
                model_id
            )));
        }
        Ok(best)
    }

    /// 导出为 New API 兼容的扁平分组格式
    pub async fn export_flat_groups(&self) -> Result<Vec<FlatExportedGroup>, RelayError> {
        let mut exported = Vec::new();
        for group in self.list_groups().await? {
            if let Some(group) = self.flatten_group_for_export(&group).await? {
                exported.push(group);
            }
        }
        Ok(exported)
    }

    // ========== 内部 ==========

    fn group_store(&self) -> Result<&RelayDownstreamGroupStore, RelayError> {
        self.group_store
            .as_ref()
            .ok_or(RelayError::DatabaseBackedGroupExportUnavailable)
    }

    async fn insert_group(&self, group: &DownstreamGroup) -> Result<(), RelayError> {
        let persisted = PersistedRelayDownstreamGroup::try_from(group)?;
        self.group_store()?
            .insert(&persisted)
            .await
            .map_err(|error| RelayError::Internal(format!("persist downstream group: {error}")))?;
        self.cache_group(group).await;
        Ok(())
    }

    async fn replace_group(&self, group: &DownstreamGroup) -> Result<(), RelayError> {
        let persisted = PersistedRelayDownstreamGroup::try_from(group)?;
        let updated = self
            .group_store()?
            .replace(&persisted)
            .await
            .map_err(|error| RelayError::Internal(format!("update downstream group: {error}")))?;
        if !updated {
            return Err(RelayError::NotFound(format!("group {}", group.id)));
        }
        self.cache_group(group).await;
        Ok(())
    }

    async fn cache_group(&self, group: &DownstreamGroup) {
        let key = format!("relay:dgroup:{}", group.id);
        let json = match serde_json::to_string(group) {
            Ok(json) => json,
            Err(error) => {
                warn!(group_id = %group.id, %error, "failed to serialize downstream group cache entry");
                return;
            }
        };
        if let Err(error) = self.runtime_state.kv_set(&key, json, None).await {
            warn!(group_id = %group.id, %error, "failed to refresh downstream group cache entry");
            return;
        }
        if let Err(error) = self
            .runtime_state
            .set_add("relay:dgroup:ids", &group.id)
            .await
        {
            warn!(group_id = %group.id, %error, "failed to refresh downstream group cache index");
        }
    }

    async fn remove_cached_group(&self, group_id: &str) {
        let key = format!("relay:dgroup:{}", group_id);
        if let Err(error) = self.runtime_state.kv_delete(&key).await {
            warn!(%group_id, %error, "failed to remove downstream group cache entry");
        }
        if let Err(error) = self
            .runtime_state
            .set_remove("relay:dgroup:ids", group_id)
            .await
        {
            warn!(%group_id, %error, "failed to remove downstream group cache index");
        }
    }

    async fn flatten_group_for_export(
        &self,
        group: &DownstreamGroup,
    ) -> Result<Option<FlatExportedGroup>, RelayError> {
        let lineage = self.resolve_group_lineage(group).await?;
        if !lineage.enabled {
            return Ok(None);
        }

        let ratio = lineage.flat_export_ratio(&group.id)?;
        let models =
            flatten_models_after_blacklist(lineage.model_whitelist, &lineage.model_blacklist)?;
        Ok(Some(FlatExportedGroup {
            id: group.id.clone(),
            name: group.name.clone(),
            ratio,
            models,
            priority: group.priority,
        }))
    }

    async fn resolve_group_lineage(
        &self,
        group: &DownstreamGroup,
    ) -> Result<ResolvedGroupLineage, RelayError> {
        Ok(ResolvedGroupLineage::from_groups(
            self.group_lineage(group).await?,
        ))
    }

    async fn validate_parent_assignment(
        &self,
        group_id: &str,
        parent_id: &str,
    ) -> Result<(), RelayError> {
        if parent_id == group_id {
            return Err(RelayError::InvalidConfig(format!(
                "group {group_id} cannot be its own parent"
            )));
        }

        let parent = self
            .get_group(parent_id)
            .await?
            .ok_or_else(|| RelayError::NotFound(format!("parent group {parent_id}")))?;
        let parent_lineage = self.group_lineage(&parent).await?;
        if parent_lineage.iter().any(|group| group.id == group_id) {
            return Err(RelayError::InvalidConfig(format!(
                "setting parent {parent_id} for group {group_id} would create an inheritance cycle"
            )));
        }
        Ok(())
    }

    async fn group_lineage(
        &self,
        group: &DownstreamGroup,
    ) -> Result<Vec<DownstreamGroup>, RelayError> {
        let mut lineage = vec![group.clone()];
        let mut visited = HashSet::from([group.id.clone()]);
        let mut parent_id = group.parent_id.clone();

        while let Some(id) = parent_id {
            if !visited.insert(id.clone()) {
                return Err(RelayError::InvalidConfig(format!(
                    "group inheritance cycle includes {}",
                    group.id
                )));
            }
            let parent = self
                .get_group(&id)
                .await?
                .ok_or_else(|| RelayError::NotFound(format!("parent group {id}")))?;
            parent_id = parent.parent_id.clone();
            lineage.push(parent);
        }

        lineage.reverse();
        Ok(lineage)
    }
}

fn model_matches(rule_model_id: &str, model_id: &str) -> bool {
    rule_model_id == model_id || model_id.starts_with(rule_model_id)
}

fn active_time_multiplier(
    rules: &[TimeBasedPricingRule],
    now: DateTime<Utc>,
) -> Result<f64, RelayError> {
    for rule in rules {
        if rule.start_hour > 23 || rule.end_hour > 23 {
            return Err(RelayError::InvalidConfig(format!(
                "time rule for timezone {} has an hour outside 0..=23",
                rule.timezone
            )));
        }
        if let Some(weekday) = rule
            .weekdays
            .iter()
            .find(|weekday| !(1..=7).contains(*weekday))
        {
            return Err(RelayError::InvalidConfig(format!(
                "time rule for timezone {} has invalid weekday {weekday}",
                rule.timezone
            )));
        }
        if !rule.ratio_multiplier.is_finite() {
            return Err(RelayError::InvalidConfig(format!(
                "time rule for timezone {} has a non-finite pricing multiplier",
                rule.timezone
            )));
        }

        let timezone = rule.timezone.parse::<Tz>().map_err(|_| {
            RelayError::InvalidConfig(format!(
                "time rule has unsupported timezone {}",
                rule.timezone
            ))
        })?;
        let local_now = now.with_timezone(&timezone);
        let current_hour = local_now.hour() as u8;
        let current_weekday = local_now.weekday().num_days_from_monday() as u8 + 1;
        if !rule.weekdays.is_empty() && !rule.weekdays.contains(&current_weekday) {
            continue;
        }

        // Supports overnight spans such as 22:00-06:00 in the rule's declared timezone.
        let in_range = if rule.start_hour <= rule.end_hour {
            current_hour >= rule.start_hour && current_hour < rule.end_hour
        } else {
            current_hour >= rule.start_hour || current_hour < rule.end_hour
        };
        if in_range {
            return Ok(rule.ratio_multiplier);
        }
    }
    Ok(1.0)
}

fn intersect_model_whitelists(
    inherited: Option<Vec<String>>,
    current: &[String],
) -> Option<Vec<String>> {
    if current.is_empty() {
        return inherited;
    }
    let Some(inherited) = inherited else {
        return Some(
            current
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        );
    };

    Some(
        inherited
            .iter()
            .flat_map(|parent_model| {
                current.iter().filter_map(move |child_model| {
                    if parent_model.starts_with(child_model) {
                        Some(parent_model.clone())
                    } else if child_model.starts_with(parent_model) {
                        Some(child_model.clone())
                    } else {
                        None
                    }
                })
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    )
}

fn flatten_models_after_blacklist(
    model_whitelist: Option<Vec<String>>,
    model_blacklist: &[String],
) -> Result<Option<Vec<String>>, RelayError> {
    let Some(model_whitelist) = model_whitelist else {
        if model_blacklist.is_empty() {
            return Ok(None);
        }
        return Err(RelayError::InvalidConfig(
            "a flat pricing export cannot represent an allow-all group with a model blacklist"
                .to_string(),
        ));
    };

    let mut allowed = BTreeSet::new();
    for model in model_whitelist {
        let mut fully_blocked = false;
        for blocked in model_blacklist {
            if model.starts_with(blocked) {
                fully_blocked = true;
                break;
            }
            if blocked.starts_with(&model) {
                return Err(RelayError::InvalidConfig(format!(
                    "a flat pricing export cannot represent blacklist entry {blocked} within allowlist entry {model}"
                )));
            }
        }
        if !fully_blocked {
            allowed.insert(model);
        }
    }
    if allowed.is_empty() {
        return Err(RelayError::InvalidConfig(
            "group has no exportable models after inherited allow/deny policy is applied"
                .to_string(),
        ));
    }
    Ok(Some(allowed.into_iter().collect()))
}

impl TryFrom<PersistedRelayDownstreamGroup> for DownstreamGroup {
    type Error = RelayError;

    fn try_from(group: PersistedRelayDownstreamGroup) -> Result<Self, Self::Error> {
        let model_whitelist =
            serde_json::from_str(&group.model_whitelist_json).map_err(|error| {
                RelayError::Internal(format!("parse persisted group model whitelist: {error}"))
            })?;
        let model_blacklist =
            serde_json::from_str(&group.model_blacklist_json).map_err(|error| {
                RelayError::Internal(format!("parse persisted group model blacklist: {error}"))
            })?;
        let model_ratio_overrides = serde_json::from_str(&group.model_ratio_overrides_json)
            .map_err(|error| {
                RelayError::Internal(format!("parse persisted group model overrides: {error}"))
            })?;
        let time_rules = serde_json::from_str(&group.time_rules_json).map_err(|error| {
            RelayError::Internal(format!("parse persisted group time rules: {error}"))
        })?;
        let requests_per_minute = u32::try_from(group.requests_per_minute).map_err(|_| {
            RelayError::Internal("persisted group requests_per_minute is outside u32 range".into())
        })?;
        let requests_per_day = u32::try_from(group.requests_per_day).map_err(|_| {
            RelayError::Internal("persisted group requests_per_day is outside u32 range".into())
        })?;

        Ok(Self {
            id: group.id,
            name: group.name,
            description: group.description,
            parent_id: group.parent_id,
            global_ratio_multiplier: group.global_ratio_multiplier,
            model_whitelist,
            model_blacklist,
            model_ratio_overrides,
            priority: group.priority,
            requests_per_minute,
            requests_per_day,
            daily_quota_limit: group.daily_quota_limit,
            monthly_quota_limit: group.monthly_quota_limit,
            time_rules,
            enabled: group.enabled,
            created_at: group.created_at,
            updated_at: group.updated_at,
        })
    }
}

impl TryFrom<&DownstreamGroup> for PersistedRelayDownstreamGroup {
    type Error = RelayError;

    fn try_from(group: &DownstreamGroup) -> Result<Self, Self::Error> {
        Ok(Self {
            id: group.id.clone(),
            name: group.name.clone(),
            description: group.description.clone(),
            parent_id: group.parent_id.clone(),
            global_ratio_multiplier: group.global_ratio_multiplier,
            model_whitelist_json: serde_json::to_string(&group.model_whitelist).map_err(
                |error| RelayError::Internal(format!("serialize group model whitelist: {error}")),
            )?,
            model_blacklist_json: serde_json::to_string(&group.model_blacklist).map_err(
                |error| RelayError::Internal(format!("serialize group model blacklist: {error}")),
            )?,
            model_ratio_overrides_json: serde_json::to_string(&group.model_ratio_overrides)
                .map_err(|error| {
                    RelayError::Internal(format!("serialize group model overrides: {error}"))
                })?,
            priority: group.priority,
            requests_per_minute: i64::from(group.requests_per_minute),
            requests_per_day: i64::from(group.requests_per_day),
            daily_quota_limit: group.daily_quota_limit,
            monthly_quota_limit: group.monthly_quota_limit,
            time_rules_json: serde_json::to_string(&group.time_rules).map_err(|error| {
                RelayError::Internal(format!("serialize group time rules: {error}"))
            })?,
            enabled: group.enabled,
            created_at: group.created_at.clone(),
            updated_at: group.updated_at.clone(),
        })
    }
}

/// New API 兼容的扁平分组格式
#[derive(Debug, Clone, Serialize)]
pub struct FlatExportedGroup {
    pub id: String,
    pub name: String,
    /// 有效倍率
    pub ratio: f64,
    /// 允许的模型列表（None = 所有）
    pub models: Option<Vec<String>>,
    pub priority: i32,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aether_data::repository::relay_groups::RelayDownstreamGroupStore;
    use aether_runtime_state::{MemoryRuntimeStateConfig, RuntimeState};
    use sqlx::sqlite::SqlitePoolOptions;

    use super::super::engine::RelayEngineConfig;
    use super::{
        CreateGroupInput, DownstreamGroup, DownstreamGroupManager, RelayError,
        TimeBasedPricingRule, UpdateGroupInput,
    };

    async fn group_store_pool() -> sqlx::SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite group test pool should connect");
        sqlx::query(
            "CREATE TABLE relay_downstream_groups (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                description TEXT,
                parent_id TEXT,
                global_ratio_multiplier REAL NOT NULL,
                model_whitelist_json TEXT NOT NULL,
                model_blacklist_json TEXT NOT NULL,
                model_ratio_overrides_json TEXT NOT NULL,
                priority INTEGER NOT NULL,
                requests_per_minute INTEGER NOT NULL,
                requests_per_day INTEGER NOT NULL,
                daily_quota_limit REAL NOT NULL,
                monthly_quota_limit REAL NOT NULL,
                time_rules_json TEXT NOT NULL,
                enabled INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .expect("downstream group test table should be created");
        pool
    }

    fn group_input(name: &str, parent_id: Option<String>) -> CreateGroupInput {
        CreateGroupInput {
            name: name.to_string(),
            description: None,
            parent_id,
            global_ratio_multiplier: None,
            model_whitelist: None,
            model_blacklist: None,
            model_ratio_overrides: None,
            priority: None,
            requests_per_minute: None,
            requests_per_day: None,
            daily_quota_limit: None,
            monthly_quota_limit: None,
            time_rules: None,
        }
    }

    fn parent_update(parent_id: String) -> UpdateGroupInput {
        UpdateGroupInput {
            name: None,
            description: None,
            parent_id: Some(parent_id),
            global_ratio_multiplier: None,
            model_whitelist: None,
            model_blacklist: None,
            model_ratio_overrides: None,
            priority: None,
            requests_per_minute: None,
            requests_per_day: None,
            daily_quota_limit: None,
            monthly_quota_limit: None,
            time_rules: None,
            enabled: None,
        }
    }

    fn enabled_update(enabled: bool) -> UpdateGroupInput {
        UpdateGroupInput {
            name: None,
            description: None,
            parent_id: None,
            global_ratio_multiplier: None,
            model_whitelist: None,
            model_blacklist: None,
            model_ratio_overrides: None,
            priority: None,
            requests_per_minute: None,
            requests_per_day: None,
            daily_quota_limit: None,
            monthly_quota_limit: None,
            time_rules: None,
            enabled: Some(enabled),
        }
    }

    async fn group_manager() -> DownstreamGroupManager {
        let pool = group_store_pool().await;
        DownstreamGroupManager::with_store(
            Arc::new(RelayEngineConfig::default()),
            RuntimeState::memory(MemoryRuntimeStateConfig::default()),
            RelayDownstreamGroupStore::sqlite(pool),
        )
    }

    #[tokio::test]
    async fn group_create_rejects_a_missing_parent_without_persisting_an_orphan() {
        let manager = group_manager().await;

        let error = manager
            .create_group(group_input("orphan", Some("missing-parent".to_string())))
            .await
            .expect_err("creating a group with an unknown parent must fail");

        assert!(matches!(error, RelayError::NotFound(_)));
        assert!(manager
            .list_groups()
            .await
            .expect("group list should remain readable")
            .is_empty());
    }

    #[tokio::test]
    async fn group_update_rejects_missing_self_and_descendant_parents() {
        let manager = group_manager().await;
        let root = manager
            .create_group(group_input("root", None))
            .await
            .expect("root fixture should persist");
        let child = manager
            .create_group(group_input("child", Some(root.id.clone())))
            .await
            .expect("child fixture should persist");
        let grandchild = manager
            .create_group(group_input("grandchild", Some(child.id.clone())))
            .await
            .expect("grandchild fixture should persist");

        let missing_parent = manager
            .update_group(&root.id, parent_update("missing-parent".to_string()))
            .await
            .expect_err("updates must reject missing parents");
        assert!(matches!(missing_parent, RelayError::NotFound(_)));

        let self_parent = manager
            .update_group(&root.id, parent_update(root.id.clone()))
            .await
            .expect_err("updates must reject a group as its own parent");
        assert!(matches!(self_parent, RelayError::InvalidConfig(_)));

        let descendant_parent = manager
            .update_group(&root.id, parent_update(grandchild.id.clone()))
            .await
            .expect_err("updates must reject a descendant as parent");
        assert!(matches!(descendant_parent, RelayError::InvalidConfig(_)));

        let stored_root = manager
            .get_group(&root.id)
            .await
            .expect("root should remain readable")
            .expect("root should remain present");
        assert_eq!(stored_root.parent_id, None);
    }

    #[tokio::test]
    async fn group_delete_rejects_parent_while_children_exist() {
        let manager = group_manager().await;
        let parent = manager
            .create_group(group_input("parent", None))
            .await
            .expect("parent fixture should persist");
        let child = manager
            .create_group(group_input("child", Some(parent.id.clone())))
            .await
            .expect("child fixture should persist");

        let error = manager
            .delete_group(&parent.id)
            .await
            .expect_err("deleting a parent must not leave a dangling child");
        assert!(matches!(error, RelayError::InvalidConfig(_)));
        assert!(manager
            .get_group(&parent.id)
            .await
            .expect("parent lookup should succeed")
            .is_some());
        assert!(manager
            .get_group(&child.id)
            .await
            .expect("child lookup should succeed")
            .is_some());

        manager
            .delete_group(&child.id)
            .await
            .expect("child without descendants should delete");
        manager
            .delete_group(&parent.id)
            .await
            .expect("parent should delete once children are gone");
    }

    #[tokio::test]
    async fn runtime_and_flat_export_share_full_lineage_policy() {
        let manager = group_manager().await;
        let mut root_input = group_input("root", None);
        root_input.global_ratio_multiplier = Some(2.0);
        root_input.model_whitelist = Some(vec!["gpt-4".to_string(), "gpt-5".to_string()]);
        root_input.model_blacklist = Some(vec!["gpt-4".to_string()]);
        let root = manager
            .create_group(root_input)
            .await
            .expect("root fixture should persist");

        let mut child_input = group_input("child", Some(root.id.clone()));
        child_input.global_ratio_multiplier = Some(3.0);
        child_input.model_whitelist = Some(vec!["gpt".to_string()]);
        let child = manager
            .create_group(child_input)
            .await
            .expect("child fixture should persist");

        let multiplier = manager
            .get_effective_multiplier(&child.id, "gpt-5-mini")
            .await
            .expect("models allowed by every ancestor should be priced");
        assert!((multiplier - 6.0).abs() < f64::EPSILON);
        assert!(matches!(
            manager
                .get_effective_multiplier(&child.id, "gpt-4")
                .await,
            Err(RelayError::InvalidConfig(_))
        ));

        let exported = manager
            .export_flat_groups()
            .await
            .expect("the representable policy should export");
        let exported_child = exported
            .iter()
            .find(|group| group.id == child.id)
            .expect("child should export");
        assert!((exported_child.ratio - multiplier).abs() < f64::EPSILON);
        assert_eq!(exported_child.models, Some(vec!["gpt-5".to_string()]));

        manager
            .update_group(&root.id, enabled_update(false))
            .await
            .expect("ancestor should be disabled");
        assert!(matches!(
            manager
                .get_effective_multiplier(&child.id, "gpt-5-mini")
                .await,
            Err(RelayError::InvalidConfig(_))
        ));
        assert!(manager
            .export_flat_groups()
            .await
            .expect("disabled policy should be omitted from export")
            .iter()
            .all(|group| group.id != child.id));
    }

    #[tokio::test]
    async fn effective_multiplier_honors_each_rule_declared_timezone() {
        let manager = group_manager().await;
        let mut root_input = group_input("shanghai-parent", None);
        root_input.global_ratio_multiplier = Some(2.0);
        root_input.time_rules = Some(vec![TimeBasedPricingRule {
            start_hour: 9,
            end_hour: 10,
            timezone: "Asia/Shanghai".to_string(),
            ratio_multiplier: 1.5,
            weekdays: vec![1],
        }]);
        let root = manager
            .create_group(root_input)
            .await
            .expect("parent fixture should persist");

        let mut child_input = group_input("utc-child", Some(root.id.clone()));
        child_input.global_ratio_multiplier = Some(3.0);
        child_input.time_rules = Some(vec![TimeBasedPricingRule {
            start_hour: 1,
            end_hour: 2,
            timezone: "UTC".to_string(),
            ratio_multiplier: 1.25,
            weekdays: vec![1],
        }]);
        let child = manager
            .create_group(child_input)
            .await
            .expect("child fixture should persist");

        let monday_utc = chrono::DateTime::parse_from_rfc3339("2026-07-20T01:30:00Z")
            .expect("fixture timestamp should parse")
            .with_timezone(&chrono::Utc);
        let multiplier = manager
            .get_effective_multiplier_at(&child.id, "gpt-5", monday_utc)
            .await
            .expect("both declared timezone rules should apply");
        assert!((multiplier - 11.25).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn flat_group_export_reads_persisted_groups_after_a_fresh_manager_and_ignores_runtime_only_groups(
    ) {
        let pool = group_store_pool().await;
        let initial_runtime = RuntimeState::memory(MemoryRuntimeStateConfig::default());
        let manager = DownstreamGroupManager::with_store(
            Arc::new(RelayEngineConfig::default()),
            initial_runtime,
            RelayDownstreamGroupStore::sqlite(pool.clone()),
        );
        let persisted = manager
            .create_group(CreateGroupInput {
                name: "persisted".to_string(),
                description: None,
                parent_id: None,
                global_ratio_multiplier: Some(1.25),
                model_whitelist: Some(vec!["gpt-5".to_string()]),
                model_blacklist: None,
                model_ratio_overrides: None,
                priority: Some(10),
                requests_per_minute: None,
                requests_per_day: None,
                daily_quota_limit: None,
                monthly_quota_limit: None,
                time_rules: None,
            })
            .await
            .expect("persisted fixture should be created");

        let fresh_runtime = RuntimeState::memory(MemoryRuntimeStateConfig::default());
        let runtime_only = DownstreamGroup {
            id: "runtime-only".to_string(),
            name: "runtime-only".to_string(),
            description: None,
            parent_id: None,
            global_ratio_multiplier: 9.0,
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
        fresh_runtime
            .kv_set(
                "relay:dgroup:runtime-only",
                serde_json::to_string(&runtime_only).expect("runtime fixture should serialize"),
                None,
            )
            .await
            .expect("runtime fixture should be cached");
        fresh_runtime
            .set_add("relay:dgroup:ids", &runtime_only.id)
            .await
            .expect("runtime fixture id should be cached");

        let fresh_manager = DownstreamGroupManager::with_store(
            Arc::new(RelayEngineConfig::default()),
            fresh_runtime,
            RelayDownstreamGroupStore::sqlite(pool),
        );
        let exported = fresh_manager
            .export_flat_groups()
            .await
            .expect("persisted groups should export after a fresh manager is constructed");

        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].id, persisted.id);
        assert_eq!(exported[0].name, "persisted");
        assert_eq!(exported[0].ratio, 1.25);
        assert_eq!(exported[0].models, Some(vec!["gpt-5".to_string()]));
        assert_eq!(exported[0].priority, 10);
    }

    #[tokio::test]
    async fn flat_group_export_combines_persisted_parent_and_child_global_multipliers() {
        let pool = group_store_pool().await;
        let manager = DownstreamGroupManager::with_store(
            Arc::new(RelayEngineConfig::default()),
            RuntimeState::memory(MemoryRuntimeStateConfig::default()),
            RelayDownstreamGroupStore::sqlite(pool.clone()),
        );
        let parent = manager
            .create_group(CreateGroupInput {
                name: "parent".to_string(),
                description: None,
                parent_id: None,
                global_ratio_multiplier: Some(1.2),
                model_whitelist: Some(vec!["gpt-4".to_string(), "gpt-5".to_string()]),
                model_blacklist: Some(vec!["gpt-4".to_string()]),
                model_ratio_overrides: None,
                priority: Some(1),
                requests_per_minute: None,
                requests_per_day: None,
                daily_quota_limit: None,
                monthly_quota_limit: None,
                time_rules: None,
            })
            .await
            .expect("parent fixture should persist");
        manager
            .create_group(CreateGroupInput {
                name: "child".to_string(),
                description: None,
                parent_id: Some(parent.id),
                global_ratio_multiplier: Some(1.25),
                model_whitelist: Some(vec!["gpt-5".to_string()]),
                model_blacklist: None,
                model_ratio_overrides: None,
                priority: Some(2),
                requests_per_minute: None,
                requests_per_day: None,
                daily_quota_limit: None,
                monthly_quota_limit: None,
                time_rules: None,
            })
            .await
            .expect("child fixture should persist");

        let fresh_manager = DownstreamGroupManager::with_store(
            Arc::new(RelayEngineConfig::default()),
            RuntimeState::memory(MemoryRuntimeStateConfig::default()),
            RelayDownstreamGroupStore::sqlite(pool),
        );
        let exported = fresh_manager
            .export_flat_groups()
            .await
            .expect("flattenable inherited groups should export");
        let child = exported
            .iter()
            .find(|group| group.name == "child")
            .expect("child should be exported");

        assert!((child.ratio - 1.5).abs() < f64::EPSILON);
        assert_eq!(child.models, Some(vec!["gpt-5".to_string()]));
    }

    #[tokio::test]
    async fn flat_group_export_fails_closed_for_policies_the_flat_contract_cannot_represent() {
        let unsupported_inputs = [
            CreateGroupInput {
                name: "blacklist".to_string(),
                description: None,
                parent_id: None,
                global_ratio_multiplier: None,
                model_whitelist: None,
                model_blacklist: Some(vec!["gpt-4".to_string()]),
                model_ratio_overrides: None,
                priority: None,
                requests_per_minute: None,
                requests_per_day: None,
                daily_quota_limit: None,
                monthly_quota_limit: None,
                time_rules: None,
            },
            CreateGroupInput {
                name: "override".to_string(),
                description: None,
                parent_id: None,
                global_ratio_multiplier: None,
                model_whitelist: None,
                model_blacklist: None,
                model_ratio_overrides: Some([("gpt-5".to_string(), 1.5)].into_iter().collect()),
                priority: None,
                requests_per_minute: None,
                requests_per_day: None,
                daily_quota_limit: None,
                monthly_quota_limit: None,
                time_rules: None,
            },
            CreateGroupInput {
                name: "time-rule".to_string(),
                description: None,
                parent_id: None,
                global_ratio_multiplier: None,
                model_whitelist: None,
                model_blacklist: None,
                model_ratio_overrides: None,
                priority: None,
                requests_per_minute: None,
                requests_per_day: None,
                daily_quota_limit: None,
                monthly_quota_limit: None,
                time_rules: Some(vec![super::TimeBasedPricingRule {
                    start_hour: 0,
                    end_hour: 1,
                    timezone: "UTC".to_string(),
                    ratio_multiplier: 1.1,
                    weekdays: Vec::new(),
                }]),
            },
        ];

        for input in unsupported_inputs {
            let pool = group_store_pool().await;
            let manager = DownstreamGroupManager::with_store(
                Arc::new(RelayEngineConfig::default()),
                RuntimeState::memory(MemoryRuntimeStateConfig::default()),
                RelayDownstreamGroupStore::sqlite(pool),
            );
            manager
                .create_group(input)
                .await
                .expect("unsupported fixture itself should persist");

            let error = manager
                .export_flat_groups()
                .await
                .expect_err("lossy group policies must not be exported as pricing truth");
            assert!(matches!(error, super::RelayError::InvalidConfig(_)));
        }
    }
}
