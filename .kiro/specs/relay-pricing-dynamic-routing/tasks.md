# Implementation Plan: 中转定价与动态路由引擎

## Overview

按两层架构实现：先完成 `crates/aether-relay-core/`（纯逻辑层，无 I/O），再在 `apps/aether-gateway/src/relay/` 中实现网关集成层。核心层优先确保所有业务算法可通过 property-based testing 验证，集成层依赖核心层的正确性。

## Tasks

- [ ] 1. 创建 aether-relay-core crate 基础结构与核心类型
  - [ ] 1.1 初始化 `crates/aether-relay-core/` crate（Cargo.toml、lib.rs）
    - 添加 `serde`、`serde_json` 依赖，dev-dependencies 添加 `proptest`
    - 在 workspace `Cargo.toml` 注册新 crate
    - _Requirements: 无直接对应，基础设施准备_
  - [ ] 1.2 定义核心数据模型（`src/models.rs`）
    - 实现 `HealthConfig`、`ChannelHealthState`、`HealthMetrics`、`PricingParams`、`RouteCandidate`、`RouteConfig`、`RouteDecision`、`ProfitInput`、`ProfitResult`、`MarkupStrategy` 结构体和枚举
    - 为所有类型派生 `Debug, Clone, Serialize, Deserialize`
    - _Requirements: 1.1, 2.2, 3.1, 4.2, 5.1, 6.1_

- [ ] 2. 实现定价计算（纯函数）
  - [ ] 2.1 实现 `src/pricing.rs` 模块
    - 实现 `calculate_token_cost_quota(token_count, is_completion, params)` 函数
    - 实现 `quota_to_usd(quota_points)` 和 `usd_to_quota(usd)` 转换函数
    - _Requirements: 2.2_
  - [ ]* 2.2 编写定价计算的 property test
    - **Property 2: Token 成本公式正确性**
    - **Validates: Requirements 2.2**

- [ ] 3. 实现健康评分（纯函数 + 熔断状态机）
  - [ ] 3.1 实现 `src/health.rs` 模块
    - 实现 `calculate_health_score(metrics, config)` 函数
    - 实现 `should_circuit_break(metrics, config)` 函数
    - 实现 `should_half_open(metrics, config, now_ms)` 函数
    - 处理窗口内无数据时返回 0.5 的逻辑
    - _Requirements: 3.2, 3.3, 3.4, 3.5, 3.6_
  - [ ]* 3.2 编写健康评分公式的 property test
    - **Property 3: 健康评分公式正确性**
    - **Validates: Requirements 3.2, 3.3**
  - [ ]* 3.3 编写熔断状态转换的 property test
    - **Property 4: 熔断状态触发与冷却转换**
    - **Validates: Requirements 3.4, 3.5**

- [ ] 4. 实现路由选择算法（综合评分）
  - [ ] 4.1 实现 `src/routing.rs` 模块
    - 实现 `calculate_composite_score(candidate, min_price, max_price, config)` 函数
    - 实现 `select_route(candidates, config, seed)` 函数
    - 包含过滤禁用/熔断通道、评分计算、排序、加权随机平局打破逻辑
    - _Requirements: 4.2, 4.3, 4.4, 4.5, 4.6, 4.7_
  - [ ]* 4.2 编写综合评分公式的 property test
    - **Property 5: 综合评分公式正确性**
    - **Validates: Requirements 4.2**
  - [ ]* 4.3 编写路由选择排序不变量的 property test
    - **Property 6: 路由选择排序不变量**
    - **Validates: Requirements 4.3, 4.4**
  - [ ]* 4.4 编写禁用和熔断通道排除的 property test
    - **Property 7: 禁用和熔断通道排除**
    - **Validates: Requirements 1.4, 4.5**
  - [ ]* 4.5 编写空候选列表返回 None 的 property test
    - **Property 8: 空候选列表返回 None**
    - **Validates: Requirements 4.7**
  - [ ]* 4.6 编写加权随机平局打破的 property test
    - **Property 9: 加权随机平局打破尊重权重**
    - **Validates: Requirements 4.6**

- [ ] 5. 实现利润计算（纯函数）
  - [ ] 5.1 实现 `src/profit.rs` 模块
    - 实现 `calculate_profit(input)` 函数
    - 包含上游成本、下游收入、手续费、净利润、利润率的完整计算
    - 实现利润率告警阈值检测辅助函数
    - _Requirements: 6.1, 6.2, 6.3, 6.7_
  - [ ]* 5.2 编写利润计算公式的 property test
    - **Property 13: 利润计算公式一致性**
    - **Validates: Requirements 6.1, 6.2, 6.3**
  - [ ]* 5.3 编写利润率告警阈值检测的 property test
    - **Property 14: 利润率告警阈值检测**
    - **Validates: Requirements 6.7**

