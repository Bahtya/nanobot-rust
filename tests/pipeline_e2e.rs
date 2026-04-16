//! E2E pipeline integration tests — extended message flow verification.
//!
//! Tests the full lifecycle from config loading through agent execution,
//! multi-turn context persistence, sub-agent spawning, and cron scheduling.
//! All assertions are fully deterministic using mock providers.

use async_trait::async_trait;
use nanobot_agent::{AgentLoop, ParallelSpawnConfig, SubAgentManager, SubAgentTask};
use nanobot_bus::events::AgentEvent;
use nanobot_bus::MessageBus;
use nanobot_config::Config;
use nanobot_core::{FunctionCall, MessageType, Platform, ToolCall, Usage};
use nanobot_cron::{CronPayload, CronSchedule, CronService, JobState, ScheduleKind};
use nanobot_providers::base::{BoxStream, CompletionChunk};
use nanobot_providers::{CompletionRequest, CompletionResponse, LlmProvider, ProviderRegistry};
use nanobot_session::SessionManager;
use nanobot_tools::{Tool, ToolError, ToolRegistry};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::time::{sleep, timeout, Duration, Instant};

// ─── Mock Provider ──────────────────────────────────────────────────────────

/// A mock LLM provider that returns deterministic, preconfigured responses.
struct MockProvider {
    responses: Vec<CompletionResponse>,
    call_count: Arc<AtomicU32>,
}

impl MockProvider {
    /// Create a provider that always returns a simple text response.
    fn simple(text: &str) -> Self {
        Self {
            responses: vec![CompletionResponse {
                content: Some(text.to_string()),
                tool_calls: None,
                usage: Some(Usage {
                    prompt_tokens: Some(10),
                    completion_tokens: Some(5),
                    total_tokens: Some(15),
                }),
                finish_reason: Some("stop".to_string()),
            }],
            call_count: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Create a provider that returns different text per call.
    fn multi(texts: Vec<&str>) -> Self {
        Self {
            responses: texts
                .into_iter()
                .map(|text| CompletionResponse {
                    content: Some(text.to_string()),
                    tool_calls: None,
                    usage: Some(Usage {
                        prompt_tokens: Some(10),
                        completion_tokens: Some(5),
                        total_tokens: Some(15),
                    }),
                    finish_reason: Some("stop".to_string()),
                })
                .collect(),
            call_count: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Create a provider that returns a sequence of responses (for tool-call flows).
    fn multi_step(responses: Vec<CompletionResponse>) -> Self {
        Self {
            responses,
            call_count: Arc::new(AtomicU32::new(0)),
        }
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    async fn complete(&self, _request: CompletionRequest) -> anyhow::Result<CompletionResponse> {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst) as usize;
        self.responses.get(idx).cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "MockProvider exhausted: call {} but only {} responses",
                idx + 1,
                self.responses.len()
            )
        })
    }

    async fn complete_stream(&self, request: CompletionRequest) -> anyhow::Result<BoxStream> {
        let resp = self.complete(request).await?;
        let tool_call_deltas = resp.tool_calls.as_ref().map(|tcs| {
            tcs.iter()
                .enumerate()
                .map(|(i, tc)| nanobot_providers::base::ToolCallDelta {
                    index: i,
                    id: Some(tc.id.clone()),
                    function_name: Some(tc.function.name.clone()),
                    function_arguments: Some(tc.function.arguments.clone()),
                })
                .collect::<Vec<_>>()
        });
        let chunk = CompletionChunk {
            delta: resp.content.clone(),
            tool_call_deltas,
            usage: resp.usage.clone(),
            done: true,
        };
        Ok(Box::pin(futures::stream::once(async move { Ok(chunk) })))
    }

    fn supports_model(&self, _model: &str) -> bool {
        true
    }
}

// ─── Mock Tools ───────────────────────────────────────────────────────────────

/// A deterministic echo tool.
struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echoes back the input arguments"
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": { "message": { "type": "string" } },
            "required": ["message"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let msg = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
        Ok(format!("ECHO: {}", msg))
    }
}

/// A deterministic counter tool that tracks invocations.
struct CounterTool {
    count: Arc<std::sync::atomic::AtomicUsize>,
}

