# Requirements Document

## Introduction

本文档定义了 Aether Gateway 中转引擎的「增强协同能力」功能需求。该功能在现有中转定价与动态路由引擎基础上，扩展以下核心能力：

1. **全站上游数据完整拉取** — 不再仅获取本 key 所在分组数据，而是获取上游站点的全站定价结构
2. **上游余额实时监控** — 实时追踪每个上游 key 的余额变化，预测耗尽时间并自动降权
3. **实时价格变化检测与自动切换** — 检测上游价格变动并即时调整路由决策
4. **高级下游分组管理** — 支持多级分组、时间段定价、分组继承等高级能力
5. **New API 协同** — 包含签名验证入站转发、数据导出接口、事件消费与去重
6. **路由模式模块化** — 支持 direct_channel、parallel_shadow、aether_decision 三种可切换模式
7. **双向安全凭据** — 独立凭据体系与轮换机制
8. **历史数据完整性** — 确保缺失数据明确标注而非伪造
9. **可观测性与追踪** — 端到端请求关联与协同指标暴露

该系统构建于现有 relay 引擎模块（config、discovery、health、routing、profit、reconcile、metrics、resilience、upstream_client、api、engine）之上，复用现有的 axum 0.8 框架、sqlx 0.8 数据库层、tokio 异步运行时和 Redis 运行时状态。

## Glossary

- **Relay_Engine**: 中转定价与动态路由引擎的核心组件，负责协调上游价格发现、健康评分、路由决策和利润计算
- **Price_Discovery_Service**: 价格发现服务，负责从上游平台管理 API 拉取定价信息并缓存
- **Upstream_Channel**: 上游通道，代表一个连接到上游 New API 兼容平台的通道配置
- **Balance_Monitor**: 余额监控器，负责定期查询每个上游 key 的余额并计算消耗速率
- **Price_Change_Detector**: 价格变化检测器，负责比对历史定价缓存与最新同步数据，识别价格变化
- **Downstream_Group_Manager**: 下游分组管理器，管理多级分组、继承关系、时间段定价和模型级倍率覆盖
- **Downstream_Group**: 下游分组，包含名称、描述、模型白名单/黑名单、全局倍率乘数、优先级和并发限制
- **Inbound_Handler**: 入站处理器，负责验证 New API 转发请求的签名并解析上下文
- **Export_API**: 数据导出接口，向 New API 暴露只读的定价、用量和事件数据
- **Event_Consumer**: 事件消费器，从 New API 拉取增量业务事件并幂等处理
- **Routing_Mode**: 路由模式，定义请求处理策略（direct_channel、parallel_shadow、aether_decision）
- **Credential_Store**: 凭据存储，管理双向通信凭据的加密存储、轮换和撤销
- **Inbound_Service_Secret**: 入站服务密钥，New API → Aether 请求签名验证使用的预共享凭据
- **Outbound_Export_Token**: 出站导出令牌，Aether → New API 事件拉取使用的认证令牌
- **Cost_Confidence**: 成本置信度标记，标识历史记录中成本数据的可靠性（known、estimated、unknown）
- **Route_Selector**: 路由选择器，根据综合评分为请求选择最优上游通道
- **Health_Scorer**: 健康评分器，基于延迟、错误率和可用性维护每个上游通道的健康分数
- **Composite_Score**: 综合评分，结合价格评分和健康评分的加权分数，用于路由决策
- **Quota_Point**: 额度单位，New API 生态中的计费单位（1 USD = 500,000 额度）
- **Model_Ratio**: 模型倍率，New API 生态中用于计算模型价格的倍率系数
- **Group_Ratio**: 分组倍率，New API 生态中用户所在分组的价格倍率
- **Runtime_State**: 运行时状态，基于 Redis 或内存的分布式状态存储（aether-runtime-state）
- **Settlement_Record**: 对账记录，记录下游消耗与上游支出的匹配关系
- **Event_Cursor**: 事件游标，标记事件消费的位置，用于增量拉取和断点续传

## Requirements

### Requirement 1: 全站上游数据完整拉取

**User Story:** 作为网关运营者，我需要获取上游站点的完整数据（不仅是我的 key 所在分组），以便全面了解上游成本结构并做出最优路由决策。

#### Acceptance Criteria

