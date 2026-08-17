# 上游策略（Upstream Policy）

为单个提供商配置**上游响应的透传与重试行为**。典型场景：上游是按次计费的 NewAPI/账号反代链路（如 Gemini），网关的自动重试与错误拦截会直接放大成本，甚至把"上游扣费但回空"的损失留给运营者自己承担。

配置入口：管理端 → 提供商 → 创建/编辑 → **上游策略** 区块；等价于 `providers.config.upstream_policy` JSON 命名空间。未配置时，提供商保持既有的故障转移行为（完全向后兼容）。

## 配置结构

```jsonc
"upstream_policy": {
  "mode": "default" | "full_passthrough",   // 缺省 default
  "max_attempts": 2,                         // 可选，1-100：该提供商每请求总上游尝试次数硬上限
  "passthrough_upstream_errors": true,       // 可选：重试耗尽/终止时把上游原始错误透传下游
  "empty_response": {                        // 可选：200 但无可见内容（风控回空）
    "detect": true,                          // 非 gemini:generate_content 格式需显式开启
    "max_attempts": 1,                       // 0-100：回空持续发生时愿意花费的尝试次数
    "on_exhausted": "passthrough" | "error"  // 预算耗尽后：透传空 200 / 按失败处理
  }
}
```

## 三种典型配方

### 1. 完全透传（上游返回什么，下游收到什么）

```json
{ "upstream_policy": { "mode": "full_passthrough" } }
```

- 任何状态码（4xx/5xx）**不重试、不拦截**，上游响应原样下发；
- 传输层错误（连不上上游等）也不再轮换候选，直接终止；
- 回空（200 无内容）同样原样下发；
- 同格式请求（如 openai:chat → openai:chat）为字节级透传；跨格式时仍做必要的格式转换，但不做任何重试与错误改写。

### 2. 限额重试 + 上游错误透传

```json
{ "upstream_policy": { "max_attempts": 2, "passthrough_upstream_errors": true } }
```

- 最多 2 次上游尝试；
- 耗尽后，下游收到**最后一次的上游原始错误**（状态码 + 错误体），而不是网关生成的 503。

> 未开启 `passthrough_upstream_errors` 时，耗尽走既有行为（网关 503 + 原因头）。

### 3. 回空策略（Gemini 风控场景）

```json
{
  "upstream_policy": {
    "empty_response": { "detect": true, "max_attempts": 1, "on_exhausted": "passthrough" }
  }
}
```

语义：检测到"HTTP 200 但无可见输出"时先重试 1 次；仍然回空则把**空 200 原样透传给下游**——由下游承担"谷歌官方回空但上游照常扣费"的后果，而不是网关拦截成 503 把损失留给运营者。

- `gemini:generate_content` 格式恒定检测回空（保持既有行为，无需 `detect`）；
- `openai:chat` 等格式需 `detect: true` 才启用（同步与流式均生效；流式在提交下游前预取判定，因此仍有机会重试）；
- `on_exhausted: "error"` 保持旧的失败链路（502 重写 → 重试 → 可能耗尽为网关 503）。

## 行为细节

| 场景 | 默认行为 | 启用上游策略后 |
|---|---|---|
| 上游 5xx | 轮换候选重试 | 受 `max_attempts` 上限约束；透传模式下直接下发 |
| 上游 4xx | 轮换候选重试 | 同上（透传模式下立即下发原始 400 错误体） |
| 200 回空（gemini 格式） | 502 重写并重试，耗尽 503 | 受 `empty_response` 预算控制 |
| 200 回空（openai:chat） | 静默透传空流 | `detect: true` 时受预算控制 |
| 耗尽后的最终响应 | 网关 503 | `passthrough_upstream_errors` 时为最后一次上游错误 |

- `max_attempts` 计入该提供商在一个请求内的所有上游尝试（含回空重试）；回空预算独立计数；
- 传输快照在候选规划时内嵌该策略，单个请求内策略一致；
- 修改配置即时生效（新请求），无需重启。

## 与其他配置的关系

- `failover_rules`（stop/continue 状态码、正则规则）在 `mode: default` 下继续生效；`full_passthrough` 覆盖一切重试决策；
- `max_transfer_count` / `max_transfer_timeout_seconds` 的换 Key 预算不受影响，`max_attempts` 是更外层的硬闸；
- 健康分、熔断等反馈效果不变（透传模式下的 5xx 仍计入上游健康问题）。
