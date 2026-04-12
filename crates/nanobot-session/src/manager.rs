//! Session manager — high-level session lifecycle management.
//!
//! Provides session lookup, creation, and persistence with concurrent access
//! via DashMap, matching the Python session/manager.py SessionManager pattern.

use crate::store::SessionStore;
use crate::types::{Session, SessionEntry};
use anyhow::Result;
use dashmap::DashMap;
use nanobot_core::{SessionSource, DEFAULT_SESSION_HISTORY_LIMIT};
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{debug, info};

/// Manages session lifecycle with in-memory cache and persistent storage.
#[derive(Clone)]
pub struct SessionManager {
    /// In-memory session cache.
    sessions: Arc<DashMap<String, Session>>,

    /// Persistent JSONL storage.
    store: Arc<Mutex<SessionStore>>,

    /// Maximum messages per session before truncation.
    max_history: usize,
}

impl SessionManager {
    /// Create a new SessionManager with the given data directory.
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        let session_dir = data_dir.join("sessions");
        let store = SessionStore::new(session_dir)?;
        Ok(Self {
            sessions: Arc::new(DashMap::new()),
            store: Arc::new(Mutex::new(store)),
            max_history: DEFAULT_SESSION_HISTORY_LIMIT,
        })
    }

    /// Create with a custom max history size.
    pub fn with_max_history(data_dir: PathBuf, max_history: usize) -> Result<Self> {
        let session_dir = data_dir.join("sessions");
        let store = SessionStore::new(session_dir)?;
        Ok(Self {
            sessions: Arc::new(DashMap::new()),
            store: Arc::new(Mutex::new(store)),
            max_history,
        })
    }

    /// Get or create a session for the given key.
    pub fn get_or_create(&self, key: &str, source: Option<SessionSource>) -> Session {
        if let Some(mut session) = self.sessions.get_mut(key) {
            session.metadata.last_active = Some(chrono::Local::now());
            return session.clone();
        }

        // Try loading from disk
        if let Ok(Some(mut session)) = self.store.lock().load(key) {
            session.metadata.last_active = Some(chrono::Local::now());
            if let Some(src) = source {
                session.source = Some(src);
            }
            self.sessions.insert(key.to_string(), session.clone());
            return session;
        }

        // Create new session
        let mut session = Session::new(key.to_string());
        session.metadata.created_at = Some(chrono::Local::now());
        session.metadata.last_active = Some(chrono::Local::now());
        session.source = source;
        self.sessions.insert(key.to_string(), session.clone());
        debug!("Created new session: {}", key);
        session
    }

    /// Update a session in the cache and persist to disk.
    pub fn save_session(&self, session: &Session) -> Result<()> {
        // Truncate if needed
        let mut session = session.clone();
        if session.messages.len() > self.max_history {
            info!(
                "Truncating session {} from {} to {} messages",
                session.key,
                session.messages.len(),
                self.max_history
            );
            session.truncate(self.max_history);
        }

        // Persist to disk
        self.store.lock().save(&session)?;

        // Update cache
        self.sessions.insert(session.key.clone(), session);

        Ok(())
    }

    /// Append a single entry and update the cache.
    pub fn append_entry(&self, key: &str, entry: &SessionEntry) -> Result<()> {
        self.store.lock().append_entry(key, entry)?;

        if let Some(mut session) = self.sessions.get_mut(key) {
            session.messages.push(entry.clone());
        }

        Ok(())
    }

    /// Reset (clear) a session.
    pub fn reset_session(&self, key: &str) -> Result<()> {
        if let Some(mut session) = self.sessions.get_mut(key) {
            session.reset();
        }
        // Delete the persisted file so we start fresh
        self.store.lock().delete(key)?;
        debug!("Reset session: {}", key);
        Ok(())
    }

    /// Remove a session from cache and storage.
    pub fn remove_session(&self, key: &str) -> Result<()> {
        self.sessions.remove(key);
        self.store.lock().delete(key)?;
        Ok(())
    }

    /// Get all active session keys.
    pub fn active_session_keys(&self) -> Vec<String> {
        self.sessions.iter().map(|r| r.key().clone()).collect()
    }

    /// Persist all dirty sessions to disk.
    pub fn flush_all(&self) -> Result<()> {
        let store = self.store.lock();
        for entry in self.sessions.iter() {
            store.save(entry.value())?;
        }
        Ok(())
    }

    /// Get the number of active sessions.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(tmp.path().to_path_buf()).unwrap();

        // Create
        let session = mgr.get_or_create("test:chat1", None);
        assert_eq!(session.key, "test:chat1");

        // Add messages
        let mut session = session;
        session.add_user_message("Hello".to_string());
        session.add_assistant_message("Hi!".to_string());
        mgr.save_session(&session).unwrap();

        // Retrieve
        let loaded = mgr.get_or_create("test:chat1", None);
        assert_eq!(loaded.messages.len(), 2);
    }

    #[test]
    fn test_session_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(tmp.path().to_path_buf()).unwrap();

        let mut session = mgr.get_or_create("test:reset", None);
        session.add_user_message("Hello".to_string());
        mgr.save_session(&session).unwrap();

        mgr.reset_session("test:reset").unwrap();

        let loaded = mgr.get_or_create("test:reset", None);
        assert_eq!(loaded.messages.len(), 0);
    }

    #[test]
    fn test_multiple_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(tmp.path().to_path_buf()).unwrap();

        let mut s1 = mgr.get_or_create("platform:chat1", None);
        let mut s2 = mgr.get_or_create("platform:chat2", None);

        s1.add_user_message("hello from s1".to_string());
        s2.add_user_message("hello from s2".to_string());
        s2.add_assistant_message("response s2".to_string());

        mgr.save_session(&s1).unwrap();
        mgr.save_session(&s2).unwrap();

        let loaded1 = mgr.get_or_create("platform:chat1", None);
        let loaded2 = mgr.get_or_create("platform:chat2", None);

        assert_eq!(loaded1.messages.len(), 1);
        assert_eq!(loaded1.messages[0].content, "hello from s1");
        assert_eq!(loaded2.messages.len(), 2);
        assert_eq!(loaded2.messages[0].content, "hello from s2");
    }

    #[test]
    fn test_session_truncation_on_save() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = SessionManager::with_max_history(tmp.path().to_path_buf(), 3).unwrap();

        let mut session = mgr.get_or_create("test:trunc", None);
        for i in 0..5 {
            session.add_user_message(format!("msg {}", i));
        }

        assert_eq!(session.messages.len(), 5);
        mgr.save_session(&session).unwrap();

        let loaded = mgr.get_or_create("test:trunc", None);
        assert_eq!(loaded.messages.len(), 3);
    }

    #[test]
    fn test_session_count() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(tmp.path().to_path_buf()).unwrap();

        assert_eq!(mgr.session_count(), 0);

        mgr.get_or_create("test:a", None);
        assert_eq!(mgr.session_count(), 1);

        mgr.get_or_create("test:b", None);
        assert_eq!(mgr.session_count(), 2);
    }

    #[test]
    fn test_active_session_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(tmp.path().to_path_buf()).unwrap();

        let keys = mgr.active_session_keys();
        assert!(keys.is_empty());

        mgr.get_or_create("platform:chat1", None);
        mgr.get_or_create("platform:chat2", None);

        let mut keys = mgr.active_session_keys();
        keys.sort();
        assert_eq!(keys, vec!["platform:chat1", "platform:chat2"]);
    }
}
