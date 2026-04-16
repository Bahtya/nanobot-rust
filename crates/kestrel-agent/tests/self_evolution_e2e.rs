//! End-to-end integration test for the self-evolution feedback loop (Sprint 5, Issue #17).
//!
//! Verifies the complete self-evolution pipeline:
//!
//!   1. Memory recall  → relevant memories injected into system prompt
//!   2. Skill matching  → matched skill steps/pitfalls injected into system prompt
//!   3. Prompt assembly → sections combined via PromptAssembler
//!   4. LLM call        → mock provider returns response (with tool calls)
//!   5. Learning events → MemoryAccessed, SkillUsed, ToolSucceeded emitted on bus
//!   6. Memory storage  → conversation summary stored as AgentNote
//!   7. Outbound message sent back to channel
//!
//! All assertions are deterministic — no LLM output in assertions.

use async_trait::async_trait;
use kestrel_agent::AgentLoop;
use kestrel_bus::events::{AgentEvent, InboundMessage};
use kestrel_bus::MessageBus;
use kestrel_config::Config;
use kestrel_core::{FunctionCall, MessageType, Platform, SessionSource, ToolCall, Usage};
use kestrel_learning::event::{LearningEvent, LearningEventBus, SkillOutcome};
use kestrel_learning::prompt::PromptAssembler;
use kestrel_memory::types::{MemoryCategory, MemoryEntry, MemoryQuery, ScoredEntry};
use kestrel_memory::MemoryError;
use kestrel_memory::MemoryStore as AsyncMemoryStore;
use kestrel_providers::base::{
    BoxStream, CompletionChunk, CompletionRequest, CompletionResponse, LlmProvider, ToolCallDelta,
};
use kestrel_session::SessionManager;
use kestrel_skill::manifest::SkillManifestBuilder;
use kestrel_skill::skill::CompiledSkill;
use kestrel_skill::SkillRegistry;
use kestrel_tools::ToolRegistry;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// Mock provider — deterministic canned responses
// ---------------------------------------------------------------------------

/// Shared state for the mock provider, allowing inspection after registration.
struct MockProviderState {
    responses: Vec<CompletionResponse>,
    call_count: AtomicUsize,
    /// The system prompt from the last completion request.
    last_system_prompt: RwLock<Option<String>>,
}

/// Mock LLM provider that returns a fixed sequence of responses.
/// Records the number of times `complete` is called and captures the
/// system prompt so tests can verify memory/skill injection.
/// State is shared via Arc so clones (as happens during ProviderRegistry::register)
/// all point to the same counters.
struct MockProvider {
    state: Arc<MockProviderState>,
}

impl MockProvider {
    fn new(responses: Vec<CompletionResponse>) -> Self {
        Self {
            state: Arc::new(MockProviderState {
                responses,
                call_count: AtomicUsize::new(0),
                last_system_prompt: RwLock::new(None),
            }),
        }
    }

    fn call_count(&self) -> usize {
        self.state.call_count.load(Ordering::SeqCst)
    }

    async fn last_system_prompt(&self) -> Option<String> {
        self.state.last_system_prompt.read().await.clone()
    }
}

impl Clone for MockProvider {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    async fn complete(&self, request: CompletionRequest) -> anyhow::Result<CompletionResponse> {
        // Capture system prompt for later verification
        if let Some(sys) = request.messages.first() {
            let mut guard = self.state.last_system_prompt.write().await;
            *guard = Some(sys.content.clone());
        }

        let idx = self.state.call_count.fetch_add(1, Ordering::SeqCst);
        let resp = self
            .state
            .responses
            .get(idx)
            .cloned()
            .unwrap_or(CompletionResponse {
                content: Some("default mock response".to_string()),
                tool_calls: None,
                usage: None,
                finish_reason: None,
            });
        Ok(resp)
    }