1. WHEN Price_Discovery_Service 执行同步, THE Price_Discovery_Service SHALL 从上游拉取全站所有分组列表（GET /api/group/）包含每个分组的名称、倍率、描述和优先级
2. WHEN Price_Discovery_Service 执行同步, THE Price_Discovery_Service SHALL 从上游拉取完整的模型倍率配置（GET /api/ratio_config）包含所有模型的输入倍率、输出倍率和补全倍率
3. WHEN 上游支持健康度 API（GET /api/channel/health 或等效接口）, THE Price_Discovery_Service SHALL 拉取每个渠道的健康状态、响应时间和错误率
4. THE Price_Discovery_Service SHALL 同时获取本 key 所在分组的专属倍率（通过 GET /api/user/groups 确认分组归属）
5. IF 上游 API 不支持某个端点（返回 HTTP 404 或 405）, THEN THE Price_Discovery_Service SHALL 降级为仅使用本地观测数据，并记录 INFO 级别日志说明降级原因
6. THE Price_Discovery_Service SHALL 为每个上游通道独立存储"全站分组数据"和"本 key 所属分组数据"，标注二者的倍率差异值

### Requirement 2: 上游余额实时监控

**User Story:** 作为网关运营者，我需要实时了解每个上游 key 的余额变化，以便在余额不足前及时切换或充值。

#### Acceptance Criteria

1. THE Balance_Monitor SHALL 以可配置间隔（默认 60 秒）查询每个已启用上游 key 的余额（GET /api/user/self 或 GET /api/user/dashboard）
2. WHEN 余额查询成功, THE Balance_Monitor SHALL 记录当前余额、已用额度和与上次查询的余额差值
3. WHEN 余额占总额度的比例低于可配置阈值（默认 10%）, THE Balance_Monitor SHALL 触发低余额告警并将该通道的路由权重降低 50%
4. WHEN 余额为零或查询返回欠费状态（HTTP 402 或余额字段为负值）, THE Balance_Monitor SHALL 立即将该通道标记为不可用状态，等同于熔断处理
5. THE Balance_Monitor SHALL 基于最近 10 次余额记录计算每分钟消耗速率，并预测余额耗尽的 UTC 时间戳
6. THE Balance_Monitor SHALL 通过可配置的字段映射表适配不同上游平台的余额 API 响应格式（字段名不同但语义等价）

### Requirement 3: 实时价格变化检测与自动切换

**User Story:** 作为网关运营者，我需要系统在上游价格变化时立即感知并调整路由，避免被高价通道持续扣费。

#### Acceptance Criteria

1. WHEN 定期同步检测到模型倍率与上次缓存值不同, THE Price_Change_Detector SHALL 在 5 秒内更新 Runtime_State 中的定价数据供路由决策使用
2. WHEN 检测到分组倍率变化, THE Price_Change_Detector SHALL 重新计算所有受影响模型的实际成本（Token 数量 × 新模型倍率 × 新分组倍率）并更新路由排序
3. THE Price_Change_Detector SHALL 将每次价格变化持久化至 relay_price_history 表，记录：变化时间戳、旧倍率值、新倍率值、通道 ID 和模型标识
4. WHEN 某通道某模型的实际成本较上次记录上涨超过可配置阈值（默认 20%）, THE Price_Change_Detector SHALL 触发价格飙升告警并立即将该模型流量切换到成本更低的通道
5. THE Relay_Engine SHALL 支持通过 WebSocket 或 SSE 向已连接的管理客户端推送价格变化事件（包含通道 ID、模型 ID、旧值、新值）
6. WHEN 所有可用通道的某模型价格均上涨, THE Relay_Engine SHALL 在目标利润率模式下自动按新的最低上游成本重新计算下游定价

### Requirement 4: 高级下游分组管理

**User Story:** 作为网关运营者，我需要一套比 New API 更灵活的分组系统，以便向不同客户提供差异化的模型访问和定价。

#### Acceptance Criteria

