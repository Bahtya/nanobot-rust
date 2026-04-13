//! Dedicated note store — filesystem-backed persistence with search.
//!
//! Each session's notes are stored as a separate JSON file
//! (`{sanitized_key}.notes.json`). This allows fast note-only reads and
//! writes without needing to load/save the entire session.

use crate::types::Note;
use anyhow::{Context, Result};
use std::path::PathBuf;
use tracing::debug;

/// Filesystem store for notes, one JSON file per session.
pub struct NoteStore {
    /// Directory where note files are stored.
    dir: PathBuf,
}

impl NoteStore {
    /// Create a new `NoteStore`, creating the directory if needed.
    pub fn new(dir: PathBuf) -> Result<Self> {
        if !dir.exists() {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("Failed to create notes dir: {}", dir.display()))?;
        }
        Ok(Self { dir })
    }

    /// Sanitize a session key for use as a filename.
    fn safe_key(key: &str) -> String {
        key.replace(['/', '\\', ':', ' '], "_")
    }

    /// Get the file path for a session's notes.
    fn notes_path(&self, session_key: &str) -> PathBuf {
        self.dir.join(format!("{}.notes.json", Self::safe_key(session_key)))
    }

    /// Load all notes for a session.
    ///
    /// Returns an empty vec if no notes file exists.
    pub fn load_notes(&self, session_key: &str) -> Result<Vec<Note>> {
        let path = self.notes_path(session_key);
        if !path.exists() {
            return Ok(Vec::new());
        }

        debug!("Loading notes from {}", path.display());
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read notes file: {}", path.display()))?;

        let notes: Vec<Note> = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse notes file: {}", path.display()))?;

        Ok(notes)
    }

    /// Save notes for a session (atomic write via temp file + rename).
    pub fn save_notes(&self, session_key: &str, notes: &[Note]) -> Result<()> {
        let path = self.notes_path(session_key);

        if notes.is_empty() {
            // Remove the file if there are no notes
            if path.exists() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("Failed to remove empty notes file: {}", path.display()))?;
            }
            return Ok(());
        }

        debug!("Saving {} notes for session '{}'", notes.len(), session_key);

        let json = serde_json::to_string_pretty(notes)
            .with_context(|| "Failed to serialize notes")?;

        // Atomic write: write to temp file then rename
        let tmp_path = path.with_extension("notes.tmp");
        std::fs::write(&tmp_path, &json)
            .with_context(|| format!("Failed to write temp notes file: {}", tmp_path.display()))?;

        std::fs::rename(&tmp_path, &path)
            .with_context(|| format!("Failed to rename notes file: {}", path.display()))?;

        Ok(())
    }

    /// Delete all notes for a session.
    pub fn delete_notes(&self, session_key: &str) -> Result<()> {
        let path = self.notes_path(session_key);
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("Failed to delete notes file: {}", path.display()))?;
        }
        Ok(())
    }

    /// Search notes across all sessions by a free-text query.
    ///
    /// Matches case-insensitively against title, content, and tags.
    /// Returns `(session_key, Note)` pairs for every match.
    pub fn search_notes(&self, query: &str) -> Result<Vec<(String, Note)>> {
        let mut results = Vec::new();
        let q = query.to_lowercase();

        for session_key in self.list_sessions_with_notes()? {
            if let Ok(notes) = self.load_notes(&session_key) {
                for note in notes {
                    let matches = note.title.to_lowercase().contains(&q)
                        || note.content.to_lowercase().contains(&q)
                        || note.tags.iter().any(|t| t.to_lowercase().contains(&q));
                    if matches {
                        results.push((session_key.clone(), note));
                    }
                }
            }
        }

        Ok(results)
    }

    /// Search notes across all sessions by a specific tag.
    ///
    /// Returns `(session_key, Note)` pairs for notes that have the
    /// given tag (case-insensitive match).
    pub fn search_notes_by_tag(&self, tag: &str) -> Result<Vec<(String, Note)>> {
        let mut results = Vec::new();

        for session_key in self.list_sessions_with_notes()? {
            if let Ok(notes) = self.load_notes(&session_key) {
                for note in notes {
                    if note.tags.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
                        results.push((session_key.clone(), note));
                    }
                }
            }
        }

        Ok(results)
    }

    /// List all session keys that have at least one note file.
    pub fn list_sessions_with_notes(&self) -> Result<Vec<String>> {
        let mut keys = Vec::new();

        let entries = std::fs::read_dir(&self.dir)
            .with_context(|| format!("Failed to read notes dir: {}", self.dir.display()))?;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                if let Some(filename) = path.file_name() {
                    let name = filename.to_string_lossy();
                    if name.ends_with(".notes.json") {
                        // Strip ".notes.json" suffix and convert back from safe key
                        let key = name
                            .trim_end_matches(".notes.json")
                            .to_string();
                        keys.push(key);
                    }
                }
            }
        }

        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_note_store_new_creates_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let notes_dir = tmp.path().join("notes");
        assert!(!notes_dir.exists());

        let store = NoteStore::new(notes_dir.clone()).unwrap();
        assert!(notes_dir.exists());
        assert!(store.list_sessions_with_notes().unwrap().is_empty());
    }

    #[test]
    fn test_save_and_load_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf()).unwrap();

        let notes = vec![
            Note::new(
                "Architecture".to_string(),
                "Use microservices".to_string(),
                vec!["design".to_string()],
            ),
            Note::new(
                "Language".to_string(),
                "Rust".to_string(),
                vec!["tech".to_string(), "backend".to_string()],
            ),
        ];

        store.save_notes("session:abc", &notes).unwrap();

        let loaded = store.load_notes("session:abc").unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].title, "Architecture");
        assert_eq!(loaded[1].tags, vec!["tech", "backend"]);
    }

    #[test]
    fn test_load_notes_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf()).unwrap();

        let notes = store.load_notes("nonexistent").unwrap();
        assert!(notes.is_empty());
    }

    #[test]
    fn test_save_empty_removes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf()).unwrap();

        // Save some notes first
        let notes = vec![Note::new("t".to_string(), "c".to_string(), vec![])];
        store.save_notes("session:x", &notes).unwrap();
        assert!(store.load_notes("session:x").unwrap().len() == 1);

        // Save empty — should remove the file
        store.save_notes("session:x", &[]).unwrap();
        assert!(store.load_notes("session:x").unwrap().is_empty());
    }

    #[test]
    fn test_delete_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf()).unwrap();

        let notes = vec![Note::new("t".to_string(), "c".to_string(), vec![])];
        store.save_notes("session:del", &notes).unwrap();
        assert_eq!(store.list_sessions_with_notes().unwrap().len(), 1);

        store.delete_notes("session:del").unwrap();
        assert!(store.load_notes("session:del").unwrap().is_empty());
        assert!(store.list_sessions_with_notes().unwrap().is_empty());
    }

    #[test]
    fn test_delete_notes_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf()).unwrap();
        // Should succeed idempotently
        assert!(store.delete_notes("no_such_session").is_ok());
    }

    #[test]
    fn test_search_notes_cross_session() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf()).unwrap();

        store
            .save_notes(
                "session:a",
                &vec![Note::new(
                    "API Design".to_string(),
                    "Use REST endpoints".to_string(),
                    vec!["architecture".to_string()],
                )],
            )
            .unwrap();
        store
            .save_notes(
                "session:b",
                &vec![Note::new(
                    "Database".to_string(),
                    "Use PostgreSQL".to_string(),
                    vec!["backend".to_string()],
                )],
            )
            .unwrap();
        store
            .save_notes(
                "session:c",
                &vec![Note::new(
                    "Frontend".to_string(),
                    "Use React".to_string(),
                    vec!["frontend".to_string()],
                )],
            )
            .unwrap();

        let results = store.search_notes("rest").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "session:a");

        let results = store.search_notes("postgresql").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "session:b");
    }

    #[test]
    fn test_search_notes_by_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf()).unwrap();

        store
            .save_notes(
                "session:a",
                &vec![
                    Note::new("n1".to_string(), "a".to_string(), vec!["decision".to_string()]),
                    Note::new("n2".to_string(), "b".to_string(), vec!["todo".to_string()]),
                ],
            )
            .unwrap();
        store
            .save_notes(
                "session:b",
                &vec![Note::new(
                    "n3".to_string(),
                    "c".to_string(),
                    vec!["decision".to_string()],
                )],
            )
            .unwrap();

        let decisions = store.search_notes_by_tag("decision").unwrap();
        assert_eq!(decisions.len(), 2);

        let todos = store.search_notes_by_tag("todo").unwrap();
        assert_eq!(todos.len(), 1);
    }

    #[test]
    fn test_search_notes_by_tag_case_insensitive() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf()).unwrap();

        store
            .save_notes(
                "session:x",
                &vec![Note::new("n1".to_string(), "a".to_string(), vec!["Backend".to_string()])],
            )
            .unwrap();

        assert_eq!(store.search_notes_by_tag("backend").unwrap().len(), 1);
        assert_eq!(store.search_notes_by_tag("BACKEND").unwrap().len(), 1);
    }

    #[test]
    fn test_list_sessions_with_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf()).unwrap();

        assert!(store.list_sessions_with_notes().unwrap().is_empty());

        let notes = vec![Note::new("t".to_string(), "c".to_string(), vec![])];
        store.save_notes("platform:chat1", &notes).unwrap();
        store.save_notes("platform:chat2", &notes).unwrap();

        let mut keys = store.list_sessions_with_notes().unwrap();
        keys.sort();
        assert_eq!(keys, vec!["platform_chat1", "platform_chat2"]);
    }

    #[test]
    fn test_safe_key_sanitization() {
        assert_eq!(NoteStore::safe_key("telegram:chat/123"), "telegram_chat_123");
        assert_eq!(NoteStore::safe_key("discord:guild\\x"), "discord_guild_x");
    }

    #[test]
    fn test_note_persistence_is_independent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NoteStore::new(tmp.path().to_path_buf()).unwrap();

        // Save notes for session A
        store
            .save_notes(
                "session:a",
                &vec![Note::new(
                    "Note A".to_string(),
                    "Content A".to_string(),
                    vec!["alpha".to_string()],
                )],
            )
            .unwrap();

        // Save notes for session B
        store
            .save_notes(
                "session:b",
                &vec![Note::new(
                    "Note B".to_string(),
                    "Content B".to_string(),
                    vec!["beta".to_string()],
                )],
            )
            .unwrap();

        // Modify session A's notes
        store
            .save_notes(
                "session:a",
                &vec![Note::new(
                    "Note A Modified".to_string(),
                    "New content".to_string(),
                    vec!["alpha".to_string(), "updated".to_string()],
                )],
            )
            .unwrap();

        // Session B should be unaffected
        let b_notes = store.load_notes("session:b").unwrap();
        assert_eq!(b_notes.len(), 1);
        assert_eq!(b_notes[0].title, "Note B");
    }
}