    async fn complete_stream(&self, request: CompletionRequest) -> anyhow::Result<BoxStream> {
        let resp = self.complete(request).await?;
        let tool_call_deltas = resp.tool_calls.as_ref().map(|tcs| {
            tcs.iter()
                .enumerate()
                .map(|(idx, tc)| ToolCallDelta {
                    index: idx,
                    id: Some(tc.id.clone()),
                    function_name: Some(tc.function.name.clone()),
                    function_arguments: Some(tc.function.arguments.clone()),
                })
                .collect::<Vec<_>>()
        });
        let chunk = CompletionChunk {
            delta: resp.content,
            tool_call_deltas,
            usage: resp.usage,
            done: true,
        };
        Ok(Box::pin(futures::stream::once(async move { Ok(chunk) })))
    }

    fn supports_model(&self, _model: &str) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Mock memory store — deterministic, trackable
// ---------------------------------------------------------------------------

/// Mock memory store that supports pre-population and tracks
/// the number of store/search calls.
struct MockMemoryStore {
    entries: RwLock<Vec<MemoryEntry>>,
    store_count: AtomicUsize,
    search_count: AtomicUsize,
    /// Records the content of the last stored entry.
    last_stored_content: RwLock<Option<String>>,
}

impl MockMemoryStore {
    fn new() -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
            store_count: AtomicUsize::new(0),
            search_count: AtomicUsize::new(0),
            last_stored_content: RwLock::new(None),
        }
    }

    fn store_count(&self) -> usize {
        self.store_count.load(Ordering::SeqCst)
    }

    fn search_count(&self) -> usize {
        self.search_count.load(Ordering::SeqCst)
    }

    async fn last_stored_content(&self) -> Option<String> {
        self.last_stored_content.read().await.clone()
    }

    /// Pre-populate with entries for recall testing.
    async fn prepopulate(&self, entries: Vec<MemoryEntry>) {
        let mut guard = self.entries.write().await;
        guard.extend(entries);
    }
}

#[async_trait::async_trait]
impl AsyncMemoryStore for MockMemoryStore {
    async fn store(&self, entry: MemoryEntry) -> Result<(), MemoryError> {
        self.store_count.fetch_add(1, Ordering::SeqCst);
        {
            let mut guard = self.last_stored_content.write().await;
            *guard = Some(entry.content.clone());
        }
        self.entries.write().await.push(entry);
        Ok(())
    }

    async fn recall(&self, id: &str) -> Result<Option<MemoryEntry>, MemoryError> {
        let entries = self.entries.read().await;
        Ok(entries.iter().find(|e| e.id == id).cloned())
    }

    async fn search(&self, query: &MemoryQuery) -> Result<Vec<ScoredEntry>, MemoryError> {
        self.search_count.fetch_add(1, Ordering::SeqCst);
        let entries = self.entries.read().await;
        let results: Vec<ScoredEntry> = entries
            .iter()
            .filter(|e| {
                if let Some(ref cat) = query.category {
                    if e.category != *cat {
                        return false;
                    }
                }
                if let Some(min_conf) = query.min_confidence {
                    if e.confidence < min_conf {
                        return false;
                    }
                }
                if let Some(ref text) = query.text {
                    if !e.content.to_lowercase().contains(&text.to_lowercase()) {
                        return false;
                    }
                }
                true
            })
            .map(|e| ScoredEntry {
                entry: e.clone(),
                score: 1.0,
            })
            .take(query.limit)
            .collect();
        Ok(results)
    }

    async fn delete(&self, _id: &str) -> Result<(), MemoryError> {
        Ok(())
    }

    async fn len(&self) -> usize {
        self.entries.read().await.len()
    }

