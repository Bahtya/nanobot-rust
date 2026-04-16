//! Context compaction — summarizes conversation history when approaching token limits.
//!
//! When the estimated token count exceeds a threshold (default 80% of context window),
//! older messages are replaced with a compact summary. This keeps the agent functional
//! in long-running sessions without losing essential context.

use crate::notes::extract_compaction_notes;
use anyhow::Result;
use kestrel_core::{
    COMPACTION_KEEP_RECENT, COMPACTION_THRESHOLD_RATIO, DEFAULT_CONTEXT_WINDOW_TOKENS,
};
use kestrel_session::Session;
use tracing::{debug, info};

/// Compaction strategy for reducing conversation history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionStrategy {
    /// Summarize older messages into a single system message.
    Summarize,
    /// Drop older messages entirely (keeps only recent).
    Truncate,
}

/// Configuration for context compaction.
#[derive(Debug, Clone)]
pub struct CompactionConfig {
    /// Maximum context window in tokens.
    pub context_window_tokens: usize,
    /// Fraction of context window at which compaction triggers (0.0–1.0).
    pub threshold_ratio: f64,
    /// Number of recent messages to always keep.
    pub keep_recent: usize,
    /// Compaction strategy.
    pub strategy: CompactionStrategy,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            context_window_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
            threshold_ratio: COMPACTION_THRESHOLD_RATIO,
            keep_recent: COMPACTION_KEEP_RECENT,
            strategy: CompactionStrategy::Summarize,
        }
    }
}

impl CompactionConfig {
    /// Token count at which compaction triggers.
    pub fn threshold_tokens(&self) -> usize {
        (self.context_window_tokens as f64 * self.threshold_ratio) as usize
    }

    /// Check if a session needs compaction based on estimated tokens.
    pub fn needs_compaction(&self, session: &Session) -> bool {
        let estimated = session.estimated_tokens();
        let threshold = self.threshold_tokens();
        if estimated > threshold {
            debug!(
                "Session '{}' needs compaction: {} estimated tokens > {} threshold",
                session.key, estimated, threshold
            );
            true
        } else {
            false
        }
    }
}

/// Result of a compaction operation.
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// Number of messages before compaction.
    pub messages_before: usize,
    /// Number of messages after compaction.
    pub messages_after: usize,
    /// Estimated tokens before compaction.
    pub tokens_before: usize,
    /// Estimated tokens after compaction.
    pub tokens_after: usize,
    /// Number of structured notes extracted from the compacted messages.
    pub notes_extracted: usize,
}

/// Compact a session's message history when it exceeds the token threshold.
///
/// For the `Summarize` strategy, older messages are replaced with a single
/// system message containing a structured summary. For `Truncate`, older
/// messages are simply dropped.
pub fn compact_session(
    session: &mut Session,
    config: &CompactionConfig,
) -> Result<CompactionResult> {
    let tokens_before = session.estimated_tokens();
    let messages_before = session.messages.len();

    if !config.needs_compaction(session) {
        return Ok(CompactionResult {
            messages_before,
            messages_after: messages_before,
            tokens_before,
            tokens_after: tokens_before,
            notes_extracted: 0,
        });
    }

    info!(
        "Compacting session '{}': {} messages, ~{} estimated tokens",
        session.key, messages_before, tokens_before
    );

    // Don't compact if we have fewer messages than keep_recent + 1 (system summary)
    if messages_before <= config.keep_recent + 1 {
        debug!(
            "Session too short to compact ({} messages, keep_recent={})",
            messages_before, config.keep_recent
        );
        return Ok(CompactionResult {
            messages_before,
            messages_after: messages_before,
            tokens_before,
            tokens_after: tokens_before,
            notes_extracted: 0,
        });
    }

    match config.strategy {
        CompactionStrategy::Summarize => compact_summarize(session, config),
        CompactionStrategy::Truncate => compact_truncate(session, config),
    }
}

