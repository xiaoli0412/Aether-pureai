//! 中转定价与动态路由引擎 - 网关集成层
//!
//! 负责将 aether-relay-core 的纯逻辑与网关基础设施（数据库、Redis、HTTP）集成。

pub(crate) mod api;
pub(crate) mod balance_monitor;
pub(crate) mod collaboration;
pub(crate) mod config;
pub(crate) mod discovery;
pub(crate) mod event_consumer;
pub(crate) mod event_outbox;
pub(crate) mod export_api;
pub(crate) mod groups;
pub(crate) mod health;
pub(crate) mod integrations_api;
pub(crate) mod metrics;
pub(crate) mod price_detector;
pub(crate) mod profit;
pub(crate) mod reconcile;
pub(crate) mod resilience;
pub(crate) mod routing;
pub(crate) mod trace;

mod engine;
mod error;
mod upstream_client;

pub(crate) use engine::{RelayEngine, RelayEngineConfig};
pub(crate) use error::RelayError;
