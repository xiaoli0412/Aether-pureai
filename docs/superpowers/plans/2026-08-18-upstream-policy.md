# 上游透传与重试策略（Upstream Policy）实施计划 — 计划 A

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为提供商增加 `upstream_policy` 配置：完全透传模式、强制重试上限 + 上游错误透传、回空（200 无内容）可配置重试与透传。

**Architecture:** 配置存于 `providers.config.upstream_policy` JSON 命名空间（零迁移，沿用 `responses_websocket` 模板）。策略解析进 `LocalFailoverPolicy`，随 report_context 内嵌传递；分类器/候选循环/执行运行时消费。回空预算用 AppState 内存计数器（按 request_id）。未配置时行为与现状完全一致。

**Tech Stack:** Rust 1.95（axum/reqwest/sqlx）、Tokio、serde_json；测试 `cargo test -p aether-gateway --lib`；前端 Vue3 + vitest。

**规格依据:** `docs/superpowers/specs/2026-08-18-upstream-passthrough-cost-routing.md` F1 部分。

## Global Constraints

- 未配置 `upstream_policy` 的提供商：行为零变化（现有测试全部保持通过）。
- `cargo fmt --all` 必须通过；`cargo clippy -p aether-gateway --lib --bins -- -D warnings` 必须通过。
- 配置校验在写入时强制执行（参照 `validate_responses_websocket_config`），防止经 raw `config` 字段绕过。
- 架构测试 `apps/aether-gateway/src/tests/architecture/admin_provider.rs` 约束 create/update builder 的函数名与调用结构——不得改名。
- 敏感头脱敏约定不变（`trace_header_is_sensitive` 列表）。
- 提交信息格式遵循仓库历史：`feat(gateway): ...` / `fix(gateway): ...`。

## 配置 Schema（最终定义）

```jsonc
// providers.config.upstream_policy
{
  "mode": "default" | "full_passthrough",   // default：failover_rules 等现有规则继续生效
  "max_attempts": 1,                         // 可选，1..=100；该提供商每请求总尝试次数硬上限
  "passthrough_upstream_errors": false,      // 可选；停止/耗尽时把最后一次上游错误响应原样返回下游，而不是 AE 生成的 503/错误体
  "empty_response": {                        // 可选
    "detect": false,                         // 对非 gemini:generate_content 格式启用"200 无可见输出"检测（gemini 格式恒检测）
    "max_attempts": 1,                       // 0..=100；回空持续发生时愿意花费的尝试次数
    "on_exhausted": "passthrough" | "error"  // passthrough：预算耗尽后把空 200 原样透传下游；error：保持旧行为（502 重写→重试→可能 503）
  }
}
```

语义：
- `mode=full_passthrough` 覆盖一切：零重试（含传输错误）、所有状态码终止、回空原样下发、上游错误体原样下发。
- `max_attempts` 计入该提供商的所有尝试；达到上限即停止。配合 `passthrough_upstream_errors=true` 时返回保留的上游错误响应，否则走旧耗尽路径（503）。
- 回空预算独立计数（AppState 内存表，key=request_id）：空响应次数 < `empty_response.max_attempts` → 重试下一候选；≥ → 按 `on_exhausted` 处理。
- 向后兼容基线：gemini 格式回空 → 502 重写 + 重试；openai:chat 回空 → 静默透传；无尝试上限；耗尽 → AE 503。

## File Structure

| 文件 | 动作 | 职责 |
|---|---|---|
| `apps/aether-gateway/src/orchestration/policy.rs` | Modify | `LocalFailoverPolicy` 新字段 + `upstream_policy` 解析 + report_context 往返 |
| `apps/aether-gateway/src/orchestration/classifier.rs` | Modify | `StopPassthrough` 分类；passthrough 模式下传输错误 Stop |
| `apps/aether-gateway/src/orchestration/recovery.rs` | Modify | 新分类 → StopLocalFailover 映射 |
| `apps/aether-gateway/src/execution_runtime/empty_response.rs` | Create | 回空预算计数器（AppState 持有）+ 可见输出判定封装 |
| `apps/aether-gateway/src/execution_runtime/mod.rs` | Modify | 注册 empty_response 模块与导出 |
| `apps/aether-gateway/src/execution_runtime/sync/execution.rs` | Modify | 同步回空检测泛化 + 预算消费 |
| `apps/aether-gateway/src/execution_runtime/stream/execution.rs` | Modify | 流式 force_prefetch 触发 + 预提交 EOF 回空检测 |
| `apps/aether-gateway/src/executor/candidate_loop.rs` | Modify | 强制 max_attempts 计数 + 耗尽时返回保留的上游错误 fallback |
| `apps/aether-gateway/src/handlers/admin/provider/shared/payloads.rs` | Modify | `upstream_policy: Option<Value>` 载荷字段 |
| `apps/aether-gateway/src/handlers/admin/provider/write/normalize.rs` | Modify | set/remove/validate helpers |
| `apps/aether-gateway/src/handlers/admin/provider/write/provider/create.rs` | Modify | create 合并 |
| `apps/aether-gateway/src/handlers/admin/provider/write/provider/update.rs` | Modify | update patch 块 |
| `apps/aether-gateway/src/handlers/admin/provider/summary/value.rs` | Modify | 读投影 |
| `apps/aether-gateway/src/state/app.rs` (+core.rs) | Modify | 回空预算表句柄 |
| `frontend/src/api/endpoints/types/provider.ts` | Modify | TS 类型 |
| `frontend/src/features/providers/components/ProviderFormDialog.vue` | Modify | 表单区块 |
| `frontend/src/features/providers/components/__tests__/ProviderFormDialog.upstream-policy.spec.ts` | Create | 源码标记断言 |
| `docs/upstream-policy.md` | Create | 运营文档 |

---

### Task 1: 策略模型与解析（orchestration/policy.rs）

**Files:**
- Modify: `apps/aether-gateway/src/orchestration/policy.rs`

