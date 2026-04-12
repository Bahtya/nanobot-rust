//! State store trait and implementations for cron job persistence.
//!
//! Provides a `CronStateStore` trait with two implementations:
//! - `FileStateStore` — persists to a JSON file on disk (production)
//! - `MemoryStateStore` — in-memory only (testing)

use crate::types::CronJobState;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;

/// Trait for persisting and retrieving cron job state.
pub trait CronStateStore: Send + Sync {
    /// Save or update state for a single job.
    fn save_state(&self, job_id: &str, state: &CronJobState) -> Result<()>;

    /// Load state for a single job.
    fn load_state(&self, job_id: &str) -> Result<Option<CronJobState>>;

    /// List all stored job states.
    fn list_states(&self) -> Result<HashMap<String, CronJobState>>;

    /// Delete state for a single job.
    fn delete_state(&self, job_id: &str) -> Result<bool>;

    /// Save all states at once (atomic batch write for file-based stores).
    fn save_all(&self, states: &HashMap<String, CronJobState>) -> Result<()>;
}

// ── FileStateStore ──────────────────────────────────────────────

/// File-backed state store using a single JSON file.
///
/// The file contains a `HashMap<String, CronJobState>` serialized as JSON.
/// All writes are atomic (write whole file) and reads cache the last-known state.
pub struct FileStateStore {
    path: PathBuf,
    cache: parking_lot::Mutex<HashMap<String, CronJobState>>,
}

impl FileStateStore {
    /// Create a new FileStateStore backed by the given JSON file path.
    ///
    /// If the file exists, loads existing state. Otherwise starts empty.
    pub fn new(path: PathBuf) -> Result<Self> {
        let cache = if path.exists() {
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to read state file {}", path.display()))?;
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            if let Some(parent) = path.parent() {
                if !parent.exists() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            HashMap::new()
        };

        Ok(Self { path, cache: parking_lot::Mutex::new(cache) })
    }

    /// Flush the in-memory cache to disk.
    fn flush(&self) -> Result<()> {
        let states = self.cache.lock();
        let json = serde_json::to_string_pretty(&*states)
            .with_context(|| "Failed to serialize cron job states")?;
        std::fs::write(&self.path, json)
            .with_context(|| format!("Failed to write state file {}", self.path.display()))?;
        Ok(())
    }
}

impl CronStateStore for FileStateStore {
    fn save_state(&self, job_id: &str, state: &CronJobState) -> Result<()> {
        self.cache.lock().insert(job_id.to_string(), state.clone());
        self.flush()
    }

    fn load_state(&self, job_id: &str) -> Result<Option<CronJobState>> {
        Ok(self.cache.lock().get(job_id).cloned())
    }

    fn list_states(&self) -> Result<HashMap<String, CronJobState>> {
        Ok(self.cache.lock().clone())
    }

    fn delete_state(&self, job_id: &str) -> Result<bool> {
        let removed = self.cache.lock().remove(job_id).is_some();
        if removed {
            self.flush()?;
        }
        Ok(removed)
    }

    fn save_all(&self, states: &HashMap<String, CronJobState>) -> Result<()> {
        *self.cache.lock() = states.clone();
        self.flush()
    }
}

// ── MemoryStateStore ────────────────────────────────────────────

/// In-memory state store for testing.
pub struct MemoryStateStore {
    states: parking_lot::Mutex<HashMap<String, CronJobState>>,
}

impl MemoryStateStore {
    /// Create a new empty MemoryStateStore.
    pub fn new() -> Self {
        Self {
            states: parking_lot::Mutex::new(HashMap::new()),
        }
    }
}

impl Default for MemoryStateStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CronStateStore for MemoryStateStore {
    fn save_state(&self, job_id: &str, state: &CronJobState) -> Result<()> {
        self.states.lock().insert(job_id.to_string(), state.clone());
        Ok(())
    }

    fn load_state(&self, job_id: &str) -> Result<Option<CronJobState>> {
        Ok(self.states.lock().get(job_id).cloned())
    }

    fn list_states(&self) -> Result<HashMap<String, CronJobState>> {
        Ok(self.states.lock().clone())
    }

    fn delete_state(&self, job_id: &str) -> Result<bool> {
        Ok(self.states.lock().remove(job_id).is_some())
    }

