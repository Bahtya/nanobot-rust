//! # kestrel-heartbeat
//!
//! Heartbeat service — periodic health checks, auto-restart, and state persistence.
//!
//! Components implement the `HealthCheck` trait and register with the
//! `HealthCheckRegistry`. The `HeartbeatService` polls all registered
//! components periodically, tracks consecutive failures, and triggers
//! automatic restarts with exponential backoff via the message bus.
//!
//! Built-in checks for core system components are provided in [`checks`]:
//! - [`checks::ProviderHealthCheck`] — LLM provider connectivity
//! - [`checks::BusHealthCheck`] — message bus liveness
//! - [`checks::MemoryStoreHealthCheck`] — memory store responsiveness
//! - [`checks::ChannelHealthCheck`] — channel connectivity

pub mod checks;
pub mod service;
pub mod types;

pub use checks::{BusHealthCheck, ChannelHealthCheck, MemoryStoreHealthCheck, ProviderHealthCheck};
pub use service::HeartbeatService;
pub use types::*;
