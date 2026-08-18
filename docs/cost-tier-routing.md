# 成本分层路由（Cost-Tier Routing）

同一模型可按"请求上下文规模"分流到不同计费模式的上游，在满足会话/缓存粘性的前提下
**最大化利润**。配置挂在 provider 的 `config.cost_tier`（零迁移），`enabled=false`（默认）
即等价于未启用，路由行为与现状完全一致。

## 配置结构

```jsonc
"cost_tier": {
  "enabled": true,
  "context_threshold_tokens": 32000,   // 上下文阈值（token）
  "tiers": {
    "below": { "prefer": "per_use" },      // 低于阈值 → 偏好按量计费
    "above": { "prefer": "per_request" }   // 高于阈值 → 偏好按次计费
  },
  "stickiness": {
    "respect_session_affinity": true,   // 默认 true
    "respect_cache_affinity": true,     // 默认 true
    "max_profit_sacrifice_usd": 0.0     // 为粘性可放弃的最大利润差；0=利润绝对优先
  }
}
```

- `prefer` 取值：`per_request`（按次结算，`price_per_request`）或 `per_use`（按 token 计量）。
- 省略 `tiers.below`/`tiers.above` 或 `prefer` 非法时，该档位不参与偏好重排。
- `stickiness` 省略时使用默认值（尊重会话与缓存粘性，利润差容忍为 0）。

## 校验规则（管理端写入时）

- 顶层仅允许 `enabled` / `context_threshold_tokens` / `tiers` / `stickiness`。
- `context_threshold_tokens` 必须是 **大于 0** 的整数。
- `tiers` 仅允许 `below` / `above`，各自仅允许 `prefer`，且取值必须是 `per_request`/`per_use`。
- `stickiness` 仅允许 `respect_session_affinity` / `respect_cache_affinity` / `max_profit_sacrifice_usd`；
  布尔字段必须为布尔，`max_profit_sacrifice_usd` 必须是非负有限数字。

## 当前状态

- **已完成（C1）**：配置解析、请求时附加到本地故障策略并经 report context 往返、
  管理端 create/update 透传与校验、provider 摘要投影、前端类型。未启用时零影响。
- **待实现（C2/C3）**：请求上下文估算（复用 billing token 估算）→ 依据阈值确定目标档位
  → 候选按"利润优先、粘性次之"重排（粘性仅在不超出 `max_profit_sacrifice_usd` 时保留）。
  该行为层落地后，`enabled=true` 才真正影响路由。

## 相关文档

- [upstream-policy](upstream-policy.md) — 上游透传/重试策略
- [error-diagnostics](error-diagnostics.md) — 失败事件取证与检索
