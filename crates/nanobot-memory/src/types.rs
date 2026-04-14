//! Core types for the memory system.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Unique identifier for a memory entry (UUID v4 string).
pub type EntryId = String;

/// Classification of memory content.
///
/// Categories determine how memory entries are organized and retrieved.
/// Higher-level categories (UserProfile, AgentNote, Fact, Preference) are the
/// primary classification used by the agent. Extended categories provide
/// finer-grained filtering for specialized queries.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCategory {
    /// User profile information (name, role, preferences).
    UserProfile,
    /// Agent's personal notes and observations.
    AgentNote,
    /// Factual knowledge about the environment.
    Fact,
    /// User or system preferences.
    Preference,
    /// Environment facts (OS, tools, project structure).
    Environment,
    /// Project conventions (code style, deployment).
    ProjectConvention,
    /// Tool discoveries and usage tips.
    ToolDiscovery,
    /// Lessons learned from errors.
    ErrorLesson,
    /// Workflow patterns and habits.
    WorkflowPattern,
}

impl std::fmt::Display for MemoryCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UserProfile => write!(f, "user_profile"),
            Self::AgentNote => write!(f, "agent_note"),
            Self::Fact => write!(f, "fact"),
            Self::Preference => write!(f, "preference"),
            Self::Environment => write!(f, "environment"),
            Self::ProjectConvention => write!(f, "project_convention"),
            Self::ToolDiscovery => write!(f, "tool_discovery"),
            Self::ErrorLesson => write!(f, "error_lesson"),
            Self::WorkflowPattern => write!(f, "workflow_pattern"),
        }
    }
}

/// A single memory entry with metadata and optional embedding vector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    /// Unique identifier (UUID v4).
    pub id: EntryId,
    /// Text content of the memory.
    pub content: String,
    /// Classification category.
    pub category: MemoryCategory,
    /// Confidence score (0.0–1.0). Higher means more trusted.
    pub confidence: f64,
    /// Timestamp when the entry was created.
    pub created_at: DateTime<Utc>,
    /// Timestamp when the entry was last updated.
    pub updated_at: DateTime<Utc>,
    /// Number of times this entry has been accessed via recall.
    pub access_count: u32,
    /// Optional embedding vector for semantic search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
}

impl MemoryEntry {
    /// Create a new memory entry with auto-generated ID and timestamps.
    pub fn new(content: impl Into<String>, category: MemoryCategory) -> Self {
        let now = Utc::now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            content: content.into(),
            category,
            confidence: 1.0,
            created_at: now,
            updated_at: now,
            access_count: 0,
            embedding: None,
        }
    }

    /// Set the confidence score (clamped to 0.0–1.0).
    pub fn with_confidence(mut self, confidence: f64) -> Self {
        self.confidence = confidence.clamp(0.0, 1.0);
        self
    }

    /// Set the embedding vector.
    pub fn with_embedding(mut self, embedding: Vec<f32>) -> Self {
        self.embedding = Some(embedding);
        self
    }

    /// Record an access and update the timestamp.
    pub fn touch(&mut self) {
        self.access_count += 1;
        self.updated_at = Utc::now();
    }
}

/// A memory entry paired with a relevance score from search.
#[derive(Debug, Clone)]
pub struct ScoredEntry {
    /// The memory entry.
    pub entry: MemoryEntry,
    /// Relevance score (higher = more relevant).
    pub score: f64,
}

/// Query parameters for searching memories.
#[derive(Debug, Clone, Default)]
pub struct MemoryQuery {
    /// Full-text search pattern (case-insensitive substring match).
    pub text: Option<String>,
    /// Filter by category.
    pub category: Option<MemoryCategory>,
    /// Filter by minimum confidence (0.0–1.0).
    pub min_confidence: Option<f64>,
    /// Semantic search embedding vector for KNN search.
    pub embedding: Option<Vec<f32>>,
    /// Maximum number of results to return.
    pub limit: usize,
}

impl MemoryQuery {
    /// Create a new query with a default limit of 10.
    pub fn new() -> Self {
        Self {
            limit: 10,
            ..Default::default()
        }
    }

    /// Set the text search pattern.
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    /// Filter by category.
    pub fn with_category(mut self, category: MemoryCategory) -> Self {
        self.category = Some(category);
        self
    }

    /// Set minimum confidence threshold.
    pub fn with_min_confidence(mut self, confidence: f64) -> Self {
        self.min_confidence = Some(confidence);
        self
    }

    /// Set the embedding vector for semantic search.
    pub fn with_embedding(mut self, embedding: Vec<f32>) -> Self {
        self.embedding = Some(embedding);
        self
    }

