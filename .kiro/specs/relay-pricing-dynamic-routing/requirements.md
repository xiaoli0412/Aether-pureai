# Requirements Document

## Introduction

本文档定义了 Aether Gateway 的「中转定价与动态路由引擎」功能需求。该功能使网关能够作为 AI API 中间商/转售商运营：自动从多个上游 New API 兼容平台获取模型定价信息，根据价格和健康状态动态路由请求至最优上游通道，设置下游加价策略，并实时计算利润。

该系统深度集成到现有 aether-gateway 架构中，复用现有的数据库后端（Postgres/MySQL/SQLite）、Redis 运行时状态、路由核心（aether-routing-core）和调度核心（aether-dispatch-core）。

## Glossary

- **Relay_Engine**: 中转定价与动态路由引擎的核心组件，负责协调上游价格发现、健康评分、路由决策和利润计算
- **Upstream_Channel**: 上游通道，代表一个连接到上游 New API 兼容平台的通道配置，包含 API 端点、密钥和所属定价组
- **Upstream_Provider**: 上游供应商平台（如 New API、AnyRouter、Cubence、Done-hub、NekoCode、Sub2API、YesCode）
- **Price_Discovery_Service**: 价格发现服务，负责从上游平台管理 API 拉取定价信息并缓存
- **Health_Scorer**: 健康评分器，基于延迟、错误率和可用性维护每个上游通道的健康分数
- **Route_Selector**: 路由选择器，根据综合评分（价格 + 健康）为请求选择最优上游通道
- **Profit_Accounting_Engine**: 利润核算引擎，实时计算上游成本、下游收入、手续费和净利润
- **Downstream_Instance**: 下游实例，代表一个连接的下游 New API 平台，用于拉取收入和用量数据进行对账
- **Model_Ratio**: 模型倍率，New API 生态中用于计算模型价格的倍率系数
- **Group_Ratio**: 分组倍率，New API 生态中用户所在分组的价格倍率
- **Quota_Point**: 额度单位，New API 生态中的计费单位（1 USD = 500,000 额度）
- **Health_Score**: 健康分数，取值 0.0 到 1.0 的浮点数，表示上游通道的综合健康状态
- **Composite_Score**: 综合评分，结合价格评分和健康评分的加权分数，用于路由决策
- **Rolling_Window**: 滚动窗口，用于计算健康指标的时间窗口（默认 5 分钟）
- **Markup_Strategy**: 加价策略，定义下游定价方式（目标利润率模式或手动定价模式）
- **Settlement_Record**: 对账记录，记录下游消耗与上游支出的匹配关系
- **Runtime_State**: 运行时状态，基于 Redis 或内存的分布式状态存储（aether-runtime-state）

## Requirements

### Requirement 1: 上游通道配置管理

**User Story:** 作为网关管理员，我希望能够配置和管理多个上游通道，以便系统知道可以将请求路由到哪些上游供应商。

#### Acceptance Criteria

1. WHEN 管理员通过管理 API 创建上游通道配置, THE Relay_Engine SHALL 持久化该通道的端点 URL、API 密钥、供应商类型、权重和启用状态至数据库
2. WHEN 管理员更新上游通道配置, THE Relay_Engine SHALL 在 5 秒内将变更同步至所有网关节点的运行时状态
3. THE Relay_Engine SHALL 支持同时配置至少 20 个上游通道
4. WHEN 管理员禁用一个上游通道, THE Route_Selector SHALL 立即停止向该通道路由新请求
5. WHEN 上游通道配置包含多个 API 密钥, THE Relay_Engine SHALL 为每个密钥独立追踪其所属定价分组
6. IF 上游通道配置缺少必填字段（端点 URL 或至少一个 API 密钥）, THEN THE Relay_Engine SHALL 拒绝该配置并返回具体的字段校验错误信息

### Requirement 2: 上游价格发现

**User Story:** 作为网关管理员，我希望系统自动从上游平台获取最新的模型定价信息，以便路由决策基于准确的成本数据。

#### Acceptance Criteria