**Interfaces:**
- Produces: `LocalFailoverPolicy` 新字段（classifier/recovery/candidate_loop/execution_runtime 消费）：
  ```rust
  pub(crate) upstream_passthrough_mode: bool,
  pub(crate) enforced_max_attempts: Option<u64>,
  pub(crate) passthrough_upstream_errors: bool,
  pub(crate) empty_response_policy: Option<LocalEmptyResponsePolicy>,

  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub(crate) struct LocalEmptyResponsePolicy {
      pub(crate) detect: bool,
      pub(crate) max_attempts: u64,
      pub(crate) on_exhausted: LocalEmptyResponseExhaustion,
  }
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub(crate) enum LocalEmptyResponseExhaustion { Passthrough, Error }
  ```
- Produces: `pub(crate) const UPSTREAM_POLICY_CONFIG_KEY: &str = "upstream_policy";`

- [ ] **Step 1: 写失败测试**（追加到 policy.rs 底部 `mod tests`）

```rust
#[test]
fn upstream_policy_parses_full_passthrough_mode() {
    let policy = local_failover_policy_from_transport(&sample_transport(
        None,
        None,
        Some(json!({
            "upstream_policy": { "mode": "full_passthrough" }
        })),
    ));
    assert!(policy.upstream_passthrough_mode);
    assert!(policy.empty_response_policy.is_none());
}

#[test]
fn upstream_policy_parses_capped_retry_and_empty_response() {
    let policy = local_failover_policy_from_transport(&sample_transport(
        None,
        None,
        Some(json!({
            "upstream_policy": {
                "max_attempts": 2,
                "passthrough_upstream_errors": true,
                "empty_response": { "detect": true, "max_attempts": 1, "on_exhausted": "passthrough" }
            }
        })),
    ));
    assert!(!policy.upstream_passthrough_mode);
    assert_eq!(policy.enforced_max_attempts, Some(2));
    assert!(policy.passthrough_upstream_errors);
    assert_eq!(
        policy.empty_response_policy,
        Some(LocalEmptyResponsePolicy {
            detect: true,
            max_attempts: 1,
            on_exhausted: LocalEmptyResponseExhaustion::Passthrough,
        })
    );
}

#[test]
fn upstream_policy_round_trips_through_report_context() {
    let report_context = append_local_failover_policy_to_value(
        json!({}),
        &sample_transport(
            None,
            None,
            Some(json!({
                "upstream_policy": {
                    "mode": "full_passthrough",
                    "max_attempts": 3,
                    "passthrough_upstream_errors": true,
                    "empty_response": { "detect": true, "max_attempts": 0, "on_exhausted": "error" }
                }
            })),
        ),
    );
    let parsed = local_failover_policy_from_report_context(Some(&report_context)).unwrap();
    assert!(parsed.upstream_passthrough_mode);
    assert_eq!(parsed.enforced_max_attempts, Some(3));
    assert!(parsed.passthrough_upstream_errors);
    assert_eq!(
        parsed.empty_response_policy,
        Some(LocalEmptyResponsePolicy {
            detect: true,
            max_attempts: 0,
            on_exhausted: LocalEmptyResponseExhaustion::Error,
        })
    );
}

#[test]
fn upstream_policy_defaults_preserve_legacy_behavior() {
    let policy = local_failover_policy_from_transport(&sample_transport(None, None, None));
    assert!(!policy.upstream_passthrough_mode);
    assert_eq!(policy.enforced_max_attempts, None);
    assert!(!policy.passthrough_upstream_errors);
    assert!(policy.empty_response_policy.is_none());
}

#[test]
fn upstream_policy_ignores_malformed_values() {
    let policy = local_failover_policy_from_transport(&sample_transport(
        None,
        None,
        Some(json!({
            "upstream_policy": {
                "mode": "nonsense",
                "max_attempts": "lots",
                "passthrough_upstream_errors": "yes",
                "empty_response": { "on_exhausted": "explode" }
            }
        })),
    ));
    assert!(!policy.upstream_passthrough_mode);
    assert_eq!(policy.enforced_max_attempts, None);
    assert!(!policy.passthrough_upstream_errors);
    // empty_response 存在但字段非法 → 仍可解析出保守默认（detect=false，不改变行为）
    let empty = policy.empty_response_policy.unwrap();
    assert!(!empty.detect);
    assert_eq!(empty.on_exhausted, LocalEmptyResponseExhaustion::Error);
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p aether-gateway --lib orchestration::policy::tests::upstream_policy 2>&1 | tail -5`
Expected: 编译错误（字段不存在）

- [ ] **Step 3: 实现**

在 `LocalFailoverPolicy` struct 与 `Default` 中加四个字段（默认：false/None/false/None）。新增类型与解析函数：

```rust
pub(crate) const UPSTREAM_POLICY_CONFIG_KEY: &str = "upstream_policy";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalEmptyResponseExhaustion {
    Passthrough,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalEmptyResponsePolicy {
    pub(crate) detect: bool,
    pub(crate) max_attempts: u64,
    pub(crate) on_exhausted: LocalEmptyResponseExhaustion,
}

impl LocalEmptyResponsePolicy {
    pub(crate) const fn retries_before_passthrough(self) -> u64 {
        self.max_attempts
    }
}

fn upstream_policy_from_config(provider_config: Option<&Value>) -> UpstreamPolicyFields {
    // UpstreamPolicyFields 为内部聚合结构 { mode, max_attempts, passthrough_errors, empty_response }
    let rules = provider_config
        .and_then(|config| config.get(UPSTREAM_POLICY_CONFIG_KEY))
        .and_then(Value::as_object);
    ...
}
```

