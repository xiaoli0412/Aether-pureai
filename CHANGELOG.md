# 更新说明

所有重要变更均记录在此文件中。

## v0.7.11-aether-newapi.1 - 2026-07-20

### 新增

- 新增 New API x Aether Relay 协同能力：签名 Relay 上下文、集成配置与能力同步、实例状态、修订冲突反馈，以及管理后台的 Relay Integration 状态页。
- Relay 请求复用 Aether 既有的 API Key 认证、Provider Catalog、Provider Pool、Routing Profile、代理、SSE、重试和 Usage Settlement 主链；不新增旁路转发系统。
- 新增 New API 事件消费、inbox/outbox 去重、只读价格/用量/事件导出，以及用于成本、利润、健康、余额和路由分析的持久化数据模型。
- 新增动态 `quota_per_unit` 定价与利润计算支持，并增加 SQLite、MySQL、PostgreSQL 的 Relay 相关迁移。

### 安全与数据边界

- New API 保持用户、余额、充值、退款、订阅、结算和用户价格的唯一金融权威；Aether 仅处理匿名化、只读的协同数据，不能自动回写价格或用户金融记录。
- 每个集成实例使用独立的控制面与 Relay 签名凭据，凭据与 Aether 用户 API Key、Provider API Key 和普通渠道凭据隔离；支持加密存储、受控轮换、双密钥过渡和撤销。
- `direct_channel` 是本预发布版本唯一允许真实上游请求的模式。`parallel_shadow` 与 `aether_decision`，以及无效、过期或上下文不匹配的 Relay 请求，均失败关闭。

### 上线要求

- 这是预发布版本。上线前必须在生产数据库的完整副本上验证迁移、回归、备份和回滚，分别覆盖 SQLite、MySQL 或 PostgreSQL 的实际部署引擎。
- 在副本验证完成前，不要让 `AETHER_GATEWAY_AUTO_PREPARE_DATABASE` 对生产数据库自动执行迁移。启用真实流量前，还应核对 New API 与 Aether 的 `aether-newapi/v1` 合同、配置修订、实例状态和凭据轮换状态。