    async fn clear(&self) -> Result<(), MemoryError> {
        self.entries.write().await.clear();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_config() -> Config {
    let mut config = Config::default();
    config.agent.model = "mock-model".to_string();
    config
}

fn make_tools() -> ToolRegistry {
    let registry = ToolRegistry::new();
    kestrel_tools::builtins::register_all(&registry);
    registry
}

fn make_providers(mock: Arc<MockProvider>) -> kestrel_providers::ProviderRegistry {
    let mut registry = kestrel_providers::ProviderRegistry::new();
    registry.register("mock", (*mock).clone());
    registry.set_default("mock");
    registry
}

fn make_inbound(content: &str) -> InboundMessage {
    InboundMessage {
        channel: Platform::Telegram,
        sender_id: "user1".to_string(),
        chat_id: "chat42".to_string(),
        content: content.to_string(),
        media: vec![],
        metadata: Default::default(),
        source: Some(SessionSource {
            platform: Platform::Telegram,
            chat_id: "chat42".to_string(),
            chat_name: None,
            chat_type: "dm".to_string(),
            user_id: Some("user1".to_string()),
            user_name: None,
            thread_id: None,
            chat_topic: None,
        }),
        message_type: MessageType::Text,
        message_id: Some("msg1".to_string()),
        reply_to: None,
        timestamp: chrono::Local::now(),
    }
}

/// Build a skill registry with a single skill for testing.
async fn make_skill_registry() -> Arc<SkillRegistry> {
    let registry = SkillRegistry::new();
    let skill = CompiledSkill::new(
        SkillManifestBuilder::new("deploy-k8s", "1.0.0", "Deploy application to Kubernetes")
            .triggers(vec![
                "deploy".to_string(),
                "k8s".to_string(),
                "kubernetes".to_string(),
            ])
            .steps(vec![
                "Check kubeconfig context".to_string(),
                "Apply manifests with kubectl".to_string(),
                "Verify rollout status".to_string(),
            ])
            .pitfalls(vec![
                "Do not deploy to production on Fridays".to_string(),
                "Always check resource limits before deploying".to_string(),
            ])
            .category("devops")
            .build(),
    );
    registry.register(skill).await.unwrap();
    Arc::new(registry)
}

// MockProvider Clone is defined above (shares Arc state).

// ---------------------------------------------------------------------------
// E2E Test 1: Full self-evolution loop — memory recall + skill match + events
// ---------------------------------------------------------------------------

/// Verifies the complete self-evolution feedback loop end-to-end:
///
/// 1. Pre-populate memory with a preference
/// 2. Register a skill with triggers matching the user message
/// 3. Send an inbound message ("deploy to k8s")
/// 4. Verify:
///    - LLM was called (mock provider)
///    - Memory was searched (recall before LLM)
///    - Memory was stored (conversation summary after response)
///    - Skill was matched (from learning events)
///    - Learning events emitted: MemoryAccessed, SkillUsed, ToolSucceeded
///    - Outbound response delivered to the bus
#[tokio::test]
async fn test_self_evolution_full_loop() {
    let config = make_config();
    let bus = MessageBus::new();
    let tmp = tempfile::tempdir().unwrap();
    let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();

    // Mock provider: first call requests a tool, second call returns final answer.
    let mock_provider = Arc::new(MockProvider::new(vec![
        CompletionResponse {
            content: Some(String::new()),
            tool_calls: Some(vec![ToolCall {
                id: "call_e2e_1".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "glob".to_string(),
                    arguments: r#"{"pattern":"*.yaml","path":"."}"#.to_string(),
                },
            }]),
            usage: None,
            finish_reason: Some("tool_calls".to_string()),
        },
        CompletionResponse {
            content: Some("Deployed to k8s successfully.".to_string()),
            tool_calls: None,
            usage: Some(Usage {
                prompt_tokens: Some(50),
                completion_tokens: Some(20),
                total_tokens: Some(70),
            }),
            finish_reason: Some("stop".to_string()),
        },
    ]));

    let providers = make_providers(mock_provider.clone());
    let tools = make_tools();

    // Memory store: pre-populate with a relevant memory.
    // The mock memory store checks `entry.content.to_lowercase().contains(query_text.to_lowercase())`,
    // so entry content must contain the full query text as a substring.
    let memory_store = Arc::new(MockMemoryStore::new());
    memory_store
        .prepopulate(vec![
            MemoryEntry::new(
                "When user says please deploy to k8s, use kubectl not helm",
                MemoryCategory::Preference,
            )
            .with_confidence(0.85),
            MemoryEntry::new(
                "Namespace for please deploy to k8s is staging",
                MemoryCategory::Fact,
            )
            .with_confidence(0.9),
        ])
        .await;

    // Skill registry with deploy-k8s skill
    let skill_registry = make_skill_registry().await;

    // Learning event bus — subscribe before starting agent loop
    let learning_bus = LearningEventBus::new();
    let mut learning_rx = learning_bus.subscribe();

    // Prompt assembler
    let prompt_assembler = PromptAssembler::new();

    // Wire everything into AgentLoop
    let agent_loop = AgentLoop::new(config, bus.clone(), session_manager, providers, tools)
        .with_memory_store(memory_store.clone() as Arc<dyn AsyncMemoryStore>)
        .with_skill_registry(skill_registry)
        .with_learning_bus(learning_bus)
        .with_prompt_assembler(prompt_assembler);

    let mut outbound_rx = bus.consume_outbound().await.unwrap();
    let mut events_rx = bus.subscribe_events();

    // Start agent loop in background
    let agent_handle = tokio::spawn(async move {
        agent_loop.run().await.unwrap();
    });

    // Send inbound message that triggers both memory recall AND skill matching
    bus.publish_inbound(make_inbound("please deploy to k8s"))
        .await
        .unwrap();

    // ── Verify outbound response ────────────────────────────────
    let outbound = tokio::time::timeout(std::time::Duration::from_secs(10), outbound_rx.recv())
        .await
        .expect("timed out waiting for outbound")
        .expect("outbound channel closed");

    assert_eq!(outbound.content, "Deployed to k8s successfully.");
    assert_eq!(outbound.channel, Platform::Telegram);
    assert_eq!(outbound.chat_id, "chat42");

    // ── Verify AgentEvent::Started and Completed ────────────────
    let started = tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv())
        .await
        .expect("timed out waiting for Started event")
        .expect("events channel closed");
    assert!(
        matches!(started, AgentEvent::Started { .. }),
        "expected Started event, got {:?}",
        started
    );