1. WHEN Price_Discovery_Service 启动定期同步任务, THE Price_Discovery_Service SHALL 通过上游管理 API（GET /api/group/、GET /api/user/groups、GET /api/ratio_config）获取分组信息和模型倍率
2. WHEN 上游平台返回定价数据, THE Price_Discovery_Service SHALL 使用公式「Token 数量 × 模型倍率 × 分组倍率」计算每个模型在每个通道的实际每 Token 成本（以 Quota_Point 为单位，1 USD = 500,000 Quota_Point）
3. THE Price_Discovery_Service SHALL 以可配置的间隔（默认 300 秒）定期从所有已启用的上游通道同步定价数据
4. WHEN 定价数据成功获取, THE Price_Discovery_Service SHALL 将结果缓存至 Runtime_State 中，并设置与同步间隔匹配的 TTL
5. IF 上游平台 API 请求失败, THEN THE Price_Discovery_Service SHALL 保留上一次成功获取的缓存数据，并记录错误日志包含通道标识和 HTTP 状态码
6. WHEN 管理员手动触发价格同步, THE Price_Discovery_Service SHALL 忽略缓存 TTL 立即执行一次完整同步
7. THE Price_Discovery_Service SHALL 为每次同步操作记录同步时间戳、获取到的模型数量和失败通道列表

### Requirement 3: 上游通道健康评分

**User Story:** 作为网关管理员，我希望系统实时追踪上游通道的健康状态，以便将请求路由到稳定可用的通道。

#### Acceptance Criteria

1. WHEN 一次上游请求完成（成功或失败）, THE Health_Scorer SHALL 更新该通道的延迟、错误率和可用性指标
2. THE Health_Scorer SHALL 使用 Rolling_Window（默认 300 秒）计算健康指标，窗口内无数据的通道 Health_Score 为 0.5
3. THE Health_Scorer SHALL 使用以下公式计算 Health_Score：(1 - error_rate) × availability_weight + latency_score × latency_weight，其中 latency_score = max(0, 1 - (p95_latency_ms / latency_threshold_ms))
4. WHILE 上游通道连续失败次数超过可配置阈值（默认 5 次）, THE Health_Scorer SHALL 将该通道标记为「熔断」状态并将 Health_Score 设置为 0.0
5. WHEN 处于「熔断」状态的通道经过可配置的冷却时间（默认 60 秒）, THE Health_Scorer SHALL 进入「半开」状态，允许单个探测请求通过以验证恢复
6. WHEN「半开」状态的探测请求成功, THE Health_Scorer SHALL 将通道恢复为「正常」状态并重置连续失败计数
7. THE Health_Scorer SHALL 将健康评分数据通过 Runtime_State 同步至所有网关节点，更新延迟不超过 2 秒

### Requirement 4: 动态路由选择

**User Story:** 作为网关运营者，我希望系统自动将请求路由到性价比最高且健康的上游通道，以便在保证服务质量的同时最小化成本。

#### Acceptance Criteria

1. WHEN 一个 AI API 请求到达网关, THE Route_Selector SHALL 在 10 毫秒内完成路由决策并选择目标上游通道
2. THE Route_Selector SHALL 使用 Composite_Score 公式计算每个候选通道的评分：price_score × price_weight + health_score × health_weight，其中 price_weight 和 health_weight 可配置（默认 0.6 和 0.4）
3. WHEN 存在多个通道支持请求的模型, THE Route_Selector SHALL 选择 Composite_Score 最高的通道
4. WHEN 最优通道请求失败, THE Route_Selector SHALL 自动故障转移至下一个评分最高的可用通道
5. WHILE 某通道处于「熔断」状态, THE Route_Selector SHALL 将该通道排除在候选列表之外
6. WHEN 同一通道的 Composite_Score 相同, THE Route_Selector SHALL 使用加权随机选择以实现负载均衡
7. IF 没有任何已启用通道支持请求的模型, THEN THE Route_Selector SHALL 返回 HTTP 503 错误并附带错误详情说明无可用通道
8. THE Route_Selector SHALL 支持按模型粒度覆盖 price_weight 和 health_weight 配置

### Requirement 5: 下游加价与定价策略

**User Story:** 作为网关运营者，我希望能够设置灵活的下游定价策略，以便在保证利润的同时对不同客户群体实施差异化定价。

#### Acceptance Criteria