impl CounterTool {
    fn new() -> Self {
        Self {
            count: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl Tool for CounterTool {
    fn name(&self) -> &str {
        "counter"
    }
    fn description(&self) -> &str {
        "Increments and returns the current count"
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: Value) -> Result<String, ToolError> {
        let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(format!("count={}", n))
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn make_config() -> Config {
    let mut config = Config::default();
    config.agent.model = "mock-model".to_string();
    config.agent.max_iterations = 10;
    config.agent.max_tokens = 1024;
    config.agent.temperature = 0.7;
    config
}

fn make_inbound(content: &str) -> nanobot_bus::events::InboundMessage {
    nanobot_bus::events::InboundMessage {
        channel: Platform::Telegram,
        sender_id: "user_42".to_string(),
        chat_id: "chat_100".to_string(),
        content: content.to_string(),
        media: vec![],
        metadata: HashMap::new(),
        source: None,
        message_type: MessageType::Text,
        message_id: Some("msg_001".to_string()),
        reply_to: None,
        timestamp: chrono::Local::now(),
    }
}

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

async fn wait_for_session_message_count(
    session_dir: std::path::PathBuf,
    session_key: &str,
    expected_len: usize,
) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    loop {
        let mgr = SessionManager::new(session_dir.clone()).unwrap();
        let session = mgr.get_or_create(session_key, None);
        if session.messages.len() >= expected_len {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Expected >= {expected_len} session entries, got {}",
            session.messages.len()
        );
        sleep(Duration::from_millis(25)).await;
    }
}

fn make_provider_registry(provider: MockProvider) -> ProviderRegistry {
    let mut reg = ProviderRegistry::new();
    reg.register("mock", provider);
    reg.set_default("mock");
    reg
}

/// Build a fully wired agent stack and return handles for testing.
#[allow(dead_code)]
struct AgentStack {
    bus: MessageBus,
    outbound_rx: tokio::sync::mpsc::Receiver<nanobot_bus::events::OutboundMessage>,
    event_rx: tokio::sync::broadcast::Receiver<AgentEvent>,
    agent_handle: tokio::task::JoinHandle<()>,
    session_dir: tempfile::TempDir,
}

#[allow(dead_code)]
async fn build_agent_stack(provider: MockProvider, tools: ToolRegistry) -> AgentStack {
    let bus = MessageBus::new();
    let config = make_config();
    let tmp = tempfile::tempdir().unwrap();
    let session_mgr = SessionManager::new(tmp.path().to_path_buf()).unwrap();
    let provider_reg = make_provider_registry(provider);
    let agent_loop = AgentLoop::new(config, bus.clone(), session_mgr, provider_reg, tools);

    let outbound_rx = bus.consume_outbound().await.unwrap();
    let event_rx = bus.subscribe_events();
    let agent_handle = tokio::spawn(async move {
        let _ = agent_loop.run().await;
    });

    AgentStack {
        bus,
        outbound_rx,
        event_rx,
        agent_handle,
        session_dir: tmp,
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Scenario 1: Config loading → Agent initialization → Provider ready
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_pipeline_startup_flow() {
    // Verify: Config::default() produces valid config, all components initialize.
    let config = make_config();
    assert_eq!(config.agent.model, "mock-model");
    assert!(config.agent.max_iterations > 0);

    let tmp = tempfile::tempdir().unwrap();
    let session_mgr = SessionManager::new(tmp.path().to_path_buf()).unwrap();
    let bus = MessageBus::new();
    let provider_reg = make_provider_registry(MockProvider::simple("ready"));
    let tool_reg = ToolRegistry::new();
    tool_reg.register(EchoTool);

    // Verify: ToolRegistry has the registered tool
    assert_eq!(tool_reg.len(), 1);
    assert!(tool_reg.get("echo").is_some());

    // Verify: Provider resolves
    let provider = provider_reg.get_provider("mock-model");
    assert!(provider.is_some(), "Provider should resolve for mock-model");

    // Verify: AgentLoop constructs without error
    let agent_loop = AgentLoop::new(config, bus.clone(), session_mgr, provider_reg, tool_reg);

    // Verify: Bus channels work
    let mut outbound_rx = bus.consume_outbound().await.unwrap();
    let mut event_rx = bus.subscribe_events();

    let agent_handle = tokio::spawn(async move {
        let _ = agent_loop.run().await;
    });

    // Send a message to confirm the full pipeline is live
    bus.publish_inbound(make_inbound("ping")).await.unwrap();

    let outbound = timeout(TEST_TIMEOUT, outbound_rx.recv())
        .await
        .expect("Timeout waiting for outbound")
        .expect("Channel closed");
    assert_eq!(outbound.content, "ready");

    // Verify Started event
    let mut saw_started = false;
    for _ in 0..5 {
        match timeout(Duration::from_secs(2), event_rx.recv()).await {
            Ok(Ok(AgentEvent::Started { .. })) => {
                saw_started = true;
                break;
            }
            _ => continue,
        }
    }
    assert!(saw_started, "Expected Started event during startup flow");

    agent_handle.abort();
    let _ = agent_handle.await;
}

// ═══════════════════════════════════════════════════════════════════════════════
// Scenario 2: User message → Tool call → Response (full conversation cycle)
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_pipeline_tool_call_conversation_cycle() {
    let bus = MessageBus::new();
    let config = make_config();
    let tmp = tempfile::tempdir().unwrap();
    let session_mgr = SessionManager::new(tmp.path().to_path_buf()).unwrap();

    // Provider: first call requests tool, second call produces final answer
    let provider = MockProvider::multi_step(vec![
        CompletionResponse {
            content: Some("Let me echo that.".to_string()),
            tool_calls: Some(vec![ToolCall {
                id: "call_echo_1".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "echo".to_string(),
                    arguments: r#"{"message":"hello pipeline"}"#.to_string(),
                },
            }]),
            usage: Some(Usage {
                prompt_tokens: Some(20),
                completion_tokens: Some(10),
                total_tokens: Some(30),
            }),
            finish_reason: Some("tool_calls".to_string()),
        },
        CompletionResponse {
            content: Some("Tool returned: ECHO: hello pipeline".to_string()),
            tool_calls: None,
            usage: Some(Usage {
                prompt_tokens: Some(40),
                completion_tokens: Some(15),
                total_tokens: Some(55),
            }),
            finish_reason: Some("stop".to_string()),
        },
    ]);

    let provider_reg = make_provider_registry(provider);
    let tool_reg = ToolRegistry::new();
    tool_reg.register(EchoTool);

    let agent_loop = AgentLoop::new(config, bus.clone(), session_mgr, provider_reg, tool_reg);
    let mut outbound_rx = bus.consume_outbound().await.unwrap();
    let mut event_rx = bus.subscribe_events();

    let agent_handle = tokio::spawn(async move {
        let _ = agent_loop.run().await;
    });

    // User sends message
    bus.publish_inbound(make_inbound("Echo hello pipeline"))
        .await
        .unwrap();

    // Verify outbound response
    let outbound = timeout(TEST_TIMEOUT, outbound_rx.recv())
        .await
        .expect("Timeout on outbound")
        .expect("Channel closed");
    assert_eq!(outbound.content, "Tool returned: ECHO: hello pipeline");
    assert_eq!(outbound.channel, Platform::Telegram);
    assert_eq!(outbound.chat_id, "chat_100");
    assert_eq!(outbound.reply_to, Some("msg_001".to_string()));

    // Verify full lifecycle: Started → ToolCall → Completed
    let mut saw_started = false;
    let mut saw_tool_call = false;
    let mut saw_completed = false;

    for _ in 0..15 {
        match timeout(Duration::from_secs(2), event_rx.recv()).await {
            Ok(Ok(AgentEvent::Started { .. })) => saw_started = true,
            Ok(Ok(AgentEvent::ToolCall {
                tool_name,
                iteration,
                ..
            })) => {
                assert_eq!(tool_name, "echo");
                assert_eq!(iteration, 1);
                saw_tool_call = true;
            }
            Ok(Ok(AgentEvent::Completed {
                tool_calls,
                iterations,
                ..
            })) => {
                assert_eq!(tool_calls, 1);
                assert_eq!(iterations, 2);
                saw_completed = true;
                break;
            }
            Ok(Ok(AgentEvent::Error { error, .. })) => panic!("Agent error: {}", error),
            _ => continue,
        }
    }

    assert!(saw_started, "Expected Started event");
    assert!(saw_tool_call, "Expected ToolCall event");
    assert!(saw_completed, "Expected Completed event");

    agent_handle.abort();
    let _ = agent_handle.await;
}

// ═══════════════════════════════════════════════════════════════════════════════
// Scenario 3: Multi-turn conversation context persistence
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_pipeline_multi_turn_context_persistence() {
    let bus = MessageBus::new();
    let config = make_config();
    let tmp = tempfile::tempdir().unwrap();
    let session_dir = tmp.path().to_path_buf();
    let session_mgr = SessionManager::new(session_dir.clone()).unwrap();

    // Provider returns different responses for each turn
    let provider = MockProvider::multi(vec![
        "Turn 1: Hello!",
        "Turn 2: I remember our conversation.",
        "Turn 3: Goodbye!",
    ]);

    let provider_reg = make_provider_registry(provider);
    let tool_reg = ToolRegistry::new();

    let agent_loop = AgentLoop::new(config, bus.clone(), session_mgr, provider_reg, tool_reg);
    let mut outbound_rx = bus.consume_outbound().await.unwrap();

    let agent_handle = tokio::spawn(async move {
        let _ = agent_loop.run().await;
    });

    // Turn 1
    bus.publish_inbound(make_inbound("Hi")).await.unwrap();
    let out1 = timeout(TEST_TIMEOUT, outbound_rx.recv())
        .await
        .expect("Timeout on turn 1")
        .expect("Channel closed");
    assert_eq!(out1.content, "Turn 1: Hello!");

    // Turn 2
    bus.publish_inbound(make_inbound("Do you remember?"))
        .await
        .unwrap();
    let out2 = timeout(TEST_TIMEOUT, outbound_rx.recv())
        .await
        .expect("Timeout on turn 2")
        .expect("Channel closed");
    assert_eq!(out2.content, "Turn 2: I remember our conversation.");

    // Turn 3
    bus.publish_inbound(make_inbound("Bye")).await.unwrap();
    let out3 = timeout(TEST_TIMEOUT, outbound_rx.recv())
        .await
        .expect("Timeout on turn 3")
        .expect("Channel closed");
    assert_eq!(out3.content, "Turn 3: Goodbye!");

    // Verify session persistence: reload session from disk and check message count
    wait_for_session_message_count(session_dir.clone(), "telegram:chat_100", 6).await;

    let reloaded_mgr = SessionManager::new(session_dir).unwrap();
    let session = reloaded_mgr.get_or_create("telegram:chat_100", None);

    // Should have at least 6 messages: user1, assistant1, user2, assistant2, user3, assistant3
    assert!(session.messages.len() >= 6);

    // Verify message ordering and roles
    let roles: Vec<String> = session
        .messages
        .iter()
        .map(|m| format!("{:?}", m.role))
        .collect();
    // Pattern should alternate: User, Assistant, User, Assistant, User, Assistant
    assert!(
        roles.windows(2).all(|w| w[0] != w[1]),
        "Expected alternating user/assistant roles, got {:?}",
        roles
    );

    agent_handle.abort();
    let _ = agent_handle.await;
}

#[tokio::test]
async fn test_pipeline_multi_turn_with_tool_calls() {
    // Multi-turn conversation that includes tool calls in some turns
    let bus = MessageBus::new();
    let config = make_config();
    let tmp = tempfile::tempdir().unwrap();
    let session_mgr = SessionManager::new(tmp.path().to_path_buf()).unwrap();

    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter_clone = counter.clone();

    // Turn 1: simple response. Turn 2: tool call then response. Turn 3: simple.
    let provider = MockProvider::multi_step(vec![
        // Turn 1: simple
        CompletionResponse {
            content: Some("Welcome!".to_string()),
            tool_calls: None,
            usage: None,
            finish_reason: Some("stop".to_string()),
        },
        // Turn 2: tool call
        CompletionResponse {
            content: Some("Checking counter.".to_string()),
            tool_calls: Some(vec![ToolCall {
                id: "call_counter".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "counter".to_string(),
                    arguments: "{}".to_string(),
                },
            }]),
            usage: None,
            finish_reason: Some("tool_calls".to_string()),
        },
        // Turn 2: final response after tool
        CompletionResponse {
            content: Some("Counter is at 1.".to_string()),
            tool_calls: None,
            usage: None,
            finish_reason: Some("stop".to_string()),
        },
        // Turn 3: simple
        CompletionResponse {
            content: Some("Done!".to_string()),
            tool_calls: None,
            usage: None,
            finish_reason: Some("stop".to_string()),
        },
    ]);

    let provider_reg = make_provider_registry(provider);

    struct CountingTool {
        counter: Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait]
    impl Tool for CountingTool {
        fn name(&self) -> &str {
            "counter"
        }
        fn description(&self) -> &str {
            "Count"
        }
        fn parameters_schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _args: Value) -> Result<String, ToolError> {
            let n = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(format!("count={}", n))
        }
    }

    let tool_reg = ToolRegistry::new();
    tool_reg.register(CountingTool {
        counter: counter_clone,
    });

    let agent_loop = AgentLoop::new(config, bus.clone(), session_mgr, provider_reg, tool_reg);
    let mut outbound_rx = bus.consume_outbound().await.unwrap();

    let agent_handle = tokio::spawn(async move {
        let _ = agent_loop.run().await;
    });

    // Turn 1
    bus.publish_inbound(make_inbound("Hello")).await.unwrap();
    let out1 = timeout(TEST_TIMEOUT, outbound_rx.recv())
        .await
        .expect("T1 timeout")
        .expect("T1 closed");
    assert_eq!(out1.content, "Welcome!");

    // Turn 2 — triggers counter tool
    bus.publish_inbound(make_inbound("Count")).await.unwrap();
    let out2 = timeout(TEST_TIMEOUT, outbound_rx.recv())
        .await
        .expect("T2 timeout")
        .expect("T2 closed");
    assert_eq!(out2.content, "Counter is at 1.");

    // Turn 3
    bus.publish_inbound(make_inbound("Done")).await.unwrap();
    let out3 = timeout(TEST_TIMEOUT, outbound_rx.recv())
        .await
        .expect("T3 timeout")
        .expect("T3 closed");
    assert_eq!(out3.content, "Done!");

    // Verify counter was called exactly once
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    agent_handle.abort();
    let _ = agent_handle.await;
}

// ═══════════════════════════════════════════════════════════════════════════════
// Scenario 4: Sub-agent parallel spawning E2E
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_pipeline_subagent_parallel_spawn() {
    let config = Arc::new(make_config());
    let provider = MockProvider::multi(vec!["Sub-result A", "Sub-result B", "Sub-result C"]);
    let provider_reg = Arc::new(make_provider_registry(provider));
    let tool_reg = Arc::new(ToolRegistry::new());

    let mgr = SubAgentManager::new(config, provider_reg, tool_reg);

    let tasks = vec![
        SubAgentTask {
            id: "parallel-a".into(),
            prompt: "Task A".into(),
            context: Some("Context for A".into()),
            model_override: None,
            max_tokens: None,
        },
        SubAgentTask {
            id: "parallel-b".into(),
            prompt: "Task B".into(),
            context: None,
            model_override: None,
            max_tokens: None,
        },
        SubAgentTask {
            id: "parallel-c".into(),
            prompt: "Task C".into(),
            context: None,
            model_override: None,
            max_tokens: None,
        },
    ];

    let spawn_config = ParallelSpawnConfig {
        max_concurrent: 3,
        per_task_timeout_secs: 10,
        ..Default::default()
    };

    let summary = mgr.spawn_parallel(tasks, &spawn_config).await.unwrap();

    assert_eq!(summary.succeeded, 3);
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.results.len(), 3);

    // Verify all task IDs present
    let ids: Vec<&str> = summary.results.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"parallel-a"));
    assert!(ids.contains(&"parallel-b"));
    assert!(ids.contains(&"parallel-c"));