1. THE Downstream_Group_Manager SHALL 支持创建 Downstream_Group，每个分组包含：名称（唯一）、描述、模型白名单、模型黑名单、全局倍率乘数和优先级字段
2. THE Downstream_Group_Manager SHALL 支持为每个分组的每个模型单独设置倍率覆盖值（model_ratio_overrides 映射表）
3. THE Downstream_Group_Manager SHALL 支持分组继承：子分组继承父分组的模型列表和倍率设置，子分组可覆盖父分组的特定模型倍率
4. THE Downstream_Group_Manager SHALL 支持时间段定价：每个分组可配置多个时间段规则（start_hour、end_hour、timezone、ratio_multiplier），在指定时段应用对应倍率乘数
5. THE Downstream_Group_Manager SHALL 支持为每个分组独立设置并发限制（requests_per_minute 和 requests_per_day），该限制独立于全局限制
6. WHEN 向 New API 导出分组信息, THE Downstream_Group_Manager SHALL 将内部多级继承结构扁平化为 New API 兼容的单级分组格式（合并父子倍率为最终有效值）
7. THE Downstream_Group_Manager SHALL 支持为分组设置成本上限（daily_quota_limit 和 monthly_quota_limit），WHEN 消耗超限, THE Downstream_Group_Manager SHALL 拒绝该分组的新请求并返回 HTTP 429
8. THE Downstream_Group_Manager SHALL 支持 API Key 到分组的多对多映射，WHEN 一个 key 属于多个分组, THE Downstream_Group_Manager SHALL 对每个模型取所有关联分组中最优（最低）的有效倍率

### Requirement 5: New API 协同 — 签名上下文与入站转发

**User Story:** 作为系统架构师，我需要 Aether 能安全地接收 New API 转发的请求并完成路由。

#### Acceptance Criteria

1. WHEN New API 转发请求到达, THE Inbound_Handler SHALL 验证 X-Aether-Signature header 的 HMAC-SHA256 签名（密钥为 Inbound_Service_Secret）
2. WHEN 签名验证通过, THE Inbound_Handler SHALL 解析 X-Aether-Context header（JSON 格式）获取字段：request_id、user_ident（匿名化标识）、model、group 和 expires_at
3. IF 签名无效或 expires_at 早于当前时间, THEN THE Inbound_Handler SHALL 返回 HTTP 401 并记录安全审计日志（包含来源 IP、失败原因和请求标识）
4. WHEN 上下文解析成功, THE Inbound_Handler SHALL 使用 context 中的 model 和 group 映射到内部 Downstream_Group，确定该请求的有效倍率和可用通道列表
5. THE Inbound_Handler SHALL 支持同步响应和流式 SSE 响应，保留 OpenAI Chat Completions、Claude Messages 和 Gemini GenerateContent 的协议语义
6. THE Inbound_Handler SHALL 在响应 header 中附带 X-Aether-Upstream-Request-Id（实际上游使用的请求 ID）供 New API 进行日志关联
7. THE Inbound_Handler SHALL 传播入站请求中的 X-Oneapi-Request-Id header 和 W3C traceparent header 至上游请求

### Requirement 6: New API 协同 — 数据导出接口

**User Story:** 作为系统架构师，我需要向 New API 暴露只读数据接口，使其了解 Aether 的定价和运营状况。

#### Acceptance Criteria

1. THE Export_API SHALL 提供 GET /api/export/pricing 端点，返回所有可用模型的定价快照（包含 quota_per_unit、分组倍率矩阵和能力标签）
2. THE Export_API SHALL 提供 GET /api/export/usage-summary 端点，接受 start_date、end_date 参数，返回用量汇总（维度：Token 数、请求数、quota 消耗，按模型和分组聚合）
3. THE Export_API SHALL 提供 GET /api/export/events 端点，接受 cursor 和 limit 参数，返回增量事件流（事件类型：定价变更、通道状态变化、健康异常）
4. THE Export_API SHALL 使用独立的 Outbound_Export_Token 通过 Bearer 认证验证所有导出接口请求
5. THE Export_API SHALL 在响应中包含 ETag header，WHEN 客户端发送 If-None-Match header 且数据未变化, THE Export_API SHALL 返回 HTTP 304
6. THE Export_API SHALL 在所有响应中过滤上游原始 API 密钥，确保密钥内容不出现在任何导出数据中

### Requirement 7: New API 协同 — 事件消费与去重

**User Story:** 作为系统架构师，我需要从 New API 消费业务事件以保持数据同步。

#### Acceptance Criteria

1. THE Event_Consumer SHALL 以可配置间隔（默认 30 秒）从 New API 拉取增量事件（GET {new_api_endpoint}/api/aether/events?cursor={last_cursor}&limit={batch_size}）
2. THE Event_Consumer SHALL 使用 instance_id 与 event_id 的组合作为唯一键进行幂等去重，重复事件跳过处理
3. THE Event_Consumer SHALL 处理以下事件类型：final_usage（用量结算）、balance_change（充值或退款）、channel_config_change（渠道配置变更）、pricing_change（价格变更）
4. WHEN 网络断开后恢复连接, THE Event_Consumer SHALL 从持久化的最后成功 Event_Cursor 位置重新拉取，保证不丢失事件
5. THE Event_Consumer SHALL 仅读取 New API 数据，在任何情况下不回写 New API 的用户余额或配置
6. IF 事件数据缺少 quota_per_unit 字段, THEN THE Event_Consumer SHALL 在对应记录中设置 Cost_Confidence 为 "unknown"，不使用估算值替代