解析规则：
- `mode`：字符串，仅 `"full_passthrough"` 生效（大小写不敏感），其余视为 default。
- `max_attempts`：`parse_u64_value`，clamp 到 `1..=100`，超出/非法 → None。
- `passthrough_upstream_errors`：仅 `as_bool`。
- `empty_response`：对象；`detect` = `as_bool` 默认 false；`max_attempts` clamp `0..=100` 默认 1；`on_exhausted` 仅 `"passthrough"` → Passthrough，其余 → Error。对象存在即产出 `Some(LocalEmptyResponsePolicy)`（哪怕字段全非法——保守默认不改变行为，但保持 round-trip 一致性）。

在 `local_failover_policy_from_transport` 中填充字段；在 `local_failover_policy_to_value` 中序列化（供 report_context 内嵌）：

```rust
"upstream_passthrough_mode": policy.upstream_passthrough_mode,
"enforced_max_attempts": policy.enforced_max_attempts,
"passthrough_upstream_errors": policy.passthrough_upstream_errors,
"empty_response_policy": policy.empty_response_policy.map(|empty| json!({
    "detect": empty.detect,
    "max_attempts": empty.max_attempts,
    "on_exhausted": match empty.on_exhausted {
        LocalEmptyResponseExhaustion::Passthrough => "passthrough",
        LocalEmptyResponseExhaustion::Error => "error",
    },
})),
```

在 `local_failover_policy_from_report_context` 中对称解析回来。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p aether-gateway --lib orchestration::policy`
Expected: 全部 PASS（含既有测试）

- [ ] **Step 5: Commit**

```bash
git add apps/aether-gateway/src/orchestration/policy.rs
git commit -m "feat(gateway): parse upstream_policy into local failover policy"
```

---

### Task 2: 分类器与恢复层支持透传模式

**Files:**
- Modify: `apps/aether-gateway/src/orchestration/classifier.rs`
- Modify: `apps/aether-gateway/src/orchestration/recovery.rs`
- Modify: `apps/aether-gateway/src/orchestration/mod.rs`（仅当新枚举项需 re-export）

**Interfaces:**
- Consumes: `LocalFailoverPolicy.upstream_passthrough_mode`（Task 1）
- Produces: `LocalFailoverClassification::StopPassthrough`；passthrough 模式下 `classify_local_transport_error` 返回 `StopTransportError`

- [ ] **Step 1: 写失败测试**（classifier.rs tests 模块）

```rust
#[test]
fn passthrough_mode_stops_every_error_status_without_retry() {
    let policy = LocalFailoverPolicy {
        upstream_passthrough_mode: true,
        retry_client_errors_by_default: true,
        ..LocalFailoverPolicy::default()
    };
    for status in [400u16, 404, 429, 500, 502, 503] {
        let analysis = crate::orchestration::recovery::analyze_local_failover_for_tests(&policy, status);
        assert_eq!(analysis.decision, LocalFailoverDecision::StopLocalFailover, "status {status}");
        assert_eq!(analysis.classification, LocalFailoverClassification::StopPassthrough, "status {status}");
    }
}

#[test]
fn passthrough_mode_stops_transport_errors() {
    let policy = LocalFailoverPolicy {
        upstream_passthrough_mode: true,
        ..LocalFailoverPolicy::default()
    };
    assert_eq!(
        classify_local_transport_error(&policy),
        LocalTransportFailoverClassification::StopTransportError
    );
}
```

（`analyze_local_failover_for_tests` 不存在时直接用 `analyze_local_failover(&policy, LocalFailoverInput::new(status, None))`——优先用公开函数，不要为测试新造入口。）

并在 classifier.rs 中新增断言：passthrough 模式下 `failure_disposition_from_local_classification(StopPassthrough, _)` 的 `preserve_upstream_error == true` 且 `retry_action == Stop`。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p aether-gateway --lib orchestration::classifier`
Expected: FAIL/编译失败

- [ ] **Step 3: 实现**

classifier.rs：
1. `LocalFailoverClassification` 增加变体 `StopPassthrough`，`as_str()` 返回 `"stop_passthrough"`。
2. `classify_local_failover` 开头插入：
```rust
if policy.upstream_passthrough_mode && input.status_code >= 400 {
    return LocalFailoverClassification::StopPassthrough;
}
```
（200 不在此拦截——回空由执行运行时处理；`success_failover_patterns` 在 passthrough 模式下也必须失效，所以该短路放在所有分支之前。）
3. `classify_local_transport_error`：`if policy.stop_on_transport_errors || policy.upstream_passthrough_mode { Stop } else { Retry }`。
4. `failure_disposition_from_local_classification`：把 `StopPassthrough` 并入 Stop 分支组（preserve=true）。

recovery.rs：`decision_from_classification` 把 `StopPassthrough` 并入 StopLocalFailover 分支。

检查 `build_local_error_flow_metadata`（orchestration/mod.rs:120-150）的 `safe_to_expose` 匹配——把 `StopPassthrough` 加入 safe_to_expose 列表（它的语义就是"原样暴露上游错误"）。

- [ ] **Step 4: 全量回归**

Run: `cargo test -p aether-gateway --lib orchestration`
Expected: 全部 PASS（注意既有测试 `classifier_retries_all_error_statuses_without_custom_rule` 不受影响——它用默认 policy）

- [ ] **Step 5: Commit**

```bash
git add apps/aether-gateway/src/orchestration/classifier.rs apps/aether-gateway/src/orchestration/recovery.rs apps/aether-gateway/src/orchestration/mod.rs
git commit -m "feat(gateway): classify upstream passthrough mode as terminal"
```

---

### Task 3: 候选循环强制尝试上限 + 上游错误兜底返回

**Files:**
- Modify: `apps/aether-gateway/src/executor/candidate_loop.rs`（`run_dynamic_attempt_loop` :772 附近、`build_exhaustion` 两个实现 :326/:1090 附近、静态路径 :65-148）
- Test: 同文件 `mod tests`（既有 fake port 基建 :1648+）

**Interfaces:**
- Consumes: `local_failover_policy_from_report_context`（report_context 内嵌策略，Task 1 已含新字段）；`AiAttemptExecutionOutcome::Retry { fallback_response }` 既有机制
- Produces: 达到 `enforced_max_attempts` 后不再取下一候选；存在保留的上游错误 fallback 时以该响应结束请求