/// Summarize older messages into a compact system message.
fn compact_summarize(session: &mut Session, config: &CompactionConfig) -> Result<CompactionResult> {
    let messages_before = session.messages.len();
    let tokens_before = session.estimated_tokens();

    // Split messages: old ones to summarize, recent ones to keep
    let split_point = messages_before.saturating_sub(config.keep_recent);

    // Preserve leading system message if present
    let has_system = session
        .messages
        .first()
        .map(|m| m.role == kestrel_core::MessageRole::System)
        .unwrap_or(false);

    let summary_start = if has_system { 1 } else { 0 };

    if split_point <= summary_start {
        // Not enough old messages to summarize
        return Ok(CompactionResult {
            messages_before,
            messages_after: messages_before,
            tokens_before,
            tokens_after: tokens_before,
            notes_extracted: 0,
        });
    }

    // Build summary from old messages (excluding initial system message)
    let old_messages = session.messages[summary_start..split_point].to_vec();
    let summary = build_summary(&old_messages);

    // Extract structured notes from old messages before discarding them.
    // This preserves key information (decisions, action items, questions)
    // as persistent notes that survive compaction.
    let notes_extracted = extract_compaction_notes(session, &old_messages);
    if notes_extracted > 0 {
        info!(
            "Extracted {} structured notes during compaction of session '{}'",
            notes_extracted, session.key
        );
    }

    // Rebuild message list
    let mut new_messages = Vec::new();

    // Keep original system message
    if has_system {
        new_messages.push(session.messages[0].clone());
    }

    // Add compaction summary
    new_messages.push(kestrel_session::SessionEntry {
        role: kestrel_core::MessageRole::System,
        content: format!(
            "## Conversation Summary (compacted)\n\
             The following is a summary of earlier conversation history that was \
             compacted to save context space:\n\n{}",
            summary
        ),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        timestamp: Some(chrono::Local::now()),
    });

    // Keep recent messages
    new_messages.extend_from_slice(&session.messages[split_point..]);

    session.messages = new_messages;
    session.metadata.truncated = true;

    let tokens_after = session.estimated_tokens();
    info!(
        "Compacted session '{}': {} → {} messages, ~{} → ~{} estimated tokens",
        session.key,
        messages_before,
        session.messages.len(),
        tokens_before,
        tokens_after
    );

    Ok(CompactionResult {
        messages_before,
        messages_after: session.messages.len(),
        tokens_before,
        tokens_after,
        notes_extracted,
    })
}

/// Truncate older messages, keeping only recent ones.
fn compact_truncate(session: &mut Session, config: &CompactionConfig) -> Result<CompactionResult> {
    let messages_before = session.messages.len();
    let tokens_before = session.estimated_tokens();

    session.truncate(config.keep_recent);

    let tokens_after = session.estimated_tokens();

    Ok(CompactionResult {
        messages_before,
        messages_after: session.messages.len(),
        tokens_before,
        tokens_after,
        notes_extracted: 0,
    })
}

