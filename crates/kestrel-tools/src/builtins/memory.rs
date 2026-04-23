//! Memory tools: `store_memory` and `recall_memory`.
//!
//! These tools let the LLM actively store and retrieve memories via the
//! [`MemoryStore`] trait. Full-text search is handled by tantivy with jieba
//! CJK tokenization — no embedding vectors needed.

use async_trait::async_trait;
use kestrel_memory::{MemoryCategory, MemoryEntry, MemoryQuery, MemoryStore};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::trait_def::{Tool, ToolError};

// ─── store_memory ────────────────────────────────────────────────

/// Tool for storing a memory entry that the LLM can later recall.
///
/// The LLM supplies the content, category, and optional confidence.
/// The content is indexed by tantivy with jieba CJK tokenization.
pub struct StoreMemoryTool {
    store: Arc<dyn MemoryStore>,
}

impl StoreMemoryTool {
    /// Create a new store_memory tool backed by the given store.
    pub fn new(store: Arc<dyn MemoryStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for StoreMemoryTool {
    fn name(&self) -> &str {
        "store_memory"
    }

    fn description(&self) -> &str {
        "Store a piece of information in long-term memory for later recall. \
         Use this to remember facts about the user, project conventions, \
         lessons learned, or any knowledge worth persisting across conversations."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The information to store."
                },
                "category": {
                    "type": "string",
                    "enum": [
                        "user_profile", "agent_note", "fact", "preference",
                        "environment", "project_convention", "tool_discovery",
                        "error_lesson", "workflow_pattern", "critical"
                    ],
                    "description": "Category for the memory entry."
                },
                "confidence": {
                    "type": "number",
                    "description": "Confidence score from 0.0 to 1.0 (default 1.0).",
                    "minimum": 0.0,
                    "maximum": 1.0
                }
            },
            "required": ["content", "category"]
        })
    }

    fn is_mutating(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let content = args["content"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("missing or invalid 'content' field".into()))?;

        if content.trim().is_empty() {
            return Err(ToolError::Validation("content must not be empty".into()));
        }

        let content = content.to_string();

        let category_str = args["category"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("missing or invalid 'category' field".into()))?;

        let category = parse_category(category_str)?;

        let confidence = match args.get("confidence") {
            Some(v) if !v.is_null() => v.as_f64().ok_or_else(|| {
                ToolError::Validation("confidence must be a number between 0.0 and 1.0".into())
            })?,
            _ => 1.0,
        };

        let entry = MemoryEntry::new(content, category).with_confidence(confidence);

        let id = entry.id.clone();
        self.store
            .store(entry)
            .await
            .map_err(|e| ToolError::Execution(format!("store failed: {e}")))?;

        Ok(json!({
            "id": id,
            "status": "stored"
        })
        .to_string())
    }
}

// ─── recall_memory ───────────────────────────────────────────────

/// Tool for searching and recalling stored memories.
///
/// The LLM supplies a text query which is tokenized by jieba for CJK support
/// and searched via BM25 full-text ranking.
pub struct RecallMemoryTool {
    store: Arc<dyn MemoryStore>,
}

impl RecallMemoryTool {
    /// Create a new recall_memory tool backed by the given store.
    pub fn new(store: Arc<dyn MemoryStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for RecallMemoryTool {
    fn name(&self) -> &str {
        "recall_memory"
    }

    fn description(&self) -> &str {
        "Search long-term memory for information previously stored. \
         Returns matching entries sorted by relevance. Use this to recall \
         user preferences, project facts, or past lessons."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Text to search for in memory."
                },
                "category": {
                    "type": "string",
                    "enum": [
                        "user_profile", "agent_note", "fact", "preference",
                        "environment", "project_convention", "tool_discovery",
                        "error_lesson", "workflow_pattern", "critical"
                    ],
                    "description": "Optional category filter."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results (default 5).",
                    "minimum": 1,
                    "maximum": 50
                }
            },
            "required": ["query"]
        })
    }

    fn is_mutating(&self) -> bool {
        false
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let query_text = args["query"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("missing or invalid 'query' field".into()))?
            .to_string();

        let limit = args["limit"].as_u64().unwrap_or(5).min(50) as usize;

        let category = match args["category"].as_str() {
            Some(s) => Some(parse_category(s)?),
            None => None,
        };

        let mut query = MemoryQuery::new().with_text(&query_text).with_limit(limit);

        if let Some(cat) = category {
            query = query.with_category(cat);
        }

        let results = self
            .store
            .search(&query)
            .await
            .map_err(|e| ToolError::Execution(format!("search failed: {e}")))?;

        let output: Vec<Value> = results
            .into_iter()
            .map(|scored| {
                json!({
                    "id": scored.entry.id,
                    "content": scored.entry.content,
                    "category": scored.entry.category.to_string(),
                    "confidence": scored.entry.confidence,
                    "score": (scored.score * 100.0).round() / 100.0,
                })
            })
            .collect();

        if output.is_empty() {
            Ok(json!({"results": [], "count": 0}).to_string())
        } else {
            Ok(json!({"results": output, "count": output.len()}).to_string())
        }
    }
}

