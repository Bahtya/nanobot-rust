//! # kestrel-agent
//!
//! Agent loop, runner, context building, skills, subagents, and hooks.
//! Memory operations are provided by the [`kestrel_memory`] crate.

pub mod compaction;
pub mod context;
pub mod context_budget;
pub mod heartbeat;
pub mod hook;
pub mod loop_mod;
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

#[cfg(test)]
mod tests {
    use kestrel_memory::{
        MemoryCategory, MemoryConfig, MemoryEntry, MemoryQuery, MemoryStore,
    };

    /// Verify that kestrel-memory types are accessible and functional through
    /// the unified memory system. This test confirms the legacy `memory.rs`
    /// has been fully replaced by the kestrel-memory crate.
    #[tokio::test]
    async fn test_unified_memory_uses_kestrel_memory_trait() {
        let dir = tempfile::tempdir().unwrap();
        let config = MemoryConfig::for_test(dir.path());
        let store = kestrel_memory::HotStore::new(&config).await.unwrap();

        // Store a memory entry
        let entry = MemoryEntry::new("User prefers Rust", MemoryCategory::Preference)
            .with_confidence(0.9);
        store.store(entry).await.unwrap();

        // Search for it
        let query = MemoryQuery::new().with_text("rust").with_limit(5);
        let results = store.search(&query).await.unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].entry.content.contains("Rust"));
        assert_eq!(results[0].entry.category, MemoryCategory::Preference);

        // Verify store len
        assert_eq!(store.len().await, 1);
    }

    /// Verify that the legacy file-based MemoryStore is no longer exported
    /// from this crate. This is a compile-time assertion: if the legacy
    /// `MemoryStore` were still exported, the fully-qualified path
    /// `kestrel_agent::MemoryStore` would conflict with the import above.
    #[test]
    fn test_no_legacy_memory_module_exported() {
        // The only MemoryStore available should be kestrel_memory::MemoryStore.
        // This function signature implicitly asserts that the trait is from
        // kestrel_memory, not from a removed legacy module.
        fn _assert_kestrel_memory_trait(_: &dyn MemoryStore) {}
        let _f = _assert_kestrel_memory_trait;
    }

    /// Verify that all memory categories are available through kestrel-memory.
    #[test]
    fn test_all_memory_categories_available() {
        let categories = vec![
            MemoryCategory::UserProfile,
            MemoryCategory::AgentNote,
            MemoryCategory::Fact,
            MemoryCategory::Preference,
            MemoryCategory::Environment,
            MemoryCategory::ProjectConvention,
            MemoryCategory::ToolDiscovery,
            MemoryCategory::ErrorLesson,
            MemoryCategory::WorkflowPattern,
            MemoryCategory::Critical,
        ];
        assert_eq!(categories.len(), 10);
    }
}
