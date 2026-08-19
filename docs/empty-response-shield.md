# 回空屏蔽（Empty-Response Shield）

针对"上游持续回空"的本地熔断保护，与 [error-diagnostics](error-diagnostics.md)、
[upstream-policy](upstream-policy.md) 配合使用：

- upstream-policy 决定**单次回空怎么重试/透传**；
- error-diagnostics 负责**记录每一次回空的完整证据**；
- 回空屏蔽负责**在某个会话/请求被谷歌风控反复拦截时停止浪费配额**：不再把请求
  打上上游，而是在网关本地直接返回一个"谷歌安全审查"风格的响应。

> 设计立场：网关**不做风控、不做内容拦截**。屏蔽仅在上游（谷歌）已经多次拦截
> 该会话之后触发，属于止损行为——避免同一会话持续消耗上游按次配额。

## 触发逻辑

1. 每次观测到"空响应"（HTTP 200 但无可见模型输出，与诊断层
   `kind=empty_response` 完全同一检测点）时，对**屏蔽键**记一次 strike：
   - 请求体带有会话标识（`prompt_cache_key` / `conversation_id` / `session_id`
     等，与号池粘性会话同一套提取规则）→ 键为 `session:{会话ID}`；
   - 否则退化为**请求指纹**：`fp:{sha256(去凭据请求头 + 规范化请求体)}`，
     即"相同请求体/请求头"的请求视为同一来源。
2. 分发前的屏蔽检查：当某个键在 `window_secs` 窗口内累计达到 `threshold`
   次 strike，该键被屏蔽 `block_secs`（默认 300 秒 = 5 分钟）。屏蔽窗口从
   最近一次 strike 起算，持续回空会自动续期。
3. 被屏蔽的聊天请求**不再分发到上游**，网关本地直接返回安全审查风格响应
   （HTTP 200，按客户端格式构造）：
   - OpenAI Chat：`finish_reason: "content_filter"`（非流式为完整 completion，
     流式为 SSE chunk + `[DONE]`）；
   - Gemini 原生：`candidates[0].finishReason: "SAFETY"` +
     `promptFeedback.blockReason: "SAFETY"`；
   - Claude：空 content + `stop_reason: "refusal"`。
   - 响应体附带 `aether_shield` 标记（`blocked`/`reason`/`retry_after_secs`），
     便于下游识别这是网关本地熔断而非真实模型输出。
   - 非聊天类请求（图片/视频/embedding 等）不在屏蔽响应范围内，直接放行。

每次本地拦截都会写入一条用量记录（`provider_name=empty-response-shield`、
`error_diagnostic.kind=empty_response_shield`），因此在**错误诊断页**可以按
类型"回空屏蔽"检索到每一次拦截（含会话ID、请求指纹、被拦截请求体）。

## 配置（可装卸模块）

系统配置键 `empty_response_shield`。**未配置 = 模块未安装**，零开销直通：

```jsonc
"empty_response_shield": {
  "enabled": true,        // false 时同样视为未安装
  "threshold": 3,         // 窗口内多少次回空触发屏蔽（默认 3）
  "window_secs": 600,     // strike 统计窗口（默认 600 秒）
  "block_secs": 300       // 屏蔽时长（默认 300 秒 = 5 分钟）
}
```

屏蔽状态保存在内存（带 TTL 清理与容量上限），无数据库迁移；网关重启后屏蔽
自然解除。

> 注意：屏蔽键直接使用**客户端提供的**会话标识（`prompt_cache_key` 优先，它
> 本身可被多个调用方共享）。若不同用户共享同一会话标识，其中一方的持续回空
> 可能连带屏蔽另一方，最长 `block_secs`，窗口过期后自动解除。需要更严格隔离
> 时请让下游为不同用户生成不同的会话标识。

## 管理端 API

复用 `admin:usage` 权限：

- `GET /api/admin/empty-response-shield` — 返回模块安装状态、当前配置与全部被
  屏蔽键：

  ```jsonc
  {
    "installed": true,
    "config": { "threshold": 3, "window_secs": 600, "block_secs": 300 },
    "blocked": [
      { "key": "session:conv-123", "kind": "session", "remaining_secs": 240 }
    ],
    "total": 1
  }
  ```

- `DELETE /api/admin/empty-response-shield/{key}` — 手动解除屏蔽（key 需 URL
  编码），返回 `{ "key": ..., "unblocked": true|false }`。

## 会话ID 可见性

无论是否触发屏蔽，所有 AI 请求的会话标识都会持久化：

- 请求体带会话标识 → 用量记录 `request_metadata.session_id`；
- 无会话标识 → `request_metadata.request_fingerprint`。

管理前端"使用记录"表格提供"会话ID"列（列设置中开启），"错误诊断"列表/详情
同样展示会话ID与请求指纹，用于精确定位并封禁滥用来源。

## 相关文档

- [upstream-policy](upstream-policy.md) — 单次回空的重试/透传策略
- [error-diagnostics](error-diagnostics.md) — 回空取证与 AI 摘要
- [cost-tier-routing](cost-tier-routing.md) — 成本分层路由
