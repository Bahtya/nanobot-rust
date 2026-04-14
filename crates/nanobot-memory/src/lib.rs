//! # nanobot-memory
//!
//! Layered memory system for the nanobot-rust AI agent framework.
//!
//! This crate provides:
//! - [`MemoryStore`] trait — unified async interface for memory backends
//! - [`HotStore`] (L1) — in-memory HashMap with JSON lines file persistence
//! - [`WarmStore`] (L2) — semantic vector search (in-memory KNN)
//! - [`MemoryEntry`] — typed memory entries with metadata and embeddings
//! - [`MemoryConfig`] — TOML-based configuration

pub mod config;
pub mod error;
pub mod hot_store;
pub mod store;
pub mod types;
pub mod warm_store;

pub use config::MemoryConfig;
pub use error::MemoryError;
pub use hot_store::HotStore;
pub use store::MemoryStore;
pub use types::{EntryId, MemoryCategory, MemoryEntry, MemoryQuery, ScoredEntry};
pub use warm_store::WarmStore;