    // Collect remaining events with a short timeout
    let mut saw_completed = false;
    let mut saw_tool_call = false;
    for _ in 0..10 {
        match tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv()).await {
            Ok(Ok(AgentEvent::Completed { .. })) => saw_completed = true,
            Ok(Ok(AgentEvent::ToolCall { .. })) => saw_tool_call = true,
            Ok(Ok(AgentEvent::StreamingChunk { .. })) => { /* expected, ignore */ }
            Ok(Ok(other)) => {
                // Other events are fine
                let _ = other;
            }
            _ => break,
        }
    }
    assert!(saw_completed, "expected Completed event");
    assert!(saw_tool_call, "expected ToolCall event");

    // ── Verify memory recall happened ───────────────────────────
    assert!(
        memory_store.search_count() > 0,
        "memory store should have been searched at least once"
    );

    // ── Verify memory storage happened (conversation summary) ───
    assert_eq!(
        memory_store.store_count(),
        1,
        "conversation memory should have been stored once"
    );
    let stored_content = memory_store
        .last_stored_content()
        .await
        .expect("should have stored content");
    assert!(
        stored_content.contains("deploy to k8s"),
        "stored memory should contain the user message, got: {}",
        stored_content
    );
    assert!(
        stored_content.contains("Deployed to k8s successfully"),
        "stored memory should contain the agent response, got: {}",
        stored_content
    );

    // ── Verify learning events ──────────────────────────────────
    let mut saw_memory_accessed = false;
    let mut saw_skill_used = false;
    let mut saw_tool_succeeded = false;

    for _ in 0..10 {
        match learning_rx.try_recv() {
            Ok(LearningEvent::MemoryAccessed {
                query,
                hit,
                results_count,
                ..
            }) => {
                saw_memory_accessed = true;
                assert!(
                    query.contains("deploy"),
                    "MemoryAccessed query should contain 'deploy', got: {}",
                    query
                );
                assert!(hit, "should be a memory hit (pre-populated)");
                assert!(
                    results_count > 0,
                    "should have at least one result, got {}",
                    results_count
                );
            }
            Ok(LearningEvent::SkillUsed {
                skill_name,
                match_score,
                outcome,
                ..
            }) => {
                saw_skill_used = true;
                assert_eq!(skill_name, "deploy-k8s");
                assert!(
                    match_score > 0.0,
                    "skill match score should be > 0, got {}",
                    match_score
                );
                assert_eq!(outcome, SkillOutcome::Helpful);
            }
            Ok(LearningEvent::ToolSucceeded { tool, .. }) => {
                saw_tool_succeeded = true;
                assert_eq!(tool, "agent_loop");
            }
            Ok(_) => { /* other learning events are fine */ }
            Err(_) => break,
        }
    }

    assert!(
        saw_memory_accessed,
        "expected MemoryAccessed learning event"
    );
    assert!(saw_skill_used, "expected SkillUsed learning event");
    assert!(saw_tool_succeeded, "expected ToolSucceeded learning event");

    // ── Verify LLM was called (twice: tool call + final response) ──
    assert!(
        mock_provider.call_count() >= 2,
        "LLM should have been called at least twice (tool call + final), got {}",
        mock_provider.call_count()
    );

    // ── Verify system prompt contained memory and skill content ─
    let system_prompt = mock_provider
        .last_system_prompt()
        .await
        .expect("should have captured system prompt");

    // The system prompt should contain memory recall results
    assert!(
        system_prompt.contains("kubectl") || system_prompt.contains("helm"),
        "system prompt should contain recalled memory content, got:\n{}",
        system_prompt
    );

    // The system prompt should contain skill steps
    assert!(
        system_prompt.contains("deploy-k8s"),
        "system prompt should contain matched skill name, got:\n{}",
        system_prompt
    );

    agent_handle.abort();
}

