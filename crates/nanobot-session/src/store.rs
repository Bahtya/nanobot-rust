//! Session persistence via JSONL files.

use crate::types::{Session, SessionEntry};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// Persistent session storage using JSONL files.
pub struct SessionStore {
    /// Directory where session files are stored.
    dir: PathBuf,
}

impl SessionStore {
    /// Create a new session store at the given directory.
    pub fn new(dir: PathBuf) -> Result<Self> {
        if !dir.exists() {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("Failed to create session dir: {}", dir.display()))?;
        }
        Ok(Self { dir })
    }

    /// Get the file path for a session key.
    fn session_path(&self, key: &str) -> PathBuf {
        // Sanitize the key for use as a filename.
        let safe_key = key.replace(['/', '\\', ':', ' '], "_");
        self.dir.join(format!("{}.jsonl", safe_key))
    }

    /// Load a session from disk.
    pub fn load(&self, key: &str) -> Result<Option<Session>> {
        let path = self.session_path(key);
        if !path.exists() {
            return Ok(None);
        }

        debug!("Loading session from {}", path.display());
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read session file: {}", path.display()))?;

        let mut session = Session::new(key.to_string());
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionEntry>(line) {
                Ok(entry) => session.messages.push(entry),
                Err(e) => {
                    warn!("Skipping malformed session entry: {}", e);
                }
            }
        }

        if session.messages.is_empty() {
            return Ok(None);
        }

        Ok(Some(session))
    }

    /// Save a session to disk (full overwrite).
    pub fn save(&self, session: &Session) -> Result<()> {
        let path = self.session_path(&session.key);
        debug!("Saving session to {}", path.display());

        let mut lines = Vec::new();
        for entry in &session.messages {
            let line = serde_json::to_string(entry)?;
            lines.push(line);
        }

        let content = lines.join("\n");
        let content = format!("{}\n", content);
        std::fs::write(&path, content)
            .with_context(|| format!("Failed to write session file: {}", path.display()))?;

        Ok(())
    }

    /// Append a single entry to a session file.
    pub fn append_entry(&self, key: &str, entry: &SessionEntry) -> Result<()> {
        let path = self.session_path(key);
        let line = serde_json::to_string(entry)?;
        let line_with_newline = format!("{}\n", line);

        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| {
                format!("Failed to open session file for append: {}", path.display())
            })?;

        file.write_all(line_with_newline.as_bytes())
            .with_context(|| format!("Failed to append to session file: {}", path.display()))?;

        Ok(())
    }

    /// Delete a session file.
    pub fn delete(&self, key: &str) -> Result<()> {
        let path = self.session_path(key);
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("Failed to delete session file: {}", path.display()))?;
        }
        Ok(())
    }

    /// List all session keys.
    pub fn list_keys(&self) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        let entries = std::fs::read_dir(dir(&self.dir))
            .with_context(|| format!("Failed to read session dir: {}", self.dir.display()))?;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                if let Some(stem) = path.file_stem() {
                    keys.push(stem.to_string_lossy().to_string());
                }
            }
        }

        Ok(keys)
    }
}

fn dir(path: &Path) -> &Path {
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanobot_core::MessageRole;

    #[test]
    fn test_session_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(tmp.path().to_path_buf()).unwrap();

        let mut session = Session::new("test:session1".to_string());
        session.add_user_message("Hello".to_string());
        session.add_assistant_message("Hi there!".to_string());

        store.save(&session).unwrap();

        let loaded = store.load("test:session1").unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].content, "Hello");
        assert_eq!(loaded.messages[0].role, MessageRole::User);
    }

    #[test]
    fn test_session_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(tmp.path().to_path_buf()).unwrap();
        assert!(store.load("nonexistent").unwrap().is_none());
    }

    #[test]
    fn test_append_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(tmp.path().to_path_buf()).unwrap();

        let mut session = Session::new("test:append".to_string());
        session.add_user_message("First".to_string());
        store.save(&session).unwrap();

        let entry = SessionEntry {
            role: MessageRole::Assistant,
            content: "Second".to_string(),
            ..Default::default()
        };
        store.append_entry("test:append", &entry).unwrap();

        let loaded = store.load("test:append").unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 2);
    }

    #[test]
    fn test_delete_session() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(tmp.path().to_path_buf()).unwrap();

        let mut session = Session::new("test:delete_me".to_string());
        session.add_user_message("data".to_string());
        store.save(&session).unwrap();

        // Verify it exists
        assert!(store.load("test:delete_me").unwrap().is_some());

        // Delete it
        store.delete("test:delete_me").unwrap();
        assert!(store.load("test:delete_me").unwrap().is_none());
    }

    #[test]
    fn test_delete_nonexistent_session() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(tmp.path().to_path_buf()).unwrap();
        // Deleting a nonexistent session should succeed (idempotent)
        assert!(store.delete("no_such_session").is_ok());
    }

    #[test]
    fn test_list_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(tmp.path().to_path_buf()).unwrap();

        assert!(store.list_keys().unwrap().is_empty());

        let mut s1 = Session::new("platform:chat1".to_string());
        s1.add_user_message("hi".to_string());
        store.save(&s1).unwrap();

        let mut s2 = Session::new("platform:chat2".to_string());
        s2.add_user_message("hello".to_string());
        store.save(&s2).unwrap();

        let mut keys = store.list_keys().unwrap();
        keys.sort();
        assert_eq!(keys, vec!["platform_chat1", "platform_chat2"]);
    }

    #[test]
    fn test_session_key_sanitization() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(tmp.path().to_path_buf()).unwrap();

        let mut session = Session::new("telegram:chat/123:thread_456".to_string());
        session.add_user_message("test".to_string());
        store.save(&session).unwrap();

        let loaded = store.load("telegram:chat/123:thread_456").unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 1);
    }

    #[test]
    fn test_create_store_creates_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let new_dir = tmp.path().join("nested").join("sessions");
        assert!(!new_dir.exists());

        let store = SessionStore::new(new_dir.clone()).unwrap();
        assert!(new_dir.exists());
        assert!(store.list_keys().unwrap().is_empty());
    }

    #[test]
    fn test_overwrite_session() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(tmp.path().to_path_buf()).unwrap();

        let mut session = Session::new("test:overwrite".to_string());
        session.add_user_message("v1".to_string());
        store.save(&session).unwrap();

        let mut session = Session::new("test:overwrite".to_string());
        session.add_user_message("v2".to_string());
        session.add_user_message("v3".to_string());
        store.save(&session).unwrap();

        let loaded = store.load("test:overwrite").unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].content, "v2");
        assert_eq!(loaded.messages[1].content, "v3");
    }
}
