//! Context window budget manager — token-aware allocation and smart pruning.
//!
//! When the context window is full, not all content is equally important.
//! This module provides:
//!
//! - **[`ContextBudget`]**: allocates a fixed token budget across system prompt,
//!   skills, notes, and message history sections.
//! - **[`prune_messages`]**: when history exceeds its allocated budget, intelligently
//!   selects which messages to keep — preferring recent messages and those that
//!   contain tool calls (which anchor tool-result chains).
//!
//! ## Budget allocation
//!
//! The total context window is split into sections with configurable ratios.
//! For example, with a 128k window and the default ratios:
//!
//! | Section | Ratio | Tokens |
//! |---|---|---|
//! | System prompt | 10% | 12 800 |
//! | Skills / tools | 5% | 6 400 |
//! | Notes | 5% | 6 400 |
//! | History | 70% | 89 600 |
//! | Reserved (response) | 10% | 12 800 |

use nanobot_core::{COMPACTION_KEEP_RECENT, DEFAULT_CONTEXT_WINDOW_TOKENS};
use nanobot_session::SessionEntry;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

// ---------------------------------------------------------------------------
// ContextBudgetConfig
// ---------------------------------------------------------------------------

/// Configuration for how the context window budget is divided.
///
/// Ratios are fractions of the total context window. They must sum to ≤ 1.0;
/// the remainder is implicitly reserved for the model's response tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextBudgetConfig {
    /// Total context window in tokens.
    pub total_tokens: usize,
    /// Fraction reserved for the system prompt. Default: 0.10.
    pub system_ratio: f64,
    /// Fraction reserved for skills / tool definitions. Default: 0.05.
    pub skills_ratio: f64,
    /// Fraction reserved for structured notes. Default: 0.05.
    pub notes_ratio: f64,
    /// Fraction reserved for message history. Default: 0.70.
    pub history_ratio: f64,
    /// Number of recent messages that are always preserved during pruning.
    /// Default: 10.
    pub keep_recent: usize,
}

impl Default for ContextBudgetConfig {
    fn default() -> Self {
        Self {
            total_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS,
            system_ratio: 0.10,
            skills_ratio: 0.05,
            notes_ratio: 0.05,
            history_ratio: 0.70,
            keep_recent: COMPACTION_KEEP_RECENT,
        }
    }
}

impl ContextBudgetConfig {
    /// Create a config with a custom total token count and default ratios.
    pub fn with_total_tokens(total_tokens: usize) -> Self {
        Self {
            total_tokens,
            ..Self::default()
        }
    }

    /// Validate that ratios sum to ≤ 1.0.
    pub fn validate(&self) -> bool {
        let total = self.system_ratio + self.skills_ratio + self.notes_ratio + self.history_ratio;
        total <= 1.0 + f64::EPSILON
    }
}

// ---------------------------------------------------------------------------
// BudgetAllocation
// ---------------------------------------------------------------------------

/// The concrete token allocation for each context section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetAllocation {
    /// Tokens available for the system prompt.
    pub system_tokens: usize,
    /// Tokens available for skills / tool definitions.
    pub skills_tokens: usize,
    /// Tokens available for structured notes.
    pub notes_tokens: usize,
    /// Tokens available for message history.
    pub history_tokens: usize,
    /// Tokens reserved for the model response (remaining budget).
    pub reserved_tokens: usize,
    /// Total context window.
    pub total_tokens: usize,
}

// ---------------------------------------------------------------------------
// ContextBudget
// ---------------------------------------------------------------------------

/// Token budget manager for the context window.
///
/// Given a [`ContextBudgetConfig`], computes the token allocation for each
/// section and checks whether a set of messages fits within the history
/// budget.
pub struct ContextBudget {
    config: ContextBudgetConfig,
    allocation: BudgetAllocation,
}

impl std::fmt::Debug for ContextBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextBudget")
            .field("allocation", &self.allocation)
            .finish_non_exhaustive()
    }
}

