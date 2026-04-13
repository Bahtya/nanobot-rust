//! Structured notes — persistent, searchable notes stored in the session.
//!
//! Notes are key-value pairs that the agent can write during a conversation
//! to preserve important information across sessions. They are loaded into
//! the context for subsequent messages, enabling the agent to "remember"
//! key facts without re-reading the entire conversation history.
//!
//! All storage is delegated to the `Session` type's built-in `notes` field
//! and CRUD methods, so notes survive serialization and session reload.

use anyhow::Result;
use nanobot_session::{Note, Session};
use tracing::{debug, info};

/// Manages structured notes within a session.
///
/// Delegates all operations to `Session`'s built-in note methods.
pub struct NotesManager;

impl NotesManager {
    /// Save a note to the session (create or update by title).
    pub fn save_note(
        session: &mut Session,
        title: String,
        content: String,
        tags: Vec<String>,
    ) -> Result<()> {
        session.save_note(title, content, tags);
        debug!("Saved note to session '{}'", session.key);
        Ok(())
    }

    /// Load all notes from a session.
    pub fn load_notes(session: &Session) -> Vec<Note> {
        session.all_notes().to_vec()
    }

    /// Delete a note from the session by title.
    pub fn delete_note(session: &mut Session, title: &str) -> Result<bool> {
        let deleted = session.delete_note(title);
        if deleted {
            info!("Deleted note '{}' from session '{}'", title, session.key);
        }
        Ok(deleted)
    }

    /// Format notes as a context string for inclusion in the system prompt.
    pub fn format_notes_context(session: &Session) -> Option<String> {
        session.format_notes_context()
    }

    /// Get notes filtered by a tag.
    pub fn notes_by_tag(session: &Session, tag: &str) -> Vec<Note> {
        session.notes_by_tag(tag).into_iter().cloned().collect()
    }

    /// Search notes by a free-text query.
    pub fn search_notes(session: &Session, query: &str) -> Vec<Note> {
        session.search_notes(query).into_iter().cloned().collect()
    }

    /// Extract and save notes from an agent response.
    ///
    /// Looks for structured note blocks in the response text of the form:
    /// ```text
    /// [NOTE:title:tag1,tag2]content[/NOTE]
    /// ```
    /// or
    /// ```text
    /// [NOTE:title]content[/NOTE]
    /// ```
    ///
    /// Returns the number of notes extracted.
    pub fn extract_notes_from_response(session: &mut Session, response: &str) -> usize {
        let mut count = 0;
        let re = regex::Regex::new(r"\[NOTE:([^:\]]+)(?::([^\]]+))?\](.*?)\[/NOTE\]")
            .expect("note extraction regex should compile");

        for cap in re.captures_iter(response) {
            let title = cap[1].to_string();
            let tags: Vec<String> = cap
                .get(2)
                .map(|m| {
                    m.as_str()
                        .split(',')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let content = cap[3].trim().to_string();

            if !title.is_empty() && !content.is_empty() {
                session.save_note(title, content, tags);
                count += 1;
            }
        }

        if count > 0 {
            debug!(
                "Extracted {} notes from agent response in session '{}'",
                count, session.key
            );
        }

        count
    }

    /// Run note compaction if the session has too many notes.
    ///
    /// Returns true if compaction was performed.
    pub fn compact_if_needed(session: &mut Session) -> bool {
        session.compact_notes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanobot_session::Session;

    fn make_session() -> Session {
        Session::new("test:notes".to_string())
    }

    #[test]
    fn test_save_and_load_note() {
        let mut session = make_session();
        NotesManager::save_note(
            &mut session,
            "user_preference".to_string(),
            "Prefers concise answers".to_string(),
            vec!["preferences".to_string()],
        )
        .unwrap();

        let notes = NotesManager::load_notes(&session);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].title, "user_preference");
        assert_eq!(notes[0].content, "Prefers concise answers");
        assert_eq!(notes[0].tags, vec!["preferences"]);
    }

    #[test]
    fn test_save_multiple_notes() {
        let mut session = make_session();
        NotesManager::save_note(&mut session, "note1".to_string(), "First note".to_string(), vec![])
            .unwrap();
        NotesManager::save_note(&mut session, "note2".to_string(), "Second note".to_string(), vec![])
            .unwrap();

        let notes = NotesManager::load_notes(&session);
        assert_eq!(notes.len(), 2);
    }

    #[test]
    fn test_update_existing_note() {
        let mut session = make_session();
        NotesManager::save_note(&mut session, "key1".to_string(), "Original".to_string(), vec![])
            .unwrap();
        NotesManager::save_note(&mut session, "key1".to_string(), "Updated".to_string(), vec![])
            .unwrap();

        let notes = NotesManager::load_notes(&session);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content, "Updated");
        assert!(!notes[0].created_at.is_empty());
    }

    #[test]
    fn test_delete_note() {
        let mut session = make_session();
        NotesManager::save_note(
            &mut session,
            "to_delete".to_string(),
            "Will be deleted".to_string(),
            vec![],
        )
        .unwrap();

        let deleted = NotesManager::delete_note(&mut session, "to_delete").unwrap();
        assert!(deleted);

        let notes = NotesManager::load_notes(&session);
        assert!(notes.is_empty());
    }