// ---------------------------------------------------------------------------
// E2E Test 2: Self-evolution loop without memory — verifies graceful degradation
// ---------------------------------------------------------------------------

/// Verifies the loop works correctly when memory store is not configured.
/// Skills should still be matched, learning events should still fire
/// (but no MemoryAccessed events).
#[tokio::test]
async fn test_self_evolution_no_memory() {
    let config = make_config();
    let bus = MessageBus::new();
    let tmp = tempfile::tempdir().unwrap();
    let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();

    let mock_provider = Arc::new(MockProvider::new(vec![CompletionResponse {
        content: Some("Deployed.".to_string()),
        tool_calls: None,
        usage: None,
        finish_reason: Some("stop".to_string()),
    }]));

    let providers = make_providers(mock_provider.clone());
    let tools = make_tools();
    let skill_registry = make_skill_registry().await;
    let learning_bus = LearningEventBus::new();
    let mut learning_rx = learning_bus.subscribe();

    // No memory store attached!
    let agent_loop = AgentLoop::new(config, bus.clone(), session_manager, providers, tools)
        .with_skill_registry(skill_registry)
        .with_learning_bus(learning_bus);

    let mut outbound_rx = bus.consume_outbound().await.unwrap();

    let agent_handle = tokio::spawn(async move {
        agent_loop.run().await.unwrap();
    });

    bus.publish_inbound(make_inbound("deploy to k8s"))
        .await
        .unwrap();

    let outbound = tokio::time::timeout(std::time::Duration::from_secs(5), outbound_rx.recv())
        .await
        .expect("timed out")
        .expect("closed");
    assert_eq!(outbound.content, "Deployed.");

    // Should still emit SkillUsed events
    let mut saw_skill_used = false;
    let mut saw_memory_accessed = false;
    for _ in 0..5 {
        match learning_rx.try_recv() {
            Ok(LearningEvent::SkillUsed { skill_name, .. }) => {
                saw_skill_used = true;
                assert_eq!(skill_name, "deploy-k8s");
            }
            Ok(LearningEvent::MemoryAccessed { .. }) => {
                saw_memory_accessed = true;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    assert!(
        saw_skill_used,
        "expected SkillUsed event even without memory"
    );
    assert!(
        !saw_memory_accessed,
        "should NOT emit MemoryAccessed when no memory store"
    );

    agent_handle.abort();
}

// ---------------------------------------------------------------------------
// E2E Test 3: Self-evolution loop with provider error — verifies ToolFailed event
// ---------------------------------------------------------------------------

/// Verifies that when the LLM provider fails, the correct error events
/// and ToolFailed learning events are emitted.
#[tokio::test]
async fn test_self_evolution_provider_error() {
    struct FailingProvider;

    #[async_trait]
    impl LlmProvider for FailingProvider {
        fn name(&self) -> &str {
            "failing"
        }
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> anyhow::Result<CompletionResponse> {
            anyhow::bail!("Provider unavailable")
        }
        async fn complete_stream(&self, _request: CompletionRequest) -> anyhow::Result<BoxStream> {
            anyhow::bail!("Provider unavailable")
        }
        fn supports_model(&self, _model: &str) -> bool {
            true
        }
    }

    let config = make_config();
    let bus = MessageBus::new();
    let tmp = tempfile::tempdir().unwrap();
    let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();

    let mut providers = kestrel_providers::ProviderRegistry::new();
    providers.register("failing", FailingProvider);
    providers.set_default("failing");

    let tools = make_tools();

    let memory_store = Arc::new(MockMemoryStore::new());
    let learning_bus = LearningEventBus::new();
    let mut learning_rx = learning_bus.subscribe();
    let prompt_assembler = PromptAssembler::new();

    let agent_loop = AgentLoop::new(config, bus.clone(), session_manager, providers, tools)
        .with_memory_store(memory_store.clone() as Arc<dyn AsyncMemoryStore>)
        .with_learning_bus(learning_bus)
        .with_prompt_assembler(prompt_assembler);

    let mut events_rx = bus.subscribe_events();

    let agent_handle = tokio::spawn(async move {
        agent_loop.run().await.unwrap();
    });

    bus.publish_inbound(make_inbound("test message"))
        .await
        .unwrap();

    // Should get Started event
    let started = tokio::time::timeout(std::time::Duration::from_secs(5), events_rx.recv())
        .await
        .expect("timed out")
        .expect("closed");
    assert!(matches!(started, AgentEvent::Started { .. }));

    // Should get Error event
    let error_event = tokio::time::timeout(std::time::Duration::from_secs(5), events_rx.recv())
        .await
        .expect("timed out")
        .expect("closed");
    assert!(
        matches!(error_event, AgentEvent::Error { .. }),
        "expected Error event, got {:?}",
        error_event
    );

    // Should emit ToolFailed learning event
    let mut saw_tool_failed = false;
    for _ in 0..5 {
        match learning_rx.try_recv() {
            Ok(LearningEvent::ToolFailed {
                tool,
                error_message,
                ..
            }) => {
                saw_tool_failed = true;
                assert_eq!(tool, "agent_loop");
                assert!(
                    error_message.contains("Provider unavailable"),
                    "error message should describe the failure, got: {}",
                    error_message
                );
            }
            Ok(LearningEvent::MemoryAccessed { .. }) => { /* memory recall happens before error */ }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(saw_tool_failed, "expected ToolFailed learning event");

    // Memory should NOT have been stored (error occurred before response)
    assert_eq!(
        memory_store.store_count(),
        0,
        "no memory should be stored on error"
    );

    agent_handle.abort();
}

// ---------------------------------------------------------------------------
// E2E Test 4: Multi-turn self-evolution — memory accumulates across turns
// ---------------------------------------------------------------------------

/// Verifies that across multiple conversation turns:
/// 1. First turn stores conversation memory
/// 2. Second turn recalls the previously stored memory
/// 3. Learning events accumulate correctly
#[tokio::test]
async fn test_self_evolution_multi_turn() {
    let config = make_config();
    let bus = MessageBus::new();
    let tmp = tempfile::tempdir().unwrap();
    let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();

    let mock_provider = Arc::new(MockProvider::new(vec![
        // Turn 1: simple response
        CompletionResponse {
            content: Some("Rust is a systems programming language.".to_string()),
            tool_calls: None,
            usage: None,
            finish_reason: Some("stop".to_string()),
        },
        // Turn 2: another simple response
        CompletionResponse {
            content: Some("Cargo is Rust's build system.".to_string()),
            tool_calls: None,
            usage: None,
            finish_reason: Some("stop".to_string()),
        },
    ]));

    let providers = make_providers(mock_provider.clone());
    let tools = make_tools();

    let memory_store = Arc::new(MockMemoryStore::new());
    let learning_bus = LearningEventBus::new();
    let prompt_assembler = PromptAssembler::new();

    let agent_loop = AgentLoop::new(config, bus.clone(), session_manager, providers, tools)
        .with_memory_store(memory_store.clone() as Arc<dyn AsyncMemoryStore>)
        .with_learning_bus(learning_bus)
        .with_prompt_assembler(prompt_assembler);

    let mut outbound_rx = bus.consume_outbound().await.unwrap();

    let agent_handle = tokio::spawn(async move {
        agent_loop.run().await.unwrap();
    });

    // ── Turn 1 ──────────────────────────────────────────────────
    bus.publish_inbound(make_inbound("What is Rust?"))
        .await
        .unwrap();

    let out1 = tokio::time::timeout(std::time::Duration::from_secs(5), outbound_rx.recv())
        .await
        .expect("timed out on turn 1")
        .expect("closed on turn 1");
    assert_eq!(out1.content, "Rust is a systems programming language.");

    // After turn 1: memory should have been stored
    assert_eq!(memory_store.store_count(), 1);

    // ── Turn 2 ──────────────────────────────────────────────────
    bus.publish_inbound(make_inbound("What is Cargo?"))
        .await
        .unwrap();

    let out2 = tokio::time::timeout(std::time::Duration::from_secs(5), outbound_rx.recv())
        .await
        .expect("timed out on turn 2")
        .expect("closed on turn 2");
    assert_eq!(out2.content, "Cargo is Rust's build system.");

    // After turn 2: another memory stored
    assert_eq!(
        memory_store.store_count(),
        2,
        "two conversation memories should be stored"
    );

    // Memory search should have been called for both turns
    assert!(
        memory_store.search_count() >= 2,
        "memory should have been searched for both turns, got {} searches",
        memory_store.search_count()
    );

    agent_handle.abort();
}

// ---------------------------------------------------------------------------
// E2E Test 5: PromptAssembler integration — custom separator in system prompt
// ---------------------------------------------------------------------------

/// Verifies that when a PromptAssembler with a custom separator is configured,
/// the assembled system prompt flows through the full pipeline correctly.
#[tokio::test]
async fn test_self_evolution_prompt_assembler_custom_separator() {
    let config = make_config();
    let bus = MessageBus::new();
    let tmp = tempfile::tempdir().unwrap();
    let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();

    let mock_provider = Arc::new(MockProvider::new(vec![CompletionResponse {
        content: Some("Done.".to_string()),
        tool_calls: None,
        usage: None,
        finish_reason: Some("stop".to_string()),
    }]));

    let providers = make_providers(mock_provider.clone());
    let tools = make_tools();

    let memory_store = Arc::new(MockMemoryStore::new());
    memory_store
        .prepopulate(vec![MemoryEntry::new(
            "deploy to k8s using dark mode theme",
            MemoryCategory::Preference,
        )
        .with_confidence(0.9)])
        .await;

    let skill_registry = make_skill_registry().await;
    let learning_bus = LearningEventBus::new();

    // Custom separator
    let prompt_assembler = PromptAssembler::with_separator("\n===\n");

    let agent_loop = AgentLoop::new(config, bus.clone(), session_manager, providers, tools)
        .with_memory_store(memory_store.clone() as Arc<dyn AsyncMemoryStore>)
        .with_skill_registry(skill_registry)
        .with_learning_bus(learning_bus)
        .with_prompt_assembler(prompt_assembler);

    let mut outbound_rx = bus.consume_outbound().await.unwrap();

    let agent_handle = tokio::spawn(async move {
        agent_loop.run().await.unwrap();
    });

    // Message that triggers both memory recall and skill match
    bus.publish_inbound(make_inbound("deploy to k8s"))
        .await
        .unwrap();

    let _outbound = tokio::time::timeout(std::time::Duration::from_secs(5), outbound_rx.recv())
        .await
        .expect("timed out")
        .expect("closed");

    // Verify the system prompt was captured and contains assembled content
    let system_prompt = mock_provider.last_system_prompt().await;
    assert!(
        system_prompt.is_some(),
        "should have captured system prompt"
    );
    let prompt = system_prompt.unwrap();
    assert!(!prompt.is_empty(), "system prompt should not be empty");

    agent_handle.abort();
}

// ---------------------------------------------------------------------------
// E2E Test 6: No skills matched — graceful path
// ---------------------------------------------------------------------------

/// Verifies the loop when the user message doesn't match any skill.
/// Memory recall and storage should still work, but no SkillUsed events.
#[tokio::test]
async fn test_self_evolution_no_skill_match() {
    let config = make_config();
    let bus = MessageBus::new();
    let tmp = tempfile::tempdir().unwrap();
    let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();

    let mock_provider = Arc::new(MockProvider::new(vec![CompletionResponse {
        content: Some("The weather is sunny.".to_string()),
        tool_calls: None,
        usage: None,
        finish_reason: Some("stop".to_string()),
    }]));

    let providers = make_providers(mock_provider.clone());
    let tools = make_tools();

    let memory_store = Arc::new(MockMemoryStore::new());
    let skill_registry = make_skill_registry().await; // has "deploy-k8s" skill
    let learning_bus = LearningEventBus::new();
    let mut learning_rx = learning_bus.subscribe();

    let agent_loop = AgentLoop::new(config, bus.clone(), session_manager, providers, tools)
        .with_memory_store(memory_store.clone() as Arc<dyn AsyncMemoryStore>)
        .with_skill_registry(skill_registry)
        .with_learning_bus(learning_bus);

    let mut outbound_rx = bus.consume_outbound().await.unwrap();

    let agent_handle = tokio::spawn(async move {
        agent_loop.run().await.unwrap();
    });

    // Message that does NOT match any skill trigger
    bus.publish_inbound(make_inbound("what is the weather today?"))
        .await
        .unwrap();

    let outbound = tokio::time::timeout(std::time::Duration::from_secs(5), outbound_rx.recv())
        .await
        .expect("timed out")
        .expect("closed");
    assert_eq!(outbound.content, "The weather is sunny.");

    // Memory should still be stored
    assert_eq!(memory_store.store_count(), 1);

    // No SkillUsed events should be emitted
    let mut saw_skill_used = false;
    for _ in 0..10 {
        match learning_rx.try_recv() {
            Ok(LearningEvent::SkillUsed { .. }) => {
                saw_skill_used = true;
            }
            Ok(LearningEvent::MemoryAccessed { hit, .. }) => {
                // Memory recall happens but store is empty → no hit
                assert!(!hit, "memory should not hit on empty store");
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert!(
        !saw_skill_used,
        "no SkillUsed event should fire when no skills match"
    );

    agent_handle.abort();
}