impl ContextBudget {
    /// Create a new budget manager from the given config.
    ///
    /// Panics if the config ratios are invalid (sum > 1.0).
    pub fn new(config: ContextBudgetConfig) -> Self {
        assert!(config.validate(), "Budget ratios must sum to <= 1.0");
        let allocation = Self::compute_allocation(&config);
        Self { config, allocation }
    }

    /// Create with default ratios and a custom total token count.
    pub fn with_total_tokens(total_tokens: usize) -> Self {
        Self::new(ContextBudgetConfig::with_total_tokens(total_tokens))
    }

    /// Get the computed allocation.
    pub fn allocation(&self) -> &BudgetAllocation {
        &self.allocation
    }

    /// Get the config.
    pub fn config(&self) -> &ContextBudgetConfig {
        &self.config
    }

    /// Compute concrete token allocations from the config ratios.
    fn compute_allocation(config: &ContextBudgetConfig) -> BudgetAllocation {
        let t = config.total_tokens;
        let system_tokens = (t as f64 * config.system_ratio) as usize;
        let skills_tokens = (t as f64 * config.skills_ratio) as usize;
        let notes_tokens = (t as f64 * config.notes_ratio) as usize;
        let history_tokens = (t as f64 * config.history_ratio) as usize;
        let used = system_tokens + skills_tokens + notes_tokens + history_tokens;
        let reserved_tokens = t.saturating_sub(used);
        BudgetAllocation {
            system_tokens,
            skills_tokens,
            notes_tokens,
            history_tokens,
            reserved_tokens,
            total_tokens: t,
        }
    }

    /// Estimate the token count for a string (4 chars ≈ 1 token).
    pub fn estimate_tokens(text: &str) -> usize {
        text.len() / 4
    }

    /// Estimate total tokens for a slice of messages.
    pub fn estimate_messages_tokens(messages: &[SessionEntry]) -> usize {
        messages.iter().map(|m| Self::estimate_tokens(&m.content)).sum()
    }

    /// Check whether the given messages fit within the history budget.
    pub fn fits_history(&self, messages: &[SessionEntry]) -> bool {
        let tokens = Self::estimate_messages_tokens(messages);
        tokens <= self.allocation.history_tokens
    }

    /// Check whether the system prompt fits within the system budget.
    pub fn fits_system(&self, system_prompt: &str) -> bool {
        Self::estimate_tokens(system_prompt) <= self.allocation.system_tokens
    }

    /// Check whether notes text fits within the notes budget.
    pub fn fits_notes(&self, notes_text: &str) -> bool {
        Self::estimate_tokens(notes_text) <= self.allocation.notes_tokens
    }
}

// ---------------------------------------------------------------------------
// PruneResult
// ---------------------------------------------------------------------------

/// Result of pruning messages to fit within the history budget.
#[derive(Debug, Clone)]
pub struct PruneResult {
    /// Messages that were kept.
    pub kept: Vec<SessionEntry>,
    /// Messages that were removed.
    pub removed: Vec<SessionEntry>,
    /// Estimated tokens before pruning.
    pub tokens_before: usize,
    /// Estimated tokens after pruning.
    pub tokens_after: usize,
}

// ---------------------------------------------------------------------------
// prune_messages
// ---------------------------------------------------------------------------

