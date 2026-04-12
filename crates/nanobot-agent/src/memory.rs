//! Memory store — file-based persistent memory.
//!
//! Mirrors the Python `agent/memory.py` MemoryStore with file I/O,
//! Consolidator, and Dream for scheduled memory consolidation.

use anyhow::{Context, Result};
use std::path::PathBuf;
use tracing::info;

/// Persistent memory store using markdown files.
pub struct MemoryStore {
    memory_dir: PathBuf,
}

impl MemoryStore {
    pub fn new(memory_dir: PathBuf) -> Result<Self> {
        if !memory_dir.exists() {
            std::fs::create_dir_all(&memory_dir)?;
        }
        Ok(Self { memory_dir })
    }

    /// Read the main memory file.
    pub fn read_memory(&self) -> Result<String> {
        let path = self.memory_dir.join("MEMORY.md");
        if !path.exists() {
            return Ok(String::new());
        }
        std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read memory file: {}", path.display()))
    }

    /// Write the main memory file.
    pub fn write_memory(&self, content: &str) -> Result<()> {
        let path = self.memory_dir.join("MEMORY.md");
        std::fs::write(&path, content)
            .with_context(|| format!("Failed to write memory file: {}", path.display()))
    }

    /// Read user-specific memory.
    pub fn read_user_memory(&self, user_id: &str) -> Result<String> {
        let safe_name = user_id.replace(['/', '\\', ' '], "_");
        let path = self.memory_dir.join(format!("USER_{}.md", safe_name));
        if !path.exists() {
            return Ok(String::new());
        }
        std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read user memory: {}", path.display()))
    }

    /// Write user-specific memory.
    pub fn write_user_memory(&self, user_id: &str, content: &str) -> Result<()> {
        let safe_name = user_id.replace(['/', '\\', ' '], "_");
        let path = self.memory_dir.join(format!("USER_{}.md", safe_name));
        std::fs::write(&path, content)
            .with_context(|| format!("Failed to write user memory: {}", path.display()))
    }

    /// Get the memory context for a session (main + user-specific).
    pub fn get_context(&self, user_id: Option<&str>) -> Result<String> {
        let mut parts = Vec::new();

        let main = self.read_memory()?;
        if !main.is_empty() {
            parts.push(format!("## Memory\n\n{}", main));
        }

        if let Some(uid) = user_id {
            let user_mem = self.read_user_memory(uid)?;
            if !user_mem.is_empty() {
                parts.push(format!("## User Memory\n\n{}", user_mem));
            }
        }

        Ok(parts.join("\n\n"))
    }
}

/// Consolidator for archiving old conversation history into memory.
pub struct Consolidator {
    memory_store: MemoryStore,
}

impl Consolidator {
    pub fn new(memory_store: MemoryStore) -> Self {
        Self { memory_store }
    }

    /// Consolidate a session's history into memory.
    pub async fn consolidate(&self, session_key: &str, summary: &str) -> Result<()> {
        info!("Consolidating memory for session: {}", session_key);
        let existing = self.memory_store.read_memory()?;
        let updated = if existing.is_empty() {
            format!("# Memory\n\n{}", summary)
        } else {
            format!("{}\n\n---\n\n{}", existing, summary)
        };
        self.memory_store.write_memory(&updated)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_store_new() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf()).unwrap();
        // Verify the directory was created (already exists from tempdir)
        assert!(dir.path().exists());
        let _ = store; // just verify it constructed
    }

    #[test]
    fn test_memory_store_write_read() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf()).unwrap();
        store.write_memory("Hello memory").unwrap();
        let content = store.read_memory().unwrap();
        assert_eq!(content, "Hello memory");
    }

    #[test]
    fn test_memory_store_read_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf()).unwrap();
        let content = store.read_memory().unwrap();
        assert!(content.is_empty());
    }

    #[test]
    fn test_memory_store_user_memory() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf()).unwrap();
        store.write_user_memory("user123", "User data").unwrap();
        let content = store.read_user_memory("user123").unwrap();
        assert_eq!(content, "User data");
        // Different user should be empty
        let content2 = store.read_user_memory("other").unwrap();
        assert!(content2.is_empty());
    }

    #[test]
    fn test_memory_store_get_context() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf()).unwrap();
        store.write_memory("Main memory content").unwrap();
        store.write_user_memory("alice", "Alice's data").unwrap();

        let ctx = store.get_context(Some("alice")).unwrap();
        assert!(ctx.contains("Main memory content"));
        assert!(ctx.contains("Alice's data"));
        assert!(ctx.contains("## Memory"));
        assert!(ctx.contains("## User Memory"));
    }

    #[tokio::test]
    async fn test_consolidator_consolidate() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().to_path_buf()).unwrap();
        let consolidator = Consolidator::new(store);

        consolidator
            .consolidate("session1", "First summary")
            .await
            .unwrap();
        let content = consolidator.memory_store.read_memory().unwrap();
        assert!(content.contains("First summary"));
        assert!(content.contains("# Memory"));

        // Consolidate again — should append
        consolidator
            .consolidate("session2", "Second summary")
            .await
            .unwrap();
        let content = consolidator.memory_store.read_memory().unwrap();
        assert!(content.contains("First summary"));
        assert!(content.contains("Second summary"));
        assert!(content.contains("---"));
    }
}
