//! The MemoryStore trait — unified async interface for all memory backends.

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{MemoryEntry, MemoryQuery, ScoredEntry};

/// Async interface for memory storage backends.
///
/// All memory stores (HotStore L1, WarmStore L2) implement this trait,
/// providing a uniform API for storing, recalling, searching, and deleting
/// memory entries.
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// Store a memory entry. If an entry with the same ID exists, it is replaced.
    async fn store(&self, entry: MemoryEntry) -> Result<()>;

    /// Recall a specific memory entry by ID.
    ///
    /// Returns `None` if no entry with the given ID exists.
    /// Increments the entry's access count on successful recall.
    async fn recall(&self, id: &str) -> Result<Option<MemoryEntry>>;

    /// Search memories matching the given query.
    ///
    /// Results are sorted by relevance score (descending) and limited
    /// to `query.limit` entries.
    async fn search(&self, query: &MemoryQuery) -> Result<Vec<ScoredEntry>>;

    /// Delete a memory entry by ID. No-op if the ID doesn't exist.
    async fn delete(&self, id: &str) -> Result<()>;

    /// Return the number of entries in the store.
    async fn len(&self) -> usize;

    /// Check if the store is empty.
    async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// Remove all entries from the store.
    async fn clear(&self) -> Result<()>;
}
