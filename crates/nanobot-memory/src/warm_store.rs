//! WarmStore (L2) — semantic vector search with in-memory KNN.
//!
//! This module provides KNN (K-Nearest Neighbors) search over memory entries
//! using cosine similarity on embedding vectors. The current implementation
//! uses an in-memory store; a LanceDB backend can be swapped in by implementing
//! the [`MemoryStore`] trait with the `lancedb` feature flag.

use async_trait::async_trait;
use std::collections::HashMap;
use tokio::sync::RwLock;

use crate::config::MemoryConfig;
use crate::error::{MemoryError, Result};
use crate::hot_store::cosine_similarity;
use crate::store::MemoryStore;
use crate::types::{EntryId, MemoryEntry, MemoryQuery, ScoredEntry};

/// L2 warm memory store — semantic vector search over embeddings.
///
/// Entries are stored in memory with their embedding vectors.
/// Search uses cosine similarity for KNN retrieval, providing
/// millisecond-level latency for semantic queries.
pub struct WarmStore {
    /// In-memory entry map (id → entry).
    entries: RwLock<HashMap<EntryId, MemoryEntry>>,
    /// Maximum number of entries.
    max_entries: usize,
    /// Expected embedding dimension.
    embedding_dim: usize,
}

impl WarmStore {
    /// Create a new WarmStore with the given configuration.
    pub fn new(config: &MemoryConfig) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            max_entries: config.max_entries,
            embedding_dim: config.embedding_dim,
        }
    }

    /// Validate that an entry's embedding matches the expected dimension.
    fn validate_embedding(&self, entry: &MemoryEntry) -> Result<()> {
        if let Some(ref embedding) = entry.embedding {
            if embedding.len() != self.embedding_dim {
                return Err(MemoryError::InvalidEmbedding {
                    expected: self.embedding_dim,
                    actual: embedding.len(),
                });
            }
        }
        Ok(())
    }
}

#[async_trait]
impl MemoryStore for WarmStore {
    async fn store(&self, entry: MemoryEntry) -> Result<()> {
        self.validate_embedding(&entry)?;
        let mut entries = self.entries.write().await;
        if entries.len() >= self.max_entries && !entries.contains_key(&entry.id) {
            return Err(MemoryError::CapacityExceeded {
                max: self.max_entries,
                current: entries.len(),
            });
        }
        entries.insert(entry.id.clone(), entry);
        Ok(())
    }

    async fn recall(&self, id: &str) -> Result<Option<MemoryEntry>> {
        let mut entries = self.entries.write().await;
        if let Some(entry) = entries.get_mut(id) {
            entry.touch();
            Ok(Some(entry.clone()))
        } else {
            Ok(None)
        }
    }

    async fn search(&self, query: &MemoryQuery) -> Result<Vec<ScoredEntry>> {
        let entries = self.entries.read().await;
        let query_embedding = match &query.embedding {
            Some(e) => e,
            None => {
                // Without an embedding, fall back to text/category filter
                let mut results: Vec<ScoredEntry> = entries
                    .values()
                    .filter(|entry| matches_filters(entry, query))
                    .map(|entry| ScoredEntry {
                        entry: entry.clone(),
                        score: 1.0,
                    })
                    .collect();
                results.truncate(query.limit);
                return Ok(results);
            }
        };

        // KNN search using cosine similarity
        let mut scored: Vec<ScoredEntry> = entries
            .values()
            .filter(|entry| matches_filters(entry, query))
            .filter_map(|entry| {
                entry.embedding.as_ref().map(|emb| {
                    let score = cosine_similarity(query_embedding, emb);
                    ScoredEntry {
                        entry: entry.clone(),
                        score,
                    }
                })
            })
            .collect();

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(query.limit);
        Ok(scored)
    }

    async fn delete(&self, id: &str) -> Result<()> {
        self.entries.write().await.remove(id);
        Ok(())
    }

    async fn len(&self) -> usize {
        self.entries.read().await.len()
    }

    async fn clear(&self) -> Result<()> {
        self.entries.write().await.clear();
        Ok(())
    }
}

