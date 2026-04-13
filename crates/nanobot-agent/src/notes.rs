//! Structured notes — persistent, searchable notes stored in the session.
//!
//! Notes are key-value pairs that the agent can write during a conversation
//! to preserve important information across sessions. They are loaded into
//! the context for subsequent messages, enabling the agent to "remember"
//! key facts without re-reading the entire conversation history.
//!
//! ## Note formats
//!
//! [`NoteFormat`] classifies notes into semantic categories so the context
//! builder can load the right subset for each situation:
//!
//! | Format | Purpose |
//! |---|---|
//! | `Summary` | High-level conversation summary |
//! | `ActionItems` | TODO items and follow-ups |
//! | `Decisions` | Important choices made |
//! | `OpenQuestions` | Unresolved issues |
//!
//! All storage is delegated to the `Session` type's built-in `notes` field
//! and CRUD methods, so notes survive serialization and session reload.

use anyhow::Result;
use nanobot_session::{Note, Session};
use tracing::{debug, info};

// ---------------------------------------------------------------------------
// NoteFormat
// ---------------------------------------------------------------------------

/// Semantic category for a structured note.
///
/// Each variant maps to a tag used in the session's note system.
/// During context compaction, the compacted information is split into
/// these categories and stored as individual notes rather than a single
/// blob of text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteFormat {
    /// High-level conversation summary.
    Summary,
    /// TODO items and follow-ups extracted from the conversation.
    ActionItems,
    /// Important decisions or choices that were made.
    Decisions,
    /// Unresolved issues or open questions.
    OpenQuestions,
}

impl NoteFormat {
    /// Returns the tag string used to mark notes of this format.
    pub fn tag(&self) -> &'static str {
        match self {
            NoteFormat::Summary => "summary",
            NoteFormat::ActionItems => "action_items",
            NoteFormat::Decisions => "decisions",
            NoteFormat::OpenQuestions => "open_questions",
        }
    }

    /// Returns a human-readable label for this format.
    pub fn label(&self) -> &'static str {
        match self {
            NoteFormat::Summary => "Summary",
            NoteFormat::ActionItems => "Action Items",
            NoteFormat::Decisions => "Decisions",
            NoteFormat::OpenQuestions => "Open Questions",
        }
    }

    /// Iterate over all variants.
    pub fn all() -> &'static [NoteFormat] {
        &[
            NoteFormat::Summary,
            NoteFormat::ActionItems,
            NoteFormat::Decisions,
            NoteFormat::OpenQuestions,
        ]
    }

    /// Look up a [`NoteFormat`] by its tag string.
    ///
    /// Returns `None` if the tag doesn't match any variant.
    pub fn from_tag(tag: &str) -> Option<NoteFormat> {
        match tag {
            "summary" => Some(NoteFormat::Summary),
            "action_items" => Some(NoteFormat::ActionItems),
            "decisions" => Some(NoteFormat::Decisions),
            "open_questions" => Some(NoteFormat::OpenQuestions),
            _ => None,
        }
    }
}

impl std::fmt::Display for NoteFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.tag())
    }
}