    // Verify results
    for result in &summary.results {
        assert!(result.success);
        assert!(result.tokens_used > 0);
    }

    // Verify structured notes
    let notes = summary.to_structured_notes();
    assert!(notes.contains("3/3 tasks succeeded"));
    assert!(notes.contains("Sub-result A"));
}

#[tokio::test]
async fn test_pipeline_subagent_denied_tools_filter() {
    let config = Arc::new(make_config());
    let provider_reg = Arc::new(make_provider_registry(MockProvider::simple("ok")));
    let tool_reg = Arc::new(ToolRegistry::new());
    tool_reg.register(EchoTool);
    tool_reg.register(CounterTool::new());

    // Verify both tools registered
    assert_eq!(tool_reg.len(), 2);

    let mgr = SubAgentManager::new(config, provider_reg, tool_reg);

    let tasks = vec![SubAgentTask {
        id: "filtered-task".into(),
        prompt: "Test".into(),
        context: None,
        model_override: None,
        max_tokens: None,
    }];

    let spawn_config = ParallelSpawnConfig {
        denied_tools: vec!["counter".to_string()],
        ..Default::default()
    };

    let summary = mgr.spawn_parallel(tasks, &spawn_config).await.unwrap();
    assert_eq!(summary.succeeded, 1);
}

