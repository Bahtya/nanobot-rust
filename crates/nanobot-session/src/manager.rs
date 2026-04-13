//! Session manager — high-level session lifecycle management.
//!
//! Provides session lookup, creation, and persistence with concurrent access
//! via DashMap, matching the Python session/manager.py SessionManager pattern.

use crate::note_store::NoteStore;
use crate::store::SessionStore;
use crate::types::{Note, Session, SessionEntry};
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

    /// Persistent JSONL storage for messages + notes.
    store: Arc<Mutex<SessionStore>>,

    /// Dedicated note file storage.
    note_store: Arc<Mutex<NoteStore>>,

    /// Maximum messages per session before truncation.
    max_history: usize,
}

impl SessionManager {
    /// Create a new SessionManager with the given data directory.
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        let session_dir = data_dir.join("sessions");
        let notes_dir = data_dir.join("notes");
        let store = SessionStore::new(session_dir)?;
        let note_store = NoteStore::new(notes_dir)?;
        Ok(Self {
            sessions: Arc::new(DashMap::new()),
            store: Arc::new(Mutex::new(store)),
            note_store: Arc::new(Mutex::new(note_store)),
            max_history: DEFAULT_SESSION_HISTORY_LIMIT,
        })
    }

    /// Create with a custom max history size.
    pub fn with_max_history(data_dir: PathBuf, max_history: usize) -> Result<Self> {
        let session_dir = data_dir.join("sessions");
        let notes_dir = data_dir.join("notes");
        let store = SessionStore::new(session_dir)?;
        let note_store = NoteStore::new(notes_dir)?;
        Ok(Self {
            sessions: Arc::new(DashMap::new()),
            store: Arc::new(Mutex::new(store)),
            note_store: Arc::new(Mutex::new(note_store)),
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

            // Merge notes from dedicated note store (note store is authoritative)
            if let Ok(notes) = self.note_store.lock().load_notes(key) {
                if !notes.is_empty() {
                    session.notes = notes;
                }
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

        // Persist messages + notes to JSONL
        self.store.lock().save(&session)?;

        // Also persist notes to dedicated note store
        self.note_store.lock().save_notes(&session.key, &session.notes)?;

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
        // Delete the persisted files so we start fresh
        self.store.lock().delete(key)?;
        self.note_store.lock().delete_notes(key)?;
        debug!("Reset session: {}", key);
        Ok(())
    }

    /// Remove a session from cache and storage.
    pub fn remove_session(&self, key: &str) -> Result<()> {
        self.sessions.remove(key);
        self.store.lock().delete(key)?;
        self.note_store.lock().delete_notes(key)?;
        Ok(())
    }

    /// Get all active session keys.
    pub fn active_session_keys(&self) -> Vec<String> {
        self.sessions.iter().map(|r| r.key().clone()).collect()
    }

    /// Persist all dirty sessions to disk.
    pub fn flush_all(&self) -> Result<()> {
        let store = self.store.lock();
        let note_store = self.note_store.lock();
        for entry in self.sessions.iter() {
            store.save(entry.value())?;
            note_store.save_notes(entry.key(), &entry.value().notes)?;
        }
        Ok(())
    }

    /// Get the number of active sessions.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    // ── Note search convenience methods ──────────────────────

    /// Search notes within a specific session.
    pub fn search_notes(&self, session_key: &str, query: &str) -> Vec<Note> {
        if let Some(session) = self.sessions.get(session_key) {
            session.search_notes(query).into_iter().cloned().collect()
        } else {
            Vec::new()
        }
    }

    /// Search notes across all sessions.
    ///
    /// Returns `(session_key, Note)` pairs for every match.
    pub fn search_all_notes(&self, query: &str) -> Vec<(String, Note)> {
        let mut results = Vec::new();

        // Check in-memory sessions first
        for entry in self.sessions.iter() {
            for note in entry.value().search_notes(query) {
                results.push((entry.key().clone(), note.clone()));
            }
        }

        // Also check persisted sessions not currently in memory
        if let Ok(disk_results) = self.note_store.lock().search_notes(query) {
            let in_memory_keys: std::collections::HashSet<String> = self
                .sessions
                .iter()
                .map(|e| e.key().clone())
                .collect();

            for (key, note) in disk_results {
                if !in_memory_keys.contains(&key) {
                    results.push((key, note));
                }
            }
        }

        results
    }

    /// Search notes across all sessions by a specific tag.
    pub fn search_all_notes_by_tag(&self, tag: &str) -> Vec<(String, Note)> {
        let mut results = Vec::new();

        for entry in self.sessions.iter() {
            for note in entry.value().notes_by_tag(tag) {
                results.push((entry.key().clone(), note.clone()));
            }
        }

        if let Ok(disk_results) = self.note_store.lock().search_notes_by_tag(tag) {
            let in_memory_keys: std::collections::HashSet<String> = self
                .sessions
                .iter()
                .map(|e| e.key().clone())
                .collect();

            for (key, note) in disk_results {
                if !in_memory_keys.contains(&key) {
                    results.push((key, note));
                }
            }
        }

        results
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

    #[test]
    fn test_search_notes_in_session() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(tmp.path().to_path_buf()).unwrap();

        let mut session = mgr.get_or_create("test:search", None);
        session.save_note(
            "API Design".to_string(),
            "Use REST".to_string(),
            vec!["architecture".to_string()],
        );
        session.save_note(
            "Database".to_string(),
            "PostgreSQL".to_string(),
            vec!["backend".to_string()],
        );
        mgr.save_session(&session).unwrap();

        let results = mgr.search_notes("test:search", "rest");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "API Design");

        let results = mgr.search_notes("test:search", "postgresql");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_search_all_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(tmp.path().to_path_buf()).unwrap();

        let mut s1 = mgr.get_or_create("session:a", None);
        s1.save_note(
            "Framework".to_string(),
            "Tokio for async".to_string(),
            vec!["rust".to_string()],
        );
        mgr.save_session(&s1).unwrap();

        let mut s2 = mgr.get_or_create("session:b", None);
        s2.save_note(
            "Testing".to_string(),
            "Use tokio::test".to_string(),
            vec!["rust".to_string()],
        );
        mgr.save_session(&s2).unwrap();

        let results = mgr.search_all_notes("tokio");
        assert!(results.len() >= 2);
    }

    #[test]
    fn test_search_all_notes_by_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = SessionManager::new(tmp.path().to_path_buf()).unwrap();

        let mut s1 = mgr.get_or_create("session:x", None);
        s1.save_note(
            "n1".to_string(),
            "decision note".to_string(),
            vec!["decision".to_string()],
        );
        mgr.save_session(&s1).unwrap();

        let mut s2 = mgr.get_or_create("session:y", None);
        s2.save_note(
            "n2".to_string(),
            "another decision".to_string(),
            vec!["decision".to_string()],
        );
        mgr.save_session(&s2).unwrap();

        let results = mgr.search_all_notes_by_tag("decision");
        assert!(results.len() >= 2);
    }
}