- [ ] 6. 实现加价策略（纯函数）
  - [ ] 6.1 实现 `src/markup.rs` 模块
    - 实现 `calculate_downstream_price(upstream_cost, strategy)` 函数
    - 实现亏损检测辅助函数 `is_loss(downstream_price, upstream_cost) -> bool`
    - _Requirements: 5.1, 5.2, 5.6_
  - [ ]* 6.2 编写目标利润率定价公式的 property test
    - **Property 10: 目标利润率定价公式**
    - **Validates: Requirements 5.1**
  - [ ]* 6.3 编写固定定价策略透传的 property test
    - **Property 11: 固定定价策略透传**
    - **Validates: Requirements 5.2**
  - [ ]* 6.4 编写亏损检测的 property test
    - **Property 12: 亏损检测**
    - **Validates: Requirements 5.6**

- [ ] 7. 实现通道配置校验与对账异常检测
  - [ ] 7.1 实现通道配置校验函数（`src/models.rs` 或独立 `src/validation.rs`）
    - 校验端点 URL 非空、至少一个 API 密钥存在
    - 返回具体缺失字段名称
    - _Requirements: 1.6_
  - [ ] 7.2 实现对账异常检测辅助函数
    - 判断 `|difference_percent| > threshold` 逻辑
    - _Requirements: 7.4_
  - [ ]* 7.3 编写通道配置校验的 property test
    - **Property 1: 通道配置校验拒绝不完整配置**
    - **Validates: Requirements 1.6**
  - [ ]* 7.4 编写对账异常检测的 property test
    - **Property 15: 对账异常检测**
    - **Validates: Requirements 7.4**

- [ ] 8. Checkpoint - 核心层验证
  - Ensure all tests pass, ask the user if questions arise.
  - 确认 `crates/aether-relay-core/` 所有 property tests 和 unit tests 通过
  - 运行 `cargo test -p aether-relay-core`

- [ ] 9. 创建数据库迁移（8 张表）
  - [ ] 9.1 创建 sqlx 迁移文件
    - 在 `apps/aether-gateway/` 的迁移目录下创建迁移 SQL
    - 包含全部 8 张表：`relay_channels`、`relay_channel_keys`、`relay_pricing_cache`、`relay_markup_rules`、`relay_profit_ledger`、`relay_downstream_instances`、`relay_settlements`、`relay_health_snapshots`
    - 包含所有索引定义
    - _Requirements: 10.1, 10.2, 10.3, 10.5_

- [ ] 10. 创建网关 relay 模块结构
  - [ ] 10.1 创建 `apps/aether-gateway/src/relay/mod.rs` 模块入口
    - 定义 `RelayEngine` 结构体骨架
    - 声明子模块：`api`、`config`、`discovery`、`health`、`routing`、`profit`、`reconcile`、`metrics`、`migration`
    - 在 `aether-gateway` 的 Cargo.toml 添加对 `aether-relay-core` 的依赖
    - _Requirements: 无直接对应，基础设施准备_
  - [ ] 10.2 定义 `RelayEngineConfig` 配置结构体
    - 包含价格同步间隔、健康窗口参数、路由权重、告警阈值等所有可配置项
    - 支持从环境变量 `AETHER_RELAY_*` 读取
    - _Requirements: 2.3, 3.2, 4.2, 4.8, 5.4, 6.7, 7.4_

- [ ] 11. 实现 ChannelConfigStore（数据库 CRUD + RuntimeState 同步）
  - [ ] 11.1 实现 `src/relay/config.rs`
    - 实现通道 CRUD 操作（sqlx 查询 `relay_channels` 和 `relay_channel_keys` 表）
    - 实现配置变更后 5 秒内同步至 RuntimeState 的逻辑
    - 实现启动时从数据库加载全部通道配置到内存
    - _Requirements: 1.1, 1.2, 1.3, 1.4, 1.5, 10.4_

- [ ] 12. 实现 Upstream Client（New API 管理 API 适配器）
  - [ ] 12.1 实现 `src/relay/discovery.rs` 中的上游 API 客户端
    - 实现调用上游 `GET /api/group/`、`GET /api/user/groups`、`GET /api/ratio_config` 接口
    - 解析上游响应，提取模型倍率和分组信息
    - 抽象为 trait 以支持单元测试 mock
    - _Requirements: 2.1_