    #[test]
    fn test_delete_nonexistent_note() {
        let mut session = make_session();
        let deleted = NotesManager::delete_note(&mut session, "nope").unwrap();
        assert!(!deleted);
    }

    #[test]
    fn test_format_notes_context_empty() {
        let session = make_session();
        assert!(NotesManager::format_notes_context(&session).is_none());
    }

    #[test]
    fn test_format_notes_context_with_notes() {
        let mut session = make_session();
        NotesManager::save_note(
            &mut session,
            "lang".to_string(),
            "Rust".to_string(),
            vec!["tech".to_string()],
        )
        .unwrap();
        NotesManager::save_note(&mut session, "style".to_string(), "Concise".to_string(), vec![])
            .unwrap();

        let ctx = NotesManager::format_notes_context(&session).unwrap();
        assert!(ctx.contains("## Session Notes"));
        assert!(ctx.contains("[tech] lang: Rust"));
        assert!(ctx.contains("style: Concise"));
    }

    #[test]
    fn test_notes_by_tag() {
        let mut session = make_session();
        NotesManager::save_note(
            &mut session,
            "n1".to_string(),
            "First".to_string(),
            vec!["cat_a".to_string()],
        )
        .unwrap();
        NotesManager::save_note(
            &mut session,
            "n2".to_string(),
            "Second".to_string(),
            vec!["cat_b".to_string()],
        )
        .unwrap();
        NotesManager::save_note(
            &mut session,
            "n3".to_string(),
            "Third".to_string(),
            vec!["cat_a".to_string()],
        )
        .unwrap();

        let cat_a = NotesManager::notes_by_tag(&session, "cat_a");
        assert_eq!(cat_a.len(), 2);
        let cat_b = NotesManager::notes_by_tag(&session, "cat_b");
        assert_eq!(cat_b.len(), 1);
    }

    #[test]
    fn test_search_notes() {
        let mut session = make_session();
        NotesManager::save_note(
            &mut session,
            "api design".to_string(),
            "Use REST endpoints".to_string(),
            vec!["architecture".to_string()],
        )
        .unwrap();

        let results = NotesManager::search_notes(&session, "rest");
        assert_eq!(results.len(), 1);

        let results = NotesManager::search_notes(&session, "architecture");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_notes_persist_across_session_operations() {
        let mut session = make_session();
        session.add_user_message("hello".to_string());
        NotesManager::save_note(
            &mut session,
            "persistent".to_string(),
            "This survives".to_string(),
            vec![],
        )
        .unwrap();

        session.add_assistant_message("response".to_string());

        let notes = NotesManager::load_notes(&session);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content, "This survives");
    }

    // === Auto-extraction tests ===

    #[test]
    fn test_extract_notes_from_response_simple() {
        let mut session = make_session();
        let response = "Got it! [NOTE:user_lang:preference]Rust[/NOTE] noted.";

        let count = NotesManager::extract_notes_from_response(&mut session, response);
        assert_eq!(count, 1);

        let notes = NotesManager::load_notes(&session);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].title, "user_lang");
        assert_eq!(notes[0].content, "Rust");
        assert_eq!(notes[0].tags, vec!["preference"]);
    }

    #[test]
    fn test_extract_notes_without_tags() {
        let mut session = make_session();
        let response = "[NOTE:reminder]Check the deploy at 5pm[/NOTE]";

        let count = NotesManager::extract_notes_from_response(&mut session, response);
        assert_eq!(count, 1);

        let notes = NotesManager::load_notes(&session);
        assert!(notes[0].tags.is_empty());
    }

    #[test]
    fn test_extract_notes_multiple_tags() {
        let mut session = make_session();
        let response = "[NOTE:decision:backend,urgent]Use PostgreSQL[/NOTE]";

        let count = NotesManager::extract_notes_from_response(&mut session, response);
        assert_eq!(count, 1);

        let notes = NotesManager::load_notes(&session);
        assert_eq!(notes[0].tags, vec!["backend", "urgent"]);
    }

    #[test]
    fn test_extract_multiple_notes() {
        let mut session = make_session();
        let response = "[NOTE:a:cat1]First[/NOTE] and [NOTE:b:cat2]Second[/NOTE]";

        let count = NotesManager::extract_notes_from_response(&mut session, response);
        assert_eq!(count, 2);

        let notes = NotesManager::load_notes(&session);
        assert_eq!(notes.len(), 2);
    }

    #[test]
    fn test_extract_notes_no_match() {
        let mut session = make_session();
        let response = "No notes here, just a regular response.";

        let count = NotesManager::extract_notes_from_response(&mut session, response);
        assert_eq!(count, 0);
        assert!(NotesManager::load_notes(&session).is_empty());
    }

    #[test]
    fn test_compact_if_needed_under_limit() {
        let mut session = make_session();
        for i in 0..10 {
            session.save_note(format!("n{}", i), format!("note {}", i), vec![]);
        }

        assert!(!NotesManager::compact_if_needed(&mut session));
        assert_eq!(session.notes.len(), 10);
    }
}
