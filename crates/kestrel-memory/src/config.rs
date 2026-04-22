//! Configuration for memory stores.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Configuration for the memory subsystem.
///
/// Can be loaded from a TOML file or constructed programmatically.
///
/// # Example TOML
///
/// ```toml
/// max_entries = 1000
/// hot_store_path = "/home/user/.kestrel/memory/hot.jsonl"
/// warm_store_path = "/home/user/.kestrel/memory/warm"
/// embedding_dim = 1536
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Maximum number of entries per store layer.
    #[serde(default = "default_max_entries")]
    pub max_entries: usize,

    /// Path to the hot store persistence file (JSON lines format).
    #[serde(default = "default_hot_store_path")]
    pub hot_store_path: PathBuf,

    /// Path to the warm store data directory.
    #[serde(default = "default_warm_store_path")]
    pub warm_store_path: PathBuf,

    /// Dimension of embedding vectors for semantic search.
    #[serde(default = "default_embedding_dim")]
    pub embedding_dim: usize,

    /// Character budget for recalled memory content injected into prompts.
    #[serde(default = "default_memory_char_budget")]
    pub memory_char_budget: usize,

    /// Overflow character budget used during compaction or tight-context scenarios.
    #[serde(default = "default_memory_char_budget_overflow")]
    pub memory_char_budget_overflow: usize,
}

fn default_max_entries() -> usize {
    1000
}

fn default_hot_store_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".kestrel")
        .join("memory")
        .join("hot.jsonl")
}

fn default_warm_store_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".kestrel")
        .join("memory")
        .join("warm")
}

fn default_embedding_dim() -> usize {
    1536
}

fn default_memory_char_budget() -> usize {
    2200
}

fn default_memory_char_budget_overflow() -> usize {
    1375
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            max_entries: default_max_entries(),
            hot_store_path: default_hot_store_path(),
            warm_store_path: default_warm_store_path(),
            embedding_dim: default_embedding_dim(),
            memory_char_budget: default_memory_char_budget(),
            memory_char_budget_overflow: default_memory_char_budget_overflow(),
        }
    }
}

impl MemoryConfig {
    /// Create a config for testing with temporary directories.
    pub fn for_test(temp_dir: &std::path::Path) -> Self {
        Self {
            max_entries: 100,
            hot_store_path: temp_dir.join("hot.jsonl"),
            warm_store_path: temp_dir.join("warm"),
            embedding_dim: 8,
            memory_char_budget: default_memory_char_budget(),
            memory_char_budget_overflow: default_memory_char_budget_overflow(),
        }
    }

    /// Parse config from a TOML string.
    pub fn from_toml(toml_str: &str) -> crate::error::Result<Self> {
        toml::from_str(toml_str).map_err(|e| crate::error::MemoryError::Config(e.to_string()))
    }

    /// Serialize config to a TOML string.
    pub fn to_toml(&self) -> crate::error::Result<String> {
        toml::to_string_pretty(self).map_err(|e| crate::error::MemoryError::Config(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = MemoryConfig::default();
        assert_eq!(config.max_entries, 1000);
        assert_eq!(config.embedding_dim, 1536);
        assert_eq!(config.memory_char_budget, 2200);
        assert_eq!(config.memory_char_budget_overflow, 1375);
        assert!(config.hot_store_path.to_string_lossy().contains(".kestrel"));
        assert!(config
            .warm_store_path
            .to_string_lossy()
            .contains(".kestrel"));
    }

    #[test]
    fn test_for_test_config() {
        let temp = std::env::temp_dir();
        let config = MemoryConfig::for_test(&temp);
        assert_eq!(config.max_entries, 100);
        assert_eq!(config.embedding_dim, 8);
        assert!(config.hot_store_path.starts_with(&temp));
        assert!(config.warm_store_path.starts_with(&temp));
    }

    #[test]
    fn test_toml_roundtrip() {
        let config = MemoryConfig {
            max_entries: 500,
            hot_store_path: PathBuf::from("/tmp/hot.jsonl"),
            warm_store_path: PathBuf::from("/tmp/warm"),
            embedding_dim: 768,
            memory_char_budget: 3000,
            memory_char_budget_overflow: 1500,
        };
        let toml_str = config.to_toml().unwrap();
        let parsed = MemoryConfig::from_toml(&toml_str).unwrap();
        assert_eq!(parsed.max_entries, 500);
        assert_eq!(parsed.embedding_dim, 768);
        assert_eq!(parsed.memory_char_budget, 3000);
        assert_eq!(parsed.memory_char_budget_overflow, 1500);
        assert_eq!(parsed.hot_store_path, PathBuf::from("/tmp/hot.jsonl"));
        assert_eq!(parsed.warm_store_path, PathBuf::from("/tmp/warm"));
    }

    #[test]
    fn test_toml_parse_partial() {
        let toml_str = "max_entries = 42";
        let config = MemoryConfig::from_toml(toml_str).unwrap();
        assert_eq!(config.max_entries, 42);
        // Other fields get defaults
        assert_eq!(config.embedding_dim, 1536);
        assert_eq!(config.memory_char_budget, 2200);
        assert_eq!(config.memory_char_budget_overflow, 1375);
    }

    #[test]
    fn test_toml_invalid() {
        let toml_str = "max_entries = \"not a number\"";
        let result = MemoryConfig::from_toml(toml_str);
        assert!(result.is_err());
    }

    #[test]
    fn test_custom_char_budget_from_toml() {
        let toml_str = "memory_char_budget = 1000\nmemory_char_budget_overflow = 500";
        let config = MemoryConfig::from_toml(toml_str).unwrap();
        assert_eq!(config.memory_char_budget, 1000);
        assert_eq!(config.memory_char_budget_overflow, 500);
    }
}