    /// Set maximum number of results.
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entry_new() {
        let entry = MemoryEntry::new("test content", MemoryCategory::Fact);
        assert!(!entry.id.is_empty());
        assert_eq!(entry.content, "test content");
        assert_eq!(entry.category, MemoryCategory::Fact);
        assert_eq!(entry.confidence, 1.0);
        assert_eq!(entry.access_count, 0);
        assert!(entry.embedding.is_none());
        assert_eq!(entry.created_at, entry.updated_at);
    }

    #[test]
    fn test_entry_with_confidence_clamps() {
        let entry = MemoryEntry::new("x", MemoryCategory::AgentNote)
            .with_confidence(1.5);
        assert!((entry.confidence - 1.0).abs() < f64::EPSILON);

        let entry = MemoryEntry::new("x", MemoryCategory::AgentNote)
            .with_confidence(-0.5);
        assert!(entry.confidence.abs() < f64::EPSILON);

        let entry = MemoryEntry::new("x", MemoryCategory::AgentNote)
            .with_confidence(0.7);
        assert!((entry.confidence - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn test_entry_with_embedding() {
        let entry = MemoryEntry::new("x", MemoryCategory::Fact)
            .with_embedding(vec![0.1, 0.2, 0.3]);
        assert_eq!(entry.embedding.as_deref(), Some([0.1, 0.2, 0.3].as_slice()));
    }

    #[test]
    fn test_entry_touch() {
        let mut entry = MemoryEntry::new("x", MemoryCategory::Fact);
        assert_eq!(entry.access_count, 0);
        entry.touch();
        assert_eq!(entry.access_count, 1);
        entry.touch();
        assert_eq!(entry.access_count, 2);
    }

    #[test]
    fn test_entry_serde_roundtrip() {
        let entry = MemoryEntry::new("serde test", MemoryCategory::UserProfile)
            .with_confidence(0.85)
            .with_embedding(vec![1.0, 2.0, 3.0]);

        let json = serde_json::to_string(&entry).unwrap();
        let back: MemoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry.id, back.id);
        assert_eq!(entry.content, back.content);
        assert_eq!(entry.category, back.category);
        assert!((entry.confidence - back.confidence).abs() < f64::EPSILON);
        assert_eq!(entry.embedding, back.embedding);
    }

    #[test]
    fn test_category_serde_roundtrip() {
        let categories = [
            MemoryCategory::UserProfile,
            MemoryCategory::AgentNote,
            MemoryCategory::Fact,
            MemoryCategory::Preference,
            MemoryCategory::Environment,
            MemoryCategory::ProjectConvention,
            MemoryCategory::ToolDiscovery,
            MemoryCategory::ErrorLesson,
            MemoryCategory::WorkflowPattern,
        ];
        for cat in categories {
            let json = serde_json::to_string(&cat).unwrap();
            let back: MemoryCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(cat, back);
        }
    }

    #[test]
    fn test_category_display() {
        assert_eq!(MemoryCategory::UserProfile.to_string(), "user_profile");
        assert_eq!(MemoryCategory::AgentNote.to_string(), "agent_note");
        assert_eq!(MemoryCategory::Fact.to_string(), "fact");
        assert_eq!(MemoryCategory::Preference.to_string(), "preference");
    }

    #[test]
    fn test_query_builder() {
        let query = MemoryQuery::new()
            .with_text("rust")
            .with_category(MemoryCategory::Fact)
            .with_min_confidence(0.5)
            .with_embedding(vec![0.1, 0.2])
            .with_limit(5);

        assert_eq!(query.text.as_deref(), Some("rust"));
        assert_eq!(query.category, Some(MemoryCategory::Fact));
        assert_eq!(query.min_confidence, Some(0.5));
        assert_eq!(query.embedding.as_deref(), Some([0.1_f32, 0.2_f32].as_slice()));
        assert_eq!(query.limit, 5);
    }

    #[test]
    fn test_query_default_limit() {
        let query = MemoryQuery::new();
        assert_eq!(query.limit, 10);
        assert!(query.text.is_none());
        assert!(query.category.is_none());
    }

    #[test]
    fn test_scored_entry() {
        let entry = MemoryEntry::new("scored", MemoryCategory::Fact);
        let scored = ScoredEntry {
            entry: entry.clone(),
            score: 0.95,
        };
        assert_eq!(scored.entry.id, entry.id);
        assert!((scored.score - 0.95).abs() < f64::EPSILON);
    }
}
