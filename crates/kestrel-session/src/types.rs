//! Session data types.

use kestrel_core::{Message, MessageRole, SessionSource};
use serde::{Deserialize, Serialize};

/// Maximum notes before compaction triggers.
pub const MAX_NOTES_BEFORE_COMPACTION: usize = 50;

/// A structured note attached to a session.
///
/// Notes provide persistent, searchable memory that survives across
/// conversation turns and process restarts. Each note belongs to a
/// single session (identified by `session_key`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Note {
    /// Unique identifier within the session.
    pub id: String,
    /// Short title for the note.
    pub title: String,
    /// The note content.
    pub content: String,
    /// Searchable tags for categorisation and filtering.
    #[serde(default)]
    pub tags: Vec<String>,
    /// When this note was created.
    pub created_at: String,
    /// When this note was last updated.
    pub updated_at: String,
}

impl Note {
    /// Create a new note with auto-generated ID and current timestamps.
    pub fn new(title: String, content: String, tags: Vec<String>) -> Self {
        let now = chrono::Local::now().to_rfc3339();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            title,
            content,
            tags,
            created_at: now.clone(),
            updated_at: now,
        }
    }
}

// ─── Backward-compatible alias ─────────────────────────────────
// `SessionNote` is re-exported as an alias so that downstream crates
// referencing the old name still compile. It maps to the new `Note`.

/// Backward-compatible alias for [`Note``.
#[deprecated(since = "0.2.0", note = "Use `Note` instead")]
pub type SessionNote = Note;

/// A conversation session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Unique session key (`platform:chat_id[:thread_id]`).
    pub key: String,

    /// Conversation message history.
    pub messages: Vec<SessionEntry>,

    /// Structured notes attached to this session.
    #[serde(default)]
    pub notes: Vec<Note>,

    /// Session metadata.
    #[serde(default)]
    pub metadata: SessionMetadata,

    /// Session source information.
    #[serde(default)]
    pub source: Option<SessionSource>,
}

impl Session {
    /// Create a new empty session with the given key.
    pub fn new(key: String) -> Self {
        Self {
            key,
            messages: Vec::new(),
            notes: Vec::new(),
            metadata: SessionMetadata::default(),
            source: None,
        }
    }

    /// Add a user message to the session.
    pub fn add_user_message(&mut self, content: String) {
        self.messages.push(SessionEntry {
            role: MessageRole::User,
            content,
            timestamp: Some(chrono::Local::now()),
            ..Default::default()
        });
    }

    /// Add an assistant message to the session.
    pub fn add_assistant_message(&mut self, content: String) {
        self.messages.push(SessionEntry {
            role: MessageRole::Assistant,
            content,
            timestamp: Some(chrono::Local::now()),
            ..Default::default()
        });
    }

    /// Add a system message to the session.
    pub fn add_system_message(&mut self, content: String) {
        self.messages.push(SessionEntry {
            role: MessageRole::System,
            content,
            timestamp: Some(chrono::Local::now()),
            ..Default::default()
        });
    }

    /// Add a tool result message to the session.
    pub fn add_tool_result(&mut self, tool_call_id: String, content: String) {
        self.messages.push(SessionEntry {
            role: MessageRole::Tool,
            content,
            tool_call_id: Some(tool_call_id),
            timestamp: Some(chrono::Local::now()),
            ..Default::default()
        });
    }

    /// Convert session entries to LLM-ready Messages.
    pub fn to_messages(&self) -> Vec<Message> {
        self.messages
            .iter()
            .map(|entry| Message {
                role: entry.role.clone(),
                content: entry.content.clone(),
                name: entry.name.clone(),
                tool_call_id: entry.tool_call_id.clone(),
                tool_calls: entry.tool_calls.clone(),
            })
            .collect()
    }