- [ ] **Step 1: 写失败测试**（candidate_loop.rs tests，复用既有 fake source/port 模式）

测试一：`dynamic_attempt_loop_stops_after_enforced_max_attempts`
- fake source 无限供给候选，每次执行返回 `Retry { scope: Candidate, fallback_response: None }`。
- report_context 内嵌 `local_failover_policy`：`{"max_retries": null, "enforced_max_attempts": 2, ...}`（用 Task 1 的 to_value 形状）。
- 断言：fake source 只被取了 2 次；最终 outcome 为耗尽（build_exhaustion 被调用一次，携带最后一次 plan）。

测试二：`dynamic_attempt_loop_returns_preserved_upstream_error_when_cap_reached`
- 第二次尝试返回 `Retry { scope: Candidate, fallback_response: Some(response_400_with_body) }`，cap=2。
- 断言：循环返回该 fallback 响应（状态 400、body 原样），而不是继续取候选或走 build_exhaustion。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p aether-gateway --lib executor::candidate_loop::tests::dynamic_attempt_loop`
Expected: FAIL

- [ ] **Step 3: 实现**

在 `run_dynamic_attempt_loop` 中：
1. 循环开始前从当前 attempt 的 report_context 读取策略上限：
```rust
let enforced_max_attempts = first_report_context
    .as_ref()
    .and_then(local_failover_policy_from_report_context)
    .and_then(|policy| policy.enforced_max_attempts);
let mut attempts_started: u64 = 0;
let mut last_fallback_response: Option<Response<Body>> = None;
```
2. 每次取出候选、执行前 `attempts_started += 1`；执行结果为 `Retry { fallback_response, .. }` 时，若 `fallback_response.is_some()` 则更新 `last_fallback_response`（后一次覆盖前一次——最后一次上游错误优先）。
3. 在请求下一候选前检查：
```rust
if enforced_max_attempts.is_some_and(|cap| attempts_started >= cap) {
    if let Some(response) = last_fallback_response.take() {
        return Ok(response);  // 或该循环的等价成功返回路径
    }
    break; // 走既有耗尽路径（503）
}
```
4. 静态路径（`execute_sync_plan_and_reports` / `execute_stream_plan_and_reports` 走 `run_ai_attempt_loop`）同样需要 cap：若静态路径实现成本过高，先在静态路径入口将候选列表截断到 cap 长度并记录 debug 日志（保持语义等价）。
5. 耗尽返回 fallback 的既有分支（:858-862 "stored fallback_response is returned as a deferred upstream response"）保持不变——cap 分支复用同一构造。

注意：`ProviderTransferTracker` 的预算语义不变；cap 是更外层的硬闸。

- [ ] **Step 4: 运行确认通过 + 回归**

Run: `cargo test -p aether-gateway --lib executor::candidate_loop`
Expected: 全部 PASS

- [ ] **Step 5: Commit**

```bash
git add apps/aether-gateway/src/executor/candidate_loop.rs
git commit -m "feat(gateway): enforce per-provider attempt cap with upstream error fallback"
```

---

### Task 4: 回空预算模块（empty_response.rs）

**Files:**
- Create: `apps/aether-gateway/src/execution_runtime/empty_response.rs`
- Modify: `apps/aether-gateway/src/execution_runtime/mod.rs`（注册模块）
- Modify: `apps/aether-gateway/src/state/app.rs` 或 `state/core.rs`（挂载计数器句柄——跟随现有 AppState 字段风格）

**Interfaces:**
- Produces（后续 Task 5/6 消费）：
```rust
pub(crate) struct EmptyResponseBudgetTracker { /* parking_lot::Mutex<HashMap<String, EmptyResponseBudgetEntry>> */ }
impl EmptyResponseBudgetTracker {
    pub(crate) fn new() -> Self;
    /// 记录一次回空并返回该请求已发生的回空次数（含本次）。
    pub(crate) fn record_empty_response(&self, request_id: &str) -> u64;
    pub(crate) fn forget(&self, request_id: &str);
}
/// 判定同步成功响应是否无可见输出（格式感知，复用 ai_serving 既有 helper）。
pub(crate) fn sync_success_response_lacks_visible_output(
    client_api_format: &str,
    provider_api_format: &str,
    body_json: &serde_json::Value,
) -> bool;
```

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn budget_tracker_counts_per_request_and_forgets() {
        let tracker = EmptyResponseBudgetTracker::new();
        assert_eq!(tracker.record_empty_response("req-1"), 1);
        assert_eq!(tracker.record_empty_response("req-1"), 2);
        assert_eq!(tracker.record_empty_response("req-2"), 1);
        tracker.forget("req-1");
        assert_eq!(tracker.record_empty_response("req-1"), 1);
    }

    #[test]
    fn visible_output_detection_covers_openai_chat_and_gemini_shapes() {
        // openai:chat 空 choices
        assert!(sync_success_response_lacks_visible_output(
            "openai:chat", "openai:chat",
            &json!({"id":"x","choices":[{"message":{"content":""}}]}),
        ));
        // openai:chat 有内容
        assert!(!sync_success_response_lacks_visible_output(
            "openai:chat", "openai:chat",
            &json!({"choices":[{"message":{"content":"hi"}}]}),
        ));
        // openai:chat tool_calls 视为有输出
        assert!(!sync_success_response_lacks_visible_output(
            "openai:chat", "openai:chat",
            &json!({"choices":[{"message":{"tool_calls":[{"id":"c1","function":{"name":"f"}}]}}]}),
        ));
        // gemini generateContent 无 candidates
        assert!(sync_success_response_lacks_visible_output(
            "gemini:generate_content", "gemini:generate_content", &json!({}),
        ));
    }

    #[test]
    fn tracker_prunes_stale_entries() {
        let tracker = EmptyResponseBudgetTracker::new();
        tracker.record_empty_response("req-old");
        tracker.prune_entries_older_than(std::time::Duration::from_secs(0)); // 测试钩子：立即过期
        assert_eq!(tracker.record_empty_response("req-old"), 1);
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p aether-gateway --lib execution_runtime::empty_response`
Expected: 编译失败（模块不存在）