#[tokio::test]
async fn test_pipeline_subagent_error_isolation() {
    // One sub-agent fails, others succeed — isolation verified
    #[allow(dead_code)]
    struct FailProvider;
    #[async_trait]
    impl LlmProvider for FailProvider {
        fn name(&self) -> &str {
            "fail-mock"
        }
        async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<CompletionResponse> {
            Err(anyhow::anyhow!("Simulated sub-agent failure"))
        }
        async fn complete_stream(&self, req: CompletionRequest) -> anyhow::Result<BoxStream> {
            let resp = self.complete(req).await?;
            let chunk = CompletionChunk {
                delta: resp.content,
                tool_call_deltas: None,
                usage: resp.usage,
                done: true,
            };
            Ok(Box::pin(futures::stream::once(async move { Ok(chunk) })))
        }
        fn supports_model(&self, _model: &str) -> bool {
            true
        }
    }

    // Use a shared call counter to alternate success/failure
    let call_count = Arc::new(AtomicU32::new(0));
    struct AlternatingProvider {
        call_count: Arc<AtomicU32>,
    }
    #[async_trait]
    impl LlmProvider for AlternatingProvider {
        fn name(&self) -> &str {
            "alt-mock"
        }
        async fn complete(&self, _req: CompletionRequest) -> anyhow::Result<CompletionResponse> {
            let n = self.call_count.fetch_add(1, Ordering::SeqCst);
            if n == 1 {
                Err(anyhow::anyhow!("Sub-agent B failed"))
            } else {
                Ok(CompletionResponse {
                    content: Some(format!("Sub-agent {} ok", n)),
                    tool_calls: None,
                    usage: Some(Usage {
                        prompt_tokens: Some(10),
                        completion_tokens: Some(5),
                        total_tokens: Some(15),
                    }),
                    finish_reason: Some("stop".to_string()),
                })
            }
        }
        async fn complete_stream(&self, req: CompletionRequest) -> anyhow::Result<BoxStream> {
            let resp = self.complete(req).await?;
            let chunk = CompletionChunk {
                delta: resp.content,
                tool_call_deltas: None,
                usage: resp.usage,
                done: true,
            };
            Ok(Box::pin(futures::stream::once(async move { Ok(chunk) })))
        }
        fn supports_model(&self, _model: &str) -> bool {
            true
        }
    }

    let config = Arc::new(make_config());
    let mut reg = ProviderRegistry::new();
    reg.register("alt", AlternatingProvider { call_count });
    reg.set_default("alt");

    let mgr = SubAgentManager::new(config, Arc::new(reg), Arc::new(ToolRegistry::new()));

    let tasks = vec![
        SubAgentTask {
            id: "sa-ok-1".into(),
            prompt: "Task 1".into(),
            context: None,
            model_override: None,
            max_tokens: None,
        },
        SubAgentTask {
            id: "sa-fail".into(),
            prompt: "Task 2".into(),
            context: None,
            model_override: None,
            max_tokens: None,
        },
        SubAgentTask {
            id: "sa-ok-2".into(),
            prompt: "Task 3".into(),
            context: None,
            model_override: None,
            max_tokens: None,
        },
    ];

    let spawn_config = ParallelSpawnConfig {
        max_concurrent: 3,
        per_task_timeout_secs: 10,
        ..Default::default()
    };

    let summary = mgr.spawn_parallel(tasks, &spawn_config).await.unwrap();

    // At least one should fail, at least one should succeed
    assert!(summary.failed >= 1, "Expected >= 1 failure");
    assert!(summary.succeeded >= 1, "Expected >= 1 success");
    assert_eq!(summary.results.len(), 3);
}

