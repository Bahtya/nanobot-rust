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
//! ## Persistence
//!
//! Notes are persisted to disk as JSON files via [`NotesStore`], independent
//! of session message storage. The [`NotesManager`] provides convenience
//! methods that coordinate between the in-memory `Session` and the
//! file-backed store.
//!
//! ## Compaction
//!
//! When the number of notes exceeds a configurable threshold
//! ([`NoteCompactionConfig`]), older notes are automatically summarized
//! into a compacted note, keeping recent notes intact.

use anyhow::{Context, Result};
use nanobot_session::{Note, Session};
use std::path::PathBuf;
use tracing::{debug, info, warn};

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
// NoteCompactionConfig
// ---------------------------------------------------------------------------

/// Configuration for note compaction behaviour.
///
/// Controls when and how notes are compacted when they exceed a threshold.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NoteCompactionConfig {
    /// Maximum number of notes before compaction triggers.
    /// Default: 50.
    pub max_notes: usize,
    /// Number of recent notes to keep intact during compaction.
    /// The remaining older notes are summarized. Default: 25.
    pub keep_recent: usize,
    /// Whether compaction is enabled. Default: true.
    pub enabled: bool,
}

impl Default for NoteCompactionConfig {
    fn default() -> Self {
        Self {
            max_notes: 50,
            keep_recent: 25,
            enabled: true,
        }
    }
}

impl NoteCompactionConfig {
    /// Create a new config with custom thresholds.
    pub fn new(max_notes: usize, keep_recent: usize) -> Self {
        Self {
            max_notes,
            keep_recent,
            enabled: true,
        }
    }

    /// Check if compaction should run for the given note count.
    pub fn needs_compaction(&self, note_count: usize) -> bool {
        self.enabled && note_count > self.max_notes
    }
}

// ---------------------------------------------------------------------------
// NotesStore
// ---------------------------------------------------------------------------

/// File-backed store for structured notes, independent of session storage.
///
/// Each session's notes are stored as a JSON file named `{safe_key}.notes.json`
/// in the configured directory. Writes are atomic (temp file + rename).
pub struct NotesStore {
    dir: PathBuf,
}

impl NotesStore {
    /// Create a new `NotesStore`, creating the directory if needed.
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
        self.dir
            .join(format!("{}.notes.json", Self::safe_key(session_key)))
    }

    /// Save notes to disk (atomic write via temp file + rename).
    pub fn save(&self, session_key: &str, notes: &[Note]) -> Result<()> {
        let path = self.notes_path(session_key);

        if notes.is_empty() {
            if path.exists() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("Failed to remove notes file: {}", path.display()))?;
            }
            return Ok(());
        }

        debug!(
            "Persisting {} notes for session '{}'",
            notes.len(),
            session_key
        );

        let json =
            serde_json::to_string_pretty(notes).with_context(|| "Failed to serialize notes")?;

        let tmp_path = path.with_extension("notes.tmp");
        std::fs::write(&tmp_path, &json)
            .with_context(|| format!("Failed to write temp notes file: {}", tmp_path.display()))?;

        std::fs::rename(&tmp_path, &path)
            .with_context(|| format!("Failed to rename notes file: {}", path.display()))?;

        Ok(())
    }

    /// Load notes from disk.
    ///
    /// Returns an empty vec if no file exists. Returns an error if the file
    /// exists but is corrupted (logs a warning and returns empty vec as fallback).
    pub fn load(&self, session_key: &str) -> Vec<Note> {
        let path = self.notes_path(session_key);
        if !path.exists() {
            return Vec::new();
        }

        match std::fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<Vec<Note>>(&content) {
                Ok(notes) => {
                    debug!(
                        "Loaded {} notes for session '{}' from disk",
                        notes.len(),
                        session_key
                    );
                    notes
                }
                Err(e) => {
                    warn!(
                        "Corrupted notes file for session '{}': {}. Starting fresh.",
                        session_key, e
                    );
                    Vec::new()
                }
            },
            Err(e) => {
                warn!(
                    "Cannot read notes file for session '{}': {}. Starting fresh.",
                    session_key, e
                );
                Vec::new()
            }
        }
    }

    /// Delete all notes for a session.
    pub fn delete(&self, session_key: &str) -> Result<()> {
        let path = self.notes_path(session_key);
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("Failed to delete notes file: {}", path.display()))?;
        }
        Ok(())
    }

    /// List all session keys that have persisted notes.
    pub fn list_sessions(&self) -> Result<Vec<String>> {
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
                        keys.push(name.trim_end_matches(".notes.json").to_string());
                    }
                }
            }
        }

        Ok(keys)
    }
}