- [ ] **Step 3: 实现**

```rust
use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::Value;

const EMPTY_RESPONSE_BUDGET_TTL: Duration = Duration::from_secs(300);
const EMPTY_RESPONSE_BUDGET_MAX_ENTRIES: usize = 100_000;

#[derive(Debug)]
struct EmptyResponseBudgetEntry {
    count: u64,
    touched_at: Instant,
}

#[derive(Debug, Default)]
pub(crate) struct EmptyResponseBudgetTracker {
    entries: Mutex<HashMap<String, EmptyResponseBudgetEntry>>,
}

impl EmptyResponseBudgetTracker {
    pub(crate) fn new() -> Self { Self::default() }

    pub(crate) fn record_empty_response(&self, request_id: &str) -> u64 {
        let mut entries = self.entries.lock();
        if entries.len() >= EMPTY_RESPONSE_BUDGET_MAX_ENTRIES {
            prune_locked_entries(&mut entries, EMPTY_RESPONSE_BUDGET_TTL);
        }
        let entry = entries.entry(request_id.to_string()).or_insert(EmptyResponseBudgetEntry { count: 0, touched_at: Instant::now() });
        entry.count += 1;
        entry.touched_at = Instant::now();
        entry.count
    }

    pub(crate) fn forget(&self, request_id: &str) { self.entries.lock().remove(request_id); }

    pub(crate) fn prune_stale(&self) { prune_locked_entries(&mut self.entries.lock(), EMPTY_RESPONSE_BUDGET_TTL); }

    #[cfg(test)]
    pub(crate) fn prune_entries_older_than(&self, max_age: Duration) {
        prune_locked_entries(&mut self.entries.lock(), max_age);
    }
}

fn prune_locked_entries(entries: &mut HashMap<String, EmptyResponseBudgetEntry>, max_age: Duration) {
    let now = Instant::now();
    entries.retain(|_, entry| now.duration_since(entry.touched_at) < max_age);
}

pub(crate) fn sync_success_response_lacks_visible_output(
    _client_api_format: &str,
    provider_api_format: &str,
    body_json: &Value,
) -> bool {
    // 复用 ai_serving 的格式感知判定（它同时理解 Gemini candidates、OpenAI choices/tool_calls、Responses output）
    if crate::ai_serving::gemini_generate_content_response_has_visible_output(body_json) {
        return false;
    }
    // provider 返回中内嵌 error 对象时不视为"回空"（那是错误透传问题，不是回空）
    let has_error = body_json.as_object().is_some_and(|object| {
        object.get("error").is_some_and(|error| !error.is_null())
    });
    !has_error || provider_api_format.trim().is_empty() && false
}
```

注意：`sync_success_response_lacks_visible_output` 的最终语义——无可见输出 **且** 无内嵌 error → true。实现后按测试用例校准（上面伪码尾行需改写成清晰逻辑：`!has_visible && !has_error`）。

在 `execution_runtime/mod.rs` 加 `mod empty_response;` 与 `pub(crate) use`。AppState 挂载：在 `state/app.rs`（或 core.rs，视现有结构）加字段 `empty_response_budget: Arc<EmptyResponseBudgetTracker>`，`AppState::new()` 初始化。若已有后台清理任务注册点（搜索既有 spawn 周期任务），注册每 60s `prune_stale`；否则依赖 record 时的容量剪枝。

- [ ] **Step 4: 运行确认通过**

