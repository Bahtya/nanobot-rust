//! Built-in tool implementations.

pub mod cron;
pub mod filesystem;
pub mod memory;
pub mod message;
pub mod search;
pub mod shell;
pub mod spawn;
pub mod web;

use crate::registry::ToolRegistry;
use kestrel_memory::MemoryStore;
use std::sync::Arc;

/// Configuration applied when registering built-in tools.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuiltinsConfig {
    /// When true, disable exec sandbox restrictions intended for untrusted environments.
    pub dangerous: bool,
}

/// Register all built-in tools into the registry.
pub fn register_all(registry: &ToolRegistry) {
    register_all_with_config(registry, BuiltinsConfig::default());
}

/// Register all built-in tools into the registry with the provided configuration.
pub fn register_all_with_config(registry: &ToolRegistry, config: BuiltinsConfig) {
    registry.register(shell::ExecTool::new().dangerous(config.dangerous));
    registry.register(filesystem::ReadFileTool::new());
    registry.register(filesystem::WriteFileTool::new());
    registry.register(filesystem::EditFileTool::new());
    registry.register(filesystem::ListDirTool::new());
    registry.register(web::WebSearchTool::new());
    registry.register(web::WebFetchTool::new());
    registry.register(search::GrepTool::new());
    registry.register(search::GlobTool::new());
    registry.register(message::MessageTool::new());
    registry.register(cron::CronTool::new());
    registry.register(spawn::SpawnTool::new());
}

/// Register memory tools that require a memory store.
pub fn register_memory_tools(
    registry: &ToolRegistry,
    store: Arc<dyn MemoryStore>,
) {
    registry.register(memory::StoreMemoryTool::new(store.clone()));
    registry.register(memory::RecallMemoryTool::new(store));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_mutating_classification() {
        let registry = ToolRegistry::new();
        register_all(&registry);

        // Mutating tools
        assert!(registry.is_mutating("exec"), "exec should be mutating");
        assert!(
            registry.is_mutating("write_file"),
            "write_file should be mutating"
        );
        assert!(
            registry.is_mutating("edit_file"),
            "edit_file should be mutating"
        );
        assert!(registry.is_mutating("cron"), "cron should be mutating");
        assert!(registry.is_mutating("spawn"), "spawn should be mutating");
        assert!(
            registry.is_mutating("send_message"),
            "send_message should be mutating"
        );

        // Read-only tools
        assert!(
            !registry.is_mutating("read_file"),
            "read_file should NOT be mutating"
        );
        assert!(
            !registry.is_mutating("list_dir"),
            "list_dir should NOT be mutating"
        );
        assert!(
            !registry.is_mutating("web_search"),
            "web_search should NOT be mutating"
        );
        assert!(
            !registry.is_mutating("web_fetch"),
            "web_fetch should NOT be mutating"
        );
        assert!(!registry.is_mutating("grep"), "grep should NOT be mutating");
        assert!(!registry.is_mutating("glob"), "glob should NOT be mutating");

        // Memory tools are NOT in register_all — they need deps
        assert!(
            registry.get("store_memory").is_none(),
            "store_memory should not be in register_all"
        );
        assert!(
            registry.get("recall_memory").is_none(),
            "recall_memory should not be in register_all"
        );
    }

    #[tokio::test]
    async fn test_register_memory_tools() {
        use kestrel_memory::{MemoryConfig, TantivyStore};

        let registry = ToolRegistry::new();
        register_all(&registry);

        let dir = tempfile::tempdir().unwrap();
        let config = MemoryConfig::for_test(dir.path());
        let store: Arc<dyn MemoryStore> = Arc::new(TantivyStore::new(&config).await.unwrap());

        register_memory_tools(&registry, store);

        assert!(
            registry.get("store_memory").is_some(),
            "store_memory should be registered"
        );
        assert!(
            registry.get("recall_memory").is_some(),
            "recall_memory should be registered"
        );
        assert!(
            registry.is_mutating("store_memory"),
            "store_memory should be mutating"
        );
        assert!(
            !registry.is_mutating("recall_memory"),
            "recall_memory should NOT be mutating"
        );
    }
}