1. WHEN 管理员选择「目标利润率」模式, THE Relay_Engine SHALL 根据上游成本自动计算下游价格：downstream_price = upstream_cost / (1 - target_margin)
2. WHEN 管理员选择「手动定价」模式, THE Relay_Engine SHALL 使用管理员指定的固定下游价格
3. THE Relay_Engine SHALL 支持按模型粒度和按用户分组粒度设置独立的加价策略
4. WHEN 上游成本变化超过可配置阈值（默认 10%）, THE Relay_Engine SHALL 在「目标利润率」模式下自动更新下游价格并记录价格变更日志
5. THE Relay_Engine SHALL 在管理界面展示每个模型的当前上游成本、下游价格和实际利润率
6. IF 手动定价低于上游成本, THEN THE Relay_Engine SHALL 发出告警通知管理员该模型处于亏损状态
7. WHEN 新的上游模型被发现, THE Relay_Engine SHALL 使用默认加价策略（可配置的默认利润率）自动生成下游定价

### Requirement 6: 利润核算引擎

**User Story:** 作为网关运营者，我希望实时了解每笔请求和整体业务的利润状况，以便做出数据驱动的运营决策。

#### Acceptance Criteria

1. WHEN 一次 AI API 请求完成, THE Profit_Accounting_Engine SHALL 计算该请求的上游成本：token_count × model_ratio × group_ratio / 500000（单位：USD）
2. WHEN 一次 AI API 请求完成, THE Profit_Accounting_Engine SHALL 计算该请求的下游收入：基于下游定价策略应用于实际 Token 使用量
3. THE Profit_Accounting_Engine SHALL 计算净利润时扣除支付平台手续费：net_profit = downstream_revenue - upstream_cost - (downstream_revenue × payment_fee_rate)
4. THE Profit_Accounting_Engine SHALL 支持配置多种手续费率（支付宝/微信 0.6%、USDT 自定义费率、上游充值汇率损耗）
5. THE Profit_Accounting_Engine SHALL 将每笔请求的利润数据（上游成本、下游收入、手续费、净利润）持久化至 profit_ledger 表
6. THE Profit_Accounting_Engine SHALL 提供汇总查询 API 返回：总收入、总成本、总利润、总请求数、总 Token 数、利润率，支持按时间范围、模型和通道维度筛选
7. WHEN 利润率低于可配置的告警阈值（默认 10%）, THE Profit_Accounting_Engine SHALL 触发告警通知

### Requirement 7: 下游 New API 实例对账

**User Story:** 作为网关运营者，我希望能够连接下游 New API 实例并自动对账，以便验证下游消耗与上游支出的一致性。

#### Acceptance Criteria

1. WHEN 管理员配置下游实例连接, THE Relay_Engine SHALL 通过下游 New API 管理 API 验证连接有效性
2. THE Relay_Engine SHALL 定期（可配置间隔，默认 3600 秒）从下游实例拉取总收入、总 Token 用量和总请求数
3. WHEN 对账任务执行完成, THE Relay_Engine SHALL 生成 Settlement_Record 包含：下游收入、对应时段上游成本、差异金额和差异百分比
4. IF 对账差异百分比超过可配置阈值（默认 5%）, THEN THE Relay_Engine SHALL 标记该 Settlement_Record 为异常并触发告警
5. THE Relay_Engine SHALL 支持同时对接至少 10 个下游实例
6. THE Relay_Engine SHALL 保存最近 90 天的 Settlement_Record 供查询和导出

### Requirement 8: 多节点协调

**User Story:** 作为系统架构师，我希望定价和路由数据在所有网关节点间保持一致，以便集群部署时每个节点做出相同质量的路由决策。

#### Acceptance Criteria

1. THE Relay_Engine SHALL 通过 Runtime_State（Redis 后端）在所有网关节点间同步定价缓存数据
2. THE Relay_Engine SHALL 通过 Runtime_State 在所有网关节点间同步健康评分数据，最终一致性延迟不超过 2 秒
3. WHEN 使用内存后端（单节点部署）运行时, THE Relay_Engine SHALL 将所有状态存储在本地内存中且功能完整
4. THE Relay_Engine SHALL 通过 Runtime_State 实现分布式速率限制，按上游通道粒度限制请求并发数
5. WHEN 一个网关节点加入集群, THE Relay_Engine SHALL 从 Runtime_State 加载最新的定价和健康数据，无需等待下一次同步周期
6. IF Redis 连接暂时中断, THEN THE Relay_Engine SHALL 使用本地缓存的最近一次有效数据继续路由，并在连接恢复后自动重新同步

### Requirement 9: 管理 API

**User Story:** 作为网关管理员，我希望通过 REST API 管理中转引擎的所有配置和查看运营数据，以便集成到现有管理后台。

#### Acceptance Criteria