- [ ] 13. 实现价格发现后台任务
  - [ ] 13.1 完成 `src/relay/discovery.rs` 中的 PriceDiscoveryTask
    - 实现定期同步逻辑（默认 300 秒间隔）
    - 调用上游 API 获取定价 → 使用 `aether-relay-core::pricing` 计算成本 → 缓存至 RuntimeState
    - 实现缓存 TTL 设置、分布式锁（避免多节点重复同步）
    - 实现同步失败时保留旧缓存的逻辑
    - 实现手动触发同步入口（忽略 TTL）
    - 记录同步时间戳、模型数量、失败通道列表
    - _Requirements: 2.1, 2.2, 2.3, 2.4, 2.5, 2.6, 2.7, 11.1, 11.5_

- [ ] 14. 实现 HealthStore（RuntimeState 读写）
  - [ ] 14.1 实现 `src/relay/health.rs`
    - 实现请求完成后更新 HealthMetrics 到 RuntimeState（滚动窗口）
    - 实现从 RuntimeState 读取通道健康数据
    - 实现 HealthUpdateHook（作为请求完成回调）
    - 调用 `aether-relay-core::health` 纯函数计算分数和状态转换
    - 实现熔断半开状态下的探测请求控制
    - _Requirements: 3.1, 3.4, 3.5, 3.6, 3.7, 11.2_

- [ ] 15. Checkpoint - 后台服务基础验证
  - Ensure all tests pass, ask the user if questions arise.
  - 确认价格发现、健康存储模块编译通过且基础单元测试通过

- [ ] 16. 实现 RouteSelectionService（连接核心逻辑与 RuntimeState）
  - [ ] 16.1 实现 `src/relay/routing.rs`
    - 从 RuntimeState 读取候选通道的定价和健康数据
    - 构造 `RouteCandidate` 列表，调用 `aether-relay-core::routing::select_route`
    - 返回 `RouteDecision` 含选中通道和备选列表
    - 实现 10ms 内完成决策的性能目标（纯内存计算）
    - 支持按模型粒度覆盖 price_weight/health_weight
    - _Requirements: 4.1, 4.2, 4.3, 4.4, 4.5, 4.6, 4.7, 4.8, 11.3_

- [ ] 17. 实现 ProfitLedgerWriter（异步数据库写入 + 重试缓冲）
  - [ ] 17.1 实现 `src/relay/profit.rs`
    - 请求完成后调用 `aether-relay-core::profit::calculate_profit` 计算利润
    - 异步写入 `relay_profit_ledger` 表
    - 实现写入失败时暂存至 RuntimeState 并后台重试的机制
    - 实现汇总查询逻辑（按时间范围、模型、通道维度筛选）
    - _Requirements: 6.1, 6.2, 6.3, 6.4, 6.5, 6.6, 10.6_

- [ ] 18. 实现下游实例管理与对账
  - [ ] 18.1 实现 `src/relay/reconcile.rs`
    - 实现下游 New API 实例连接验证
    - 实现定期对账任务（默认 3600 秒）
    - 从下游实例拉取收入、Token 用量、请求数
    - 生成 Settlement_Record，调用 `aether-relay-core` 的异常检测逻辑
    - 写入 `relay_settlements` 表
    - _Requirements: 7.1, 7.2, 7.3, 7.4, 7.5, 7.6_

