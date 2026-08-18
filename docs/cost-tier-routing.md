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
- **已完成（C2/C3）**：行为层落地，`enabled=true` 现在真正影响路由：
  - **C2 上下文估算**：复用 billing token 估算器（`estimate_request_context_tokens`，
    输入 token + 显式 cache-read/creation token，无 tokenizer 依赖），在每个请求进入
    候选物化前估算一次；估算失败（None）即不重排（零影响默认）。
  - **C3 候选重排**：`ai_serving/planner/cost_tier_routing.rs` 可装卸模块，在候选
    **resolve + rank 之后、resolved-page 缓存读取之后** 运行（避免按上下文大小污染
    共享缓存页），按"目标档位匹配者优先、同档按估算利润降序（收入恒定故利润 = 负的
    结算成本）、中性居中、不匹配沉底"稳定重排；粘性（会话/缓存亲和）仅在利润损失不
    超过 `max_profit_sacrifice_usd` 时保留，否则利润优先。
  - 计费上下文读取复用数据层既有的单飞缓存；计费缺失/计算失败的候选降级为"中性"，
    重排永不使请求失败。懒加载分页路径下重排按页生效（常见单页即覆盖全部候选）。
  - 回归保证：未启用/未配置/无估算/单候选/无匹配候选时，候选顺序与未启用完全一致。

## 相关文档

- [upstream-policy](upstream-policy.md) — 上游透传/重试策略
- [error-diagnostics](error-diagnostics.md) — 失败事件取证与检索
