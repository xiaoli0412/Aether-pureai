# 错误诊断取证（Error Diagnostics）实施计划 — 计划 B / 共 3 份

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax for tracking. 本计划在计划 A 完成后执行；执行前将每个任务展开为与计划 A 同等粒度的 TDD 步骤。

**Goal:** 让回空、4xx、5xx 等上游异常事件可被精准检索、取证（请求头/请求体/上游错误原文）并可生成 AI 摘要，支撑定位与封禁滥用下游用户。

**Architecture:** 零迁移设计——诊断元数据经用量管线的 `request_metadata` 允许列表持久化（新键 `error_diagnostic`），完整请求头/体复用既有 `usage_http_audits` + `usage_body_blobs`（`request_record_level=full` 默认开启）。管理端新增诊断列表/详情/摘要三个端点。AI 摘要为按需模块：系统配置未配置摘要器时端点明确拒绝（可装卸）。

**Tech Stack:** Rust（aether-usage runtime、aether-data、aether-admin、gateway handlers）、SQL JSON 查询（三驱动等价实现）、Vue 3。

**规格来源:** `docs/superpowers/specs/2026-08-18-upstream-passthrough-cost-routing.md` F2 部分。

## Global Constraints

- 不新增数据库表/列（零迁移，三驱动兼容）；完整头/体已在 full 记录级别持久化，诊断层只加分类与指针。
- 持久化内容遵守捕获时脱敏约定（`DEFAULT_SENSITIVE_HEADERS` 掩码、body 敏感键掩码）——不存储明文密钥。
- `request_metadata` 限制：总量 16KB、单字符串 1KB——`error_diagnostic` 中超长上游错误体截断并标记。
- 架构测试 `tests/architecture/usage.rs` 的分层约束必须保持（分类逻辑不入 reporting 模块）。

## 数据模型

```jsonc
// usage.request_metadata.error_diagnostic（新允许键）
{
  "kind": "empty_response" | "upstream_4xx" | "upstream_5xx" | "transport_error",
  "upstream_status": 400,
  "error_message": "...",            // 上游错误消息提取（≤1KB）
  "classification": "stop_status_code" | ...,
  "decision": "stop_local_failover" | "retry_next_candidate" | ...,
  "policy_mode": "full_passthrough" | "default",
  "summary": "AI 摘要（生成后回填，可选）",
  "summarized_at_unix_secs": 1234567890
}
```

检索键：`usage.user_id`、`usage.api_key_id`、`usage.status_code`、`request_metadata->'error_diagnostic'->>'kind'`、时间窗。完整取证经现有 `find_by_request_id`（头+四个 body ref）获取。

---

### Task B1: 诊断元数据注入（用量运行时）

**Files:**
- Modify: `crates/aether-usage/runtime/src/request_metadata.rs`（允许列表加 `error_diagnostic`，含嵌套深度/大小校验测试）
- Modify: `apps/aether-gateway/src/orchestration/mod.rs`（`build_local_error_flow_metadata` 旁新增 `build_error_diagnostic_metadata(kind, status, classification, decision, error_message, policy_mode)` 纯函数 + 单测）
- Modify: `apps/aether-gateway/src/execution_runtime/sync/execution.rs`、`stream/execution.rs`、`executor/outcome.rs`（在既有 error_flow/upstream_response 注入点旁注入 `error_diagnostic`；回空检测点打 `kind=empty_response`——依赖计划 A Task 5/6 的挂钩）

**Acceptance:** 单测断言：回空/4xx/5xx 终态事件的 `request_metadata` 含正确 kind；超限字段被截断占位；无错误时不注入。

### Task B2: 诊断检索（数据层，零迁移）