/// Check if an entry matches the filter criteria in a query.
fn matches_filters(entry: &MemoryEntry, query: &MemoryQuery) -> bool {
    if let Some(ref cat) = query.category {
        if entry.category != *cat {
            return false;
        }
    }
    if let Some(min_conf) = query.min_confidence {
        if entry.confidence < min_conf {
            return false;
        }
    }
    if let Some(ref text) = query.text {
        if !entry.content.to_lowercase().contains(&text.to_lowercase()) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MemoryCategory;

    fn make_test_store() -> WarmStore {
        let dir = tempfile::tempdir().unwrap();
        let config = MemoryConfig::for_test(dir.path());
        WarmStore::new(&config)
    }

    #[tokio::test]
    async fn test_store_and_recall() {
        let store = make_test_store();
        let entry = MemoryEntry::new("warm entry", MemoryCategory::Fact);
        let id = entry.id.clone();

        store.store(entry).await.unwrap();
        let recalled = store.recall(&id).await.unwrap();
        assert!(recalled.is_some());
        assert_eq!(recalled.unwrap().content, "warm entry");
    }

    #[tokio::test]
    async fn test_recall_nonexistent() {
        let store = make_test_store();
        let result = store.recall("no-id").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_recall_increments_access_count() {
        let store = make_test_store();
        let entry = MemoryEntry::new("count me", MemoryCategory::Fact);
        let id = entry.id.clone();

        store.store(entry).await.unwrap();
        assert_eq!(store.recall(&id).await.unwrap().unwrap().access_count, 1);
        assert_eq!(store.recall(&id).await.unwrap().unwrap().access_count, 2);
    }

    #[tokio::test]
    async fn test_delete() {
        let store = make_test_store();
        let entry = MemoryEntry::new("delete me", MemoryCategory::Fact);
        let id = entry.id.clone();

        store.store(entry).await.unwrap();
        assert_eq!(store.len().await, 1);

        store.delete(&id).await.unwrap();
        assert_eq!(store.len().await, 0);
    }

    #[tokio::test]
    async fn test_clear() {
        let store = make_test_store();
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
    async fn test_knn_search() {
        let store = make_test_store();

        // Store entries with known embeddings
        let mut e1 = MemoryEntry::new("cat document", MemoryCategory::Fact);
        e1.embedding = Some(vec![1.0_f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let mut e2 = MemoryEntry::new("dog document", MemoryCategory::Fact);
        e2.embedding = Some(vec![0.0_f32, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let mut e3 = MemoryEntry::new("cat related", MemoryCategory::Fact);
        e3.embedding = Some(vec![0.9_f32, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);

        store.store(e1).await.unwrap();
        store.store(e2).await.unwrap();
        store.store(e3).await.unwrap();

        // Search with a "cat-like" embedding
        let query = MemoryQuery::new()
            .with_embedding(vec![1.0_f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
            .with_limit(2);

        let results = store.search(&query).await.unwrap();
        assert_eq!(results.len(), 2);
        // First should be the exact match
        assert!(results[0].entry.content.contains("cat document"));
        assert!(results[0].score > results[1].score);
    }

    #[tokio::test]
    async fn test_search_without_embedding() {
        let store = make_test_store();
        store
            .store(MemoryEntry::new("rust lang", MemoryCategory::Fact))
            .await
            .unwrap();
        store
            .store(MemoryEntry::new("python lang", MemoryCategory::Fact))
            .await
            .unwrap();

        let results = store
            .search(&MemoryQuery::new().with_text("rust"))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].entry.content.contains("rust"));
    }

    #[tokio::test]
    async fn test_search_by_category() {
        let store = make_test_store();
        store
            .store(MemoryEntry::new("note 1", MemoryCategory::Fact))
            .await
            .unwrap();
        store
            .store(MemoryEntry::new("note 2", MemoryCategory::UserProfile))
            .await
            .unwrap();

        let results = store
            .search(&MemoryQuery::new().with_category(MemoryCategory::UserProfile))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry.category, MemoryCategory::UserProfile);
    }

    #[tokio::test]
    async fn test_search_respects_limit() {
        let store = make_test_store();
        for i in 0..20 {
            store
                .store(MemoryEntry::new(format!("entry {i}"), MemoryCategory::Fact))
                .await
                .unwrap();
        }

        let results = store
            .search(&MemoryQuery::new().with_limit(5))
            .await
            .unwrap();
        assert_eq!(results.len(), 5);
    }

    #[tokio::test]
    async fn test_invalid_embedding_dimension() {
        let store = make_test_store(); // embedding_dim = 8
        let mut entry = MemoryEntry::new("bad embedding", MemoryCategory::Fact);
        entry.embedding = Some(vec![1.0_f32, 2.0]); // Wrong dimension

        let result = store.store(entry).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("expected dimension 8"));
    }

    #[tokio::test]
    async fn test_capacity_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = MemoryConfig::for_test(dir.path());
        config.max_entries = 2;

        let store = WarmStore::new(&config);
        store
            .store(MemoryEntry::new("a", MemoryCategory::Fact))
            .await
            .unwrap();
        store
            .store(MemoryEntry::new("b", MemoryCategory::Fact))
            .await
            .unwrap();

        let result = store
            .store(MemoryEntry::new("c", MemoryCategory::Fact))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_knn_entries_without_embeddings_skipped() {
        let store = make_test_store();

        // Entry with embedding
        let mut e1 = MemoryEntry::new("with embedding", MemoryCategory::Fact);
        e1.embedding = Some(vec![1.0_f32; 8]);
        store.store(e1).await.unwrap();

        // Entry without embedding
        store
            .store(MemoryEntry::new("no embedding", MemoryCategory::Fact))
            .await
            .unwrap();

        let results = store
            .search(&MemoryQuery::new().with_embedding(vec![1.0_f32; 8]))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].entry.content.contains("with embedding"));
    }

    #[tokio::test]
    async fn test_store_overwrite_within_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = MemoryConfig::for_test(dir.path());
        config.max_entries = 1;

        let store = WarmStore::new(&config);
        let mut entry = MemoryEntry::new("original", MemoryCategory::Fact);
        let id = entry.id.clone();
        store.store(entry).await.unwrap();

        // Overwrite same ID should work
        entry = MemoryEntry::new("updated", MemoryCategory::Fact);
        entry.id = id.clone();
        store.store(entry).await.unwrap();

        let recalled = store.recall(&id).await.unwrap().unwrap();
        assert_eq!(recalled.content, "updated");
        assert_eq!(store.len().await, 1);
    }
}
