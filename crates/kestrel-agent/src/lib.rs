//! # kestrel-agent
//!
//! Agent loop, runner, context building, memory, skills, subagents, and hooks.

pub mod compaction;
pub mod context;
pub mod context_budget;
pub mod heartbeat;
pub mod hook;
pub mod loop_mod;
pub mod memory;
pub mod notes;
pub mod runner;
pub mod skills;
pub mod subagent;

pub use compaction::{compact_session, CompactionConfig, CompactionResult, CompactionStrategy};
pub use context::ContextBuilder;
pub use context_budget::{
    prune_messages, BudgetAllocation, ContextBudget, ContextBudgetConfig, PruneResult,
};
pub use heartbeat::{
    AgentLoopHealthCheck, BusHealthCheck, ChannelHealthCheck, ConfigStoreHealthCheck,
    DeepConfigStoreHealthCheck, LivenessCheck, ProviderHealthCheck, ReadinessCheck,
    SessionStoreHealthCheck,
};
pub use hook::{AgentHook, CompositeHook};
pub use loop_mod::{AgentLoop, HeartbeatHandle};
pub use memory::MemoryStore;
pub use notes::{
    extract_compaction_notes, NoteCompactionConfig, NoteFormat, NotesManager, NotesStore,
};
pub use runner::AgentRunner;
pub use skills::SkillsLoader;
pub use subagent::{
    ParallelSpawnConfig, SpawnSummary, SubAgentHandle, SubAgentManager, SubAgentManagerConfig,
    SubAgentMessage, SubAgentResult, SubAgentTask, TaskStatus,
};

/// Result from a streaming LLM completion (internal type).
pub(crate) struct StreamingResult {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<kestrel_core::ToolCall>>,
    pub usage: Option<kestrel_core::Usage>,
    pub finish_reason: Option<String>,
}

impl From<StreamingResult> for kestrel_providers::CompletionResponse {
    fn from(r: StreamingResult) -> Self {
        Self {
            content: r.content,
            tool_calls: r.tool_calls,
            usage: r.usage,
            finish_reason: r.finish_reason,
        }
    }
}