**Files:**
- Modify: `crates/aether-data/contracts/src/repository/usage/types.rs`（`UsageAuditListQuery` 增 `diagnostic_kind: Option<String>`）
- Modify: postgres/mysql/sqlite 三适配器的 list/count 动态 where 构造（`request_metadata->'error_diagnostic'->>'kind' = ?` 的驱动等价写法：postgres `->>'`、sqlite `json_extract`、mysql `JSON_UNQUOTE(JSON_EXTRACT(...))`）+ 各自单测（纯 helper 级）

**Acceptance:** 按 kind+user_id+时间窗过滤的用例在三驱动 helper 测试中通过；不带过滤时查询计划不变（SQL 前缀测试不回归）。

### Task B3: 管理端诊断 API

**Files:**
- Create: `apps/aether-gateway/src/handlers/admin/observability/diagnostics/`（mod.rs + list.rs + detail.rs + summarize.rs）
- Modify: 管理路由注册处（跟随 `handlers/admin/observability/usage/` 现有挂载模式）
- 复用: `crates/aether-admin/src/observability/usage.rs` 的 detail/body 解析 helpers（`admin_usage_resolve_request_capture_body`）

**Endpoints:**
- `GET /api/admin/diagnostics?kind=&user_id=&api_key_id=&from=&to=&limit=&offset=` → 分页列表（kind、上游状态、用户、Key、模型、时间、request_id）
- `GET /api/admin/diagnostics/{request_id}` → 完整取证 JSON：诊断元数据 + 四组头（脱敏后）+ 四个 body（经 body_ref 解压）+ 上游错误原文
- `POST /api/admin/diagnostics/{request_id}/summarize` → AI 摘要（见 B4）

**Acceptance:** 网关行为测试（`tests/` 风格，in-memory repos）：注入诊断元数据后可按 kind 列出、详情返回完整 JSON、未认证/非管理员拒绝。

### Task B4: AI 摘要模块（可装卸）

**Files:**
- Create: `apps/aether-gateway/src/diagnostics_summarizer.rs`（或 admin observability 内子模块）
- 系统配置: `error_diagnostic_summarizer = { "base_url": "...", "api_key": "...", "model": "..." }`（未配置 → 端点返回 503 + 明确错误码，功能即"卸下"）

**行为:** 收到摘要请求 → 组装提示（kind、上游状态、上游错误体节选、请求路径/模型，**不含**下游请求体全文以控成本）→ 直接 reqwest 调用 OpenAI 兼容 chat completions → 摘要写回 `request_metadata.error_diagnostic.summary`（经 usage 仓储的定向 upsert；若仓储无 metadata 局部更新方法则新增最小方法，三驱动实现）。结果缓存（已有 summary 且未过期 → 直接返回）。

**Acceptance:** mock 摘要器的单测（成功写回、上游失败返回 502、未配置返回 503、重复请求命中缓存）。

### Task B5: 前端诊断视图（最小可用）

**Files:**
- Create: `frontend/src/features/diagnostics/`（列表页：过滤器 + 表格；详情抽屉：诊断元数据 + 请求/响应 JSON 查看器 + "生成摘要"按钮）
- Modify: 路由与导航菜单注册

**Acceptance:** vitest 组件/源码测试通过；`pnpm build` 通过。

### Task B6: 文档 + 门禁

- Create: `docs/error-diagnostics.md`（取证流程：定位用户 → 查看回空/400 证据 → 封禁该 Key 的操作指引）
- 门禁: fmt / clippy（gateway+data+admin 相关包）/ 相关 cargo test / 前端测试

## Self-Review（规格 F2 覆盖）

| 规格条目 | 任务 |
|---|---|
| F2.1 回空取证（头/体/上游错误码） | B1（kind=empty_response）+ B3 详情端点（头+体+上游原文） |
| F2.2 400 类取证 + AI 摘要 + 原始 JSON | B1（kind=upstream_4xx）+ B4 + B3 详情 |
| F2.3 精准检索 + 保留返回体 + 封禁支撑 | B2（kind/用户过滤）+ full 记录级别保留体 + B5/B6 操作指引 |
| 模块化装卸 | B4 系统配置开关 |