// ═══════════════════════════════════════════════════════════════════════════════
// Scenario 5: Cron job trigger → execution → result recording
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn test_pipeline_cron_add_and_tick_immediate() {
    let tmp = tempfile::tempdir().unwrap();
    let cron_dir = tmp.path().join("cron");
    let service = CronService::new(cron_dir).unwrap();

    // Add a job scheduled for the past (should be immediately due)
    let past_ms = chrono::Utc::now().timestamp_millis() - 5000;
    let schedule = CronSchedule {
        kind: ScheduleKind::At,
        at_ms: Some(past_ms),
        every_ms: None,
        expr: None,
        tz: None,
    };
    let payload = CronPayload {
        message: "Test cron task".to_string(),
        channel: Some("telegram".to_string()),
        chat_id: Some("chat_100".to_string()),
        deliver: true,
    };

    let job = service
        .add_job(schedule, payload, Some("immediate-job".to_string()))
        .unwrap();
    assert_eq!(job.state, JobState::Active);
    assert!(job.next_run.is_some());
    assert_eq!(job.name, Some("immediate-job".to_string()));

    // Tick should find the job as due
    let due = service.tick();
    assert_eq!(due.len(), 1, "Expected 1 due job");
    assert_eq!(due[0].id, job.id);
    assert_eq!(due[0].payload.message, "Test cron task");

    // After tick, the "At" job should be marked Done
    let updated = service.get_job(&job.id).unwrap();
    assert_eq!(updated.state, JobState::Done);
    assert!(updated.last_run.is_some());
}

