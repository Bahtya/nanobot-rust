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