// ---------------------------------------------------------------------------
// NotesManager
// ---------------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // Structured note helpers
    // -----------------------------------------------------------------------

    /// Save a structured note with a [`NoteFormat`] category tag.
    ///
    /// The format tag is prepended to any existing tags.
    pub fn save_structured_note(
        session: &mut Session,
        title: String,
        content: String,
        format: NoteFormat,
        extra_tags: Vec<String>,
    ) -> Result<()> {
        let mut tags = vec![format.tag().to_string()];
        tags.extend(extra_tags);
        session.save_note(title, content, tags);
        debug!(
            "Saved structured note [{}] to session '{}'",
            format.tag(),
            session.key
        );
        Ok(())
    }

    /// Load all notes of a given [`NoteFormat`].
    pub fn notes_by_format(session: &Session, format: NoteFormat) -> Vec<Note> {
        session
            .notes_by_tag(format.tag())
            .into_iter()
            .cloned()
            .collect()
    }

    /// Build a context section from notes of a specific format.
    ///
    /// Returns `None` if there are no notes in this category.
    pub fn format_section(session: &Session, format: NoteFormat) -> Option<String> {
        let notes = Self::notes_by_format(session, format);
        if notes.is_empty() {
            return None;
        }

        let mut lines = vec![format!("### {}", format.label())];
        for note in &notes {
            lines.push(format!("- {}: {}", note.title, note.content));
        }
        Some(lines.join("\n"))
    }

    /// Build the full structured-notes context block for the system prompt.
    ///
    /// Produces sections for each [`NoteFormat`] that has notes, plus any
    /// uncategorized notes. Returns `None` if the session has no notes at all.
    pub fn format_structured_context(session: &Session) -> Option<String> {
        if session.all_notes().is_empty() {
            return None;
        }

        let mut sections = vec!["## Structured Session Notes".to_string()];

        for fmt in NoteFormat::all() {
            if let Some(section) = Self::format_section(session, *fmt) {
                sections.push(section);
            }
        }

        // Include notes that don't belong to any NoteFormat category.
        let format_tags: Vec<&str> = NoteFormat::all().iter().map(|f| f.tag()).collect();
        let uncategorized: Vec<&Note> = session
            .all_notes()
            .iter()
            .filter(|n| !n.tags.iter().any(|t| format_tags.contains(&t.as_str())))
            .collect();

        if !uncategorized.is_empty() {
            sections.push("### Notes".to_string());
            for note in &uncategorized {
                if note.tags.is_empty() {
                    sections.push(format!("- {}: {}", note.title, note.content));
                } else {
                    sections.push(format!(
                        "- [{}] {}: {}",
                        note.tags.join(","),
                        note.title,
                        note.content,
                    ));
                }
            }
        }

        Some(sections.join("\n\n"))
    }

    // -----------------------------------------------------------------------
    // Auto-extraction from responses
    // -----------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Compaction note helpers
// ---------------------------------------------------------------------------

