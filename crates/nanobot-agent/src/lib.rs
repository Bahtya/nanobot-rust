//! # nanobot-agent
//!
//! Agent loop, runner, context building, memory, skills, subagents, and hooks.

pub mod context;
pub mod hook;
pub mod loop_mod;
pub mod memory;
pub mod runner;
pub mod skills;
pub mod subagent;

pub use context::ContextBuilder;
pub use hook::{AgentHook, CompositeHook};
pub use loop_mod::AgentLoop;
pub use memory::MemoryStore;
pub use runner::AgentRunner;
pub use skills::SkillsLoader;
pub use subagent::SubagentManager;

/// Result from a streaming LLM completion (internal type).
pub(crate) struct StreamingResult {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<nanobot_core::ToolCall>>,
    pub usage: Option<nanobot_core::Usage>,
    pub finish_reason: Option<String>,
}

impl From<StreamingResult> for nanobot_providers::CompletionResponse {
    fn from(r: StreamingResult) -> Self {
        Self {
            content: r.content,
            tool_calls: r.tool_calls,
            usage: r.usage,
            finish_reason: r.finish_reason,
        }
    }
}
