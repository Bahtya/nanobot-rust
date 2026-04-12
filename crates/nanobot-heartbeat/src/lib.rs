//! # nanobot-heartbeat
//!
//! Heartbeat service — periodic health checks, auto-restart, and state persistence.

pub mod service;
pub mod types;

pub use service::HeartbeatService;
pub use types::*;
