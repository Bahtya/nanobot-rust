//! # kestrel-memory
//!
//! Full-text memory system for the kestrel AI agent framework.
//!
//! This crate provides:
//! - [`MemoryStore`] trait — unified async interface for memory backends
//! - [`TantivyStore`] — tantivy-backed full-text search with jieba CJK tokenization
//! - [`MemoryEntry`] — typed memory entries with metadata
//! - [`MemoryConfig`] — TOML-based configuration

pub mod config;
pub mod error;
pub mod security_scan;
pub mod store;
pub mod tantivy_store;
pub mod text_search;
pub mod types;

pub use config::MemoryConfig;
pub use error::MemoryError;
pub use security_scan::{scan_memory_entry, SecurityScanResult};
pub use store::MemoryStore;
pub use tantivy_store::TantivyStore;
pub use types::{EntryId, MemoryCategory, MemoryEntry, MemoryQuery, ScoredEntry};
