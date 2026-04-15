//! Append-only JSON-lines event store.
//!
//! Persists [`LearningEvent`]s to disk in JSON lines format (one JSON object
//! per line), enabling later analysis and replay.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use nanobot_core::{NanobotError, Result};
use serde::{Deserialize, Serialize};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};

use crate::event::LearningEvent;

/// Metadata header persisted at the top of the event log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventLogHeader {
    /// Format version (for future migration).
    pub version: u32,
    /// When the log was created.
    pub created_at: DateTime<Utc>,
}

impl Default for EventLogHeader {
    fn default() -> Self {
        Self {
            version: 1,
            created_at: Utc::now(),
        }
    }
}

/// Append-only store that writes learning events as JSON lines.
#[derive(Debug)]
pub struct EventStore {
    path: PathBuf,
    max_events: usize,
}

impl EventStore {
    /// Creates a new event store that writes to `path`.
    ///
    /// If the file does not exist, it will be created on first append.
    /// `max_events` controls automatic pruning — when exceeded, the oldest
    /// events are removed.
    pub fn new(path: impl Into<PathBuf>, max_events: usize) -> Self {
        Self {
            path: path.into(),
            max_events,
        }
    }

    /// Returns the file path of the event log.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends a single event to the log.
    pub async fn append(&self, event: &LearningEvent) -> Result<()> {
        let mut line = serde_json::to_string(event).map_err(NanobotError::Serialization)?;
        line.push('\n');

        // Ensure parent directory exists.
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;

        file.write_all(line.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    /// Appends multiple events in a single write.
    pub async fn append_batch(&self, events: &[LearningEvent]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        // Ensure parent directory exists.
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await?;

        for event in events {
            let mut line = serde_json::to_string(event).map_err(NanobotError::Serialization)?;
            line.push('\n');
            file.write_all(line.as_bytes()).await?;
        }
        file.flush().await?;
        Ok(())
    }

    /// Reads all events from the log.
    ///
    /// Malformed lines are silently skipped.
    pub async fn read_all(&self) -> Result<Vec<LearningEvent>> {
        self.read_range(0, usize::MAX).await
    }

    /// Reads events from `offset` up to `limit` events.
    pub async fn read_range(&self, offset: usize, limit: usize) -> Result<Vec<LearningEvent>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }

        let file = File::open(&self.path).await?;
        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        let mut events = Vec::new();
        let mut skipped = 0usize;
        let mut collected = 0usize;

        while let Some(line) = lines.next_line().await? {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if collected >= limit {
                break;
            }
            if let Ok(event) = serde_json::from_str::<LearningEvent>(trimmed) {
                events.push(event);
                collected += 1;
            }
            // Malformed lines are silently skipped.
        }

        Ok(events)
    }

    /// Returns the total number of events in the log.
    pub async fn count(&self) -> Result<usize> {
        if !self.path.exists() {
            return Ok(0);
        }

        let file = File::open(&self.path).await?;
        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        let mut count = 0usize;

        while let Some(line) = lines.next_line().await? {
            if !line.trim().is_empty() {
                count += 1;
            }
        }

        Ok(count)
    }

    /// Prunes the event log to keep at most `max_events` most recent events.
    ///
    /// This rewrites the file, so it should be called periodically, not on
    /// every append.
    pub async fn prune(&self) -> Result<()> {
        if !self.path.exists() {
            return Ok(());
        }

        let events = self.read_all().await?;
        if events.len() <= self.max_events {
            return Ok(());
        }

        // Keep only the most recent events.
        let keep = &events[events.len() - self.max_events..];
        let tmp_path = self.path.with_extension("tmp");

        // Write to temp file.
        let mut file = BufWriter::new(
            OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)
                .await?,
        );

        for event in keep {
            let mut line = serde_json::to_string(event).map_err(NanobotError::Serialization)?;
            line.push('\n');
            file.write_all(line.as_bytes()).await?;
        }
        file.flush().await?;
        drop(file);

        // Atomic rename.
        tokio::fs::rename(&tmp_path, &self.path).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_event(tool: &str, ts: DateTime<Utc>) -> LearningEvent {
        LearningEvent::ToolSucceeded {
            tool: tool.into(),
            args_summary: format!("args for {tool}"),
            duration_ms: 100,
            context_hash: "hash".into(),
            timestamp: ts,
        }
    }

    #[tokio::test]
    async fn append_and_read_all() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("events.jsonl");
        let store = EventStore::new(&path, 1000);

        let e1 = make_event("shell", Utc::now());
        let e2 = make_event("web", Utc::now());
        store.append(&e1).await.expect("append 1");
        store.append(&e2).await.expect("append 2");

        let events = store.read_all().await.expect("read all");
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], LearningEvent::ToolSucceeded { .. }));
    }

    #[tokio::test]
    async fn append_batch() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("batch.jsonl");
        let store = EventStore::new(&path, 1000);

        let events: Vec<_> = (0..5)
            .map(|i| make_event(&format!("tool-{i}"), Utc::now()))
            .collect();
        store.append_batch(&events).await.expect("batch append");

        let read = store.read_all().await.expect("read");
        assert_eq!(read.len(), 5);
    }

    #[tokio::test]
    async fn read_range() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("range.jsonl");
        let store = EventStore::new(&path, 1000);

        let events: Vec<_> = (0..10)
            .map(|i| make_event(&format!("t-{i}"), Utc::now()))
            .collect();
        store.append_batch(&events).await.expect("batch");

        let page = store.read_range(2, 3).await.expect("range");
        assert_eq!(page.len(), 3);
    }

    #[tokio::test]
    async fn count_events() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("count.jsonl");
        let store = EventStore::new(&path, 1000);

        assert_eq!(store.count().await.expect("empty count"), 0);

        for _ in 0..7 {
            store
                .append(&make_event("x", Utc::now()))
                .await
                .expect("append");
        }
        assert_eq!(store.count().await.expect("count"), 7);
    }

    #[tokio::test]
    async fn prune_keeps_most_recent() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("prune.jsonl");
        let store = EventStore::new(&path, 3);

        // Write 5 events.
        let events: Vec<_> = (0..5)
            .map(|i| make_event(&format!("t-{i}"), Utc::now()))
            .collect();
        store.append_batch(&events).await.expect("batch");

        store.prune().await.expect("prune");

        let remaining = store.read_all().await.expect("read");
        assert_eq!(remaining.len(), 3);
        // Should keep the last 3.
        if let LearningEvent::ToolSucceeded { tool, .. } = &remaining[0] {
            assert_eq!(tool, "t-2");
        }
    }

    #[tokio::test]
    async fn creates_parent_dirs() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("nested/dir/events.jsonl");
        let store = EventStore::new(&path, 100);

        store
            .append(&make_event("shell", Utc::now()))
            .await
            .expect("append with nested dirs");

        assert!(path.exists());
    }

    #[tokio::test]
    async fn read_nonexistent_file() {
        let store = EventStore::new("/nonexistent/path/events.jsonl", 100);
        let events = store.read_all().await.expect("should not error");
        assert!(events.is_empty());
    }
}
