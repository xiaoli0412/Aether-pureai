# 需求文档

## 简介

本功能为 Aether 网关添加**入站协作协议**，使其能够接收来自 New API 的转发请求、向 New API 导出定价与用量数据、消费 New API 事件流并发布自身事件流。Aether 作为 New API 的一个"渠道"（channel），当 New API 选择 Aether 渠道时，将请求带签名上下文转发至 Aether，Aether 利用自身 relay 引擎完成上游路由。

## 术语表

- **Collaboration_Module**: Aether 网关中负责 New API 协作协议的模块（`apps/aether-gateway/src/collaboration/`）
- **Signature_Verifier**: 签名验证组件，负责 HMAC-SHA256 签名校验与上下文解析
- **Relay_Engine**: 现有中转路由引擎，负责上游聚合与智能路由
- **Pricing_Exporter**: 定价快照导出服务
- **Usage_Exporter**: 用量与经营汇总导出服务
- **Event_Publisher**: 事件发布服务，供 New API 拉取 Aether 事件
- **Event_Consumer**: 事件消费服务，从 New API 拉取事件流
- **Routing_Mode_Module**: 路由模式模块，支持多种路由策略切换
- **Credential_Store**: 双向凭据管理组件
- **Balance_Observer**: 上游余额观测组件
- **New_API**: 外部协作系统（不由本项目修改）
- **Signed_Context**: New API 转发请求时附带的签名上下文（含 request_id、user_ident、model、group、expires_at）
- **PSK**: 预共享服务凭据（Pre-Shared Key），用于 HMAC-SHA256 签名

## 需求

### 需求 1：签名上下文接收与验证

**用户故事：** 作为 Aether 运维人员，我希望网关能验证来自 New API 的签名上下文，以确保只有经过认证的转发请求被处理。

#### 验收标准

1. WHEN Collaboration_Module 收到带有 `X-Aether-Signature` header 的请求, THE Signature_Verifier SHALL 使用预共享密钥（PSK）通过 HMAC-SHA256 算法验证签名完整性
2. WHEN 签名验证通过, THE Signature_Verifier SHALL 解析 `X-Aether-Context` header 为包含 request_id、user_ident、model、group、expires_at 字段的 JSON 结构
3. IF 签名验证失败, THEN THE Collaboration_Module SHALL 返回 HTTP 401 并拒绝处理该请求
4. IF Signed_Context 中的 expires_at 早于当前服务器时间, THEN THE Collaboration_Module SHALL 返回 HTTP 401 并记录过期拒绝事件
5. THE Collaboration_Module SHALL 拒绝包含用户原始 API key 的请求，仅依赖 Signed_Context 进行身份识别
6. WHEN 收到合法转发请求, THE Collaboration_Module SHALL 传播请求中的 `X-Oneapi-Request-Id` header 至下游调用链
7. WHEN 收到合法转发请求且存在 `traceparent` header, THE Collaboration_Module SHALL 按 W3C Trace Context 规范传播该 header

### 需求 2：入站请求处理

**用户故事：** 作为 Aether 运维人员，我希望网关能接收 New API 转发的 AI 请求并通过 relay 引擎完成路由与执行，以便为 New API 提供上游聚合能力。

#### 验收标准

1. WHEN Collaboration_Module 收到经签名验证通过的请求, THE Relay_Engine SHALL 根据 Signed_Context 中的 model 和 group 字段执行路由决策
2. THE Collaboration_Module SHALL 支持 OpenAI、Claude、Gemini 协议语义的请求转发
3. WHEN 上游返回同步响应, THE Collaboration_Module SHALL 将完整响应体原样返回给 New API
4. WHEN 上游返回流式（SSE）响应, THE Collaboration_Module SHALL 以 SSE 格式逐块转发响应给 New API
5. THE Collaboration_Module SHALL 在响应 header 中包含上游请求 ID，供 New API 进行日志关联
6. IF 上游请求失败, THEN THE Collaboration_Module SHALL 返回包含 error_code、error_message、upstream_request_id 字段的标准 JSON 错误响应

### 需求 3：定价快照导出 API

**用户故事：** 作为 New API 系统，我需要拉取 Aether 的定价数据，以便在 New API 侧向用户展示准确的定价信息。

#### 验收标准

