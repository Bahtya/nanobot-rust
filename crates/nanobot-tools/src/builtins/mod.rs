//! Built-in tool implementations.

pub mod cron;
pub mod filesystem;
pub mod message;
pub mod search;
pub mod shell;
pub mod spawn;
pub mod web;

use crate::registry::ToolRegistry;

/// Register all built-in tools into the registry.
pub fn register_all(registry: &ToolRegistry) {
    registry.register(shell::ExecTool::new());
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
