# 错误诊断取证（Error Diagnostics）

针对"上游回空 / 4xx / 5xx"等异常事件的精准检索与取证能力。配合
[upstream-policy](upstream-policy.md) 使用：upstream policy 决定**如何重试/透传**，
诊断层负责**记录并让你查到每一次失败的完整证据**，用于定位并封禁滥用下游用户。

## 它解决什么

| 场景 | 能力 |
|---|---|
| 上游回空（HTTP 200 但无内容）被记为失败 | `kind=empty_response` 事件可被检索，详情含请求头/体与上游错误原文 |
| 下游构造性 400 被上游网关拦截 | `kind=upstream_4xx` 事件保留响应体与上游错误，可生成 AI 摘要 |
| 定位某个用户/Key 的滥用行为 | 按 `kind`、`user_id`、`api_key_id`、时间窗精准过滤，保留完整请求/响应体 |

## 零迁移设计

诊断不新增任何数据库表/列，全部复用既有用量管线：

- **诊断标记**：失败终态时，网关在 `usage.request_metadata.error_diagnostic` 写入一个紧凑、
  可查询的标记（经用量元数据允许列表持久化）：

  ```jsonc
  "error_diagnostic": {
    "kind": "empty_response",        // empty_response | upstream_4xx | upstream_5xx
    "upstream_status": 200,          // 上游 HTTP 状态码
    "classification": "...",         // 本地故障分类
    "decision": "...",               // 编排决策（重试/停止/透传）
    "message": "..."                 // 上游错误消息提取（≤1KB，超长截断）
  }
  ```

- **完整取证**：请求头/请求体/上游响应等已在 `request_record_level=full`（默认开启）时持久化
  于 `usage_http_audits` + `usage_body_blobs`，诊断详情端点按需经 `body_ref` 解压还原。
- **脱敏**：持久化遵守捕获时的脱敏约定（敏感头/密钥掩码），不存明文密钥。

## 管理端 API

三个端点挂在 `/api/admin/diagnostics`，复用 `admin:usage` 权限（GET 需 `usage:read`）：

### `GET /api/admin/diagnostics`

分页列出诊断事件。查询参数：

| 参数 | 说明 |
|---|---|
| `kind` | 诊断类别（`empty_response` / `upstream_4xx` / `upstream_5xx`） |
| `user_id` | 按下游用户过滤 |
| `api_key_id` | 按下游 API Key 过滤 |
| `from` / `to` | Unix 秒时间窗（`to` 为上界，驱动按"小于"处理） |
| `limit` / `offset` | 分页（`limit` 默认 100，最大 500） |
| `newest_first` | 默认 `true` |

返回 `{ records: [...], total, limit, offset }`。每条 record 含
`request_id`、`kind`、`upstream_status`、`user_id`、`api_key_id`、`model`、
`provider_name`、`status_code`、`created_at` 等。

### `GET /api/admin/diagnostics/{request_id}`

按 `request_id` 返回单个事件的**完整取证 JSON**：

- `error_diagnostic`：诊断标记
- `request` / `provider_request` / `response` / `client_response`：各自的
  `headers`（脱敏后）与 `body`（经 `body_ref` 解压）
- 维度信息：`user_id`、`api_key_id`、`model`、`provider_name`、`status_code`、
  `error_message`、`error_category` 等

查询参数 `include_bodies`（默认 `true`）可跳过 body 解析只取元数据。未知 `request_id`
返回 `404`；未启用用量数据读取时返回 `503`。

### `POST /api/admin/diagnostics/{request_id}/summarize`

AI 摘要（可装卸模块，见下）。

## AI 摘要（可装卸模块）

摘要器通过系统配置 `error_diagnostic_summarizer` 启停：

```jsonc
"error_diagnostic_summarizer": {
  "base_url": "https://api.openai.com/v1",
  "api_key": "sk-...",
  "model": "gpt-4o-mini"
}
```

- **未配置** = 模块"卸下"：`summarize` 端点明确拒绝（当前实现返回 `501`，
  配置化摘要器接入后为 `503` + 明确错误码）。
- **已配置** = 模块"装上"：组装提示（kind、上游状态、上游错误体节选、请求路径/模型；
  不含下游请求体全文以控成本）→ 调 OpenAI 兼容 chat completions → 摘要回填
  `request_metadata.error_diagnostic.summary` 并缓存。

## 封禁一个滥用用户的操作流程

1. `GET /api/admin/diagnostics?kind=upstream_4xx&api_key_id=key-xxx` 列出该 Key 的 400 事件。
2. 对可疑事件逐条 `GET /api/admin/diagnostics/{request_id}` 查看完整请求头/体证据。
3. 确认后，经 API Key 管理端封禁该 Key（参见 provider/pool 管理文档）。

## 相关文档

- [upstream-policy](upstream-policy.md) — 上游透传/重试策略（决定失败如何产生）
