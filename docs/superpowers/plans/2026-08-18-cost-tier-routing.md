# 成本分层智能路由（Cost-Tier Routing）实施计划 — 计划 C / 共 3 份

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax for tracking. 本计划在计划 A/B 完成后执行；执行前将每个任务展开为与计划 A 同等粒度的 TDD 步骤。

**Goal:** 对同一模型的多个上游提供商，按请求上下文长度把流量分到"按量计费"或"按次计费"上游，实现利润最大化；按量侧保持会话/缓存粘性，但利润优先级永远高于粘性。对所有模型生效。

**Architecture:** 请求体在路由决策前已完整解析（planner decision_input 阶段），在此估算上下文 token 数；提供商以 `providers.config.cost_tier` 标记计费层级；候选排序层（candidate_ranking）按"层级匹配优先"重排提供商组——匹配层级的提供商整体排在不匹配之前（利润优先），组内既有优先级/粘性机制全部保留。粘性复用现有 pool sticky（`cache_affinity` preset + `sticky_session_ttl_seconds`），它只在单个提供商池内提升 Key，不会跨提供商与层级排序冲突。

**Tech Stack:** Rust（scheduler-core/planner/pool）、providers.config JSON、系统配置（默认阈值）、Vue 3。

**规格来源:** `docs/superpowers/specs/2026-08-18-upstream-passthrough-cost-routing.md` F3 部分。

## Global Constraints

- 未启用成本路由的模型/提供商：候选顺序逐位不变（回归测试锁定）。
- 估算为启发式（字符数折算 + 消息开销），不引入分词器依赖。
- 层级重排只作用于提供商组之间；组内（endpoint/key）顺序与 pool 调度完全不动。
- 阈值缺省值来自系统配置 `cost_routing.default_threshold_tokens`；模型级覆盖为后续任务（见 Task C5）。
- 估算失败（无法解析请求体）→ 视为大上下文（路由到按次侧，保守控成本）——该默认值由系统配置 `cost_routing.fallback_tier` 控制。

## 配置 Schema

```jsonc
// providers.config（提供商层）
"cost_tier": "per_token" | "per_request"

// system_configs（全局层）
"cost_routing": {
  "enabled": true,
  "default_threshold_tokens": 32000,   // 上下文阈值：< 阈值 → per_token 侧，≥ 阈值 → per_request 侧
  "fallback_tier": "per_request",      // 估算失败时的目标层
  "require_both_tiers": false          // true = 两侧都有可用提供商时才启用重排；false = 单侧存在也生效
}
```

## File Structure

| 文件 | 动作 | 职责 |
|---|---|---|
| `crates/aether-ai/serving/src/context_estimate.rs` | Create | 纯函数上下文 token 估算（openai chat / gemini contents / claude messages 三形状） |
| `apps/aether-gateway/src/orchestration/policy.rs` 或新 `routing/cost_tier.rs` | Modify/Create | `cost_tier` 提供商配置读取（纯函数 + 单测，模板：`responses_websocket_enabled`） |
| `apps/aether-gateway/src/ai_serving/planner/decision_input.rs` | Modify | decision input 增加 `estimated_context_tokens: Option<u64>` |
| `apps/aether-gateway/src/ai_serving/planner/candidate_ranking.rs` | Modify | 层级匹配优先排序（提供商组级） |
| `apps/aether-gateway/src/handlers/admin/system/shared/configs.rs` | Modify | `cost_routing` 系统配置键与校验 |
| Admin/前端 | Modify | 提供商表单增加 cost_tier 选择（模板：responses-websocket） |
| `docs/cost-routing.md` | Create | 运营文档（含 NewAPI 按次/按量双上游配置示例） |

---

### Task C1: 上下文 token 估算（纯函数库）

**Files:**
- Create: `crates/aether-ai/serving/src/context_estimate.rs`（并在 lib.rs 注册）

**Interfaces:**
- Produces:
```rust
pub fn estimate_chat_context_tokens(body_json: &serde_json::Value) -> Option<u64>;
```
- 规则：`messages[]`/`contents[]` 所有字符串内容字符数求和 ÷ 3（中文友好折中系数）+ 每消息 4 token 开销 + `tools[]` 序列化字符 ÷ 3；无任何可识别结构 → None。
- 测试：openai chat（含多轮 + tools）、gemini generateContent contents、claude messages、空对象 → None；数值单调性（长请求 > 短请求）。

