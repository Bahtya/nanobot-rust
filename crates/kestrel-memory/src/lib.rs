//! # kestrel-memory
//!
//! Layered memory system for the kestrel AI agent framework.
//!
//! This crate provides:
//! - [`MemoryStore`] trait — unified async interface for memory backends
//! - [`HotStore`] (L1) — in-memory LRU cache with JSON lines file persistence
//! - [`WarmStore`] (L2) — persistent semantic vector search via LanceDB
//! - [`MemoryEntry`] — typed memory entries with metadata and embeddings
//! - [`MemoryConfig`] — TOML-based configuration

pub mod config;
pub mod error;
pub mod hot_store;
pub mod security_scan;
pub mod store;
pub mod tiered;
pub mod types;
pub mod warm_store;

pub use config::MemoryConfig;
pub use error::MemoryError;
pub use hot_store::HotStore;
pub use security_scan::{scan_memory_entry, SecurityScanResult};
pub use store::MemoryStore;
pub use tiered::TieredMemoryStore;
pub use types::{EntryId, MemoryCategory, MemoryEntry, MemoryQuery, ScoredEntry};
pub use warm_store::WarmStore;