#[test]
fn test_pipeline_cron_recurring_every() {
    let tmp = tempfile::tempdir().unwrap();
    let cron_dir = tmp.path().join("cron");
    let service = CronService::new(cron_dir).unwrap();

    // Add a recurring job with a very short interval.
    // Use "At" first to get it due immediately, then modify to "Every".
    // Since we can't modify schedule, use a schedule that starts in the past
    // with "Every" kind. compute_next_run sets next_run = now + every_ms,
    // so after creation we need to wait briefly.
    let schedule = CronSchedule {
        kind: ScheduleKind::Every,
        at_ms: None,
        every_ms: Some(1), // 1ms interval
        expr: None,
        tz: None,
    };
    let payload = CronPayload {
        message: "Recurring task".to_string(),
        channel: None,
        chat_id: None,
        deliver: false,
    };

    let job = service
        .add_job(schedule, payload, Some("recurring".to_string()))
        .unwrap();

    // First tick — next_run is now + 1ms, might not be due yet
    // Sleep a bit to ensure it's past
    std::thread::sleep(std::time::Duration::from_millis(5));

    let due1 = service.tick();
    assert_eq!(
        due1.len(),
        1,
        "First tick should find the recurring job due"
    );
    assert_eq!(due1[0].payload.message, "Recurring task");

    // Record completion
    service.mark_completed(&job.id, Some("result: ok".to_string()));

    // Sleep to allow next_run to pass
    std::thread::sleep(std::time::Duration::from_millis(5));

    // Second tick — should fire again since it's "Every 1ms"
    let due2 = service.tick();
    assert_eq!(due2.len(), 1, "Recurring job should fire again");

    // Verify execution history
    let updated = service.get_job(&job.id).unwrap();
    assert!(
        updated.history.len() >= 2,
        "Should have at least 2 history entries"
    );
    assert_eq!(
        updated.state,
        JobState::Active,
        "Recurring job stays active"
    );
}

