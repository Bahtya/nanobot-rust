//! HotStore (L1) — in-memory HashMap with JSON lines file persistence.
//!
//! The hot store provides the fastest access layer (zero latency) for frequently
//! used memory entries. Data is kept in memory and periodically flushed to disk
//! in JSON lines format (one JSON object per line).
//!
//! File writes use the atomic temp-file-rename pattern to prevent corruption.

use async_trait::async_trait;
use std::collections::HashMap;
use tokio::fs;
use tokio::sync::RwLock;

use crate::config::MemoryConfig;
use crate::error::{MemoryError, Result};
use crate::store::MemoryStore;
use crate::types::{EntryId, MemoryEntry, MemoryQuery, ScoredEntry};

/// L1 hot memory store — fast in-memory access with file persistence.
///
/// Entries are kept in a [`HashMap`] for O(1) lookups and persisted to disk
/// in JSON lines format (one JSON object per line). File writes are atomic
/// using the temp-file-rename pattern.
pub struct HotStore {
    /// In-memory entry map.
    entries: RwLock<HashMap<EntryId, MemoryEntry>>,
    /// Path to the persistence file.
    path: std::path::PathBuf,
    /// Maximum number of entries allowed.
    max_entries: usize,
}

impl HotStore {
    /// Create a new HotStore, loading any existing data from disk.
    pub async fn new(config: &MemoryConfig) -> Result<Self> {
        let store = Self {
            entries: RwLock::new(HashMap::new()),
            path: config.hot_store_path.clone(),
            max_entries: config.max_entries,
        };
        store.load_from_disk().await?;
        Ok(store)
    }

    /// Load entries from the JSON lines file on disk.
    async fn load_from_disk(&self) -> Result<()> {
        if !self.path.exists() {
            return Ok(());
        }
        let content = fs::read_to_string(&self.path).await?;
        let mut entries = self.entries.write().await;
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<MemoryEntry>(line) {
                Ok(entry) => {
                    entries.insert(entry.id.clone(), entry);
                }
                Err(_) => continue,
            }
        }
        Ok(())
    }

    /// Persist all entries to disk using atomic write (temp + rename).
    async fn save_to_disk(&self) -> Result<()> {
        let entries = self.entries.read().await;

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).await?;
        }

        let temp_path = self.path.with_extension("jsonl.tmp");
        let mut lines = String::new();
        for entry in entries.values() {
            lines.push_str(&serde_json::to_string(entry)?);
            lines.push('\n');
        }
        fs::write(&temp_path, &lines).await?;
        fs::rename(&temp_path, &self.path).await?;
        Ok(())
    }
}

#[async_trait]
impl MemoryStore for HotStore {
    async fn store(&self, entry: MemoryEntry) -> Result<()> {
        {
            let mut entries = self.entries.write().await;
            if entries.len() >= self.max_entries && !entries.contains_key(&entry.id) {
                return Err(MemoryError::CapacityExceeded {
                    max: self.max_entries,
                    current: entries.len(),
                });
            }
            entries.insert(entry.id.clone(), entry);
        }
        self.save_to_disk().await?;
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
        let mut results: Vec<ScoredEntry> = entries
            .values()
            .filter(|entry| matches_filters(entry, query))
            .map(|entry| {
                let score = compute_score(entry, query);
                ScoredEntry {
                    entry: entry.clone(),
                    score,
                }
            })
            .collect();

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(query.limit);
        Ok(results)
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let removed = {
            let mut entries = self.entries.write().await;
            entries.remove(id).is_some()
        };
        if removed {
            self.save_to_disk().await?;
        }
        Ok(())
    }

    async fn len(&self) -> usize {
        self.entries.read().await.len()
    }

    async fn clear(&self) -> Result<()> {
        self.entries.write().await.clear();
        self.save_to_disk().await?;
        Ok(())
    }
}