    /// Truncate history to keep only the last `max_messages` entries.
    /// Always keeps the first system message if present.
    pub fn truncate(&mut self, max_messages: usize) {
        if self.messages.len() <= max_messages {
            return;
        }

        // Preserve the first system message if it exists
        let system_msg = self
            .messages
            .first()
            .filter(|m| m.role == MessageRole::System)
            .cloned();

        // Keep the last N messages
        let mut truncated: Vec<SessionEntry> = self
            .messages
            .split_off(self.messages.len().saturating_sub(max_messages));

        // Re-prepend system message
        if let Some(sys) = system_msg {
            truncated.insert(0, sys);
        }

        self.messages = truncated;
        self.metadata.truncated = true;
    }

    /// Get the total token count estimate for the session.
    pub fn estimated_tokens(&self) -> usize {
        self.messages
            .iter()
            .map(|m| m.content.len() / 4) // rough estimate: 4 chars per token
            .sum()
    }

    /// Reset the session, clearing all messages.
    pub fn reset(&mut self) {
        self.messages.clear();
        self.metadata.truncated = false;
        self.metadata.turn_count = 0;
    }

    // ── Structured Notes CRUD ──────────────────────────────────

    /// Save a note (create or update by title).
    ///
    /// If a note with the same `title` already exists, its `content` and
    /// `tags` are updated and a new `id` is preserved. Otherwise a new note
    /// is created with a fresh UUID.
    pub fn save_note(&mut self, title: String, content: String, tags: Vec<String>) {
        let now = chrono::Local::now().to_rfc3339();
        if let Some(existing) = self.notes.iter_mut().find(|n| n.title == title) {
            existing.content = content;
            existing.tags = tags;
            existing.updated_at = now;
        } else {
            self.notes.push(Note::new(title, content, tags));
        }
    }

    /// Get a note by title.
    pub fn get_note(&self, title: &str) -> Option<&Note> {
        self.notes.iter().find(|n| n.title == title)
    }

    /// Get a note by its unique ID.
    pub fn get_note_by_id(&self, id: &str) -> Option<&Note> {
        self.notes.iter().find(|n| n.id == id)
    }

    /// Delete a note by title. Returns true if a note was removed.
    pub fn delete_note(&mut self, title: &str) -> bool {
        let before = self.notes.len();
        self.notes.retain(|n| n.title != title);
        self.notes.len() < before
    }

    /// Return all notes.
    pub fn all_notes(&self) -> &[Note] {
        &self.notes
    }

    /// Get notes filtered by a single tag.
    pub fn notes_by_tag(&self, tag: &str) -> Vec<&Note> {
        self.notes
            .iter()
            .filter(|n| n.tags.iter().any(|t| t.eq_ignore_ascii_case(tag)))
            .collect()
    }

    /// Search notes by a free-text query.
    ///
    /// Matches case-insensitively against title, content, and tags.
    /// Returns all notes that contain the query in any field.
    pub fn search_notes(&self, query: &str) -> Vec<&Note> {
        let q = query.to_lowercase();
        self.notes
            .iter()
            .filter(|n| {
                n.title.to_lowercase().contains(&q)
                    || n.content.to_lowercase().contains(&q)
                    || n.tags.iter().any(|t| t.to_lowercase().contains(&q))
            })
            .collect()
    }

    /// Compact notes when they exceed the limit.
    ///
    /// Keeps the most recent notes and merges older ones into a summary
    /// note with title `_compacted`.
    pub fn compact_notes(&mut self) -> bool {
        if self.notes.len() <= MAX_NOTES_BEFORE_COMPACTION {
            return false;
        }

        let keep = MAX_NOTES_BEFORE_COMPACTION / 2;
        let older: Vec<Note> = self
            .notes
            .drain(..self.notes.len().saturating_sub(keep))
            .collect();
        if older.is_empty() {
            return false;
        }

        let mut summary_parts = Vec::new();
        let mut tag_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for note in older.iter() {
            for tag in &note.tags {
                *tag_counts.entry(tag.clone()).or_insert(0) += 1;
            }
            summary_parts.push(format!("- {}: {}", note.title, note.content));
        }

        let mut tag_summary: Vec<String> = tag_counts
            .iter()
            .map(|(tag, count)| format!("{} ({})", tag, count))
            .collect();
        tag_summary.sort();

        let now = chrono::Local::now().to_rfc3339();
        let compacted = Note {
            id: uuid::Uuid::new_v4().to_string(),
            title: "_compacted".to_string(),
            content: format!(
                "Compacted {} older notes ({}). Key points:\n{}",
                older.len(),
                tag_summary.join(", "),
                summary_parts.join("\n"),
            ),
            tags: vec!["_system".to_string()],
            created_at: now.clone(),
            updated_at: now,
        };
        self.notes.insert(0, compacted);
        true
    }