Run: `cargo test -p aether-gateway --lib execution_runtime::empty_response`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add apps/aether-gateway/src/execution_runtime/empty_response.rs apps/aether-gateway/src/execution_runtime/mod.rs apps/aether-gateway/src/state/
git commit -m "feat(gateway): add empty-response budget tracker"
```

---

### Task 5: 同步路径回空检测泛化与预算消费

**Files:**
- Modify: `apps/aether-gateway/src/execution_runtime/sync/execution.rs`（`invalid_gemini_provider_success_message` :837、调用点 ~:2551-2575）

**Interfaces:**
- Consumes: Task 1 `LocalFailoverPolicy.empty_response_policy`（经 report_context `local_failover_policy`）；Task 4 tracker
- Produces: 非 gemini 格式在 `detect=true` 时也检测回空；预算耗尽 + `on_exhausted=passthrough` 时不再重写为 502，原样下发空 200

- [ ] **Step 1: 写失败测试**（sync/execution.rs tests 模块；若无独立测试模块则新建，遵循内联约定）

纯函数级测试——把检测决策抽成可测函数：

```rust
#[test]
fn sync_empty_success_decision_honors_empty_response_policy() {
    // gemini 格式 + 无策略 → 保持旧行为（重写 502 重试）
    assert_eq!(
        sync_empty_success_action("gemini:generate_content", None, 0),
        SyncEmptySuccessAction::RewriteRetryable,
    );
    // openai:chat + 无策略 → 不干预（静默透传）
    assert_eq!(
        sync_empty_success_action("openai:chat", None, 0),
        SyncEmptySuccessAction::None,
    );
    // openai:chat + detect + 预算内 → 重试
    let policy = LocalEmptyResponsePolicy { detect: true, max_attempts: 1, on_exhausted: LocalEmptyResponseExhaustion::Passthrough };
    assert_eq!(sync_empty_success_action("openai:chat", Some(policy), 0), SyncEmptySuccessAction::RewriteRetryable);
    // 预算耗尽 → passthrough
    assert_eq!(sync_empty_success_action("openai:chat", Some(policy), 1), SyncEmptySuccessAction::Passthrough);
    // 预算耗尽 + on_exhausted=error → 保持重写
    let policy_error = LocalEmptyResponsePolicy { on_exhausted: LocalEmptyResponseExhaustion::Error, ..policy };
    assert_eq!(sync_empty_success_action("openai:chat", Some(policy_error), 1), SyncEmptySuccessAction::RewriteRetryable);
    // detect=false 的非 gemini 格式 → 不干预
    let policy_off = LocalEmptyResponsePolicy { detect: false, ..policy };
    assert_eq!(sync_empty_success_action("openai:chat", Some(policy_off), 0), SyncEmptySuccessAction::None);
    // gemini 格式 + 策略 passthrough 预算耗尽 → 同样透传
    assert_eq!(sync_empty_success_action("gemini:generate_content", Some(policy), 2), SyncEmptySuccessAction::Passthrough);
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p aether-gateway --lib execution_runtime::sync::execution::tests::sync_empty_success`
Expected: FAIL（函数不存在）

- [ ] **Step 3: 实现**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncEmptySuccessAction {
    None,               // 不干预（响应原样继续成功流程）
    RewriteRetryable,   // 旧行为：重写 502 + retryable，走重试
    Passthrough,        // 空 200 原样下发为成功
}

fn sync_empty_success_action(
    provider_api_format: &str,
    empty_policy: Option<LocalEmptyResponsePolicy>,
    empty_attempts_so_far: u64,
) -> SyncEmptySuccessAction {
    let is_gemini = provider_api_format == "gemini:generate_content"; // 已 normalize
    let detect = is_gemini || empty_policy.is_some_and(|policy| policy.detect);
    if !detect {
        return SyncEmptySuccessAction::None;
    }
    let budget = empty_policy.map(|policy| policy.max_attempts).unwrap_or(u64::MAX);
    let on_exhausted = empty_policy
        .map(|policy| policy.on_exhausted)
        .unwrap_or(LocalEmptyResponseExhaustion::Error);
    if empty_attempts_so_far < budget {
        return SyncEmptySuccessAction::RewriteRetryable;
    }
    match on_exhausted {
        LocalEmptyResponseExhaustion::Passthrough => SyncEmptySuccessAction::Passthrough,
        LocalEmptyResponseExhaustion::Error => SyncEmptySuccessAction::RewriteRetryable,
    }
}
```

在调用点（`invalid_gemini_provider_success_message` 的使用处）改造：
1. 从 report_context 读内嵌策略：`local_failover_policy_from_report_context(report_context).and_then(|p| p.empty_response_policy)`。
2. 判定空响应（gemini 用既有函数；`detect=true` 时对 openai:chat 等用 Task 4 的 `sync_success_response_lacks_visible_output`，先经 `normalize_provider_private_response_value` 归一化）。
3. `state.empty_response_budget.record_empty_response(&plan.request_id)` 得到计数（count-1 = 本次之前的次数），带入 `sync_empty_success_action`。
4. `None` → 跳过整个重写分支；`RewriteRetryable` → 旧逻辑；`Passthrough` → 不重写，按普通 200 成功流程继续（并在 report_context 打 `empty_response_passthrough: true` 标记，供 Task 8 诊断与用量侧观察）。
5. passthrough 模式下（`upstream_passthrough_mode=true`）：`sync_empty_success_action` 的上游调用方直接按 `None` 处理（passthrough 模式不干预任何成功响应）。
6. 请求终态（成功 finalize 或耗尽）处调用 `forget(request_id)` 释放条目——加在同步 finalize 报告提交点；若难以覆盖所有出口，依赖 TTL 剪枝也可接受（记录 TODO 注释说明依赖 TTL）。

- [ ] **Step 4: 运行确认通过 + 既有 gemini 相关测试回归**

Run: `cargo test -p aether-gateway --lib execution_runtime::sync` 与 `cargo test -p aether-gateway --lib -- gemini`
Expected: PASS（既有 gemini 回空测试不受影响——它们不带 upstream_policy 配置）

- [ ] **Step 5: Commit**

```bash
git add apps/aether-gateway/src/execution_runtime/sync/execution.rs
git commit -m "feat(gateway): generalize sync empty-response detection with budget policy"
```

---

### Task 6: 流式路径回空检测（prefetch）与透传

**Files:**
- Modify: `apps/aether-gateway/src/execution_runtime/stream/execution.rs`（`should_skip_direct_finalize_prefetch` :5413 的调用点、预提交 EOF 处理区 ~:6200-6240 / :7078-7088）
- Test: 同文件 tests（参照 `synthesizes_missing_terminal_summary_for_openai_responses_empty_stream` :10613 的构造方式）

**Interfaces:**
- Consumes: Task 1/4/5 的策略与预算
- Produces: `empty_response` 启用时对同格式 SSE 也预取；预取窗口内 EOF 且无可见内容 → 预算内重试 / 预算外按 `on_exhausted` 处理（passthrough=提交并原样放空流，error=合成错误走既有重试）

- [ ] **Step 1: 写失败测试**

测试一：`stream_empty_success_retries_when_policy_budget_available`
- 构造 openai:chat 同格式流式请求，上游返回 200 + `text/event-stream` + 零数据帧即 EOF；report_context 内嵌 `empty_response { detect: true, max_attempts: 1, on_exhausted: "passthrough" }`；两个候选。
- 断言：第一候选被判定回空并重试（候选状态 failed，error_type 含 empty），第二候选同样空 → 预算耗尽 → 下游收到 200 空流（passthrough），用量记为成功。

测试二：`stream_empty_success_untouched_without_policy`
- 同构造但不带策略：下游立即收到 200 空流，无重试（锁定现状）。

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p aether-gateway --lib execution_runtime::stream::execution::tests::stream_empty_success`
Expected: FAIL

- [ ] **Step 3: 实现**

1. 计算 `empty_policy_active`：从 report_context 内嵌策略取 `empty_response_policy`（detect=true 或 provider 格式为 gemini:generate_content 且有策略）且非 passthrough 模式。
2. `StreamCommitPolicy::for_response` 调用点：`force_prefetch = force_prefetch || empty_policy_active`（保持现有 cyber force_prefetch 逻辑不动）。
3. 预提交 EOF 处理：既有代码在预取阶段遇到 EOF 会走 `inspect_prefetched_stream_body` / 合成错误路径。新增分支：EOF 且预取缓冲解析不出任何可见内容帧（SSE data 全空/只有 [DONE]/零帧）→
   - `count = state.empty_response_budget.record_empty_response(request_id)`；
   - `sync_empty_success_action`（复用 Task 5 的决策函数）→
     - `RewriteRetryable` → 走既有"合成错误 + `return Ok(None)`"重试路径（error_type `empty_upstream_response`）；
     - `Passthrough` → 提交响应，把已缓冲的空帧原样放入下游 Body（成功路径）；
     - `None` → 维持现有 EOF 行为。
4. 可见内容判定：对预取字节做最小解析——存在任一 `data:` 帧且其 JSON 含非空 delta/content/tool_calls 即视为有输出；复用 `prefetched_openai_responses_body_has_output_boundary` 的思路新写 `prefetched_chat_body_has_visible_output`（openai:chat/gemini SSE 两形状）。
5. passthrough 模式：`empty_policy_active=false`，且即使 EOF 也按现状提交空流。

- [ ] **Step 4: 运行确认通过 + 流式回归**

Run: `cargo test -p aether-gateway --lib execution_runtime::stream`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add apps/aether-gateway/src/execution_runtime/stream/execution.rs
git commit -m "feat(gateway): detect empty upstream streams pre-commit with budget policy"
```

---

### Task 7: Admin API 配置面（payload/normalize/create/update/summary）

**Files:**
- Modify: `apps/aether-gateway/src/handlers/admin/provider/shared/payloads.rs`（create :129-188 / update :191-253 各加 `#[serde(default)] pub(crate) upstream_policy: Option<serde_json::Value>`）
- Modify: `apps/aether-gateway/src/handlers/admin/provider/write/normalize.rs`
- Modify: `apps/aether-gateway/src/handlers/admin/provider/write/provider/create.rs`（:184 附近，responses_websocket 之后）
- Modify: `apps/aether-gateway/src/handlers/admin/provider/write/provider/update.rs`（:342 附近）
- Modify: `apps/aether-gateway/src/handlers/admin/provider/summary/value.rs`（:225 附近）

**Interfaces:**
- Produces: `providers.config.upstream_policy` 写入/校验/读取；summary payload 暴露 `upstream_policy` 原对象（或 null）

- [ ] **Step 1: 写失败测试**（normalize.rs tests，参照 :369-382 风格）

```rust
#[test]
fn validate_upstream_policy_accepts_full_schema() {
    let mut config = serde_json::Map::new();
    set_upstream_policy(&mut config, &serde_json::json!({
        "mode": "full_passthrough",
        "max_attempts": 2,
        "passthrough_upstream_errors": true,
        "empty_response": { "detect": true, "max_attempts": 1, "on_exhausted": "passthrough" }
    })).unwrap();
    validate_upstream_policy_config(&config).unwrap();
}

#[test]
fn validate_upstream_policy_rejects_bad_shapes() {
    for bad in [
        serde_json::json!("full_passthrough"),
        serde_json::json!({ "mode": ["full_passthrough"] }),
        serde_json::json!({ "max_attempts": 0 }),
        serde_json::json!({ "max_attempts": 101 }),
        serde_json::json!({ "passthrough_upstream_errors": "yes" }),
        serde_json::json!({ "empty_response": "on" }),
        serde_json::json!({ "empty_response": { "detect": "true" } }),
        serde_json::json!({ "empty_response": { "max_attempts": 101 } }),
        serde_json::json!({ "empty_response": { "on_exhausted": "maybe" } }),
    ] {
        let mut config = serde_json::Map::new();
        config.insert("upstream_policy".to_string(), bad);
        assert!(validate_upstream_policy_config(&config).is_err());
    }
}

#[test]
fn remove_upstream_policy_clears_namespace() {
    let mut config = serde_json::Map::new();
    set_upstream_policy(&mut config, &serde_json::json!({"mode": "full_passthrough"})).unwrap();
    remove_upstream_policy(&mut config);
    assert!(config.get("upstream_policy").is_none());
}
```

校验规则：`upstream_policy` 必须是对象；`mode` 若存在必须是字符串且 ∈ {`default`,`full_passthrough`}；`max_attempts` 若存在必须是整数 1..=100；`passthrough_upstream_errors` 若存在必须是布尔；`empty_response` 若存在必须是对象，其 `detect` 布尔、`max_attempts` 整数 0..=100、`on_exhausted` ∈ {`passthrough`,`error`}（存在时）。未知键拒绝（防止拼写错误静默失效）。

- [ ] **Step 2: 运行确认失败** → **Step 3: 实现**

normalize.rs（镜像 `set_responses_websocket_enabled` 模式）：

```rust
pub(crate) fn set_upstream_policy(config: &mut serde_json::Map<String, Value>, policy: &Value) -> Result<(), String> {
    let object = policy.as_object().ok_or_else(|| "upstream_policy 必须是对象".to_string())?;
    validate_upstream_policy_object(object)?;
    config.insert("upstream_policy".to_string(), policy.clone());
    Ok(())
}
pub(crate) fn remove_upstream_policy(config: &mut serde_json::Map<String, Value>) { config.remove("upstream_policy"); }
pub(crate) fn validate_upstream_policy_config(config: &serde_json::Map<String, Value>) -> Result<(), String> {
    match config.get("upstream_policy") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Object(object)) => validate_upstream_policy_object(object),
        Some(_) => Err("upstream_policy 必须是对象".to_string()),
    }
}
fn validate_upstream_policy_object(object: &serde_json::Map<String, Value>) -> Result<(), String> { /* 按上述规则逐项校验 */ }
```

create.rs：`payload.upstream_policy` → `set_upstream_policy`；末尾统一 `validate_upstream_policy_config(&config_map)?`（防御 raw config 绕过）。
update.rs：`fields.contains("upstream_policy")` 块——非 null 走 set，`fields.is_null("upstream_policy")` 走 remove；同样收尾 validate。
summary/value.rs：`"upstream_policy": provider.config.as_ref().and_then(|config| config.get("upstream_policy")).cloned().unwrap_or(Value::Null)`。

- [ ] **Step 4: 运行确认通过 + 架构测试回归**

Run: `cargo test -p aether-gateway --lib admin::provider` 与 `cargo test -p aether-gateway --lib tests::architecture::admin_provider`
Expected: PASS（架构测试若对 create/update builder 有新断言需求，按其报错微调——不得改函数名）

- [ ] **Step 5: Commit**

```bash
git add apps/aether-gateway/src/handlers/admin/provider/
git commit -m "feat(gateway): expose upstream_policy through provider admin API"
```

---

### Task 8: 前端配置界面

**Files:**
- Modify: `frontend/src/api/endpoints/types/provider.ts`（`ProviderConfig.upstream_policy` 类型 + summary 字段 `upstream_policy?`）
- Modify: `frontend/src/features/providers/components/ProviderFormDialog.vue`（form 默认值/resetForm/loadProviderData/submit 四处 + 模板区块）
- Create: `frontend/src/features/providers/components/__tests__/ProviderFormDialog.upstream-policy.spec.ts`

- [ ] **Step 1: 写失败测试**（沿用源码标记断言约定）

```ts
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

const source = readFileSync(resolve(__dirname, "../ProviderFormDialog.vue"), "utf-8");

describe("ProviderFormDialog upstream policy", () => {
  it("renders the upstream policy section", () => {
    expect(source).toContain('data-testid="upstream-policy-setting"');
    expect(source).toContain("upstream_policy");
  });
  it("binds mode, max_attempts, passthrough_upstream_errors and empty_response fields", () => {
    expect(source).toContain("form.upstream_policy_mode");
    expect(source).toContain("form.upstream_policy_max_attempts");
    expect(source).toContain("form.upstream_policy_passthrough_errors");
    expect(source).toContain("form.upstream_policy_empty_detect");
    expect(source).toContain("form.upstream_policy_empty_max_attempts");
    expect(source).toContain("form.upstream_policy_empty_on_exhausted");
  });
  it("resets and loads upstream policy state", () => {
    expect(source).toMatch(/resetForm[\s\S]*upstream_policy_mode/);
    expect(source).toMatch(/loadProviderData[\s\S]*upstream_policy/);
  });
});
```

- [ ] **Step 2: 运行确认失败**

Run: `cd frontend && pnpm vitest run src/features/providers/components/__tests__/ProviderFormDialog.upstream-policy.spec.ts`
Expected: FAIL

- [ ] **Step 3: 实现**

模板区块（放在 responses-websocket 区块之后）：
- `Select` 模式：默认/完全透传（`form.upstream_policy_mode`：`"default" | "full_passthrough"`）
- `InputNumber` 最大尝试次数（1-100，空=不限制 → 提交时省略）
- `Switch` 错误透传（passthrough_upstream_errors）
- 回空子区块：`Switch` 启用检测、`InputNumber` 回空尝试次数（0-100）、`Select` 耗尽行为（passthrough/error）
submit 组装：仅当任一项非默认时构造 `upstream_policy` 对象，mode=default 且其他项全默认 → 提交 `upstream_policy: null`（清除）。
类型：
```ts
export interface UpstreamPolicyEmptyResponseConfig {
  detect?: boolean;
  max_attempts?: number;
  on_exhausted?: "passthrough" | "error";
}
export interface UpstreamPolicyConfig {
  mode?: "default" | "full_passthrough";
  max_attempts?: number;
  passthrough_upstream_errors?: boolean;
  empty_response?: UpstreamPolicyEmptyResponseConfig;
}
```

- [ ] **Step 4: 运行确认通过**

Run: `cd frontend && pnpm vitest run src/features/providers`
Expected: PASS（含既有 responses-websocket spec）

- [ ] **Step 5: Commit**

```bash
git add frontend/src/api/endpoints/types/provider.ts frontend/src/features/providers/
git commit -m "feat(frontend): add upstream policy controls to provider form"
```

---

### Task 9: 运营文档 + 全量门禁

**Files:**
- Create: `docs/upstream-policy.md`

- [ ] **Step 1: 写文档**——场景（NewAPI/Gemini 回空）、三种配置配方（完全透传 / 限额重试+错误透传 / 回空策略）、JSON 示例、与 `failover_rules` 的优先级关系、风险说明（透传模式下游会看到上游原始错误体）。
- [ ] **Step 2: 全量门禁**

```bash
cargo fmt --all
cargo clippy -p aether-gateway --lib --bins -- -D warnings
cargo test -p aether-gateway --lib
```
Expected: 全部通过

- [ ] **Step 3: Commit**

```bash
git add docs/upstream-policy.md
git commit -m "docs(gateway): document upstream policy modes"
```

---

## Self-Review（规格覆盖核对）

| 规格需求 | 任务 |
|---|---|
| F1.1 完全透传模式 | Task 2（分类终止）+ Task 3（cap=1 效果由 mode 保证零重试）+ Task 5/6（回空不干预） |
| F1.2 限额重试后透传上游错误 | Task 3（cap + fallback 返回） |
| F1.3 回空策略（重试 N 次后透传空 200） | Task 4/5/6 |
| Provider 级作用域 | Task 1/7（providers.config） |
| 向后兼容 | 每个 Task 的回归步骤 + Global Constraints |
| 模块化装卸 | 全部配置驱动，未配置=关闭（F4 在计划 A 的体现） |

Key/Model 级覆盖：列为后续（规格 §4 已声明 Provider+Model 两级足够；Model 级见计划 C 的 models.config 钩子）。