1. WHEN New API 发送 GET 请求至 `/api/aether/export/pricing`, THE Pricing_Exporter SHALL 返回包含所有已配置模型定价信息的 JSON 响应
2. THE Pricing_Exporter SHALL 在每条模型定价记录中包含：模型 ID、输入 token 单价（quota 单位）、输出 token 单价（quota 单位）、分组差异定价、能力标签（vision、audio、tools）
3. THE Pricing_Exporter SHALL 支持图像定价、音频定价、固定价格和阶梯定价的表达式格式
4. WHEN 请求包含有效的 Aether→New API 只读凭据, THE Pricing_Exporter SHALL 返回定价数据；IF 凭据无效, THEN THE Pricing_Exporter SHALL 返回 HTTP 403
5. WHEN 请求包含 `If-None-Match` header 且值与当前 ETag 匹配, THE Pricing_Exporter SHALL 返回 HTTP 304 Not Modified 而非完整响应体
6. THE Pricing_Exporter SHALL 在响应中包含 `ETag` header 以支持客户端缓存

### 需求 4：用量与经营汇总导出

**用户故事：** 作为 New API 系统，我需要获取 Aether 的用量与经营汇总数据，以便进行统一的数据分析和账单核对。

#### 验收标准

1. WHEN New API 发送 GET 请求至 `/api/aether/export/usage-summary` 并指定日期范围参数, THE Usage_Exporter SHALL 返回该日期范围内的用量汇总数据
2. THE Usage_Exporter SHALL 在汇总数据中包含以下维度：Token 数、请求数、charged quota、按模型细分、按分组细分、按渠道细分
3. THE Usage_Exporter SHALL 在汇总数据中包含已知下游收费金额和上游成本金额
4. THE Usage_Exporter SHALL 确保返回数据中不包含任何原始 API 密钥或凭据信息
5. WHEN 请求包含有效的 Aether→New API 只读凭据, THE Usage_Exporter SHALL 返回用量数据；IF 凭据无效, THEN THE Usage_Exporter SHALL 返回 HTTP 403

### 需求 5：增量事件消费（从 New API 拉取）

**用户故事：** 作为 Aether 运维人员，我希望网关能主动从 New API 拉取事件流，以便同步用户充值、订阅变化等经营信息。

#### 验收标准

1. THE Event_Consumer SHALL 使用游标分页（cursor-based pagination）从 New API 的事件 API 拉取事件
2. THE Event_Consumer SHALL 使用 instance_id 和 event_id 组合进行事件去重，确保同一事件不被重复处理
3. THE Event_Consumer SHALL 支持以下事件类型：用量结算、充值/退款、订阅变化、渠道配置变化、渠道余额观测、模型/分组/价格变化
4. WHEN Aether 与 New API 之间网络连接恢复后, THE Event_Consumer SHALL 从上次成功处理的游标位置自动补拉所有遗漏事件
5. THE Event_Consumer SHALL 确保不会向 New API 回写用户余额数据

### 需求 6：增量事件发布（供 New API 拉取）

**用户故事：** 作为 New API 系统，我需要拉取 Aether 的事件流，以便感知路由变更、通道健康变化等信息。

#### 验收标准

1. WHEN New API 发送 GET 请求至 `/api/aether/events` 并携带 cursor 和 limit 参数, THE Event_Publisher SHALL 返回从该游标之后的事件列表
2. THE Event_Publisher SHALL 支持以下事件类型：路由决策变更、通道健康变化、定价更新、成本异常
3. THE Event_Publisher SHALL 为每个事件分配全局唯一的 event_id，确保幂等消费
4. THE Event_Publisher SHALL 保留事件数据至少 30 天
5. WHEN 请求包含有效的 Aether→New API 只读凭据, THE Event_Publisher SHALL 返回事件数据；IF 凭据无效, THEN THE Event_Publisher SHALL 返回 HTTP 403

### 需求 7：路由模式模块

**用户故事：** 作为 Aether 架构师，我希望路由模式是可切换的，以便未来支持仅建议模式和影子模式等高级协作策略。

#### 验收标准