    /// Format notes as context for the system prompt.
    pub fn format_notes_context(&self) -> Option<String> {
        if self.notes.is_empty() {
            return None;
        }
        let mut parts = vec!["## Session Notes".to_string()];
        for note in &self.notes {
            if note.tags.is_empty() {
                parts.push(format!("- {}: {}", note.title, note.content));
            } else {
                parts.push(format!(
                    "- [{}] {}: {}",
                    note.tags.join(","),
                    note.title,
                    note.content,
                ));
            }
        }
        Some(parts.join("\n"))
    }
}

/// A single entry in the session history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    /// Role of the message author (user, assistant, system, tool).
    pub role: MessageRole,
    /// Text content of the message.
    pub content: String,
    /// Optional sender name for function/tool message routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// ID linking a tool result back to its originating tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Tool calls requested by the assistant in this message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<kestrel_core::ToolCall>>,
    /// When this entry was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<chrono::DateTime<chrono::Local>>,
}

impl Default for SessionEntry {
    fn default() -> Self {
        Self {
            role: MessageRole::User,
            content: String::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            timestamp: Some(chrono::Local::now()),
        }
    }
}

/// Session metadata.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionMetadata {
    /// Total number of conversation turns.
    #[serde(default)]
    pub turn_count: usize,

    /// Whether the history has been truncated.
    #[serde(default)]
    pub truncated: bool,

    /// Creation timestamp.
    #[serde(default)]
    pub created_at: Option<chrono::DateTime<chrono::Local>>,

    /// Last activity timestamp.
    #[serde(default)]
    pub last_active: Option<chrono::DateTime<chrono::Local>>,
}