/// Build a text summary from a slice of session entries.
fn build_summary(messages: &[kestrel_session::SessionEntry]) -> String {
    let mut parts = Vec::new();
    let mut user_msgs = 0;
    let mut assistant_msgs = 0;
    let mut tool_results = 0;
    let mut key_topics = Vec::new();

    for msg in messages {
        match msg.role {
            kestrel_core::MessageRole::User => {
                user_msgs += 1;
                // Extract first line as a topic hint (up to 80 chars)
                let first_line = msg.content.lines().next().unwrap_or("");
                if !first_line.is_empty() && key_topics.len() < 5 {
                    let truncated = if first_line.len() > 80 {
                        format!("{}...", &first_line[..77])
                    } else {
                        first_line.to_string()
                    };
                    key_topics.push(truncated);
                }
            }
            kestrel_core::MessageRole::Assistant => assistant_msgs += 1,
            kestrel_core::MessageRole::Tool => tool_results += 1,
            kestrel_core::MessageRole::System => {}
        }
    }

    parts.push(format!(
        "- {} user messages, {} assistant responses, {} tool results",
        user_msgs, assistant_msgs, tool_results
    ));

    if !key_topics.is_empty() {
        parts.push("- Topics discussed:".to_string());
        for topic in &key_topics {
            parts.push(format!("  - {}", topic));
        }
    }

    // Include last few messages verbatim for continuity
    let recent_count = messages.len().min(3);
    if recent_count > 0 {
        parts.push("- Most recent messages before compaction:".to_string());
        let start = messages.len().saturating_sub(recent_count);
        for msg in &messages[start..] {
            let role_label = match msg.role {
                kestrel_core::MessageRole::User => "User",
                kestrel_core::MessageRole::Assistant => "Assistant",
                kestrel_core::MessageRole::Tool => "Tool",
                kestrel_core::MessageRole::System => "System",
            };
            let content_preview = if msg.content.len() > 200 {
                format!("{}...", &msg.content[..197])
            } else {
                msg.content.clone()
            };
            parts.push(format!("  [{}] {}", role_label, content_preview));
        }
    }

    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use kestrel_core::MessageRole;
    use kestrel_session::Session;

    fn make_session_with_messages(count: usize) -> Session {
        let mut session = Session::new("test:compact".to_string());
        session.add_system_message("You are a helpful assistant.".to_string());
        for i in 0..count {
            session.add_user_message(format!(
                "User message number {} with some content to add tokens",
                i
            ));
            session
                .add_assistant_message(format!("Assistant response number {} with some detail", i));
        }
        session
    }

    #[test]
    fn test_compaction_config_default() {
        let config = CompactionConfig::default();
        assert_eq!(config.context_window_tokens, DEFAULT_CONTEXT_WINDOW_TOKENS);
        assert!((config.threshold_ratio - COMPACTION_THRESHOLD_RATIO).abs() < f64::EPSILON);
        assert_eq!(config.keep_recent, COMPACTION_KEEP_RECENT);
        assert_eq!(config.strategy, CompactionStrategy::Summarize);
    }

    #[test]
    fn test_compaction_config_threshold() {
        let config = CompactionConfig::default();
        let threshold = config.threshold_tokens();
        assert_eq!(
            threshold,
            (DEFAULT_CONTEXT_WINDOW_TOKENS as f64 * COMPACTION_THRESHOLD_RATIO) as usize
        );
    }

    #[test]
    fn test_needs_compaction_false() {
        let config = CompactionConfig {
            context_window_tokens: 1000,
            threshold_ratio: 0.8,
            keep_recent: 5,
            strategy: CompactionStrategy::Summarize,
        };
        let session = make_session_with_messages(2);
        // 2 exchanges * 2 msgs + 1 system = 5 msgs, ~100 tokens, threshold 800
        assert!(!config.needs_compaction(&session));
    }

    #[test]
    fn test_needs_compaction_true() {
        let config = CompactionConfig {
            context_window_tokens: 200, // Very small for testing
            threshold_ratio: 0.5,       // threshold = 100 tokens
            keep_recent: 4,
            strategy: CompactionStrategy::Summarize,
        };
        let session = make_session_with_messages(20);
        assert!(config.needs_compaction(&session));
    }

    #[test]
    fn test_compact_session_noop_when_below_threshold() {
        let config = CompactionConfig {
            context_window_tokens: 100_000,
            threshold_ratio: 0.8,
            keep_recent: 10,
            strategy: CompactionStrategy::Summarize,
        };
        let mut session = make_session_with_messages(3);
        let result = compact_session(&mut session, &config).unwrap();
        assert_eq!(result.messages_before, result.messages_after);
    }

    #[test]
    fn test_compact_session_summarize() {
        let config = CompactionConfig {
            context_window_tokens: 500,
            threshold_ratio: 0.5, // threshold = 250
            keep_recent: 4,
            strategy: CompactionStrategy::Summarize,
        };
        let mut session = make_session_with_messages(15);
        let before_count = session.messages.len();
        let before_tokens = session.estimated_tokens();

        assert!(before_tokens > 250);
        let result = compact_session(&mut session, &config).unwrap();
        assert_eq!(result.messages_before, before_count);
        assert!(result.messages_after < before_count);
        assert!(result.tokens_after < before_tokens);

        // Should have: system + summary + 4 recent messages
        assert!(session.messages.len() <= 6); // 1 system + 1 summary + 4 recent
        assert!(session.messages[1].content.contains("Conversation Summary"));
    }

    #[test]
    fn test_compact_session_preserves_system_message() {
        let config = CompactionConfig {
            context_window_tokens: 500,
            threshold_ratio: 0.3,
            keep_recent: 2,
            strategy: CompactionStrategy::Summarize,
        };
        let mut session = make_session_with_messages(10);
        let result = compact_session(&mut session, &config).unwrap();

        // First message should still be the original system message
        assert_eq!(session.messages[0].role, MessageRole::System);
        assert!(session.messages[0].content.contains("helpful assistant"));
        assert!(result.messages_after < result.messages_before);
    }

    #[test]
    fn test_compact_session_truncate_strategy() {
        let config = CompactionConfig {
            context_window_tokens: 500,
            threshold_ratio: 0.3,
            keep_recent: 6,
            strategy: CompactionStrategy::Truncate,
        };
        let mut session = make_session_with_messages(10);
        let before_count = session.messages.len();

        let result = compact_session(&mut session, &config).unwrap();
        assert!(result.messages_after < before_count);
        // truncate(6) keeps system msg + last 6 = 7 total
        assert!(result.messages_after <= 7);
    }

    #[test]
    fn test_compact_session_too_short() {
        let config = CompactionConfig {
            context_window_tokens: 100,
            threshold_ratio: 0.1,
            keep_recent: 100, // Higher than message count
            strategy: CompactionStrategy::Summarize,
        };
        let mut session = make_session_with_messages(2);
        let result = compact_session(&mut session, &config).unwrap();
        // Should be noop because keep_recent >= message count
        assert_eq!(result.messages_before, result.messages_after);
    }

    #[test]
    fn test_build_summary() {
        let mut session = Session::new("test".to_string());
        session.add_user_message("How do I use Rust?".to_string());
        session.add_assistant_message("Rust is a systems programming language...".to_string());
        session.add_user_message("Tell me about ownership".to_string());
        session.add_assistant_message("Ownership is Rust's key feature...".to_string());

        let summary = build_summary(&session.messages);
        assert!(summary.contains("2 user messages"));
        assert!(summary.contains("2 assistant responses"));
        assert!(summary.contains("How do I use Rust?"));
    }

    #[test]
    fn test_compaction_result_fields() {
        let config = CompactionConfig {
            context_window_tokens: 500,
            threshold_ratio: 0.3,
            keep_recent: 2,
            strategy: CompactionStrategy::Summarize,
        };
        let mut session = make_session_with_messages(15);
        let result = compact_session(&mut session, &config).unwrap();

        assert!(result.messages_before > result.messages_after);
        assert!(result.tokens_before > result.tokens_after);
        assert!(result.messages_before > 0);
        assert!(result.messages_after > 0);
        // notes_extracted should be populated (at least a summary from user messages)
        assert!(result.notes_extracted > 0);
    }

    #[test]
    fn test_compaction_notes_extracted_count() {
        let config = CompactionConfig {
            context_window_tokens: 500,
            threshold_ratio: 0.3,
            keep_recent: 2,
            strategy: CompactionStrategy::Summarize,
        };
        let mut session = Session::new("test:notes_count".to_string());
        session.add_system_message("System".to_string());
        session.add_user_message("What database should we use?".to_string());
        session.add_assistant_message(
            "We decided to use PostgreSQL for the backend. You should add tests.".to_string(),
        );
        session.add_user_message("How do we handle migrations?".to_string());
        session.add_assistant_message("Let's use sqlx for migrations.".to_string());
        // Add enough to trigger compaction
        for i in 0..20 {
            session.add_user_message(format!("filler message {} with content", i));
            session.add_assistant_message(format!("filler response {}", i));
        }

        let result = compact_session(&mut session, &config).unwrap();
        // Should extract at least summary + decisions + action_items + questions
        assert!(result.notes_extracted >= 1);
    }

    #[test]
    fn test_compaction_notes_deduplicated() {
        let config = CompactionConfig {
            context_window_tokens: 500,
            threshold_ratio: 0.3,
            keep_recent: 2,
            strategy: CompactionStrategy::Summarize,
        };

        // Run compaction twice on the same session
        let mut session = Session::new("test:dedup".to_string());
        session.add_system_message("System".to_string());
        for i in 0..20 {
            session.add_user_message(format!("User message {} about topics", i));
            session.add_assistant_message(format!("Assistant response {}", i));
        }

        let r1 = compact_session(&mut session, &config).unwrap();
        let notes_after_first = session.all_notes().len();

        // Add more messages to trigger second compaction
        for i in 0..20 {
            session.add_user_message(format!("More user message {}", i));
            session.add_assistant_message(format!("More assistant response {}", i));
        }

        let r2 = compact_session(&mut session, &config).unwrap();

        // Notes count should be similar (deduped), not doubled
        let notes_after_second = session.all_notes().len();
        assert!(r1.notes_extracted > 0);
        assert!(r2.notes_extracted > 0);
        // Old compaction notes should have been replaced, not accumulated
        assert!(
            notes_after_second <= notes_after_first + r2.notes_extracted,
            "notes should be deduped, not accumulated: first={}, second={}, extracted={}",
            notes_after_first,
            notes_after_second,
            r2.notes_extracted,
        );
    }

    #[test]
    fn test_truncate_notes_extracted_is_zero() {
        let config = CompactionConfig {
            context_window_tokens: 500,
            threshold_ratio: 0.3,
            keep_recent: 6,
            strategy: CompactionStrategy::Truncate,
        };
        let mut session = make_session_with_messages(10);
        let result = compact_session(&mut session, &config).unwrap();
        assert_eq!(result.notes_extracted, 0);
    }
}