#[tokio::test]
async fn test_pipeline_cron_with_bus_events() {
    let bus = MessageBus::new();
    let tmp = tempfile::tempdir().unwrap();
    let cron_dir = tmp.path().join("cron");
    let service = CronService::with_bus(cron_dir, bus.clone()).unwrap();
    let mut event_rx = bus.subscribe_events();

    // Add a job scheduled for the past
    let past_ms = chrono::Utc::now().timestamp_millis() - 1000;
    let schedule = CronSchedule {
        kind: ScheduleKind::At,
        at_ms: Some(past_ms),
        every_ms: None,
        expr: None,
        tz: None,
    };
    let payload = CronPayload {
        message: "Scheduled greeting".to_string(),
        channel: Some("telegram".to_string()),
        chat_id: Some("chat_200".to_string()),
        deliver: true,
    };

    service
        .add_job(schedule, payload, Some("bus-event-job".to_string()))
        .unwrap();

    // Tick fires the job and emits CronFired event
    let due = service.tick();
    assert_eq!(due.len(), 1);

    // Verify CronFired event was emitted on the bus
    let event = timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("Timeout waiting for CronFired event")
        .expect("Event channel closed");

    match event {
        AgentEvent::CronFired {
            job_id,
            job_name,
            message,
        } => {
            assert!(!job_id.is_empty());
            assert_eq!(job_name, Some("bus-event-job".to_string()));
            assert_eq!(message, "Scheduled greeting");
        }
        other => panic!("Expected CronFired event, got {:?}", other),
    }
}

#[test]
fn test_pipeline_cron_mark_completed_with_result() {
    let tmp = tempfile::tempdir().unwrap();
    let cron_dir = tmp.path().join("cron");
    let service = CronService::new(cron_dir).unwrap();

    let past_ms = chrono::Utc::now().timestamp_millis() - 1000;
    let schedule = CronSchedule {
        kind: ScheduleKind::At,
        at_ms: Some(past_ms),
        every_ms: None,
        expr: None,
        tz: None,
    };
    let payload = CronPayload {
        message: "Task to record".to_string(),
        channel: None,
        chat_id: None,
        deliver: false,
    };

    let job = service
        .add_job(schedule, payload, Some("result-recorder".to_string()))
        .unwrap();

    // Tick to fire
    let due = service.tick();
    assert_eq!(due.len(), 1);

    // Record a successful result
    service.mark_completed(
        &job.id,
        Some("Execution result: 42 items processed".to_string()),
    );

    // Verify history
    let updated = service.get_job(&job.id).unwrap();
    assert_eq!(updated.history.len(), 1);
    let record = &updated.history[0];
    assert!(record.success);
    assert_eq!(
        record.result,
        Some("Execution result: 42 items processed".to_string())
    );
}

