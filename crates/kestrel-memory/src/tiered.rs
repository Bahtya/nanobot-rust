//! TieredMemoryStore — composes L1 (HotStore) and L2 (WarmStore) into a single MemoryStore.
//!
//! Write-through: `store` writes to L1 then L2. L2 failures are logged but don't fail the call.
//! Read-fallback: `recall` checks L1 first, then L2. A hit in L2 is promoted to L1.
//! Merged search: `search` queries both layers, deduplicates by entry ID, and sorts by score.

use async_trait::async_trait;
use std::sync::Arc;

use crate::error::Result;
use crate::store::MemoryStore;
use crate::types::{MemoryEntry, MemoryQuery, ScoredEntry};

/// Tiered memory store combining a fast L1 cache with a persistent L2 backend.
///
/// All write operations go to both layers (write-through). L2 write failures
/// are logged as warnings but do not propagate — L1 is the authoritative
/// write buffer. Read operations check L1 first and fall back to L2; an L2
/// hit is promoted into L1 so subsequent reads are fast.
pub struct TieredMemoryStore {
    /// L1 — fast in-memory LRU cache with JSONL persistence.
    l1: Arc<dyn MemoryStore>,
    /// L2 — persistent semantic vector store (WarmStore / LanceDB).
    l2: Arc<dyn MemoryStore>,
}

impl TieredMemoryStore {
    /// Create a new tiered store from the two backing layers.
    pub fn new(l1: Arc<dyn MemoryStore>, l2: Arc<dyn MemoryStore>) -> Self {
        Self { l1, l2 }
    }
}

#[async_trait]
impl MemoryStore for TieredMemoryStore {
    async fn store(&self, entry: MemoryEntry) -> Result<()> {
        // L1 is authoritative — must succeed.
        self.l1.store(entry.clone()).await?;

        // L2 is best-effort — log but don't propagate failure.
        if let Err(e) = self.l2.store(entry).await {
            tracing::warn!("L2 store failed (entry still in L1): {}", e);
        }
        Ok(())
    }

    async fn recall(&self, id: &str) -> Result<Option<MemoryEntry>> {
        // L1 first — zero-latency path.
        if let Some(entry) = self.l1.recall(id).await? {
            return Ok(Some(entry));
        }

        // L2 fallback — promote hit into L1.
        let entry = match self.l2.recall(id).await? {
            Some(e) => e,
            None => return Ok(None),
        };

        let promoted = entry.clone();
        if let Err(e) = self.l1.store(promoted).await {
            tracing::warn!("L1 promote from L2 failed: {}", e);
        }
        Ok(Some(entry))
    }

    async fn search(&self, query: &MemoryQuery) -> Result<Vec<ScoredEntry>> {
        let l1_results = self.l1.search(query).await?;
        let l2_results = self.l2.search(query).await?;

        // Merge and deduplicate by entry ID, keeping the higher score.
        let mut seen = std::collections::HashSet::new();
        let mut merged: Vec<ScoredEntry> = Vec::with_capacity(l1_results.len() + l2_results.len());

        for scored in l1_results.into_iter().chain(l2_results) {
            if seen.insert(scored.entry.id.clone()) {
                merged.push(scored);
            }
        }

        merged.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        merged.truncate(query.limit);
        Ok(merged)
    }

    async fn delete(&self, id: &str) -> Result<()> {
        // Delete from both layers. L2 failure is non-fatal.
        self.l1.delete(id).await?;
        if let Err(e) = self.l2.delete(id).await {
            tracing::warn!("L2 delete failed: {}", e);
        }
        Ok(())
    }

    async fn len(&self) -> usize {
        // Approximate — L1 may overlap with L2 after promotion.
        self.l1.len().await
    }

