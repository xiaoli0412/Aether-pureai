# 设计文档：Relay 增强协同能力

## 概述

本设计文档描述 Aether Gateway 中转引擎「增强协同能力」的技术架构。该功能在现有 relay 模块（config、discovery、health、routing、profit、reconcile、metrics、resilience、upstream_client、api、engine）基础上，新增以下子系统：

1. **全站数据拉取增强** — 扩展 `PriceDiscoveryService` 和 `UpstreamApiClient` trait
2. **余额监控器** — 新增 `BalanceMonitor` 后台任务
3. **价格变化检测器** — 新增 `PriceChangeDetector` 模块
4. **下游分组管理器** — 新增 `collaboration/` 模块子系统
5. **New API 协同** — 入站处理器、导出 API、事件消费器
6. **路由模式模块** — 新增 `relay/routing_mode/` 目录
7. **凭据存储** — 新增 `CredentialStore` 组件
8. **可观测性扩展** — 扩展现有 `RelayMetrics`

设计遵循现有代码风格：纯逻辑放入 `aether-relay-core` crate，I/O 集成层放入 `apps/aether-gateway/src/relay/` 或新增的 `collaboration/` 模块。

---

## 架构

### 系统架构图

```mermaid
graph TB
    subgraph "New API 平台"
        NA_API[New API 管理 API]
        NA_Events[New API 事件端点]
        NA_Forward[New API 转发请求]
    end

    subgraph "Aether Gateway"
        subgraph "collaboration/ 模块"
            IH[Inbound Handler<br/>签名验证+路由]
            EA[Export API<br/>/api/export/*]
            EC[Event Consumer<br/>增量事件拉取]
        end

        subgraph "relay/ 模块（现有+扩展）"
            PDS[Price Discovery Service<br/>全站数据拉取]
            BM[Balance Monitor<br/>余额实时监控]
            PCD[Price Change Detector<br/>价格变化检测]
            RS[Route Selection Service]
            HS[Health Store]
            PL[Profit Ledger Writer]
            RC[Reconciliation Service]
        end

        subgraph "relay/routing_mode/"
            RM_DC[direct_channel]
            RM_PS[parallel_shadow]
            RM_AD[aether_decision]
        end

        subgraph "下游分组管理"
            DGM[Downstream Group Manager]
            DG_API[分组管理 API<br/>/api/relay/groups/*]
        end

        CS[Credential Store<br/>凭据管理]
        RM[Relay Metrics<br/>协同指标]
    end

    subgraph "数据层"
        PG[(PostgreSQL)]
        Redis[(Redis / RuntimeState)]
    end

    NA_Forward -->|X-Aether-Signature| IH
    IH --> RS
    NA_API --> PDS
    NA_API --> BM
    NA_Events --> EC
    EA -->|Bearer Token| NA_API

    PDS --> Redis
    BM --> Redis
    PCD --> Redis
    DGM --> PG
    DGM --> Redis
    CS --> PG
    EC --> PG
    PL --> PG