// ─── helpers ─────────────────────────────────────────────────────

fn parse_category(s: &str) -> Result<MemoryCategory, ToolError> {
    match s {
        "user_profile" => Ok(MemoryCategory::UserProfile),
        "agent_note" => Ok(MemoryCategory::AgentNote),
        "fact" => Ok(MemoryCategory::Fact),
        "preference" => Ok(MemoryCategory::Preference),
        "environment" => Ok(MemoryCategory::Environment),
        "project_convention" => Ok(MemoryCategory::ProjectConvention),
        "tool_discovery" => Ok(MemoryCategory::ToolDiscovery),
        "error_lesson" => Ok(MemoryCategory::ErrorLesson),
        "workflow_pattern" => Ok(MemoryCategory::WorkflowPattern),
        "critical" => Ok(MemoryCategory::Critical),
        other => Err(ToolError::Validation(format!("unknown category '{other}'"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kestrel_memory::{MemoryConfig, TantivyStore};

    async fn make_tools() -> (
        Arc<dyn MemoryStore>,
        StoreMemoryTool,
        RecallMemoryTool,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let config = MemoryConfig::for_test(dir.path());
        let store: Arc<dyn MemoryStore> = Arc::new(TantivyStore::new(&config).await.unwrap());
        let store_tool = StoreMemoryTool::new(store.clone());
        let recall_tool = RecallMemoryTool::new(store.clone());
        (store, store_tool, recall_tool, dir)
    }

    #[test]
    fn test_parse_category_all() {
        let cats = [
            ("user_profile", MemoryCategory::UserProfile),
            ("agent_note", MemoryCategory::AgentNote),
            ("fact", MemoryCategory::Fact),
            ("preference", MemoryCategory::Preference),
            ("environment", MemoryCategory::Environment),
            ("project_convention", MemoryCategory::ProjectConvention),
            ("tool_discovery", MemoryCategory::ToolDiscovery),
            ("error_lesson", MemoryCategory::ErrorLesson),
            ("workflow_pattern", MemoryCategory::WorkflowPattern),
            ("critical", MemoryCategory::Critical),
        ];
        for (s, expected) in cats {
            assert_eq!(parse_category(s).unwrap(), expected);
        }
    }

    #[test]
    fn test_parse_category_invalid() {
        assert!(parse_category("nonexistent").is_err());
    }

    #[tokio::test]
    async fn test_store_memory_tool() {
        let (_store, store_tool, _recall_tool, _dir) = make_tools().await;

        let result = store_tool
            .execute(json!({
                "content": "User prefers dark mode",
                "category": "preference",
                "confidence": 0.9
            }))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["status"], "stored");
        assert!(parsed["id"].as_str().unwrap().len() > 10);
    }

    #[tokio::test]
    async fn test_store_memory_tool_missing_content() {
        let (_store, store_tool, _recall_tool, _dir) = make_tools().await;

        let result = store_tool
            .execute(json!({
                "category": "fact"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("content"));
    }

    #[tokio::test]
    async fn test_store_memory_tool_missing_category() {
        let (_store, store_tool, _recall_tool, _dir) = make_tools().await;

        let result = store_tool
            .execute(json!({
                "content": "some fact"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("category"));
    }

    #[tokio::test]
    async fn test_store_memory_tool_invalid_category() {
        let (_store, store_tool, _recall_tool, _dir) = make_tools().await;

        let result = store_tool
            .execute(json!({
                "content": "test",
                "category": "invalid_cat"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid_cat"));
    }

    #[tokio::test]
    async fn test_recall_memory_tool() {
        let (_store, store_tool, recall_tool, _dir) = make_tools().await;

        store_tool
            .execute(json!({
                "content": "User prefers dark mode for IDE",
                "category": "preference"
            }))
            .await
            .unwrap();

        store_tool
            .execute(json!({
                "content": "Project uses Rust edition 2024",
                "category": "project_convention"
            }))
            .await
            .unwrap();

        let result = recall_tool
            .execute(json!({
                "query": "dark mode",
                "limit": 5
            }))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        let results = parsed["results"].as_array().unwrap();
        assert!(!results.is_empty());
        assert_eq!(parsed["count"].as_u64().unwrap(), results.len() as u64);
    }

    #[tokio::test]
    async fn test_recall_memory_tool_with_category_filter() {
        let (_store, store_tool, recall_tool, _dir) = make_tools().await;

        store_tool
            .execute(json!({
                "content": "fact about the project",
                "category": "fact"
            }))
            .await
            .unwrap();

        store_tool
            .execute(json!({
                "content": "user preference for light theme",
                "category": "preference"
            }))
            .await
            .unwrap();

        let result = recall_tool
            .execute(json!({
                "query": "project",
                "category": "fact"
            }))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        let results = parsed["results"].as_array().unwrap();
        assert!(results.iter().all(|r| r["category"] == "fact"));
    }

    #[tokio::test]
    async fn test_recall_memory_tool_no_results() {
        let (_store, _store_tool, recall_tool, _dir) = make_tools().await;

        let result = recall_tool
            .execute(json!({
                "query": "nonexistent thing"
            }))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["count"].as_u64().unwrap(), 0);
        assert!(parsed["results"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_recall_memory_tool_missing_query() {
        let (_store, _store_tool, recall_tool, _dir) = make_tools().await;

        let result = recall_tool.execute(json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("query"));
    }

    #[tokio::test]
    async fn test_store_and_recall_roundtrip() {
        let (_store, store_tool, recall_tool, _dir) = make_tools().await;

        store_tool
            .execute(json!({
                "content": "The database runs on port 5432",
                "category": "environment",
                "confidence": 0.95
            }))
            .await
            .unwrap();

        let result = recall_tool
            .execute(json!({
                "query": "database port"
            }))
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        let results = parsed["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0]["content"].as_str().unwrap().contains("5432"));
        assert_eq!(results[0]["category"], "environment");
    }

    #[test]
    fn test_store_tool_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (store, store_tool, _, _) = rt.block_on(async {
            let config = MemoryConfig::for_test(dir.path());
            let store: Arc<dyn MemoryStore> = Arc::new(TantivyStore::new(&config).await.unwrap());
            let store_tool = StoreMemoryTool::new(store.clone());
            let recall_tool = RecallMemoryTool::new(store.clone());
            (store, store_tool, recall_tool, dir)
        });

        assert_eq!(store_tool.name(), "store_memory");
        assert!(store_tool.description().len() > 20);
        assert!(store_tool.is_mutating());
        assert!(store_tool.is_available());

        let _ = &store;
    }

    #[test]
    fn test_recall_tool_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (_, _, recall_tool, _) = rt.block_on(async {
            let config = MemoryConfig::for_test(dir.path());
            let store: Arc<dyn MemoryStore> = Arc::new(TantivyStore::new(&config).await.unwrap());
            let store_tool = StoreMemoryTool::new(store.clone());
            let recall_tool = RecallMemoryTool::new(store.clone());
            (store, store_tool, recall_tool, dir)
        });

        assert_eq!(recall_tool.name(), "recall_memory");
        assert!(recall_tool.description().len() > 20);
        assert!(!recall_tool.is_mutating());
        assert!(recall_tool.is_available());
    }

    #[tokio::test]
    async fn test_store_memory_invalid_confidence() {
        let (_store, store_tool, _recall_tool, _dir) = make_tools().await;

        let result = store_tool
            .execute(json!({
                "content": "test",
                "category": "fact",
                "confidence": "high"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("confidence"),
            "error should mention confidence: {err}"
        );
    }

    #[tokio::test]
    async fn test_store_memory_empty_content() {
        let (_store, store_tool, _recall_tool, _dir) = make_tools().await;

        let result = store_tool
            .execute(json!({
                "content": "   ",
                "category": "fact"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("empty"), "error should mention empty: {err}");
    }
}
