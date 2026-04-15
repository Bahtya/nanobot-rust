//! Built-in tool implementations.

pub mod cron;
pub mod filesystem;
pub mod message;
pub mod search;
pub mod shell;
pub mod spawn;
pub mod web;

use crate::registry::ToolRegistry;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_mutating_classification() {
        let registry = ToolRegistry::new();
        register_all(&registry);

        // Mutating tools
        assert!(registry.is_mutating("exec"), "exec should be mutating");
        assert!(registry.is_mutating("write_file"), "write_file should be mutating");
        assert!(registry.is_mutating("edit_file"), "edit_file should be mutating");
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
    }
}