// ---------------------------------------------------------------------------
// NotesManager
// ---------------------------------------------------------------------------

/// Manages structured notes within a session.
///
/// Delegates all operations to `Session`'s built-in note methods.
/// Provides additional methods for disk persistence and smart compaction.
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
    // Disk persistence
    // -----------------------------------------------------------------------

    /// Persist session notes to disk via a [`NotesStore`].
    ///
    /// Call this after modifying notes to ensure they survive restarts.
    pub fn persist_to_store(store: &NotesStore, session: &Session) -> Result<()> {
        store.save(&session.key, session.all_notes())
    }

    /// Restore notes from disk into a session, replacing any in-memory notes.
    ///
    /// This is typically called at startup to recover notes from the previous
    /// session. Returns the number of notes restored.
    pub fn restore_from_store(store: &NotesStore, session: &mut Session) -> usize {
        let disk_notes = store.load(&session.key);
        let count = disk_notes.len();
        if count > 0 {
            session.notes = disk_notes;
            info!(
                "Restored {} notes for session '{}' from disk",
                count, session.key
            );
        }
        count
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

    // -----------------------------------------------------------------------
    // Smart compaction with structured summarization
    // -----------------------------------------------------------------------

    /// Run note compaction if the session has too many notes.
    ///
    /// Uses the default compaction threshold from `Session`.
    /// Returns true if compaction was performed.
    pub fn compact_if_needed(session: &mut Session) -> bool {
        session.compact_notes()
    }

    /// Run compaction with a custom configuration.
    ///
    /// When notes exceed `config.max_notes`, older notes are summarized
    /// into categorized compaction notes (one per [`NoteFormat`] that has
    /// content among the older notes). The most recent `config.keep_recent`
    /// notes are always preserved intact.
    ///
    /// Returns the number of notes removed (not counting the summary notes added).
    /// Returns 0 if compaction was not needed.
    pub fn compact_with_config(session: &mut Session, config: &NoteCompactionConfig) -> usize {
        if !config.needs_compaction(session.notes.len()) {
            return 0;
        }

        let total = session.notes.len();
        let keep = config.keep_recent.min(total);
        let older_count = total.saturating_sub(keep);

        if older_count == 0 {
            return 0;
        }

        // Remove stale compaction notes from a prior compaction cycle
        let stale_titles = [
            "_compacted",
            "_compact_summary",
            "_compact_actions",
            "_compact_decisions",
            "_compact_questions",
        ];
        for title in &stale_titles {
            session.delete_note(title);
        }

        // Drain older notes (everything before the keep boundary)
        let older: Vec<Note> = session.notes.drain(..older_count).collect();

        // Categorize older notes by format
        let mut summary_parts: Vec<String> = Vec::new();
        let mut action_parts: Vec<String> = Vec::new();
        let mut decision_parts: Vec<String> = Vec::new();
        let mut question_parts: Vec<String> = Vec::new();
        let mut uncategorized_parts: Vec<String> = Vec::new();

        for note in &older {
            let line = format!("- {}: {}", note.title, note.content);
            let format_tags: Vec<&str> = NoteFormat::all().iter().map(|f| f.tag()).collect();

            if note.tags.iter().any(|t| t == "summary") {
                summary_parts.push(line.clone());
            } else if note.tags.iter().any(|t| t == "action_items") {
                action_parts.push(line.clone());
            } else if note.tags.iter().any(|t| t == "decisions") {
                decision_parts.push(line.clone());
            } else if note.tags.iter().any(|t| t == "open_questions") {
                question_parts.push(line.clone());
            } else if note.tags.iter().any(|t| format_tags.contains(&t.as_str())) {
                // Other known format tags — put in summary
                summary_parts.push(line.clone());
            } else {
                uncategorized_parts.push(line);
            }
        }

        let mut notes_created = 0;

        // Create a single _compacted summary note
        let mut all_parts = Vec::new();
        if !summary_parts.is_empty() {
            all_parts.push(format!("Summaries:\n{}", summary_parts.join("\n")));
        }
        if !action_parts.is_empty() {
            all_parts.push(format!("Actions:\n{}", action_parts.join("\n")));
        }
        if !decision_parts.is_empty() {
            all_parts.push(format!("Decisions:\n{}", decision_parts.join("\n")));
        }
        if !question_parts.is_empty() {
            all_parts.push(format!("Questions:\n{}", question_parts.join("\n")));
        }
        if !uncategorized_parts.is_empty() {
            all_parts.push(format!("Other:\n{}", uncategorized_parts.join("\n")));
        }

        if !all_parts.is_empty() {
            let compacted = Note::new(
                "_compacted".to_string(),
                format!(
                    "Compacted {} notes ({} kept recent). Details:\n\n{}",
                    older.len(),
                    keep,
                    all_parts.join("\n\n")
                ),
                vec!["_system".to_string()],
            );
            session.notes.insert(0, compacted);
            notes_created += 1;
        }

        info!(
            "Compacted notes for session '{}': {} older → {} summary notes, {} recent kept",
            session.key,
            older.len(),
            notes_created,
            keep,
        );

        older.len()
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
pub fn extract_compaction_notes(
    session: &mut Session,
    old_messages: &[nanobot_session::SessionEntry],
) -> usize {
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
        let summary = format!("Discussed: {}", topics.join("; "));
        if let Err(e) = NotesManager::save_structured_note(
            session,
            "_compaction_summary".to_string(),
            summary,
            NoteFormat::Summary,
            vec!["_system".to_string()],
        ) {
            warn!("Failed to save compaction summary note: {e}");
        } else {
            count += 1;
        }
    }

    // 2) Action items — lines containing "need to", "should", "TODO", "must"
    let action_patterns = [
        "need to",
        "should",
        "todo",
        "must",
        "follow up",
        "action item",
    ];
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
        if let Err(e) = NotesManager::save_structured_note(
            session,
            "_compaction_actions".to_string(),
            action_items.join("\n"),
            NoteFormat::ActionItems,
            vec!["_system".to_string()],
        ) {
            warn!("Failed to save compaction actions note: {e}");
        } else {
            count += 1;
        }
    }

    // 3) Decisions — lines containing "decided", "chose", "will use", "agreed"
    let decision_patterns = [
        "decided",
        "chose",
        "will use",
        "agreed",
        "going with",
        "let's use",
    ];
    let decisions: Vec<String> = old_messages
        .iter()
        .filter(|m| {
            m.role == nanobot_core::MessageRole::Assistant
                || m.role == nanobot_core::MessageRole::User
        })
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
        if let Err(e) = NotesManager::save_structured_note(
            session,
            "_compaction_decisions".to_string(),
            decisions.join("\n"),
            NoteFormat::Decisions,
            vec!["_system".to_string()],
        ) {
            warn!("Failed to save compaction decisions note: {e}");
        } else {
            count += 1;
        }
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
        if let Err(e) = NotesManager::save_structured_note(
            session,
            "_compaction_questions".to_string(),
            questions.join("\n"),
            NoteFormat::OpenQuestions,
            vec!["_system".to_string()],
        ) {
            warn!("Failed to save compaction questions note: {e}");
        } else {
            count += 1;
        }
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
        assert_eq!(
            NoteFormat::from_tag("action_items"),
            Some(NoteFormat::ActionItems)
        );
        assert_eq!(
            NoteFormat::from_tag("decisions"),
            Some(NoteFormat::Decisions)
        );
        assert_eq!(
            NoteFormat::from_tag("open_questions"),
            Some(NoteFormat::OpenQuestions)
        );
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
        NotesManager::save_note(
            &mut session,
            "note1".to_string(),
            "First note".to_string(),
            vec![],
        )
        .unwrap();
        NotesManager::save_note(
            &mut session,
            "note2".to_string(),
            "Second note".to_string(),
            vec![],
        )
        .unwrap();

        let notes = NotesManager::load_notes(&session);
        assert_eq!(notes.len(), 2);
    }

    #[test]
    fn test_update_existing_note() {
        let mut session = make_session();
        NotesManager::save_note(
            &mut session,
            "key1".to_string(),
            "Original".to_string(),
            vec![],
        )
        .unwrap();
        NotesManager::save_note(
            &mut session,
            "key1".to_string(),
            "Updated".to_string(),
            vec![],
        )
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
        NotesManager::save_note(
            &mut session,
            "style".to_string(),
            "Concise".to_string(),
            vec![],
        )
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

        assert_eq!(
            NotesManager::notes_by_format(&session, NoteFormat::Summary).len(),
            1
        );
        assert_eq!(
            NotesManager::notes_by_format(&session, NoteFormat::ActionItems).len(),
            1
        );
        assert_eq!(
            NotesManager::notes_by_format(&session, NoteFormat::Decisions).len(),
            0
        );
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
        let messages = vec![nanobot_session::SessionEntry {
            role: nanobot_core::MessageRole::Assistant,
            content: "You should add tests for the API layer. We need to handle errors."
                .to_string(),
            ..Default::default()
        }];

        let count = extract_compaction_notes(&mut session, &messages);
        assert!(count >= 1);

        let actions = NotesManager::notes_by_format(&session, NoteFormat::ActionItems);
        assert_eq!(actions.len(), 1);
        assert!(actions[0].content.contains("should add tests"));
    }

    #[test]
    fn test_extract_compaction_notes_decisions() {
        let mut session = make_session();
        let messages = vec![nanobot_session::SessionEntry {
            role: nanobot_core::MessageRole::Assistant,
            content: "We decided to use Rust for the backend.".to_string(),
            ..Default::default()
        }];

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

    // -------------------------------------------------------------------
    // NoteCompactionConfig
    // -------------------------------------------------------------------

    #[test]
    fn test_compaction_config_default() {
        let config = NoteCompactionConfig::default();
        assert_eq!(config.max_notes, 50);
        assert_eq!(config.keep_recent, 25);
        assert!(config.enabled);
    }

    #[test]
    fn test_compaction_config_custom() {
        let config = NoteCompactionConfig::new(100, 40);
        assert_eq!(config.max_notes, 100);
        assert_eq!(config.keep_recent, 40);
        assert!(config.enabled);
    }

    #[test]
    fn test_needs_compaction_under_threshold() {
        let config = NoteCompactionConfig::new(10, 5);
        assert!(!config.needs_compaction(10));
        assert!(!config.needs_compaction(5));
    }

    #[test]
    fn test_needs_compaction_over_threshold() {
        let config = NoteCompactionConfig::new(10, 5);
        assert!(config.needs_compaction(11));
        assert!(config.needs_compaction(50));
    }

    #[test]
    fn test_needs_compaction_disabled() {
        let config = NoteCompactionConfig {
            enabled: false,
            ..NoteCompactionConfig::default()
        };
        assert!(!config.needs_compaction(100));
    }

    // -------------------------------------------------------------------
    // NotesStore — disk persistence
    // -------------------------------------------------------------------

    #[test]
    fn test_notes_store_new_creates_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let notes_dir = tmp.path().join("notes");
        assert!(!notes_dir.exists());

        let store = NotesStore::new(notes_dir.clone()).unwrap();
        assert!(notes_dir.exists());
        assert!(store.list_sessions().unwrap().is_empty());
    }

    #[test]
    fn test_notes_store_save_and_load() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NotesStore::new(tmp.path().to_path_buf()).unwrap();

        let notes = vec![
            Note::new(
                "Architecture".into(),
                "Use microservices".into(),
                vec!["design".into()],
            ),
            Note::new(
                "Language".into(),
                "Rust".into(),
                vec!["tech".into(), "backend".into()],
            ),
        ];

        store.save("session:abc", &notes).unwrap();
        let loaded = store.load("session:abc");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].title, "Architecture");
        assert_eq!(loaded[1].tags, vec!["tech", "backend"]);
    }

    #[test]
    fn test_notes_store_load_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NotesStore::new(tmp.path().to_path_buf()).unwrap();
        assert!(store.load("nonexistent").is_empty());
    }

    #[test]
    fn test_notes_store_save_empty_removes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NotesStore::new(tmp.path().to_path_buf()).unwrap();

        let notes = vec![Note::new("t".into(), "c".into(), vec![])];
        store.save("session:x", &notes).unwrap();
        assert_eq!(store.load("session:x").len(), 1);

        store.save("session:x", &[]).unwrap();
        assert!(store.load("session:x").is_empty());
    }

    #[test]
    fn test_notes_store_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NotesStore::new(tmp.path().to_path_buf()).unwrap();

        let notes = vec![Note::new("t".into(), "c".into(), vec![])];
        store.save("session:del", &notes).unwrap();
        assert_eq!(store.list_sessions().unwrap().len(), 1);

        store.delete("session:del").unwrap();
        assert!(store.load("session:del").is_empty());
        assert!(store.list_sessions().unwrap().is_empty());
    }

    #[test]
    fn test_notes_store_delete_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NotesStore::new(tmp.path().to_path_buf()).unwrap();
        assert!(store.delete("no_such_session").is_ok());
    }

    #[test]
    fn test_notes_store_list_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NotesStore::new(tmp.path().to_path_buf()).unwrap();

        assert!(store.list_sessions().unwrap().is_empty());

        let notes = vec![Note::new("t".into(), "c".into(), vec![])];
        store.save("platform:chat1", &notes).unwrap();
        store.save("platform:chat2", &notes).unwrap();

        let mut keys = store.list_sessions().unwrap();
        keys.sort();
        assert_eq!(keys, vec!["platform_chat1", "platform_chat2"]);
    }

    #[test]
    fn test_notes_store_save_is_atomic() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NotesStore::new(tmp.path().to_path_buf()).unwrap();

        // Save initial notes
        let notes_v1 = vec![Note::new("v1".into(), "version 1".into(), vec![])];
        store.save("session:atomic", &notes_v1).unwrap();

        // Overwrite with new version
        let notes_v2 = vec![
            Note::new("v2".into(), "version 2".into(), vec![]),
            Note::new("extra".into(), "more data".into(), vec![]),
        ];
        store.save("session:atomic", &notes_v2).unwrap();

        let loaded = store.load("session:atomic");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].title, "v2");
    }

    #[test]
    fn test_notes_store_corrupted_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NotesStore::new(tmp.path().to_path_buf()).unwrap();

        // Write corrupted JSON
        let path = tmp.path().join("session_corrupt.notes.json");
        std::fs::write(&path, "{invalid json").unwrap();

        // Should gracefully return empty vec
        let notes = store.load("session:corrupt");
        assert!(notes.is_empty());
    }

    #[test]
    fn test_notes_store_multiple_sessions_independent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NotesStore::new(tmp.path().to_path_buf()).unwrap();

        store
            .save(
                "session:a",
                &[Note::new(
                    "A".into(),
                    "content A".into(),
                    vec!["alpha".into()],
                )],
            )
            .unwrap();
        store
            .save(
                "session:b",
                &[Note::new(
                    "B".into(),
                    "content B".into(),
                    vec!["beta".into()],
                )],
            )
            .unwrap();

        // Modify session A
        store
            .save(
                "session:a",
                &[Note::new(
                    "A-mod".into(),
                    "new content".into(),
                    vec!["alpha".into()],
                )],
            )
            .unwrap();

        // Session B should be unaffected
        let b_notes = store.load("session:b");
        assert_eq!(b_notes.len(), 1);
        assert_eq!(b_notes[0].title, "B");
    }

    // -------------------------------------------------------------------
    // NotesManager — disk persistence helpers
    // -------------------------------------------------------------------

    #[test]
    fn test_persist_and_restore_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NotesStore::new(tmp.path().to_path_buf()).unwrap();

        let mut session = make_session();
        session.save_note("note1".into(), "First note".into(), vec!["tag1".into()]);
        session.save_note("note2".into(), "Second note".into(), vec!["tag2".into()]);

        // Persist
        NotesManager::persist_to_store(&store, &session).unwrap();

        // Create a fresh session and restore
        let mut session2 = make_session();
        assert_eq!(session2.notes.len(), 0);

        let restored = NotesManager::restore_from_store(&store, &mut session2);
        assert_eq!(restored, 2);
        assert_eq!(session2.notes.len(), 2);
        assert_eq!(session2.notes[0].title, "note1");
        assert_eq!(session2.notes[1].title, "note2");
    }

    #[test]
    fn test_restore_empty_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NotesStore::new(tmp.path().to_path_buf()).unwrap();

        let mut session = make_session();
        let restored = NotesManager::restore_from_store(&store, &mut session);
        assert_eq!(restored, 0);
        assert!(session.notes.is_empty());
    }

    #[test]
    fn test_persist_empty_session_removes_file() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NotesStore::new(tmp.path().to_path_buf()).unwrap();

        let mut session = make_session();
        session.save_note("temp".into(), "will be removed".into(), vec![]);
        NotesManager::persist_to_store(&store, &session).unwrap();
        assert_eq!(store.list_sessions().unwrap().len(), 1);

        // Delete the note and persist again
        session.delete_note("temp");
        NotesManager::persist_to_store(&store, &session).unwrap();
        assert!(store.list_sessions().unwrap().is_empty());
    }

    #[test]
    fn test_restore_overwrites_in_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NotesStore::new(tmp.path().to_path_buf()).unwrap();

        let mut session = make_session();
        session.save_note("disk_note".into(), "from disk".into(), vec![]);
        NotesManager::persist_to_store(&store, &session).unwrap();

        // Create new session with different notes
        let mut session2 = make_session();
        session2.save_note("mem_note".into(), "from memory".into(), vec![]);
        assert_eq!(session2.notes.len(), 1);
        assert_eq!(session2.notes[0].title, "mem_note");

        // Restore should replace
        NotesManager::restore_from_store(&store, &mut session2);
        assert_eq!(session2.notes.len(), 1);
        assert_eq!(session2.notes[0].title, "disk_note");
    }

    // -------------------------------------------------------------------
    // Smart compaction with config
    // -------------------------------------------------------------------

    #[test]
    fn test_compact_with_config_noop_under_threshold() {
        let mut session = make_session();
        for i in 0..10 {
            session.save_note(format!("n{}", i), format!("note {}", i), vec![]);
        }

        let config = NoteCompactionConfig::new(20, 10);
        let removed = NotesManager::compact_with_config(&mut session, &config);
        assert_eq!(removed, 0);
        assert_eq!(session.notes.len(), 10);
    }

    #[test]
    fn test_compact_with_config_triggers_over_threshold() {
        let mut session = make_session();
        for i in 0..30 {
            session.save_note(format!("n{}", i), format!("note {}", i), vec![]);
        }
        assert_eq!(session.notes.len(), 30);

        let config = NoteCompactionConfig::new(20, 10);
        let removed = NotesManager::compact_with_config(&mut session, &config);
        assert_eq!(removed, 20); // 30 - 10 kept
                                 // Should have: 1 compacted summary + 10 recent = 11
        assert_eq!(session.notes.len(), 11);

        // The compacted note should be first
        assert_eq!(session.notes[0].title, "_compacted");
        assert!(session.notes[0].content.contains("Compacted 20 notes"));

        // Recent notes should be intact
        assert_eq!(session.notes[1].title, "n20");
        assert_eq!(session.notes[10].title, "n29");
    }

    #[test]
    fn test_compact_with_config_categorizes_by_format() {
        let mut session = make_session();
        // Add 10 summary notes
        for i in 0..10 {
            session.save_note(
                format!("summary_{}", i),
                format!("summary {}", i),
                vec!["summary".into()],
            );
        }
        // Add 5 action notes
        for i in 0..5 {
            session.save_note(
                format!("action_{}", i),
                format!("action {}", i),
                vec!["action_items".into()],
            );
        }
        // Add 10 uncategorized
        for i in 0..10 {
            session.save_note(
                format!("misc_{}", i),
                format!("misc {}", i),
                vec!["other".into()],
            );
        }
        // Total: 25 notes, threshold 15, keep 5 recent

        let config = NoteCompactionConfig::new(15, 5);
        let removed = NotesManager::compact_with_config(&mut session, &config);
        assert_eq!(removed, 20);

        // 1 compacted note + 5 recent
        assert_eq!(session.notes.len(), 6);
        let compacted = &session.notes[0];
        assert!(compacted.content.contains("Summaries:"));
        assert!(compacted.content.contains("Actions:"));
        assert!(compacted.content.contains("Other:"));
    }

    #[test]
    fn test_compact_with_config_removes_stale_compaction() {
        let mut session = make_session();
        // Add a stale _compacted note
        session.save_note(
            "_compacted".into(),
            "old compaction".into(),
            vec!["_system".into()],
        );
        // Add enough notes to trigger compaction
        for i in 0..20 {
            session.save_note(format!("n{}", i), format!("note {}", i), vec![]);
        }
        // Total: 21 notes

        let config = NoteCompactionConfig::new(15, 5);
        let removed = NotesManager::compact_with_config(&mut session, &config);
        // Removed: 21 - 5 = 16
        assert_eq!(removed, 16);

        // Should only have 1 _compacted note (the new one)
        let compacted_count = session
            .notes
            .iter()
            .filter(|n| n.title == "_compacted")
            .count();
        assert_eq!(compacted_count, 1);
        assert!(session.notes[0].content.contains("Compacted 16 notes"));
    }

    #[test]
    fn test_compact_with_config_preserves_recent_notes_order() {
        let mut session = make_session();
        for i in 0..20 {
            session.save_note(format!("n{}", i), format!("note {}", i), vec![]);
        }

        let config = NoteCompactionConfig::new(10, 5);
        NotesManager::compact_with_config(&mut session, &config);

        // Last 5 notes should be n15..n19 in order
        assert_eq!(session.notes.len(), 6); // 1 compacted + 5 recent
        assert_eq!(session.notes[1].title, "n15");
        assert_eq!(session.notes[2].title, "n16");
        assert_eq!(session.notes[3].title, "n17");
        assert_eq!(session.notes[4].title, "n18");
        assert_eq!(session.notes[5].title, "n19");
    }

    #[test]
    fn test_compact_with_config_disabled() {
        let mut session = make_session();
        for i in 0..100 {
            session.save_note(format!("n{}", i), format!("note {}", i), vec![]);
        }

        let config = NoteCompactionConfig {
            enabled: false,
            ..NoteCompactionConfig::default()
        };

        let removed = NotesManager::compact_with_config(&mut session, &config);
        assert_eq!(removed, 0);
        assert_eq!(session.notes.len(), 100);
    }

    #[test]
    fn test_compact_with_config_keep_more_than_total() {
        let mut session = make_session();
        for i in 0..10 {
            session.save_note(format!("n{}", i), format!("note {}", i), vec![]);
        }

        // keep_recent > total, threshold is low
        let config = NoteCompactionConfig::new(5, 100);
        // 10 > 5, but older_count = 10 - min(100, 10) = 10 - 10 = 0
        let removed = NotesManager::compact_with_config(&mut session, &config);
        assert_eq!(removed, 0);
    }

    // -------------------------------------------------------------------
    // Integration: persist + compact + restore
    // -------------------------------------------------------------------

    #[test]
    fn test_persist_compact_restore_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NotesStore::new(tmp.path().to_path_buf()).unwrap();

        // 1. Create session with many notes
        let mut session = make_session();
        for i in 0..30 {
            let format = if i % 3 == 0 {
                NoteFormat::Summary
            } else if i % 3 == 1 {
                NoteFormat::ActionItems
            } else {
                NoteFormat::Decisions
            };
            NotesManager::save_structured_note(
                &mut session,
                format!("note_{}", i),
                format!("content {}", i),
                format,
                vec![],
            )
            .unwrap();
        }
        assert_eq!(session.notes.len(), 30);

        // 2. Persist before compaction
        NotesManager::persist_to_store(&store, &session).unwrap();
        assert_eq!(store.load(&session.key).len(), 30);

        // 3. Compact
        let config = NoteCompactionConfig::new(20, 10);
        let removed = NotesManager::compact_with_config(&mut session, &config);
        assert_eq!(removed, 20);
        assert_eq!(session.notes.len(), 11);

        // 4. Persist after compaction
        NotesManager::persist_to_store(&store, &session).unwrap();

        // 5. Restore into fresh session
        let mut restored_session = make_session();
        let count = NotesManager::restore_from_store(&store, &mut restored_session);
        assert_eq!(count, 11);
        assert_eq!(restored_session.notes.len(), 11);
        assert_eq!(restored_session.notes[0].title, "_compacted");
    }

    #[test]
    fn test_persist_structured_notes_with_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let store = NotesStore::new(tmp.path().to_path_buf()).unwrap();

        let mut session = make_session();
        NotesManager::save_structured_note(
            &mut session,
            "s1".into(),
            "Summary 1".into(),
            NoteFormat::Summary,
            vec![],
        )
        .unwrap();
        NotesManager::save_structured_note(
            &mut session,
            "a1".into(),
            "Action 1".into(),
            NoteFormat::ActionItems,
            vec!["urgent".into()],
        )
        .unwrap();
        NotesManager::save_structured_note(
            &mut session,
            "d1".into(),
            "Decision 1".into(),
            NoteFormat::Decisions,
            vec![],
        )
        .unwrap();

        NotesManager::persist_to_store(&store, &session).unwrap();

        let mut restored = make_session();
        NotesManager::restore_from_store(&store, &mut restored);

        // Verify format tags are preserved
        let summaries = NotesManager::notes_by_format(&restored, NoteFormat::Summary);
        assert_eq!(summaries.len(), 1);
        let actions = NotesManager::notes_by_format(&restored, NoteFormat::ActionItems);
        assert_eq!(actions.len(), 1);
        assert!(actions[0].tags.contains(&"urgent".to_string()));
        let decisions = NotesManager::notes_by_format(&restored, NoteFormat::Decisions);
        assert_eq!(decisions.len(), 1);
    }

    #[test]
    fn test_compaction_config_serde() {
        let config = NoteCompactionConfig::new(42, 17);
        let json = serde_json::to_string(&config).unwrap();
        let back: NoteCompactionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.max_notes, 42);
        assert_eq!(back.keep_recent, 17);
        assert!(back.enabled);
    }

    #[test]
    fn test_notes_store_persists_across_store_instances() {
        let tmp = tempfile::tempdir().unwrap();

        // Instance 1 saves
        let store1 = NotesStore::new(tmp.path().to_path_buf()).unwrap();
        let notes = vec![Note::new(
            "persistent".into(),
            "survives restart".into(),
            vec!["tag".into()],
        )];
        store1.save("session:test", &notes).unwrap();
        drop(store1);

        // Instance 2 loads
        let store2 = NotesStore::new(tmp.path().to_path_buf()).unwrap();
        let loaded = store2.load("session:test");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].title, "persistent");
    }
}