/// Wrapper for the JSONL header line that stores session metadata + notes.
///
/// The first line of a session JSONL file (if present) is a meta record.
/// Older files that only contain `SessionEntry` lines are loaded without
/// notes or metadata (backward compatible).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SessionMeta {
    /// Discriminant so we can distinguish this line from a SessionEntry.
    #[serde(rename = "type")]
    pub type_: String,
    /// Notes attached to this session.
    #[serde(default)]
    pub notes: Vec<Note>,
    /// Session metadata.
    #[serde(default)]
    pub metadata: SessionMetadata,
    /// Session source information.
    #[serde(default)]
    pub source: Option<SessionSource>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kestrel_core::MessageRole;

    #[test]
    fn test_session_new() {
        let session = Session::new("test:key".to_string());
        assert_eq!(session.key, "test:key");
        assert!(session.messages.is_empty());
        assert!(session.source.is_none());
    }

    #[test]
    fn test_session_add_messages() {
        let mut session = Session::new("test:key".to_string());
        session.add_system_message("system prompt".to_string());
        session.add_user_message("hello".to_string());
        session.add_assistant_message("hi".to_string());
        session.add_tool_result("call_1".to_string(), "result data".to_string());

        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.messages[0].role, MessageRole::System);
        assert_eq!(session.messages[1].role, MessageRole::User);
        assert_eq!(session.messages[2].role, MessageRole::Assistant);
        assert_eq!(session.messages[3].role, MessageRole::Tool);
        assert_eq!(session.messages[3].tool_call_id, Some("call_1".to_string()));
    }

    #[test]
    fn test_session_to_messages() {
        let mut session = Session::new("test:key".to_string());
        session.add_system_message("system".to_string());
        session.add_user_message("hello".to_string());
        session.add_assistant_message("world".to_string());

        let messages: Vec<Message> = session.to_messages();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, MessageRole::System);
        assert_eq!(messages[0].content, "system");
        assert_eq!(messages[1].role, MessageRole::User);
        assert_eq!(messages[1].content, "hello");
        assert_eq!(messages[2].role, MessageRole::Assistant);
        assert_eq!(messages[2].content, "world");
    }

    #[test]
    fn test_session_truncate_preserves_system() {
        let mut session = Session::new("test:key".to_string());
        session.add_system_message("system prompt".to_string());
        for i in 0..10 {
            session.add_user_message(format!("message {}", i));
        }

        assert_eq!(session.messages.len(), 11);
        session.truncate(5);
        // System message + last 5 user messages = 6
        assert_eq!(session.messages.len(), 6);
        assert_eq!(session.messages[0].role, MessageRole::System);
        assert_eq!(session.messages[0].content, "system prompt");
        assert_eq!(session.messages[1].content, "message 5");
        assert_eq!(session.messages[5].content, "message 9");
        assert!(session.metadata.truncated);
    }

    #[test]
    fn test_session_truncate_noop_when_small() {
        let mut session = Session::new("test:key".to_string());
        session.add_user_message("a".to_string());
        session.add_user_message("b".to_string());
        session.add_user_message("c".to_string());

        session.truncate(10);
        assert_eq!(session.messages.len(), 3);
        assert!(!session.metadata.truncated);
    }

    #[test]
    fn test_session_estimated_tokens() {
        let mut session = Session::new("test:key".to_string());
        session.add_user_message("abcd1234".to_string());
        session.add_assistant_message("hello world!".to_string());

        let tokens = session.estimated_tokens();
        assert_eq!(tokens, (8 / 4) + (12 / 4));
        assert_eq!(tokens, 5);
    }

    #[test]
    fn test_session_reset() {
        let mut session = Session::new("test:key".to_string());
        session.add_user_message("hello".to_string());
        session.add_assistant_message("world".to_string());
        session.metadata.truncated = true;
        session.metadata.turn_count = 5;

        session.reset();
        assert!(session.messages.is_empty());
        assert!(!session.metadata.truncated);
        assert_eq!(session.metadata.turn_count, 0);
    }

    #[test]
    fn test_session_entry_default() {
        let entry = SessionEntry::default();
        assert_eq!(entry.role, MessageRole::User);
        assert!(entry.content.is_empty());
        assert!(entry.name.is_none());
        assert!(entry.tool_call_id.is_none());
        assert!(entry.tool_calls.is_none());
        assert!(entry.timestamp.is_some());
    }

    // === Note struct ===

    #[test]
    fn test_note_new_has_id_and_timestamps() {
        let note = Note::new(
            "My Title".to_string(),
            "Some content".to_string(),
            vec!["tag1".to_string()],
        );
        assert!(!note.id.is_empty());
        assert_eq!(note.title, "My Title");
        assert_eq!(note.content, "Some content");
        assert_eq!(note.tags, vec!["tag1"]);
        assert!(!note.created_at.is_empty());
        assert!(!note.updated_at.is_empty());
    }

    #[test]
    fn test_note_serde_roundtrip() {
        let note = Note {
            id: "abc-123".to_string(),
            title: "test".to_string(),
            content: "content".to_string(),
            tags: vec!["rust".to_string(), "ai".to_string()],
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let json = serde_json::to_string(&note).unwrap();
        let back: Note = serde_json::from_str(&json).unwrap();
        assert_eq!(note, back);
    }

    // === Notes CRUD ===

    #[test]
    fn test_session_new_has_empty_notes() {
        let session = Session::new("test:notes".to_string());
        assert!(session.notes.is_empty());
    }

    #[test]
    fn test_save_and_get_note() {
        let mut session = Session::new("test:notes".to_string());
        session.save_note(
            "lang".to_string(),
            "Rust".to_string(),
            vec!["tech".to_string()],
        );

        let note = session.get_note("lang").unwrap();
        assert_eq!(note.title, "lang");
        assert_eq!(note.content, "Rust");
        assert_eq!(note.tags, vec!["tech"]);
        assert!(!note.id.is_empty());
        assert!(!note.created_at.is_empty());
    }

    #[test]
    fn test_save_updates_existing() {
        let mut session = Session::new("test:notes".to_string());
        session.save_note("key1".to_string(), "v1".to_string(), vec![]);
        session.save_note(
            "key1".to_string(),
            "v2".to_string(),
            vec!["cat".to_string()],
        );

        assert_eq!(session.notes.len(), 1);
        let note = session.get_note("key1").unwrap();
        assert_eq!(note.content, "v2");
        assert_eq!(note.tags, vec!["cat"]);
    }

    #[test]
    fn test_get_note_missing() {
        let session = Session::new("test:notes".to_string());
        assert!(session.get_note("nope").is_none());
    }

    #[test]
    fn test_get_note_by_id() {
        let mut session = Session::new("test:notes".to_string());
        session.save_note("title".to_string(), "content".to_string(), vec![]);

        let id = session.notes[0].id.clone();
        let found = session.get_note_by_id(&id).unwrap();
        assert_eq!(found.title, "title");
    }

    #[test]
    fn test_delete_note() {
        let mut session = Session::new("test:notes".to_string());
        session.save_note("a".to_string(), "note a".to_string(), vec![]);
        session.save_note("b".to_string(), "note b".to_string(), vec![]);

        assert!(session.delete_note("a"));
        assert_eq!(session.notes.len(), 1);
        assert!(session.get_note("a").is_none());
    }

    #[test]
    fn test_delete_note_missing() {
        let mut session = Session::new("test:notes".to_string());
        assert!(!session.delete_note("nope"));
    }

    #[test]
    fn test_all_notes() {
        let mut session = Session::new("test:notes".to_string());
        session.save_note("a".to_string(), "note a".to_string(), vec![]);
        session.save_note("b".to_string(), "note b".to_string(), vec![]);

        assert_eq!(session.all_notes().len(), 2);
    }

    #[test]
    fn test_notes_by_tag() {
        let mut session = Session::new("test:notes".to_string());
        session.save_note(
            "n1".to_string(),
            "a".to_string(),
            vec!["decision".to_string()],
        );
        session.save_note(
            "n2".to_string(),
            "b".to_string(),
            vec!["preference".to_string()],
        );
        session.save_note(
            "n3".to_string(),
            "c".to_string(),
            vec!["decision".to_string(), "important".to_string()],
        );

        let decisions = session.notes_by_tag("decision");
        assert_eq!(decisions.len(), 2);
        let prefs = session.notes_by_tag("preference");
        assert_eq!(prefs.len(), 1);
        let important = session.notes_by_tag("important");
        assert_eq!(important.len(), 1);
    }

    #[test]
    fn test_notes_by_tag_case_insensitive() {
        let mut session = Session::new("test:notes".to_string());
        session.save_note("n1".to_string(), "a".to_string(), vec!["Rust".to_string()]);

        assert_eq!(session.notes_by_tag("rust").len(), 1);
        assert_eq!(session.notes_by_tag("RUST").len(), 1);
    }

    #[test]
    fn test_search_notes_by_title() {
        let mut session = Session::new("test:notes".to_string());
        session.save_note("api design".to_string(), "Use REST".to_string(), vec![]);
        session.save_note("database".to_string(), "Use SQLite".to_string(), vec![]);

        let results = session.search_notes("api");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "api design");
    }

    #[test]
    fn test_search_notes_by_content() {
        let mut session = Session::new("test:notes".to_string());
        session.save_note(
            "n1".to_string(),
            "Use PostgreSQL for production".to_string(),
            vec![],
        );

        let results = session.search_notes("postgresql");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_search_notes_by_tag() {
        let mut session = Session::new("test:notes".to_string());
        session.save_note(
            "n1".to_string(),
            "something".to_string(),
            vec!["backend".to_string()],
        );

        let results = session.search_notes("backend");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_search_notes_case_insensitive() {
        let mut session = Session::new("test:notes".to_string());
        session.save_note(
            "Architecture".to_string(),
            "Microservices pattern".to_string(),
            vec![],
        );

        assert_eq!(session.search_notes("architecture").len(), 1);
        assert_eq!(session.search_notes("MICROSERVICES").len(), 1);
    }

    #[test]
    fn test_search_notes_no_match() {
        let mut session = Session::new("test:notes".to_string());
        session.save_note("n1".to_string(), "content".to_string(), vec![]);
        assert!(session.search_notes("zzz_nonexistent").is_empty());
    }

    #[test]
    fn test_format_notes_context_empty() {
        let session = Session::new("test:notes".to_string());
        assert!(session.format_notes_context().is_none());
    }

    #[test]
    fn test_format_notes_context() {
        let mut session = Session::new("test:notes".to_string());
        session.save_note(
            "lang".to_string(),
            "Rust".to_string(),
            vec!["tech".to_string()],
        );
        session.save_note("style".to_string(), "Concise".to_string(), vec![]);

        let ctx = session.format_notes_context().unwrap();
        assert!(ctx.contains("## Session Notes"));
        assert!(ctx.contains("[tech] lang: Rust"));
        assert!(ctx.contains("style: Concise"));
    }

    #[test]
    fn test_compact_notes_noop_when_under_limit() {
        let mut session = Session::new("test:notes".to_string());
        for i in 0..10 {
            session.save_note(format!("n{}", i), format!("note {}", i), vec![]);
        }
        assert!(!session.compact_notes());
        assert_eq!(session.notes.len(), 10);
    }

    #[test]
    fn test_compact_notes_triggers_when_over_limit() {
        let mut session = Session::new("test:notes".to_string());
        for i in 0..(MAX_NOTES_BEFORE_COMPACTION + 10) {
            session.save_note(
                format!("n{}", i),
                format!("note {}", i),
                vec!["general".to_string()],
            );
        }
        assert_eq!(session.notes.len(), MAX_NOTES_BEFORE_COMPACTION + 10);

        let compacted = session.compact_notes();
        assert!(compacted);
        assert!(session.notes.len() < MAX_NOTES_BEFORE_COMPACTION + 10);
        assert!(session.get_note("_compacted").is_some());
    }

    #[test]
    fn test_note_survives_session_save_reload() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = crate::manager::SessionManager::new(dir.path().to_path_buf()).unwrap();

        {
            let mut session = mgr.get_or_create("test:persist", None);
            session.save_note(
                "persist".to_string(),
                "survives reload".to_string(),
                vec!["test".to_string()],
            );
            mgr.save_session(&session).unwrap();
        }

        let loaded = mgr.get_or_create("test:persist", None);
        let note = loaded.get_note("persist").unwrap();
        assert_eq!(note.content, "survives reload");
        assert_eq!(note.tags, vec!["test"]);
    }

    #[test]
    fn test_note_persist_across_new_manager() {
        let dir = tempfile::tempdir().unwrap();

        // Write with one manager
        {
            let mgr = crate::manager::SessionManager::new(dir.path().to_path_buf()).unwrap();
            let mut session = mgr.get_or_create("test:restart", None);
            session.save_note(
                "survives_restart".to_string(),
                "yes".to_string(),
                vec!["critical".to_string()],
            );
            mgr.save_session(&session).unwrap();
        }

        // Read with a fresh manager (simulates process restart)
        let mgr2 = crate::manager::SessionManager::new(dir.path().to_path_buf()).unwrap();
        let loaded = mgr2.get_or_create("test:restart", None);
        let note = loaded.get_note("survives_restart").unwrap();
        assert_eq!(note.content, "yes");
        assert_eq!(note.tags, vec!["critical"]);
    }
}