- [ ] 19. 实现管理 API handlers（/api/relay/*）
  - [ ] 19.1 实现 `src/relay/api.rs` 中所有管理端点
    - 上游通道 CRUD：`/api/relay/channels`
    - 加价策略 CRUD：`/api/relay/pricing`
    - 下游实例 CRUD：`/api/relay/downstream`
    - 实时仪表盘：`GET /api/relay/dashboard`
    - 手动触发同步：`POST /api/relay/sync/pricing`、`POST /api/relay/sync/health`
    - 对账记录查询：`GET /api/relay/settlements`（分页 + 时间范围）
    - 复用现有管理员认证中间件
    - _Requirements: 9.1, 9.2, 9.3, 9.4, 9.5, 9.6, 9.7, 9.8, 9.9_

- [ ] 20. 集成路由选择器到代理请求流程
  - [ ] 20.1 将 relay 路由决策嵌入现有请求处理管线
    - 在请求到达时判断是否走 relay 路径
    - 调用 RouteSelectionService 获取路由决策
    - 请求完成后触发 HealthUpdateHook 和 ProfitLedgerWriter
    - 实现故障转移逻辑（选中通道失败时重试下一候选）
    - _Requirements: 4.1, 4.4, 3.1_

- [ ] 21. Checkpoint - 核心流程集成验证
  - Ensure all tests pass, ask the user if questions arise.
  - 确认完整请求路径：路由决策 → 上游转发 → 健康更新 → 利润记录 均正常工作

- [ ] 22. 添加 Prometheus 指标
  - [ ] 22.1 实现 `src/relay/metrics.rs`
    - 注册并暴露以下指标到现有 `/metrics` 端点：
      - `relay_upstream_cost_total`（Counter）
      - `relay_downstream_revenue_total`（Counter）
      - `relay_profit_total`（Counter）
      - `relay_route_decisions_total`（Counter）
      - `relay_channel_health_score`（Gauge，per channel）
      - `relay_channel_latency_seconds`（Histogram，per channel）
      - `relay_channel_errors_total`（Counter，per channel + error type）
      - `relay_price_sync_last_success_timestamp`（Gauge）
      - `relay_price_sync_errors_total`（Counter）
      - `relay_circuit_breaker_state`（Gauge，per channel）
    - 在路由决策完成时记录结构化日志
    - _Requirements: 12.1, 12.2, 12.3, 12.4, 12.5, 12.6_

- [ ] 23. 添加 CLI 配置（AETHER_RELAY_* 环境变量）
  - [ ] 23.1 实现环境变量解析与默认值
    - 定义 `AETHER_RELAY_ENABLED`、`AETHER_RELAY_PRICE_SYNC_INTERVAL`、`AETHER_RELAY_HEALTH_WINDOW`、`AETHER_RELAY_PRICE_WEIGHT`、`AETHER_RELAY_HEALTH_WEIGHT`、`AETHER_RELAY_CIRCUIT_BREAKER_THRESHOLD`、`AETHER_RELAY_CIRCUIT_BREAKER_COOLDOWN` 等环境变量
    - 将环境变量映射到 `RelayEngineConfig`
    - 在 gateway 启动时条件性初始化 RelayEngine
    - _Requirements: 2.3, 3.2, 4.2, 4.8, 8.3_

- [ ] 24. 实现多节点协调逻辑
  - [ ] 24.1 确保所有 RuntimeState 操作支持 Redis 后端集群部署
    - 实现新节点加入时从 RuntimeState 加载最新数据
    - 实现 Redis 断连降级为本地缓存并自动恢复的逻辑
    - 实现按通道粒度的分布式速率限制
    - _Requirements: 8.1, 8.2, 8.3, 8.4, 8.5, 8.6, 11.4, 11.6_

- [ ] 25. Integration tests
  - [ ]* 25.1 编写管理 API 集成测试
    - 测试通道 CRUD、定价策略 CRUD、下游实例 CRUD 的 HTTP 请求/响应
    - 测试认证中间件拒绝未授权请求
    - _Requirements: 9.1–9.9_
  - [ ]* 25.2 编写价格发现集成测试
    - Mock 上游 New API 响应，验证定价缓存写入和 TTL 行为
    - 测试同步失败后保留旧缓存
    - _Requirements: 2.1–2.7_
  - [ ]* 25.3 编写完整请求流程集成测试
    - Mock 上游通道，验证路由决策 → 转发 → 健康更新 → 利润记录完整链路
    - 验证故障转移逻辑
    - _Requirements: 4.1–4.7, 6.1–6.5_
  - [ ]* 25.4 编写多节点状态同步集成测试
    - 使用 Redis 后端验证定价和健康数据跨节点一致性
    - _Requirements: 8.1, 8.2_

- [ ] 26. Final checkpoint - 全部验证完成
  - Ensure all tests pass, ask the user if questions arise.
  - 运行 `cargo test --workspace` 确认无回归
  - 确认所有 property tests（Property 1–15）通过

## Notes

- Tasks marked with `*` are optional and can be skipped for faster MVP
- 核心层（Task 1–7）完全无 I/O 依赖，可独立开发和验证
- Property tests 使用 `proptest` crate，每个 property 至少 100 次迭代
- 集成层（Task 9–24）依赖现有 gateway 基础设施（axum、sqlx、aether-runtime-state）
- 数据库迁移兼容 Postgres/MySQL/SQLite 三种后端
- 所有环境变量以 `AETHER_RELAY_` 为前缀，feature flag `AETHER_RELAY_ENABLED` 控制整体开关