    async fn clear(&self) -> Result<()> {
        self.l1.clear().await?;
        if let Err(e) = self.l2.clear().await {
            tracing::warn!("L2 clear failed: {}", e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MemoryConfig;
    use crate::hot_store::HotStore;
    use crate::types::MemoryCategory;
    use crate::warm_store::WarmStore;

    async fn make_tiered_store() -> (TieredMemoryStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = MemoryConfig::for_test(dir.path());
        let l1 = Arc::new(HotStore::new(&config).await.unwrap());
        let l2 = Arc::new(WarmStore::new(&config).await.unwrap());
        (TieredMemoryStore::new(l1, l2), dir)
    }

    #[tokio::test]
    async fn test_store_and_recall() {
        let (store, _dir) = make_tiered_store().await;
        let entry = MemoryEntry::new("tiered entry", MemoryCategory::Fact);
        let id = entry.id.clone();

        store.store(entry).await.unwrap();
        let recalled = store.recall(&id).await.unwrap();
        assert!(recalled.is_some());
        assert_eq!(recalled.unwrap().content, "tiered entry");
    }

    #[tokio::test]
    async fn test_recall_nonexistent() {
        let (store, _dir) = make_tiered_store().await;
        let result = store.recall("no-id").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_recall_increments_access_count() {
        let (store, _dir) = make_tiered_store().await;
        let entry = MemoryEntry::new("count me", MemoryCategory::Fact);
        let id = entry.id.clone();

        store.store(entry).await.unwrap();
        assert_eq!(store.recall(&id).await.unwrap().unwrap().access_count, 1);
        assert_eq!(store.recall(&id).await.unwrap().unwrap().access_count, 2);
    }

    #[tokio::test]
    async fn test_delete() {
        let (store, _dir) = make_tiered_store().await;
        let entry = MemoryEntry::new("delete me", MemoryCategory::Fact);
        let id = entry.id.clone();

        store.store(entry).await.unwrap();
        store.delete(&id).await.unwrap();
        assert!(store.recall(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_clear() {
        let (store, _dir) = make_tiered_store().await;
        store
            .store(MemoryEntry::new("a", MemoryCategory::Fact))
            .await
            .unwrap();
        store
            .store(MemoryEntry::new("b", MemoryCategory::AgentNote))
            .await
            .unwrap();

        store.clear().await.unwrap();
        assert!(store.is_empty().await);
    }

    #[tokio::test]
    async fn test_search_merges_both_layers() {
        let dir = tempfile::tempdir().unwrap();
        let config = MemoryConfig::for_test(dir.path());

        // Only L2 has entries, L1 is empty
        let l1 = Arc::new(HotStore::new(&config).await.unwrap());
        let l2 = Arc::new(WarmStore::new(&config).await.unwrap());

        l2.store(MemoryEntry::new("from l2", MemoryCategory::Fact))
            .await
            .unwrap();
        l1.store(MemoryEntry::new("from l1", MemoryCategory::Fact))
            .await
            .unwrap();

        let tiered = TieredMemoryStore::new(l1, l2);
        let results = tiered
            .search(&MemoryQuery::new().with_limit(10))
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
    }

    #[tokio::test]
    async fn test_l2_hit_promoted_to_l1() {
        let dir = tempfile::tempdir().unwrap();
        let config = MemoryConfig::for_test(dir.path());

        let l1 = Arc::new(HotStore::new(&config).await.unwrap());
        let l2 = Arc::new(WarmStore::new(&config).await.unwrap());

        // Store only in L2 (bypass tiered)
        let entry = MemoryEntry::new("l2 only", MemoryCategory::Fact);
        let id = entry.id.clone();
        l2.store(entry).await.unwrap();

        let tiered = TieredMemoryStore::new(l1.clone(), l2);
        let recalled = tiered.recall(&id).await.unwrap();
        assert!(recalled.is_some());
        assert_eq!(recalled.unwrap().content, "l2 only");

        // Verify promoted to L1
        let l1_recall = l1.recall(&id).await.unwrap();
        assert!(l1_recall.is_some());
        assert_eq!(l1_recall.unwrap().content, "l2 only");
    }

    #[tokio::test]
    async fn test_search_deduplicates() {
        let dir = tempfile::tempdir().unwrap();
        let config = MemoryConfig::for_test(dir.path());

        let l1 = Arc::new(HotStore::new(&config).await.unwrap());
        let l2 = Arc::new(WarmStore::new(&config).await.unwrap());

        // Same entry in both layers
        let mut entry = MemoryEntry::new("dup", MemoryCategory::Fact);
        entry.embedding = Some(vec![1.0_f32; 8]);
        let id = entry.id.clone();
        l1.store(entry.clone()).await.unwrap();
        l2.store(entry).await.unwrap();

        let tiered = TieredMemoryStore::new(l1, l2);
        let results = tiered
            .search(&MemoryQuery::new().with_limit(10))
            .await
            .unwrap();

        let matches: Vec<_> = results.iter().filter(|r| r.entry.id == id).collect();
        assert_eq!(matches.len(), 1);
    }

    #[tokio::test]
    async fn test_persistence_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let config = MemoryConfig::for_test(dir.path());

        let entry = MemoryEntry::new("persisted", MemoryCategory::Fact);
        let id = entry.id.clone();

        {
            let l1 = Arc::new(HotStore::new(&config).await.unwrap());
            let l2 = Arc::new(WarmStore::new(&config).await.unwrap());
            let tiered = TieredMemoryStore::new(l1, l2);
            tiered.store(entry).await.unwrap();
        }

        // Re-create from same paths
        let l1 = Arc::new(HotStore::new(&config).await.unwrap());
        let l2 = Arc::new(WarmStore::new(&config).await.unwrap());
        let tiered = TieredMemoryStore::new(l1, l2);

        let recalled = tiered.recall(&id).await.unwrap();
        assert!(recalled.is_some());
        assert_eq!(recalled.unwrap().content, "persisted");
    }

    #[tokio::test]
    async fn test_search_with_embedding_merges_scores() {
        let dir = tempfile::tempdir().unwrap();
        let config = MemoryConfig::for_test(dir.path());

        let l1 = Arc::new(HotStore::new(&config).await.unwrap());
        let l2 = Arc::new(WarmStore::new(&config).await.unwrap());

        // L1: entry somewhat similar to [1,0,0,...]
        let mut e1 = MemoryEntry::new("hot cat", MemoryCategory::Fact);
        e1.embedding = Some(vec![0.5_f32, 0.5, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        l1.store(e1).await.unwrap();

        // L2: entry identical to query → cosine similarity = 1.0
        let mut e2 = MemoryEntry::new("warm cat", MemoryCategory::Fact);
        e2.embedding = Some(vec![1.0_f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        l2.store(e2).await.unwrap();

        let tiered = TieredMemoryStore::new(l1, l2);
        let results = tiered
            .search(&MemoryQuery::new().with_embedding(vec![1.0_f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]).with_limit(2))
            .await
            .unwrap();

        assert_eq!(results.len(), 2);
        // Exact match (L2) scores 1.0, partial match (L1) scores ~0.707
        assert!(results[0].entry.content.contains("warm cat"));
        assert!(results[0].score > results[1].score);
    }
}