    fn save_all(&self, states: &HashMap<String, CronJobState>) -> Result<()> {
        *self.states.lock() = states.clone();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn make_state(name: &str, active: bool, count: u64) -> CronJobState {
        CronJobState {
            job_name: Some(name.to_string()),
            last_run: Some(Utc::now()),
            next_run: Some(Utc::now() + Duration::hours(1)),
            is_active: active,
            run_count: count,
            last_error: None,
        }
    }

    fn make_state_with_error(name: &str, error: &str) -> CronJobState {
        CronJobState {
            job_name: Some(name.to_string()),
            last_run: Some(Utc::now()),
            next_run: None,
            is_active: false,
            run_count: 3,
            last_error: Some(error.to_string()),
        }
    }

    // === MemoryStateStore ===

    #[test]
    fn test_memory_save_and_load() {
        let store = MemoryStateStore::new();
        let state = make_state("job1", true, 5);
        store.save_state("id1", &state).unwrap();

        let loaded = store.load_state("id1").unwrap().unwrap();
        assert_eq!(loaded.job_name.as_deref(), Some("job1"));
        assert_eq!(loaded.run_count, 5);
        assert!(loaded.is_active);
    }

    #[test]
    fn test_memory_load_missing() {
        let store = MemoryStateStore::new();
        assert!(store.load_state("nope").unwrap().is_none());
    }

    #[test]
    fn test_memory_list_states_empty() {
        let store = MemoryStateStore::new();
        assert!(store.list_states().unwrap().is_empty());
    }

    #[test]
    fn test_memory_list_states_multiple() {
        let store = MemoryStateStore::new();
        store.save_state("a", &make_state("a", true, 1)).unwrap();
        store.save_state("b", &make_state("b", false, 2)).unwrap();

        let states = store.list_states().unwrap();
        assert_eq!(states.len(), 2);
        assert!(states.contains_key("a"));
        assert!(states.contains_key("b"));
    }

    #[test]
    fn test_memory_delete() {
        let store = MemoryStateStore::new();
        store.save_state("x", &make_state("x", true, 0)).unwrap();
        assert!(store.delete_state("x").unwrap());
        assert!(store.load_state("x").unwrap().is_none());
    }

    #[test]
    fn test_memory_delete_missing() {
        let store = MemoryStateStore::new();
        assert!(!store.delete_state("nope").unwrap());
    }

    #[test]
    fn test_memory_save_overwrites() {
        let store = MemoryStateStore::new();
        store.save_state("id", &make_state("v1", true, 1)).unwrap();
        store.save_state("id", &make_state("v2", false, 2)).unwrap();

        let loaded = store.load_state("id").unwrap().unwrap();
        assert_eq!(loaded.job_name.as_deref(), Some("v2"));
        assert!(!loaded.is_active);
        assert_eq!(loaded.run_count, 2);
    }

    #[test]
    fn test_memory_save_all() {
        let store = MemoryStateStore::new();
        let mut batch = HashMap::new();
        batch.insert("a".to_string(), make_state("a", true, 1));
        batch.insert("b".to_string(), make_state("b", true, 2));
        store.save_all(&batch).unwrap();

        let states = store.list_states().unwrap();
        assert_eq!(states.len(), 2);
    }

    #[test]
    fn test_memory_default() {
        let store = MemoryStateStore::default();
        assert!(store.list_states().unwrap().is_empty());
    }

    // === FileStateStore ===

    #[test]
    fn test_file_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron_job_states.json");
        let store = FileStateStore::new(path.clone()).unwrap();

        let state = make_state("file_job", true, 10);
        store.save_state("id1", &state).unwrap();

        // Verify file was written
        assert!(path.exists());

        let loaded = store.load_state("id1").unwrap().unwrap();
        assert_eq!(loaded.job_name.as_deref(), Some("file_job"));
        assert_eq!(loaded.run_count, 10);
    }

    #[test]
    fn test_file_load_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron_job_states.json");
        let store = FileStateStore::new(path).unwrap();
        assert!(store.load_state("nope").unwrap().is_none());
    }

    #[test]
    fn test_file_list_states() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron_job_states.json");
        let store = FileStateStore::new(path).unwrap();

        store.save_state("a", &make_state("a", true, 1)).unwrap();
        store.save_state("b", &make_state("b", false, 2)).unwrap();

        let states = store.list_states().unwrap();
        assert_eq!(states.len(), 2);
    }

    #[test]
    fn test_file_delete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron_job_states.json");
        let store = FileStateStore::new(path).unwrap();

        store.save_state("x", &make_state("x", true, 0)).unwrap();
        assert!(store.delete_state("x").unwrap());
        assert!(store.load_state("x").unwrap().is_none());
    }

    #[test]
    fn test_file_delete_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron_job_states.json");
        let store = FileStateStore::new(path).unwrap();
        assert!(!store.delete_state("nope").unwrap());
    }

    #[test]
    fn test_file_persists_across_instances() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron_job_states.json");

        {
            let store = FileStateStore::new(path.clone()).unwrap();
            store.save_state("persist", &make_state("persist", true, 99)).unwrap();
        }

        // Create a new instance from the same file
        let store2 = FileStateStore::new(path).unwrap();
        let loaded = store2.load_state("persist").unwrap().unwrap();
        assert_eq!(loaded.job_name.as_deref(), Some("persist"));
        assert_eq!(loaded.run_count, 99);
    }

    #[test]
    fn test_file_creates_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("sub").join("dir").join("states.json");
        let store = FileStateStore::new(nested.clone()).unwrap();
        store.save_state("x", &make_state("x", true, 1)).unwrap();
        assert!(nested.exists());
    }

    #[test]
    fn test_file_save_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron_job_states.json");
        let store = FileStateStore::new(path).unwrap();

        let mut batch = HashMap::new();
        batch.insert("a".to_string(), make_state("a", true, 1));
        batch.insert("b".to_string(), make_state("b", true, 2));
        store.save_all(&batch).unwrap();

        let states = store.list_states().unwrap();
        assert_eq!(states.len(), 2);
    }

    #[test]
    fn test_file_survives_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron_job_states.json");

        // Write empty JSON
        std::fs::write(&path, "{}").unwrap();

        let store = FileStateStore::new(path).unwrap();
        assert!(store.list_states().unwrap().is_empty());
    }

    #[test]
    fn test_file_state_with_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cron_job_states.json");
        let store = FileStateStore::new(path).unwrap();

        let state = make_state_with_error("broken", "connection timeout");
        store.save_state("err1", &state).unwrap();

        let loaded = store.load_state("err1").unwrap().unwrap();
        assert_eq!(loaded.last_error.as_deref(), Some("connection timeout"));
        assert!(!loaded.is_active);
    }
}