/// Compute a relevance score for an entry given a query.
fn compute_score(entry: &MemoryEntry, query: &MemoryQuery) -> f64 {
    if let Some(ref query_embedding) = query.embedding {
        if let Some(ref entry_embedding) = entry.embedding {
            return cosine_similarity(query_embedding, entry_embedding);
        }
    }
    1.0
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

/// Compute cosine similarity between two vectors.
///
/// Returns 0.0 if vectors have different lengths or are empty.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(x, y)| (f64::from(*x)) * (f64::from(*y)))
        .sum();
    let norm_a: f64 = a.iter().map(|x| (f64::from(*x)).powi(2)).sum::<f64>().sqrt();
    let norm_b: f64 = b.iter().map(|x| (f64::from(*x)).powi(2)).sum::<f64>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MemoryConfig;
    use crate::types::MemoryCategory;

    async fn make_test_store() -> (HotStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = MemoryConfig::for_test(dir.path());
        let store = HotStore::new(&config).await.unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn test_store_and_recall() {
        let (store, _dir) = make_test_store().await;
        let entry = MemoryEntry::new("hello world", MemoryCategory::Fact);
        let id = entry.id.clone();

        store.store(entry).await.unwrap();
        let recalled = store.recall(&id).await.unwrap();
        assert!(recalled.is_some());
        assert_eq!(recalled.unwrap().content, "hello world");
    }

    #[tokio::test]
    async fn test_recall_nonexistent() {
        let (store, _dir) = make_test_store().await;
        let result = store.recall("nonexistent-id").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_recall_increments_access_count() {
        let (store, _dir) = make_test_store().await;
        let entry = MemoryEntry::new("access test", MemoryCategory::AgentNote);
        let id = entry.id.clone();

        store.store(entry).await.unwrap();
        assert_eq!(store.recall(&id).await.unwrap().unwrap().access_count, 1);
        assert_eq!(store.recall(&id).await.unwrap().unwrap().access_count, 2);
    }

    #[tokio::test]
    async fn test_store_persists_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let config = MemoryConfig::for_test(dir.path());
        let path = config.hot_store_path.clone();

        let entry = MemoryEntry::new("persisted", MemoryCategory::Fact);
        let id = entry.id.clone();

        {
            let store = HotStore::new(&config).await.unwrap();
            store.store(entry).await.unwrap();
        }

        // Verify file exists and contains the entry
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("persisted"));

        // Load into a new store instance
        let store2 = HotStore::new(&config).await.unwrap();
        let recalled = store2.recall(&id).await.unwrap();
        assert!(recalled.is_some());
        assert_eq!(recalled.unwrap().content, "persisted");
    }

    #[tokio::test]
    async fn test_delete() {
        let (store, _dir) = make_test_store().await;
        let entry = MemoryEntry::new("to delete", MemoryCategory::Fact);
        let id = entry.id.clone();

        store.store(entry).await.unwrap();
        assert_eq!(store.len().await, 1);

        store.delete(&id).await.unwrap();
        assert_eq!(store.len().await, 0);
        assert!(store.recall(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_delete_nonexistent() {
        let (store, _dir) = make_test_store().await;
        // Should not error
        store.delete("no-such-id").await.unwrap();
    }

    #[tokio::test]
    async fn test_clear() {
        let (store, _dir) = make_test_store().await;
        store
            .store(MemoryEntry::new("a", MemoryCategory::Fact))
            .await
            .unwrap();
        store
            .store(MemoryEntry::new("b", MemoryCategory::AgentNote))
            .await
            .unwrap();

        assert_eq!(store.len().await, 2);
        store.clear().await.unwrap();
        assert_eq!(store.len().await, 0);
        assert!(store.is_empty().await);
    }

    #[tokio::test]
    async fn test_search_by_text() {
        let (store, _dir) = make_test_store().await;
        store
            .store(MemoryEntry::new("Rust programming", MemoryCategory::Fact))
            .await
            .unwrap();
        store
            .store(MemoryEntry::new("Python scripting", MemoryCategory::Fact))
            .await
            .unwrap();

        let results = store
            .search(&MemoryQuery::new().with_text("rust"))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].entry.content.contains("Rust"));
    }

    #[tokio::test]
    async fn test_search_by_category() {
        let (store, _dir) = make_test_store().await;
        store
            .store(MemoryEntry::new("note 1", MemoryCategory::Fact))
            .await
            .unwrap();
        store
            .store(MemoryEntry::new("note 2", MemoryCategory::AgentNote))
            .await
            .unwrap();

        let results = store
            .search(&MemoryQuery::new().with_category(MemoryCategory::AgentNote))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].entry.category, MemoryCategory::AgentNote);
    }

    #[tokio::test]
    async fn test_search_by_confidence() {
        let (store, _dir) = make_test_store().await;
        store
            .store(
                MemoryEntry::new("high conf", MemoryCategory::Fact).with_confidence(0.9),
            )
            .await
            .unwrap();
        store
            .store(
                MemoryEntry::new("low conf", MemoryCategory::Fact).with_confidence(0.3),
            )
            .await
            .unwrap();

        let results = store
            .search(&MemoryQuery::new().with_min_confidence(0.5))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].entry.content.contains("high conf"));
    }

    #[tokio::test]
    async fn test_search_with_embedding() {
        let (store, _dir) = make_test_store().await;
        store
            .store(
                MemoryEntry::new("similar", MemoryCategory::Fact)
                    .with_embedding(vec![1.0, 0.0, 0.0, 0.0]),
            )
            .await
            .unwrap();
        store
            .store(
                MemoryEntry::new("different", MemoryCategory::Fact)
                    .with_embedding(vec![0.0, 0.0, 0.0, 1.0]),
            )
            .await
            .unwrap();

        let results = store
            .search(
                &MemoryQuery::new()
                    .with_embedding(vec![1.0, 0.0, 0.0, 0.0])
                    .with_limit(1),
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].entry.content.contains("similar"));
        assert!(results[0].score > 0.99);
    }

    #[tokio::test]
    async fn test_capacity_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = MemoryConfig::for_test(dir.path());
        config.max_entries = 2;

        let store = HotStore::new(&config).await.unwrap();
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
        assert!(result.unwrap_err().to_string().contains("capacity"));
    }

    #[tokio::test]
    async fn test_store_overwrite_within_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = MemoryConfig::for_test(dir.path());
        config.max_entries = 1;

        let store = HotStore::new(&config).await.unwrap();
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

    #[tokio::test]
    async fn test_load_malformed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let config = MemoryConfig::for_test(dir.path());
        let path = config.hot_store_path.clone();

        // Write file with one valid and one malformed line
        let valid_entry = MemoryEntry::new("valid", MemoryCategory::Fact);
        let valid_id = valid_entry.id.clone();
        let mut content = serde_json::to_string(&valid_entry).unwrap();
        content.push('\n');
        content.push_str("this is not valid json\n");
        std::fs::write(&path, &content).unwrap();

        let store = HotStore::new(&config).await.unwrap();
        let recalled = store.recall(&valid_id).await.unwrap();
        assert!(recalled.is_some());
        assert_eq!(recalled.unwrap().content, "valid");
        assert_eq!(store.len().await, 1);
    }

    #[test]
    fn test_cosine_similarity_identical() {
        let v = vec![1.0_f32, 0.0, 0.0];
        let sim = cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![0.0_f32, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![-1.0_f32, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - (-1.0)).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_empty() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
    }

    #[test]
    fn test_cosine_similarity_different_lengths() {
        assert_eq!(cosine_similarity(&[1.0_f32], &[1.0, 2.0]), 0.0);
    }
}