/// Extract structured notes from compacted conversation history.
///
/// Given the messages that are about to be discarded during compaction,
/// this function analyses them and produces categorized notes:
/// - **Summary**: high-level topics discussed
/// - **ActionItems**: sentences that look like tasks or follow-ups
/// - **Decisions**: statements of choice or commitment
/// - **OpenQuestions**: questions that were asked but never answered
///
/// Stale compaction notes from a prior compaction cycle are removed first
/// so they don't accumulate across multiple compactions.
///
/// Returns the number of notes created.
pub fn extract_compaction_notes(session: &mut Session, old_messages: &[nanobot_session::SessionEntry]) -> usize {
    // Remove any stale compaction notes from prior compaction cycles.
    let compaction_titles = [
        "_compaction_summary",
        "_compaction_actions",
        "_compaction_decisions",
        "_compaction_questions",
    ];
    for title in &compaction_titles {
        session.delete_note(title);
    }

    let mut count = 0;

    // 1) Summary — from user messages' first lines
    let topics: Vec<String> = old_messages
        .iter()
        .filter(|m| m.role == nanobot_core::MessageRole::User)
        .filter_map(|m| m.content.lines().next().map(|l| l.to_string()))
        .filter(|l| !l.is_empty())
        .take(5)
        .collect();

    if !topics.is_empty() {
        let summary = format!(
            "Discussed: {}",
            topics.join("; ")
        );
        NotesManager::save_structured_note(
            session,
            "_compaction_summary".to_string(),
            summary,
            NoteFormat::Summary,
            vec!["_system".to_string()],
        )
        .unwrap();
        count += 1;
    }

    // 2) Action items — lines containing "need to", "should", "TODO", "must"
    let action_patterns = ["need to", "should", "todo", "must", "follow up", "action item"];
    let action_items: Vec<String> = old_messages
        .iter()
        .filter(|m| m.role == nanobot_core::MessageRole::Assistant)
        .flat_map(|m| m.content.lines())
        .filter(|line| {
            let lower = line.to_lowercase();
            action_patterns.iter().any(|p| lower.contains(p))
        })
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .take(5)
        .collect();

    if !action_items.is_empty() {
        NotesManager::save_structured_note(
            session,
            "_compaction_actions".to_string(),
            action_items.join("\n"),
            NoteFormat::ActionItems,
            vec!["_system".to_string()],
        )
        .unwrap();
        count += 1;
    }

    // 3) Decisions — lines containing "decided", "chose", "will use", "agreed"
    let decision_patterns = ["decided", "chose", "will use", "agreed", "going with", "let's use"];
    let decisions: Vec<String> = old_messages
        .iter()
        .filter(|m| m.role == nanobot_core::MessageRole::Assistant || m.role == nanobot_core::MessageRole::User)
        .flat_map(|m| m.content.lines())
        .filter(|line| {
            let lower = line.to_lowercase();
            decision_patterns.iter().any(|p| lower.contains(p))
        })
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .take(5)
        .collect();

    if !decisions.is_empty() {
        NotesManager::save_structured_note(
            session,
            "_compaction_decisions".to_string(),
            decisions.join("\n"),
            NoteFormat::Decisions,
            vec!["_system".to_string()],
        )
        .unwrap();
        count += 1;
    }

    // 4) Open questions — user messages ending with "?"
    let questions: Vec<String> = old_messages
        .iter()
        .filter(|m| m.role == nanobot_core::MessageRole::User)
        .flat_map(|m| m.content.lines())
        .filter(|line| line.trim().ends_with('?'))
        .map(|l| l.trim().to_string())
        .take(5)
        .collect();

    if !questions.is_empty() {
        NotesManager::save_structured_note(
            session,
            "_compaction_questions".to_string(),
            questions.join("\n"),
            NoteFormat::OpenQuestions,
            vec!["_system".to_string()],
        )
        .unwrap();
        count += 1;
    }

    if count > 0 {
        info!(
            "Extracted {} structured compaction notes for session '{}'",
            count, session.key
        );
    }

    count
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use nanobot_session::Session;

    fn make_session() -> Session {
        Session::new("test:notes".to_string())
    }

    // -------------------------------------------------------------------
    // NoteFormat
    // -------------------------------------------------------------------

    #[test]
    fn test_note_format_tags() {
        assert_eq!(NoteFormat::Summary.tag(), "summary");
        assert_eq!(NoteFormat::ActionItems.tag(), "action_items");
        assert_eq!(NoteFormat::Decisions.tag(), "decisions");
        assert_eq!(NoteFormat::OpenQuestions.tag(), "open_questions");
    }

    #[test]
    fn test_note_format_labels() {
        assert_eq!(NoteFormat::Summary.label(), "Summary");
        assert_eq!(NoteFormat::ActionItems.label(), "Action Items");
        assert_eq!(NoteFormat::Decisions.label(), "Decisions");
        assert_eq!(NoteFormat::OpenQuestions.label(), "Open Questions");
    }

    #[test]
    fn test_note_format_display() {
        assert_eq!(format!("{}", NoteFormat::Summary), "summary");
        assert_eq!(format!("{}", NoteFormat::ActionItems), "action_items");
    }

    #[test]
    fn test_note_format_all() {
        assert_eq!(NoteFormat::all().len(), 4);
        assert!(NoteFormat::all().contains(&NoteFormat::Summary));
        assert!(NoteFormat::all().contains(&NoteFormat::ActionItems));
        assert!(NoteFormat::all().contains(&NoteFormat::Decisions));
        assert!(NoteFormat::all().contains(&NoteFormat::OpenQuestions));
    }

    #[test]
    fn test_note_format_from_tag() {
        assert_eq!(NoteFormat::from_tag("summary"), Some(NoteFormat::Summary));
        assert_eq!(NoteFormat::from_tag("action_items"), Some(NoteFormat::ActionItems));
        assert_eq!(NoteFormat::from_tag("decisions"), Some(NoteFormat::Decisions));
        assert_eq!(NoteFormat::from_tag("open_questions"), Some(NoteFormat::OpenQuestions));
        assert_eq!(NoteFormat::from_tag("unknown"), None);
        assert_eq!(NoteFormat::from_tag(""), None);
    }

    #[test]
    fn test_note_format_from_tag_roundtrip() {
        for fmt in NoteFormat::all() {
            let tag = fmt.tag();
            assert_eq!(NoteFormat::from_tag(tag), Some(*fmt));
        }
    }

    #[test]
    fn test_note_format_serde() {
        let json = serde_json::to_string(&NoteFormat::Summary).unwrap();
        assert_eq!(json, "\"summary\"");
        let back: NoteFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(back, NoteFormat::Summary);

        let json = serde_json::to_string(&NoteFormat::ActionItems).unwrap();
        assert_eq!(json, "\"action_items\"");
        let back: NoteFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(back, NoteFormat::ActionItems);
    }

    // -------------------------------------------------------------------
    // NotesManager — basic CRUD
    // -------------------------------------------------------------------

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

    // -------------------------------------------------------------------
    // NotesManager — context formatting
    // -------------------------------------------------------------------

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

    // -------------------------------------------------------------------
    // NotesManager — tag-based queries
    // -------------------------------------------------------------------

    #[test]
    fn test_notes_by_tag() {
        let mut session = make_session();
        NotesManager::save_note(&mut session, "n1".to_string(), "First".to_string(), vec!["cat_a".to_string()])
            .unwrap();
        NotesManager::save_note(&mut session, "n2".to_string(), "Second".to_string(), vec!["cat_b".to_string()])
            .unwrap();
        NotesManager::save_note(&mut session, "n3".to_string(), "Third".to_string(), vec!["cat_a".to_string()])
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

    // -------------------------------------------------------------------
    // Structured notes
    // -------------------------------------------------------------------

    #[test]
    fn test_save_structured_note() {
        let mut session = make_session();
        NotesManager::save_structured_note(
            &mut session,
            "session_summary".to_string(),
            "We discussed the API design".to_string(),
            NoteFormat::Summary,
            vec!["important".to_string()],
        )
        .unwrap();

        let notes = NotesManager::notes_by_format(&session, NoteFormat::Summary);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].title, "session_summary");
        assert_eq!(notes[0].content, "We discussed the API design");
        assert!(notes[0].tags.contains(&"summary".to_string()));
        assert!(notes[0].tags.contains(&"important".to_string()));
    }

    #[test]
    fn test_notes_by_format_empty() {
        let session = make_session();
        assert!(NotesManager::notes_by_format(&session, NoteFormat::Summary).is_empty());
    }

    #[test]
    fn test_notes_by_format_isolation() {
        let mut session = make_session();
        NotesManager::save_structured_note(
            &mut session,
            "s1".to_string(),
            "Summary note".to_string(),
            NoteFormat::Summary,
            vec![],
        )
        .unwrap();
        NotesManager::save_structured_note(
            &mut session,
            "a1".to_string(),
            "Action note".to_string(),
            NoteFormat::ActionItems,
            vec![],
        )
        .unwrap();

        assert_eq!(NotesManager::notes_by_format(&session, NoteFormat::Summary).len(), 1);
        assert_eq!(NotesManager::notes_by_format(&session, NoteFormat::ActionItems).len(), 1);
        assert_eq!(NotesManager::notes_by_format(&session, NoteFormat::Decisions).len(), 0);
    }

    #[test]
    fn test_format_section() {
        let mut session = make_session();
        NotesManager::save_structured_note(
            &mut session,
            "task1".to_string(),
            "Deploy to prod".to_string(),
            NoteFormat::ActionItems,
            vec![],
        )
        .unwrap();
        NotesManager::save_structured_note(
            &mut session,
            "task2".to_string(),
            "Write tests".to_string(),
            NoteFormat::ActionItems,
            vec![],
        )
        .unwrap();

        let section = NotesManager::format_section(&session, NoteFormat::ActionItems).unwrap();
        assert!(section.contains("### Action Items"));
        assert!(section.contains("task1: Deploy to prod"));
        assert!(section.contains("task2: Write tests"));

        // Empty category returns None
        assert!(NotesManager::format_section(&session, NoteFormat::Decisions).is_none());
    }

    #[test]
    fn test_format_structured_context() {
        let mut session = make_session();

        // Add notes across different formats
        NotesManager::save_structured_note(
            &mut session,
            "summary1".to_string(),
            "Discussed API design".to_string(),
            NoteFormat::Summary,
            vec![],
        )
        .unwrap();
        NotesManager::save_structured_note(
            &mut session,
            "action1".to_string(),
            "Deploy to staging".to_string(),
            NoteFormat::ActionItems,
            vec![],
        )
        .unwrap();
        NotesManager::save_structured_note(
            &mut session,
            "decision1".to_string(),
            "Use PostgreSQL".to_string(),
            NoteFormat::Decisions,
            vec![],
        )
        .unwrap();
        // An uncategorized note
        NotesManager::save_note(
            &mut session,
            "random".to_string(),
            "Some note".to_string(),
            vec!["misc".to_string()],
        )
        .unwrap();

        let ctx = NotesManager::format_structured_context(&session).unwrap();
        assert!(ctx.contains("## Structured Session Notes"));
        assert!(ctx.contains("### Summary"));
        assert!(ctx.contains("### Action Items"));
        assert!(ctx.contains("### Decisions"));
        assert!(ctx.contains("### Notes")); // uncategorized section
        assert!(!ctx.contains("### Open Questions")); // empty category omitted
    }

    #[test]
    fn test_format_structured_context_empty_session() {
        let session = make_session();
        assert!(NotesManager::format_structured_context(&session).is_none());
    }

    #[test]
    fn test_format_structured_context_all_categories() {
        let mut session = make_session();
        for fmt in NoteFormat::all() {
            NotesManager::save_structured_note(
                &mut session,
                format!("note_{:?}", fmt),
                "content".to_string(),
                *fmt,
                vec![],
            )
            .unwrap();
        }

        let ctx = NotesManager::format_structured_context(&session).unwrap();
        assert!(ctx.contains("### Summary"));
        assert!(ctx.contains("### Action Items"));
        assert!(ctx.contains("### Decisions"));
        assert!(ctx.contains("### Open Questions"));
    }

    // -------------------------------------------------------------------
    // Auto-extraction from responses
    // -------------------------------------------------------------------

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

    // -------------------------------------------------------------------
    // Compaction note extraction
    // -------------------------------------------------------------------

    #[test]
    fn test_extract_compaction_notes_summary() {
        let mut session = make_session();
        let messages = vec![
            nanobot_session::SessionEntry {
                role: nanobot_core::MessageRole::User,
                content: "How do I deploy to Kubernetes?".to_string(),
                ..Default::default()
            },
            nanobot_session::SessionEntry {
                role: nanobot_core::MessageRole::Assistant,
                content: "You need to use kubectl apply.".to_string(),
                ..Default::default()
            },
        ];

        let count = extract_compaction_notes(&mut session, &messages);
        assert!(count >= 1, "Should extract at least summary");

        let summaries = NotesManager::notes_by_format(&session, NoteFormat::Summary);
        assert_eq!(summaries.len(), 1);
        assert!(summaries[0].content.contains("Kubernetes"));
    }

    #[test]
    fn test_extract_compaction_notes_action_items() {
        let mut session = make_session();
        let messages = vec![
            nanobot_session::SessionEntry {
                role: nanobot_core::MessageRole::Assistant,
                content: "You should add tests for the API layer. We need to handle errors.".to_string(),
                ..Default::default()
            },
        ];

        let count = extract_compaction_notes(&mut session, &messages);
        assert!(count >= 1);

        let actions = NotesManager::notes_by_format(&session, NoteFormat::ActionItems);
        assert_eq!(actions.len(), 1);
        assert!(actions[0].content.contains("should add tests"));
    }

    #[test]
    fn test_extract_compaction_notes_decisions() {
        let mut session = make_session();
        let messages = vec![
            nanobot_session::SessionEntry {
                role: nanobot_core::MessageRole::Assistant,
                content: "We decided to use Rust for the backend.".to_string(),
                ..Default::default()
            },
        ];

        let count = extract_compaction_notes(&mut session, &messages);
        assert!(count >= 1);

        let decisions = NotesManager::notes_by_format(&session, NoteFormat::Decisions);
        assert_eq!(decisions.len(), 1);
        assert!(decisions[0].content.contains("decided"));
    }

    #[test]
    fn test_extract_compaction_notes_open_questions() {
        let mut session = make_session();
        let messages = vec![
            nanobot_session::SessionEntry {
                role: nanobot_core::MessageRole::User,
                content: "What database should we use?".to_string(),
                ..Default::default()
            },
            nanobot_session::SessionEntry {
                role: nanobot_core::MessageRole::User,
                content: "How do we handle migrations?".to_string(),
                ..Default::default()
            },
        ];

        let count = extract_compaction_notes(&mut session, &messages);
        assert!(count >= 1);

        let questions = NotesManager::notes_by_format(&session, NoteFormat::OpenQuestions);
        assert_eq!(questions.len(), 1);
        assert!(questions[0].content.contains("database"));
        assert!(questions[0].content.contains("migrations"));
    }

    #[test]
    fn test_extract_compaction_notes_empty_messages() {
        let mut session = make_session();
        let count = extract_compaction_notes(&mut session, &[]);
        assert_eq!(count, 0);
        assert!(NotesManager::load_notes(&session).is_empty());
    }

    #[test]
    fn test_extract_compaction_notes_no_matching_patterns() {
        let mut session = make_session();
        let messages = vec![
            nanobot_session::SessionEntry {
                role: nanobot_core::MessageRole::User,
                content: "Hello there".to_string(),
                ..Default::default()
            },
            nanobot_session::SessionEntry {
                role: nanobot_core::MessageRole::Assistant,
                content: "Hi! How can I help?".to_string(),
                ..Default::default()
            },
        ];

        let count = extract_compaction_notes(&mut session, &messages);
        // Should still get a summary (from user first lines)
        assert!(count >= 1);
        let summaries = NotesManager::notes_by_format(&session, NoteFormat::Summary);
        assert_eq!(summaries.len(), 1);
    }
}
