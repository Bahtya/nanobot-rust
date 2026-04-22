//! Shared text search utilities for memory stores.
//!
//! Provides word-boundary-aware text matching that avoids false positives
//! from naive substring matching (e.g., "rust" matching "trust").

use crate::types::{MemoryEntry, MemoryQuery};

/// Check if a content string matches a query term with word-boundary awareness.
///
/// Both the content and query are tokenized by splitting on non-alphanumeric
/// characters. The query matches if every query token is a case-insensitive
/// prefix of at least one content token.
///
/// This avoids false positives like "rust" matching "trust" or "thruster"
/// while still allowing useful matches like "rust" matching "Rust" or
/// "rust-lang".
pub fn text_matches(content: &str, query: &str) -> bool {
    let content_lower = content.to_lowercase();
    let query_lower = query.to_lowercase();

    let content_tokens = tokenize(&content_lower);
    let query_tokens = tokenize(&query_lower);

    if query_tokens.is_empty() {
        return true;
    }

    query_tokens
        .iter()
        .all(|q| content_tokens.iter().any(|c| c.starts_with(q.as_str())))
}

/// Split a string into tokens on non-alphanumeric boundaries.
fn tokenize(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| part.to_string())
        .collect()
}

/// Check if an entry matches the filter criteria in a query.
pub fn matches_filters(entry: &MemoryEntry, query: &MemoryQuery) -> bool {
    if let Some(ref cat) = query.category {
        if entry.category != *cat {
            return false;
        }
    }
    if let Some(min_conf) = query.min_confidence {
        if entry.confidence < min_conf {
            return false;
        }
    }
    if let Some(ref text) = query.text {
        if !text_matches(&entry.content, text) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryCategory;

    // -- text_matches precision tests --

    #[test]
    fn test_exact_word_match() {
        assert!(text_matches("I love Rust programming", "rust"));
    }

    #[test]
    fn test_case_insensitive() {
        assert!(text_matches("Rust programming", "RUST"));
        assert!(text_matches("rust programming", "Rust"));
    }

    #[test]
    fn test_word_prefix_match() {
        assert!(text_matches("Rust programming", "prog"));
    }

    #[test]
    fn test_false_positive_trust() {
        assert!(!text_matches("trust in the system", "rust"));
    }

    #[test]
    fn test_false_positive_thruster() {
        assert!(!text_matches("thruster module", "rust"));
    }

    #[test]
    fn test_false_positive_busting() {
        assert!(!text_matches("cache busting technique", "rust"));
    }

    #[test]
    fn test_hyphenated_word() {
        assert!(text_matches("rust-lang is great", "rust"));
    }

    #[test]
    fn test_underscore_boundary() {
        assert!(text_matches("rust_compiler config", "rust"));
    }

    #[test]
    fn test_punctuation_boundary() {
        assert!(text_matches("I use Rust.", "rust"));
        assert!(text_matches("Rust, Python, Go", "rust"));
        assert!(text_matches("(Rust)", "rust"));
    }

    #[test]
    fn test_multi_word_query_all_must_match() {
        assert!(text_matches("Rust programming language", "rust prog"));
        assert!(!text_matches("Rust programming language", "rust python"));
    }

    #[test]
    fn test_empty_query_matches_everything() {
        assert!(text_matches("anything", ""));
    }

    #[test]
    fn test_no_content_no_match() {
        assert!(!text_matches("", "rust"));
    }

    #[test]
    fn test_query_longer_than_content_token() {
        assert!(!text_matches("rust", "rusting"));
    }

    #[test]
    fn test_numeric_tokens() {
        assert!(text_matches("version 2.0 release", "2"));
        assert!(text_matches("version 2.0 release", "2.0"));
    }

    // -- matches_filters tests --

    #[test]
    fn test_filters_category_match() {
        let entry = MemoryEntry::new("test", MemoryCategory::Fact);
        let query = MemoryQuery::new().with_category(MemoryCategory::Fact);
        assert!(matches_filters(&entry, &query));
    }

    #[test]
    fn test_filters_category_mismatch() {
        let entry = MemoryEntry::new("test", MemoryCategory::Fact);
        let query = MemoryQuery::new().with_category(MemoryCategory::AgentNote);
        assert!(!matches_filters(&entry, &query));
    }

    #[test]
    fn test_filters_confidence() {
        let entry = MemoryEntry::new("test", MemoryCategory::Fact).with_confidence(0.3);
        let query = MemoryQuery::new().with_min_confidence(0.5);
        assert!(!matches_filters(&entry, &query));
    }

    #[test]
    fn test_filters_text_precision() {
        let entry = MemoryEntry::new("trust in the system", MemoryCategory::Fact);
        let query = MemoryQuery::new().with_text("rust");
        assert!(!matches_filters(&entry, &query));
    }

    #[test]
    fn test_filters_text_match() {
        let entry = MemoryEntry::new("Rust programming", MemoryCategory::Fact);
        let query = MemoryQuery::new().with_text("rust");
        assert!(matches_filters(&entry, &query));
    }

    #[test]
    fn test_filters_no_constraints() {
        let entry = MemoryEntry::new("anything", MemoryCategory::Fact);
        let query = MemoryQuery::new();
        assert!(matches_filters(&entry, &query));
    }
}