1. THE Relay_Engine SHALL 在 /api/relay/ 路径前缀下提供所有管理 API 端点
2. THE Relay_Engine SHALL 提供上游通道的 CRUD API（/api/relay/channels）
3. THE Relay_Engine SHALL 提供定价策略的 CRUD API（/api/relay/pricing）
4. THE Relay_Engine SHALL 提供下游实例的 CRUD API（/api/relay/downstream）
5. THE Relay_Engine SHALL 提供实时仪表盘 API（/api/relay/dashboard）返回汇总数据
6. THE Relay_Engine SHALL 提供手动触发价格同步的 API（POST /api/relay/sync/pricing）
7. THE Relay_Engine SHALL 提供手动触发健康探测的 API（POST /api/relay/sync/health）
8. THE Relay_Engine SHALL 提供对账记录查询 API（/api/relay/settlements）支持分页和时间范围筛选
9. WHEN 管理 API 请求缺少有效的管理员认证, THE Relay_Engine SHALL 返回 HTTP 401 并拒绝操作

### Requirement 10: 数据持久化

**User Story:** 作为系统架构师，我希望中转引擎的配置和历史数据可靠地持久化到数据库中，以便系统重启后恢复完整状态。

#### Acceptance Criteria

1. THE Relay_Engine SHALL 使用现有数据库后端（Postgres/MySQL/SQLite，通过 sqlx 访问）存储所有持久化数据
2. THE Relay_Engine SHALL 创建以下数据库表：relay_channels（通道配置）、relay_channel_keys（通道密钥）、relay_pricing_cache（定价缓存）、relay_markup_rules（加价规则）、relay_profit_ledger（利润流水）、relay_downstream_instances（下游实例）、relay_settlements（对账记录）、relay_health_snapshots（健康快照）
3. THE Relay_Engine SHALL 通过 sqlx migrate 机制管理数据库迁移
4. WHEN 系统启动时, THE Relay_Engine SHALL 从数据库加载所有通道配置和加价规则至内存
5. THE Relay_Engine SHALL 对 relay_profit_ledger 表按月分区或按时间索引以保证大数据量下的查询性能
6. IF 数据库写入失败, THEN THE Relay_Engine SHALL 将失败的利润记录暂存至 Runtime_State 中，并在数据库恢复后重试写入

### Requirement 11: 缓存策略

**User Story:** 作为系统架构师，我希望中转引擎使用合理的缓存策略，以便在保证数据新鲜度的同时最小化对上游 API 和数据库的请求压力。

#### Acceptance Criteria

1. THE Price_Discovery_Service SHALL 将上游定价数据缓存至 Runtime_State，TTL 等于同步间隔（默认 300 秒）
2. THE Health_Scorer SHALL 在 Runtime_State 中使用滚动窗口存储健康指标，窗口外的数据点自动过期
3. THE Route_Selector SHALL 每次请求实时计算路由决策，不缓存路由结果
4. THE Relay_Engine SHALL 将上游通道的分组成员关系缓存至 Runtime_State，定期刷新（默认 1800 秒）
5. WHEN 管理员手动触发同步, THE Price_Discovery_Service SHALL 清除现有缓存并写入最新数据
6. THE Relay_Engine SHALL 在 Runtime_State 中存储每个缓存条目的写入时间戳，供监控和诊断使用

### Requirement 12: 可观测性与监控

**User Story:** 作为运维工程师，我希望中转引擎暴露关键运行指标，以便通过 Prometheus 监控系统健康状态和业务指标。

#### Acceptance Criteria

1. THE Relay_Engine SHALL 通过现有的 /metrics 端点暴露以下 Prometheus 指标：relay_upstream_cost_total、relay_downstream_revenue_total、relay_profit_total、relay_route_decisions_total、relay_channel_health_score
2. THE Relay_Engine SHALL 为每个上游通道暴露请求延迟直方图（relay_channel_latency_seconds）
3. THE Relay_Engine SHALL 为每个上游通道暴露错误率计数器（relay_channel_errors_total），按错误类型标签分类
4. THE Relay_Engine SHALL 暴露价格同步状态指标（relay_price_sync_last_success_timestamp、relay_price_sync_errors_total）
5. THE Relay_Engine SHALL 暴露熔断器状态指标（relay_circuit_breaker_state），标签包含通道 ID 和当前状态
6. WHEN 路由决策完成, THE Relay_Engine SHALL 记录结构化日志包含：请求模型、选中通道、Composite_Score、决策耗时