/// Prune messages to fit within a token budget using smart selection.
///
/// Strategy (in priority order):
/// 1. **Always keep the first system message** (if present).
/// 2. **Keep the most recent `keep_recent` messages** — these are the most
///    relevant to the ongoing conversation.
/// 3. **Keep messages that contain tool calls** (`tool_calls.is_some()`) —
///    these anchor tool-result chains. Dropping a tool-call message without
///    also dropping its corresponding tool-result would create an orphan.
/// 4. **Keep remaining messages** from newest to oldest until the budget is
///    met, dropping the oldest first.
///
/// Returns a [`PruneResult`] describing what was kept and removed.
pub fn prune_messages(
    messages: &[SessionEntry],
    budget_tokens: usize,
    keep_recent: usize,
) -> PruneResult {
    let tokens_before = ContextBudget::estimate_messages_tokens(messages);

    if tokens_before <= budget_tokens {
        return PruneResult {
            kept: messages.to_vec(),
            removed: Vec::new(),
            tokens_before,
            tokens_after: tokens_before,
        };
    }

    let total = messages.len();

    // Step 1: identify the leading system message (always preserved)
    let system_msg = messages.first().and_then(|m| {
        if m.role == nanobot_core::MessageRole::System {
            Some(m.clone())
        } else {
            None
        }
    });
    let start_idx = if system_msg.is_some() { 1 } else { 0 };

    // Step 2: collect recent messages (always kept)
    let recent_start = total.saturating_sub(keep_recent).max(start_idx);
    let recent_messages: Vec<SessionEntry> =
        messages[recent_start..].to_vec();

    // Step 3: collect messages with tool_calls from the older range
    let mut tool_call_messages: Vec<SessionEntry> = Vec::new();
    let mut remaining_older: Vec<SessionEntry> = Vec::new();

    for msg in &messages[start_idx..recent_start] {
        if msg.tool_calls.is_some() {
            tool_call_messages.push(msg.clone());
        } else {
            remaining_older.push(msg.clone());
        }
    }

    // Step 4: build the kept set — system + tool_calls + recent
    let mut kept: Vec<SessionEntry> = Vec::new();
    if let Some(sys) = &system_msg {
        kept.push(sys.clone());
    }
    kept.extend(tool_call_messages.clone());

    // Check if we already exceed budget with system + tool_calls + recent
    let recent_tokens = ContextBudget::estimate_messages_tokens(&recent_messages);
    let kept_so_far_tokens = ContextBudget::estimate_messages_tokens(&kept);

    if kept_so_far_tokens + recent_tokens <= budget_tokens {
        // We can keep all tool-call messages + all recent
        kept.extend(recent_messages);
    } else {
        // Even tool_calls + recent exceed budget — drop tool_call_messages,
        // keep only recent
        debug!(
            "Tool-call messages + recent exceed budget ({} + {} > {}), dropping tool-call anchors",
            kept_so_far_tokens, recent_tokens, budget_tokens
        );
        kept.truncate(if system_msg.is_some() { 1 } else { 0 });
        kept.extend(recent_messages);
    }

    // Step 5: fill remaining budget with older messages (newest first)
    let mut current_tokens = ContextBudget::estimate_messages_tokens(&kept);
    let mut added_from_older = Vec::new();
    for msg in remaining_older.iter().rev() {
        let msg_tokens = ContextBudget::estimate_tokens(&msg.content);
        if current_tokens + msg_tokens <= budget_tokens {
            added_from_older.push(msg.clone());
            current_tokens += msg_tokens;
        }
    }
    // Reverse to maintain chronological order, then insert before recent
    added_from_older.reverse();

    // Rebuild kept: system + older_fill + tool_calls + recent
    // Actually, let's rebuild cleanly: system, then chronological order
    let system_msg_opt = kept.first().and_then(|m| {
        if m.role == nanobot_core::MessageRole::System {
            Some(m.clone())
        } else {
            None
        }
    });
    let recent_from_kept: Vec<SessionEntry> = if system_msg_opt.is_some() {
        kept[1..].to_vec()
    } else {
        kept.clone()
    };

    // Merge older_fill into the kept set chronologically
    let mut merged: Vec<SessionEntry> = Vec::new();
    if let Some(sys) = system_msg_opt {
        merged.push(sys);
    }
    // Add older fill first (chronological), then tool_calls + recent
    merged.extend(added_from_older);
    merged.extend(recent_from_kept);

    // Compute removed
    let kept_ids: Vec<(String, String)> = merged
        .iter()
        .map(|m| (m.content.clone(), format!("{:?}", m.role)))
        .collect();
    let removed: Vec<SessionEntry> = messages
        .iter()
        .filter(|m| {
            !kept_ids
                .iter()
                .any(|(content, role)| content == &m.content && role == &format!("{:?}", m.role))
        })
        .cloned()
        .collect();

    let tokens_after = ContextBudget::estimate_messages_tokens(&merged);

    info!(
        "Pruned messages: {} → {} ({} → {} tokens, {} removed)",
        total,
        merged.len(),
        tokens_before,
        tokens_after,
        removed.len()
    );

    PruneResult {
        kept: merged,
        removed,
        tokens_before,
        tokens_after,
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use nanobot_core::MessageRole;

    // ─── Helper ────────────────────────────────────────────────

    fn make_entry(role: MessageRole, content: &str) -> SessionEntry {
        SessionEntry {
            role,
            content: content.to_string(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            timestamp: None,
        }
    }

    fn make_entry_with_tool_call(role: MessageRole, content: &str, tool_name: &str) -> SessionEntry {
        SessionEntry {
            role,
            content: content.to_string(),
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![nanobot_core::ToolCall {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: nanobot_core::FunctionCall {
                    name: tool_name.to_string(),
                    arguments: "{}".to_string(),
                },
            }]),
            timestamp: None,
        }
    }

    // ─── ContextBudgetConfig ───────────────────────────────────

    #[test]
    fn test_config_default() {
        let config = ContextBudgetConfig::default();
        assert_eq!(config.total_tokens, DEFAULT_CONTEXT_WINDOW_TOKENS);
        assert!((config.system_ratio - 0.10).abs() < f64::EPSILON);
        assert!((config.skills_ratio - 0.05).abs() < f64::EPSILON);
        assert!((config.notes_ratio - 0.05).abs() < f64::EPSILON);
        assert!((config.history_ratio - 0.70).abs() < f64::EPSILON);
        assert_eq!(config.keep_recent, COMPACTION_KEEP_RECENT);
    }

    #[test]
    fn test_config_validate_valid() {
        let config = ContextBudgetConfig::default();
        assert!(config.validate());
    }

    #[test]
    fn test_config_validate_ratios_sum_exactly_one() {
        let config = ContextBudgetConfig {
            total_tokens: 1000,
            system_ratio: 0.10,
            skills_ratio: 0.10,
            notes_ratio: 0.10,
            history_ratio: 0.70,
            keep_recent: 5,
        };
        assert!(config.validate());
    }

    #[test]
    fn test_config_validate_ratios_exceed_one() {
        let config = ContextBudgetConfig {
            total_tokens: 1000,
            system_ratio: 0.30,
            skills_ratio: 0.30,
            notes_ratio: 0.30,
            history_ratio: 0.30,
            keep_recent: 5,
        };
        assert!(!config.validate()); // 1.2 > 1.0
    }

    #[test]
    fn test_config_with_total_tokens() {
        let config = ContextBudgetConfig::with_total_tokens(4096);
        assert_eq!(config.total_tokens, 4096);
        assert_eq!(config.keep_recent, COMPACTION_KEEP_RECENT);
    }

    #[test]
    fn test_config_serde_roundtrip() {
        let config = ContextBudgetConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: ContextBudgetConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total_tokens, config.total_tokens);
        assert!((back.system_ratio - config.system_ratio).abs() < f64::EPSILON);
        assert_eq!(back.keep_recent, config.keep_recent);
    }

    // ─── BudgetAllocation ──────────────────────────────────────

    #[test]
    fn test_allocation_default_128k() {
        let budget = ContextBudget::new(ContextBudgetConfig::default());
        let alloc = budget.allocation();

        assert_eq!(alloc.total_tokens, DEFAULT_CONTEXT_WINDOW_TOKENS);
        // 128000 * 0.10 = 12800
        assert_eq!(alloc.system_tokens, 12800);
        // 128000 * 0.05 = 6400
        assert_eq!(alloc.skills_tokens, 6400);
        assert_eq!(alloc.notes_tokens, 6400);
        // 128000 * 0.70 = 89600
        assert_eq!(alloc.history_tokens, 89600);
        // reserved = 128000 - (12800+6400+6400+89600) = 12800
        assert_eq!(alloc.reserved_tokens, 12800);
    }

    #[test]
    fn test_allocation_custom() {
        let budget = ContextBudget::new(ContextBudgetConfig {
            total_tokens: 1000,
            system_ratio: 0.20,
            skills_ratio: 0.10,
            notes_ratio: 0.10,
            history_ratio: 0.50,
            keep_recent: 5,
        });
        let alloc = budget.allocation();
        assert_eq!(alloc.system_tokens, 200);
        assert_eq!(alloc.skills_tokens, 100);
        assert_eq!(alloc.notes_tokens, 100);
        assert_eq!(alloc.history_tokens, 500);
        assert_eq!(alloc.reserved_tokens, 100);
    }

    #[test]
    fn test_allocation_no_reserved() {
        let budget = ContextBudget::new(ContextBudgetConfig {
            total_tokens: 100,
            system_ratio: 0.25,
            skills_ratio: 0.25,
            notes_ratio: 0.25,
            history_ratio: 0.25,
            keep_recent: 5,
        });
        let alloc = budget.allocation();
        assert_eq!(alloc.reserved_tokens, 0);
    }

    // ─── Token estimation ──────────────────────────────────────

    #[test]
    fn test_estimate_tokens_empty() {
        assert_eq!(ContextBudget::estimate_tokens(""), 0);
    }

    #[test]
    fn test_estimate_tokens_short() {
        // "hello" = 5 chars → 1 token (5/4 = 1)
        assert_eq!(ContextBudget::estimate_tokens("hello"), 1);
    }

    #[test]
    fn test_estimate_tokens_long() {
        // 40 chars → 10 tokens
        let text = "a".repeat(40);
        assert_eq!(ContextBudget::estimate_tokens(&text), 10);
    }

    #[test]
    fn test_estimate_messages_tokens() {
        let messages = vec![
            make_entry(MessageRole::User, &"a".repeat(40)),  // 10 tokens
            make_entry(MessageRole::Assistant, &"b".repeat(80)), // 20 tokens
        ];
        assert_eq!(ContextBudget::estimate_messages_tokens(&messages), 30);
    }

    // ─── fits_* checks ────────────────────────────────────────

    #[test]
    fn test_fits_history_within_budget() {
        let budget = ContextBudget::new(ContextBudgetConfig {
            total_tokens: 1000,
            system_ratio: 0.10,
            skills_ratio: 0.10,
            notes_ratio: 0.10,
            history_ratio: 0.50,
            keep_recent: 5,
        });
        // history budget = 500 tokens → need 2000 chars to fill
        let messages = vec![
            make_entry(MessageRole::User, &"x".repeat(2000)), // 500 tokens
        ];
        assert!(budget.fits_history(&messages));
    }

    #[test]
    fn test_fits_history_exceeds_budget() {
        let budget = ContextBudget::new(ContextBudgetConfig {
            total_tokens: 1000,
            system_ratio: 0.10,
            skills_ratio: 0.10,
            notes_ratio: 0.10,
            history_ratio: 0.50,
            keep_recent: 5,
        });
        let messages = vec![
            make_entry(MessageRole::User, &"x".repeat(4000)), // 1000 tokens > 500
        ];
        assert!(!budget.fits_history(&messages));
    }

    #[test]
    fn test_fits_system() {
        let budget = ContextBudget::new(ContextBudgetConfig {
            total_tokens: 100,
            system_ratio: 0.20, // 20 tokens → 80 chars
            skills_ratio: 0.20,
            notes_ratio: 0.20,
            history_ratio: 0.20,
            keep_recent: 5,
        });
        assert!(budget.fits_system(&"a".repeat(80)));
        assert!(!budget.fits_system(&"a".repeat(84)));
    }

    #[test]
    fn test_fits_notes() {
        let budget = ContextBudget::new(ContextBudgetConfig {
            total_tokens: 100,
            system_ratio: 0.20,
            skills_ratio: 0.20,
            notes_ratio: 0.20, // 20 tokens → 80 chars
            history_ratio: 0.20,
            keep_recent: 5,
        });
        assert!(budget.fits_notes(&"a".repeat(80)));
        assert!(!budget.fits_notes(&"a".repeat(84)));
    }

    // ─── prune_messages: under budget (no-op) ──────────────────

    #[test]
    fn test_prune_under_budget_returns_all() {
        let messages = vec![
            make_entry(MessageRole::System, "system"),
            make_entry(MessageRole::User, "hello"),
            make_entry(MessageRole::Assistant, "hi"),
        ];
        let result = prune_messages(&messages, 1000, 2);
        assert_eq!(result.kept.len(), 3);
        assert!(result.removed.is_empty());
        assert_eq!(result.tokens_before, result.tokens_after);
    }

    // ─── prune_messages: preserves system message ───────────────

    #[test]
    fn test_prune_preserves_system_message() {
        let messages = vec![
            make_entry(MessageRole::System, "system prompt"),
            make_entry(MessageRole::User, &"a".repeat(100)),   // 25 tokens
            make_entry(MessageRole::User, &"b".repeat(100)),   // 25 tokens
            make_entry(MessageRole::User, &"c".repeat(100)),   // 25 tokens
            make_entry(MessageRole::User, &"d".repeat(100)),   // 25 tokens
        ];
        // Budget only fits system + 1 message
        let result = prune_messages(&messages, 30, 1);
        assert_eq!(result.kept[0].role, MessageRole::System);
        assert!(result.kept[0].content.contains("system prompt"));
    }

    // ─── prune_messages: keeps recent messages ──────────────────

    #[test]
    fn test_prune_keeps_recent_messages() {
        let messages: Vec<SessionEntry> = (0..10)
            .map(|i| make_entry(MessageRole::User, &format!("message {} with enough text to add tokens abcdefghijklmnop", i)))
            .collect();

        // Budget fits only 3 messages, keep_recent=2
        let result = prune_messages(&messages, 30, 2);

        // Should keep the last 2 messages
        let last_content = &messages[9].content;
        let second_last = &messages[8].content;
        assert!(result.kept.iter().any(|m| m.content == *last_content));
        assert!(result.kept.iter().any(|m| m.content == *second_last));
    }

    // ─── prune_messages: preserves tool_call messages ───────────

    #[test]
    fn test_prune_preserves_tool_call_messages() {
        let messages = vec![
            make_entry(MessageRole::System, "system"),
            make_entry(MessageRole::User, &"a".repeat(40)),          // 10 tokens
            make_entry_with_tool_call(MessageRole::Assistant, "calling tool", "search"), // ~3 tokens
            make_entry(MessageRole::Tool, "tool result"),            // ~3 tokens
            make_entry(MessageRole::User, &"e".repeat(40)),          // 10 tokens
            make_entry(MessageRole::Assistant, "final response"),    // ~4 tokens
        ];

        // Budget: only ~20 tokens, keep_recent=2
        // The tool_call message should be preserved even though it's older
        let result = prune_messages(&messages, 20, 2);

        let has_tool_call = result.kept.iter().any(|m| m.tool_calls.is_some());
        assert!(has_tool_call, "Tool-call messages should be preserved during pruning");
    }

    #[test]
    fn test_prune_tool_calls_dropped_if_budget_too_small() {
        let messages = vec![
            make_entry(MessageRole::System, "system"),
            make_entry_with_tool_call(
                MessageRole::Assistant,
                &"x".repeat(200),  // 50 tokens — too large
                "big_tool",
            ),
            make_entry(MessageRole::User, "recent"),  // ~2 tokens
        ];

        // Budget too small for tool_call, keep_recent=1
        let result = prune_messages(&messages, 5, 1);

        let has_tool_call = result.kept.iter().any(|m| m.tool_calls.is_some());
        assert!(!has_tool_call, "Tool-call should be dropped when budget is too small");
    }

    // ─── prune_messages: result correctness ─────────────────────

    #[test]
    fn test_prune_tokens_after_within_budget() {
        let messages: Vec<SessionEntry> = (0..20)
            .map(|i| make_entry(MessageRole::User, &format!("{:040}", i)))
            .collect();

        let budget = 50; // ~50 tokens
        let result = prune_messages(&messages, budget, 5);

        assert!(
            result.tokens_after <= budget,
            "tokens_after ({}) should be <= budget ({})",
            result.tokens_after,
            budget
        );
    }

    #[test]
    fn test_prune_removed_count() {
        let messages: Vec<SessionEntry> = (0..10)
            .map(|i| make_entry(MessageRole::User, &format!("{:040}", i)))
            .collect();

        let budget = 30;
        let result = prune_messages(&messages, budget, 2);

        assert_eq!(
            result.kept.len() + result.removed.len(),
            messages.len(),
            "kept + removed should equal original count"
        );
    }

    #[test]
    fn test_prune_empty_messages() {
        let result = prune_messages(&[], 100, 5);
        assert!(result.kept.is_empty());
        assert!(result.removed.is_empty());
        assert_eq!(result.tokens_before, 0);
    }

    #[test]
    fn test_prune_single_message_under_budget() {
        let messages = vec![make_entry(MessageRole::User, "short")];
        let result = prune_messages(&messages, 100, 5);
        assert_eq!(result.kept.len(), 1);
        assert!(result.removed.is_empty());
    }

    #[test]
    fn test_prune_kept_preserves_order() {
        let messages = vec![
            make_entry(MessageRole::System, "sys"),
            make_entry(MessageRole::User, "msg 1"),
            make_entry(MessageRole::Assistant, "resp 1"),
            make_entry(MessageRole::User, "msg 2"),
            make_entry(MessageRole::Assistant, "resp 2"),
        ];
        let result = prune_messages(&messages, 100, 10);

        // All kept, order preserved
        assert_eq!(result.kept.len(), 5);
        assert_eq!(result.kept[0].content, "sys");
        assert_eq!(result.kept[4].content, "resp 2");
    }

    #[test]
    fn test_prune_no_system_message() {
        let messages = vec![
            make_entry(MessageRole::User, &"a".repeat(40)),
            make_entry(MessageRole::User, &"b".repeat(40)),
            make_entry(MessageRole::User, &"c".repeat(40)),
        ];
        let result = prune_messages(&messages, 20, 1);
        // No system message, should still work
        assert!(result.kept.iter().all(|m| m.role != MessageRole::System));
        assert!(result.tokens_after <= 20);
    }

    #[test]
    fn test_prune_all_messages_same_size() {
        // 10 messages, each ~10 tokens, budget 50 tokens, keep 3 recent
        let messages: Vec<SessionEntry> = (0..10)
            .map(|i| make_entry(MessageRole::User, &format!("msg_{:036}", i)))
            .collect();

        let result = prune_messages(&messages, 50, 3);
        assert!(result.tokens_after <= 50);
        assert!(result.removed.len() > 0);

        // Last 3 messages should be in kept
        for i in 7..10 {
            let content = format!("msg_{:036}", i);
            assert!(
                result.kept.iter().any(|m| m.content == content),
                "Message {} should be kept",
                i
            );
        }
    }

    // ─── Integration: ContextBudget + prune_messages ────────────

    #[test]
    fn test_budget_and_prune_integration() {
        let budget = ContextBudget::new(ContextBudgetConfig {
            total_tokens: 1000,
            system_ratio: 0.10,
            skills_ratio: 0.10,
            notes_ratio: 0.10,
            history_ratio: 0.50,
            keep_recent: 3,
        });

        let alloc = budget.allocation();
        assert_eq!(alloc.history_tokens, 500);

        // Create messages that exceed history budget
        let messages: Vec<SessionEntry> = (0..55)
            .map(|i| make_entry(MessageRole::User, &format!("{:040}", i))) // 10 tokens each
            .collect();

        assert!(!budget.fits_history(&messages)); // 550 tokens > 500 budget

        let result = prune_messages(&messages, alloc.history_tokens, 3);
        assert!(result.tokens_after <= alloc.history_tokens);
        assert!(result.kept.len() < messages.len());
    }

    #[test]
    fn test_debug_impl_context_budget() {
        let budget = ContextBudget::with_total_tokens(4096);
        let debug_str = format!("{:?}", budget);
        assert!(debug_str.contains("ContextBudget"));
        assert!(debug_str.contains("allocation"));
    }
}