### Task C2: cost_tier 配置读取与校验

**Files:**
- Gateway 纯函数：`cost_tier_from_provider_config(Option<&Value>) -> Option<CostTier>`（`CostTier::{PerToken, PerRequest}`）
- Admin normalize：`providers.config.cost_tier` 写入校验（∈ {per_token, per_request}，null 清除）；payload 顶层字段 `cost_tier: Option<String>`；create/update/summary 管道（模板：responses_websocket）
- 前端：ProviderFormDialog 增加"计费层级"选择器 + spec

### Task C3: decision input 携带估算值

**Files:**
- Modify: `apps/aether-gateway/src/ai_serving/planner/decision_input.rs`（:193/:610/:671 的 routing policy 解析处旁，对 chat 类请求计算 `estimate_chat_context_tokens(body_json)` 并存入 decision input 新字段）
- Modify: 消费 decision input 的各 planner 族（standard/openai/chat、passthrough family 等）——只传递，不消费

**Acceptance:** 单测断言 decision input 对含 messages 的 body 产出 Some(n)，对非 chat body 产出 None；既有 decision input 测试不回归。

### Task C4: 候选排序层级重排

**Files:**
- Modify: `apps/aether-gateway/src/ai_serving/planner/candidate_ranking.rs`（`rank_eligible_local_execution_candidates` :126-152）

**行为:**
1. 读取系统配置 `cost_routing`（带 TTL 缓存，沿用现有系统配置读取模式）。
2. 未启用 / 无估算值 / 候选中无任何 cost_tier 标记 → 顺序不变。
3. 启用时：计算目标层级（估算 < 阈值 → PerToken，否则 PerRequest；估算失败 → fallback_tier）；把**匹配目标层级的提供商组稳定地移到不匹配组之前**（stable partition，组内相对顺序与既有排序结果保持不变）；未标记层级的提供商视为匹配（不惩罚未配置者）。
4. `require_both_tiers=true` 且缺少任一侧 → 不重排。

**Acceptance:** 表驱动单测（in-memory transport snapshots）：短请求→per_token 提供商在前；长请求→per_request 在前；未配置 cost_routing → 顺序不变；同层内原顺序保持；pool 组作为一个整体移动。集成测试（tests/ 风格）：两个提供商服务同一模型，短/长请求分别命中预期提供商（`total_attempts`/provider 断言）。

### Task C5: 模型级阈值覆盖（后续）

**方案:** `models.config.cost_routing = { "threshold_tokens": N }`；候选行目前不含 models.config——需扩展 candidate_selection 查询与 `StoredMinimalCandidateSelectionRow`（三驱动 SQL + 契约 + 枚举层透传）。改动面大，单列任务，按 C1-C4 同样的 TDD 流程执行；完成前以全局阈值 + `fallback_tier` 覆盖运营需求。

### Task C6: 文档 + 门禁

- `docs/cost-routing.md`：利润模型说明（为什么短请求走按量、长请求走按次）、NewAPI 双上游配置示例、粘性配置（按量提供商开启 `cache_affinity` + sticky TTL）、利润优先语义（层级排序 > 粘性，粘性仅在所选提供商池内生效）。
- 门禁：fmt / clippy / `cargo test -p aether-gateway --lib` / scheduler-core 相关测试 / 前端测试。

## Self-Review（规格 F3 覆盖）

| 规格条目 | 任务 |
|---|---|
| 阈值分流（< 阈值按量，≥ 阈值按次） | C1 + C4 |
| 对所有模型生效 | C3（chat 族全覆盖）+ 全局阈值缺省；模型级覆盖 C5 |
| 会话/缓存粘性（按量侧） | 复用既有 pool sticky（架构说明在文档 C6） |
| 利润永远优先 | C4 stable partition 在提供商组级排序，先于组内粘性提升 |
| 模块化装卸 | 系统配置 enabled 开关 + 未标记提供商不受影响 |