### Requirement 8: 路由模式模块（可切换）

**User Story:** 作为系统架构师，我需要路由模式可在未来无缝切换，当前只启用 direct_channel。

#### Acceptance Criteria

1. THE Relay_Engine SHALL 持久化 aether_routing_mode 配置项，支持三个枚举值：direct_channel、parallel_shadow、aether_decision
2. THE Relay_Engine SHALL 默认启用 direct_channel 模式：接收转发请求 → 执行路由决策 → 将请求发送至选中通道 → 返回上游响应
3. THE Relay_Engine SHALL 将 parallel_shadow 逻辑封装为独立模块：接收请求 → 计算路由建议和成本预测 → 返回建议结果（不实际发送上游请求）
4. THE Relay_Engine SHALL 将 aether_decision 逻辑封装为独立模块：接收请求 → 生成 decision_id 和 TTL → 返回建议通道信息 → 由 New API 自行决定是否执行
5. WHILE parallel_shadow 模式启用, THE Relay_Engine SHALL 仅返回路由建议数据，不向上游发送任何生成请求
6. THE Relay_Engine SHALL 支持通过环境变量 AETHER_ROUTING_MODE 或管理 API（PUT /api/relay/config/routing-mode）动态切换路由模式

### Requirement 9: 双向安全凭据

**User Story:** 作为安全工程师，我需要双向通信使用独立凭据且可轮换。

#### Acceptance Criteria

1. THE Credential_Store SHALL 管理两类独立凭据：Inbound_Service_Secret（New API → Aether 签名验证）和 Outbound_Export_Token（Aether → New API 事件拉取认证）
2. THE Credential_Store SHALL 使用现有 ENCRYPTION_KEY 对所有凭据进行 AES-256-GCM 加密后存储至数据库
3. THE Credential_Store SHALL 支持凭据轮换：生成新凭据后，新旧凭据在可配置的过渡期内（默认 24 小时）同时有效
4. THE Credential_Store SHALL 支持凭据撤销：撤销操作立即生效，被撤销凭据的后续请求返回 HTTP 401
5. WHILE 系统运行于生产环境（AETHER_ENV=production）, THE Relay_Engine SHALL 强制所有协同通信使用 TLS（拒绝非 HTTPS 连接）

### Requirement 10: 历史数据完整性

**User Story:** 作为网关运营者，我需要历史数据真实可靠，缺失信息必须明确标注。

#### Acceptance Criteria

1. THE Relay_Engine SHALL 在 relay_profit_ledger 表中保留每笔记录的原始 charged_quota 值和对应的充值金额（recharge_amount）
2. IF 历史记录缺少当时的 quota_per_unit 或上游实际成本数据, THEN THE Relay_Engine SHALL 在该记录中设置 cost_confidence 字段为 "unknown"
3. THE Relay_Engine SHALL 当 cost_confidence 为 "unknown" 时将 profit 字段设为 null，不使用估算值计算利润
4. THE Relay_Engine SHALL 支持通过管理 API 按 cost_confidence 字段值筛选历史记录（GET /api/relay/profit-ledger?cost_confidence={value}）

### Requirement 11: 可观测性与追踪

**User Story:** 作为运维工程师，我需要 New API 和 Aether 的日志完全可关联。

#### Acceptance Criteria

1. THE Relay_Engine SHALL 在所有协同请求中传播 X-Oneapi-Request-Id header 和 W3C traceparent header
2. THE Relay_Engine SHALL 在协同响应中返回 X-Aether-Upstream-Request-Id header（值为实际上游返回的请求 ID）
3. THE Relay_Engine SHALL 在结构化日志中包含以下字段：request_id、decision_id、selected_channel、composite_score、latency_ms、upstream_request_id
4. THE Relay_Engine SHALL 通过现有 /metrics 端点暴露协同相关 Prometheus 指标：relay_inbound_requests_total（按 model 和 group 标签）、relay_signature_failures_total、relay_event_sync_lag_seconds、relay_balance_remaining_ratio（按 channel 标签）