#[test]
fn test_pipeline_cron_list_and_state() {
    let tmp = tempfile::tempdir().unwrap();
    let cron_dir = tmp.path().join("cron");
    let service = CronService::new(cron_dir).unwrap();

    // Add 3 jobs all scheduled in the past (well past)
    let past_ms = chrono::Utc::now().timestamp_millis() - 10000;
    for i in 0..3 {
        let schedule = CronSchedule {
            kind: ScheduleKind::At,
            at_ms: Some(past_ms - i * 1000), // All in the past
            every_ms: None,
            expr: None,
            tz: None,
        };
        let payload = CronPayload {
            message: format!("Job {}", i),
            channel: None,
            chat_id: None,
            deliver: false,
        };
        service
            .add_job(schedule, payload, Some(format!("job-{}", i)))
            .unwrap();
    }

    // List all jobs
    let jobs = service.list_jobs();
    assert_eq!(jobs.len(), 3);

    // Verify job states
    let states = service.list_job_states();
    assert_eq!(states.len(), 3);
    for (job, state) in &states {
        assert!(state.is_active);
        assert_eq!(job.state, JobState::Active);
    }

    // Tick fires all due jobs (past_ms - 1000, past_ms, past_ms + 1000 are all in the past)
    let due = service.tick();
    assert_eq!(due.len(), 3, "All 3 past-scheduled jobs should be due");

    // All should be Done now
    let jobs = service.list_jobs();
    assert!(jobs.iter().all(|j| j.state == JobState::Done));
}

// ═══════════════════════════════════════════════════════════════════════════════
// Bonus: Full pipeline — Cron fires → Agent processes → Response recorded
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_pipeline_cron_to_agent_full_flow() {
    // End-to-end: CronService fires a job, the message is routed through the
    // agent loop, and a response is produced.
    let bus = MessageBus::new();
    let tmp = tempfile::tempdir().unwrap();
    let cron_dir = tmp.path().join("cron");

    // Set up cron with bus integration
    let cron = Arc::new(CronService::with_bus(cron_dir, bus.clone()).unwrap());

    // Set up agent stack
    let config = make_config();
    let session_mgr = SessionManager::new(tmp.path().to_path_buf()).unwrap();
    let provider_reg =
        make_provider_registry(MockProvider::simple("Cron response: all systems go"));
    let tool_reg = ToolRegistry::new();

    let agent_loop = AgentLoop::new(config, bus.clone(), session_mgr, provider_reg, tool_reg);
    let mut outbound_rx = bus.consume_outbound().await.unwrap();
    let mut event_rx = bus.subscribe_events();

    let agent_handle = tokio::spawn(async move {
        let _ = agent_loop.run().await;
    });

    // Add a cron job scheduled for the past
    let past_ms = chrono::Utc::now().timestamp_millis() - 1000;
    let schedule = CronSchedule {
        kind: ScheduleKind::At,
        at_ms: Some(past_ms),
        every_ms: None,
        expr: None,
        tz: None,
    };
    let payload = CronPayload {
        message: "Check system status".to_string(),
        channel: Some("telegram".to_string()),
        chat_id: Some("chat_cron".to_string()),
        deliver: true,
    };
    cron.add_job(schedule, payload, Some("status-check".to_string()))
        .unwrap();

    // Tick fires the job → emits CronFired event on bus
    let due = cron.tick();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].payload.message, "Check system status");

    // Verify the CronFired event propagated through the bus
    let event = timeout(Duration::from_secs(2), event_rx.recv())
        .await
        .expect("Timeout on CronFired event")
        .expect("Event channel closed");
    match event {
        AgentEvent::CronFired {
            job_name, message, ..
        } => {
            assert_eq!(job_name, Some("status-check".to_string()));
            assert_eq!(message, "Check system status");
        }
        other => panic!("Expected CronFired, got {:?}", other),
    }

    // Now manually route the cron message through the agent (simulating gateway wiring)
    let cron_inbound = nanobot_bus::events::InboundMessage {
        channel: Platform::Telegram,
        sender_id: "cron".to_string(),
        chat_id: "chat_cron".to_string(),
        content: "Check system status".to_string(),
        media: vec![],
        metadata: HashMap::new(),
        source: None,
        message_type: MessageType::Text,
        message_id: Some("cron_msg_001".to_string()),
        reply_to: None,
        timestamp: chrono::Local::now(),
    };
    bus.publish_inbound(cron_inbound).await.unwrap();

    let outbound = timeout(TEST_TIMEOUT, outbound_rx.recv())
        .await
        .expect("Timeout on cron-agent response")
        .expect("Channel closed");
    assert_eq!(outbound.content, "Cron response: all systems go");
    assert_eq!(outbound.chat_id, "chat_cron");

    // Record result in cron
    cron.mark_completed(&due[0].id, Some(outbound.content.clone()));

    // Verify cron job is done with result
    let job = cron.get_job(&due[0].id).unwrap();
    assert_eq!(job.state, JobState::Done);
    assert!(job.history.first().unwrap().success);
    assert_eq!(
        job.history.first().unwrap().result,
        Some("Cron response: all systems go".to_string())
    );

    agent_handle.abort();
    let _ = agent_handle.await;
}