1. THE Routing_Mode_Module SHALL 作为独立模块实现，支持通过 `aether_routing_mode` 配置项切换路由策略
2. WHEN `aether_routing_mode` 配置为 `direct_channel`, THE Routing_Mode_Module SHALL 接受转发请求并通过 Relay_Engine 执行路由后返回结果
3. WHEN `aether_routing_mode` 配置为 `parallel_shadow`, THE Routing_Mode_Module SHALL 仅返回路由建议和成本预测而不执行实际上游请求
4. WHEN `aether_routing_mode` 配置为 `aether_decision`, THE Routing_Mode_Module SHALL 返回带 TTL 和 decision_id 的路由建议供 New API 决定是否采用
5. WHILE `aether_routing_mode` 为 `parallel_shadow`, THE Routing_Mode_Module SHALL 确保不产生任何真实上游 API 调用或副作用

### 需求 8：双向凭据管理

**用户故事：** 作为 Aether 运维人员，我希望协作双方的凭据被安全管理，以确保通信安全且最小权限原则得到执行。

#### 验收标准

1. THE Credential_Store SHALL 分别存储 New API→Aether 转发凭据（服务间认证）和 Aether→New API 只读导出凭据（事件拉取用）
2. THE Credential_Store SHALL 对所有凭据进行加密存储
3. THE Credential_Store SHALL 支持凭据轮换操作，新凭据生效后旧凭据在宽限期内仍可使用
4. THE Credential_Store SHALL 支持凭据即时撤销操作
5. WHILE 运行于生产环境, THE Collaboration_Module SHALL 强制使用 TLS 连接进行所有跨服务通信
6. THE Credential_Store SHALL 确保转发凭据与导出凭据完全分离，不可混用

### 需求 9：上游余额观测

**用户故事：** 作为 Aether 运维人员，我希望网关能定期观测上游渠道余额，以便在余额不足时及时告警。

#### 验收标准

1. THE Balance_Observer SHALL 按可配置的时间间隔定期查询每个上游渠道的余额
2. THE Balance_Observer SHALL 记录每次查询的余额值，形成余额变化趋势数据
3. WHEN 某上游渠道余额低于配置的告警阈值, THE Balance_Observer SHALL 触发告警通知
4. THE Balance_Observer SHALL 支持适配不同上游平台的余额查询 API（如 `GET /api/user/self` 或等价接口）

### 需求 10：历史数据完整性

**用户故事：** 作为 Aether 运维人员，我希望历史经营数据保持完整且真实，以便回溯分析时得到可信结果。

#### 验收标准

1. THE Usage_Exporter SHALL 保留原始 charged quota 和真实充值金额，不进行篡改或汇总丢失
2. IF 某条记录缺少 quota_per_unit 或上游实际成本数据, THEN THE Usage_Exporter SHALL 在该记录中标记"金额/成本未知"字段
3. THE Usage_Exporter SHALL 确保不伪造利润数据——当成本或收入数据不完整时明确标注
4. THE Usage_Exporter SHALL 支持按时间范围回溯查询带完整性标记的历史记录

### 需求 11：可观测性与关联追踪

**用户故事：** 作为 Aether 运维人员，我希望跨系统的请求可以被完整追踪，以便快速定位协作流程中的问题。

#### 验收标准

1. THE Collaboration_Module SHALL 在所有协作请求的日志中统一传播 `X-Oneapi-Request-Id` 和 W3C trace context
2. THE Collaboration_Module SHALL 在响应中返回上游请求 ID
3. THE Collaboration_Module SHALL 生成包含 request_id、decision_id、channel_id、latency 字段的结构化日志
4. WHEN 一个请求经过 New API → Aether → 上游完整链路, THE Collaboration_Module SHALL 确保该请求的所有日志可通过 request_id 完整关联

### 需求 12：安全与回退

**用户故事：** 作为 New API 运维人员，我希望可以随时停用 Aether 渠道并保持最终执行权，以确保系统安全和业务连续性。

#### 验收标准

1. THE Collaboration_Module SHALL 支持通过配置项即时停用 Aether 渠道功能，停用后拒绝所有转发请求并返回 HTTP 503
2. THE Collaboration_Module SHALL 确保 New API 始终拥有最终执行权——Aether 不缓存或延迟执行 New API 的路由决策
3. THE Collaboration_Module SHALL 对 New API 的临时渠道仅执行观测操作，不持有其 API 密钥
4. IF 收到的 Signed_Context 中 request_id 在滑动窗口内已出现过, THEN THE Collaboration_Module SHALL 拒绝该请求以防止重放攻击
