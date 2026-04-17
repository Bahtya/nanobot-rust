//! End-to-end integration test — validates the full message pipeline:
//!
//! InboundMessage → bus → agent loop → (mock) LLM provider → bus → OutboundMessage
//!
//! Key principle: all assertions are fully deterministic.
//! Mock provider returns fixed responses; we verify the message routing,
//! not LLM output quality.

use async_trait::async_trait;
use kestrel_agent::AgentLoop;
use kestrel_bus::events::{AgentEvent, InboundMessage};
use kestrel_bus::MessageBus;
use kestrel_config::Config;
use kestrel_core::{FunctionCall, MessageType, Platform, ToolCall, Usage};
use kestrel_providers::base::{BoxStream, CompletionChunk};
use kestrel_providers::{CompletionRequest, CompletionResponse, LlmProvider, ProviderRegistry};
use kestrel_session::SessionManager;
use kestrel_tools::{Tool, ToolError, ToolRegistry};
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

    fn default_model(&self) -> &str {
        "mock-model"
    }

    async fn complete(&self, _request: CompletionRequest) -> anyhow::Result<CompletionResponse> {
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst) as usize;
        let resp = self.responses.get(idx).cloned().unwrap_or_else(|| {
            panic!(
                "MockProvider called {} times but only has {} responses",
                idx + 1,
                self.responses.len()
            )
        });
        Ok(resp)
    }

    async fn complete_stream(&self, request: CompletionRequest) -> anyhow::Result<BoxStream> {
        let resp = self.complete(request).await?;

        // Convert tool calls into stream-compatible deltas
        let tool_call_deltas = resp.tool_calls.as_ref().map(|tcs| {
            tcs.iter()
                .enumerate()
                .map(|(i, tc)| kestrel_providers::base::ToolCallDelta {
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

    fn supports_model(&self, model: &str) -> bool {
        let _ = model;
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

/// A deterministic weather tool.
struct WeatherTool;

#[async_trait]
impl Tool for WeatherTool {
    fn name(&self) -> &str {
        "weather"
    }
    fn description(&self) -> &str {
        "Get the current weather for a city"
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"]
        })
    }
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let city = args
            .get("city")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown");
        Ok(format!("Weather in {}: Sunny, 22C", city))
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

fn make_inbound(content: &str) -> InboundMessage {
    InboundMessage {
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

/// Helper: create a provider registry with a mock provider set as default.
fn make_provider_registry(provider: MockProvider) -> ProviderRegistry {
    let mut reg = ProviderRegistry::new();
    reg.register("mock", provider);
    reg.set_default("mock");
    reg
}

// ─── Test 1: Simple message flow ─────────────────────────────────────────────

#[tokio::test]
async fn test_e2e_simple_message_flow() {
    let bus = MessageBus::new();
    let config = make_config();

    let tmp = tempfile::tempdir().unwrap();
    let session_mgr = SessionManager::new(tmp.path().to_path_buf()).unwrap();
    let provider_reg =
        make_provider_registry(MockProvider::simple("Hello! I am a deterministic mock."));
    let tool_reg = ToolRegistry::new();

    let agent_loop = AgentLoop::new(config, bus.clone(), session_mgr, provider_reg, tool_reg);
    let mut outbound_rx = bus.consume_outbound().await.unwrap();
    let mut event_rx = bus.subscribe_events();

    let agent_handle = tokio::spawn(async move { agent_loop.run().await.unwrap() });

    // Send inbound message
    bus.publish_inbound(make_inbound("Hi there!"))
        .await
        .unwrap();

    // Receive outbound
    let outbound = timeout(TEST_TIMEOUT, outbound_rx.recv())
        .await
        .expect("Timed out waiting for outbound")
        .expect("Channel closed");

    assert_eq!(outbound.channel, Platform::Telegram);
    assert_eq!(outbound.chat_id, "chat_100");
    assert_eq!(outbound.content, "Hello! I am a deterministic mock.");
    assert_eq!(outbound.reply_to, Some("msg_001".to_string()));

    // Verify Started event was emitted
    let event = timeout(TEST_TIMEOUT, event_rx.recv())
        .await
        .expect("Timeout on event")
        .expect("Event channel closed");
    assert!(matches!(event, AgentEvent::Started { .. }));

    agent_handle.abort();
}

// ─── Test 2: Tool call flow ──────────────────────────────────────────────────

#[tokio::test]
async fn test_e2e_tool_call_flow() {
    let bus = MessageBus::new();
    let config = make_config();

    let tmp = tempfile::tempdir().unwrap();
    let session_mgr = SessionManager::new(tmp.path().to_path_buf()).unwrap();

    let provider = MockProvider::multi_step(vec![
        // Step 1: LLM requests a tool call
        CompletionResponse {
            content: Some("Let me check the weather.".to_string()),
            tool_calls: Some(vec![ToolCall {
                id: "call_weather_1".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "weather".to_string(),
                    arguments: r#"{"city":"Berlin"}"#.to_string(),
                },
            }]),
            usage: Some(Usage {
                prompt_tokens: Some(20),
                completion_tokens: Some(10),
                total_tokens: Some(30),
            }),
            finish_reason: Some("tool_calls".to_string()),
        },
        // Step 2: LLM produces the final response after seeing tool result
        CompletionResponse {
            content: Some("The weather in Berlin is Sunny, 22C.".to_string()),
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
    tool_reg.register(WeatherTool);

    let agent_loop = AgentLoop::new(config, bus.clone(), session_mgr, provider_reg, tool_reg);
    let mut outbound_rx = bus.consume_outbound().await.unwrap();
    let mut event_rx = bus.subscribe_events();

    let agent_handle = tokio::spawn(async move { agent_loop.run().await.unwrap() });

    bus.publish_inbound(make_inbound("What's the weather in Berlin?"))
        .await
        .unwrap();

    let outbound = timeout(TEST_TIMEOUT, outbound_rx.recv())
        .await
        .expect("Timeout on outbound")
        .expect("Channel closed");

    assert_eq!(outbound.content, "The weather in Berlin is Sunny, 22C.");
    assert_eq!(outbound.channel, Platform::Telegram);

    // Verify lifecycle events
    let mut saw_started = false;
    let mut saw_tool_call = false;
    let mut saw_completed = false;

    for _ in 0..10 {
        match timeout(Duration::from_secs(2), event_rx.recv()).await {
            Ok(Ok(AgentEvent::Started { .. })) => saw_started = true,
            Ok(Ok(AgentEvent::ToolCall { tool_name, .. })) => {
                assert_eq!(tool_name, "weather");
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
    assert!(saw_tool_call, "Expected ToolCall event for 'weather'");
    assert!(saw_completed, "Expected Completed event");

    agent_handle.abort();
}

// ─── Test 3: Parallel tool calls ─────────────────────────────────────────────

#[tokio::test]
async fn test_e2e_parallel_tool_calls() {
    let bus = MessageBus::new();
    let config = make_config();

    let tmp = tempfile::tempdir().unwrap();
    let session_mgr = SessionManager::new(tmp.path().to_path_buf()).unwrap();

    let provider = MockProvider::multi_step(vec![
        CompletionResponse {
            content: Some("Checking both.".to_string()),
            tool_calls: Some(vec![
                ToolCall {
                    id: "call_tokyo".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "weather".to_string(),
                        arguments: r#"{"city":"Tokyo"}"#.to_string(),
                    },
                },
                ToolCall {
                    id: "call_paris".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "weather".to_string(),
                        arguments: r#"{"city":"Paris"}"#.to_string(),
                    },
                },
            ]),
            usage: None,
            finish_reason: Some("tool_calls".to_string()),
        },
        CompletionResponse {
            content: Some("Tokyo: Sunny, 22C. Paris: Sunny, 22C.".to_string()),
            tool_calls: None,
            usage: None,
            finish_reason: Some("stop".to_string()),
        },
    ]);

    let provider_reg = make_provider_registry(provider);
    let tool_reg = ToolRegistry::new();
    tool_reg.register(WeatherTool);

    let agent_loop = AgentLoop::new(config, bus.clone(), session_mgr, provider_reg, tool_reg);
    let mut outbound_rx = bus.consume_outbound().await.unwrap();

    let agent_handle = tokio::spawn(async move { agent_loop.run().await.unwrap() });

    bus.publish_inbound(make_inbound("Compare Tokyo and Paris weather"))
        .await
        .unwrap();

    let outbound = timeout(TEST_TIMEOUT, outbound_rx.recv())
        .await
        .expect("Timeout on outbound")
        .expect("Channel closed");

    assert!(outbound.content.contains("Tokyo"));
    assert!(outbound.content.contains("Paris"));

    agent_handle.abort();
}

// ─── Test 4: Session persists across messages ────────────────────────────────

#[tokio::test]
async fn test_e2e_session_persistence() {
    let bus = MessageBus::new();
    let config = make_config();

    let tmp = tempfile::tempdir().unwrap();
    let session_dir = tmp.path().to_path_buf();
    let session_mgr = SessionManager::new(session_dir.clone()).unwrap();

    let provider = MockProvider::multi_step(vec![
        CompletionResponse {
            content: Some("First response.".to_string()),
            tool_calls: None,
            usage: None,
            finish_reason: Some("stop".to_string()),
        },
        CompletionResponse {
            content: Some("Second response.".to_string()),
            tool_calls: None,
            usage: None,
            finish_reason: Some("stop".to_string()),
        },
    ]);

    let provider_reg = make_provider_registry(provider);
    let tool_reg = ToolRegistry::new();

    let agent_loop = AgentLoop::new(config, bus.clone(), session_mgr, provider_reg, tool_reg);
    let mut outbound_rx = bus.consume_outbound().await.unwrap();

    let agent_handle = tokio::spawn(async move { agent_loop.run().await.unwrap() });

    // First message
    bus.publish_inbound(make_inbound("First message"))
        .await
        .unwrap();
    let out1 = timeout(TEST_TIMEOUT, outbound_rx.recv())
        .await
        .expect("Timeout on first")
        .expect("Channel closed");
    assert_eq!(out1.content, "First response.");

    // Second message — same chat
    bus.publish_inbound(make_inbound("Second message"))
        .await
        .unwrap();
    let out2 = timeout(TEST_TIMEOUT, outbound_rx.recv())
        .await
        .expect("Timeout on second")
        .expect("Channel closed");
    assert_eq!(out2.content, "Second response.");

    // Verify session has at least 4 entries: user1, assistant1, user2, assistant2
    wait_for_session_message_count(session_dir.clone(), "telegram:chat_100", 4).await;

    let mgr2 = SessionManager::new(session_dir).unwrap();
    let session = mgr2.get_or_create("telegram:chat_100", None);
    assert!(session.messages.len() >= 4);

    agent_handle.abort();
}

// ─── Test 5: Echo tool flow ──────────────────────────────────────────────────

#[tokio::test]
async fn test_e2e_echo_tool_flow() {
    let bus = MessageBus::new();
    let config = make_config();

    let tmp = tempfile::tempdir().unwrap();
    let session_mgr = SessionManager::new(tmp.path().to_path_buf()).unwrap();

    let provider = MockProvider::multi_step(vec![
        CompletionResponse {
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_echo".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "echo".to_string(),
                    arguments: r#"{"message":"hello world"}"#.to_string(),
                },
            }]),
            usage: None,
            finish_reason: Some("tool_calls".to_string()),
        },
        CompletionResponse {
            content: Some("Got: ECHO: hello world".to_string()),
            tool_calls: None,
            usage: None,
            finish_reason: Some("stop".to_string()),
        },
    ]);

    let provider_reg = make_provider_registry(provider);
    let tool_reg = ToolRegistry::new();
    tool_reg.register(EchoTool);

    let agent_loop = AgentLoop::new(config, bus.clone(), session_mgr, provider_reg, tool_reg);
    let mut outbound_rx = bus.consume_outbound().await.unwrap();

    let agent_handle = tokio::spawn(async move { agent_loop.run().await.unwrap() });

    bus.publish_inbound(make_inbound("Echo hello world"))
        .await
        .unwrap();

    let outbound = timeout(TEST_TIMEOUT, outbound_rx.recv())
        .await
        .expect("Timeout on echo response")
        .expect("Channel closed");

    assert_eq!(outbound.content, "Got: ECHO: hello world");

    agent_handle.abort();
}

// ─── Test 6: Error when no provider registered ───────────────────────────────

#[tokio::test]
async fn test_e2e_no_provider_error() {
    let bus = MessageBus::new();
    let config = make_config();

    let tmp = tempfile::tempdir().unwrap();
    let session_mgr = SessionManager::new(tmp.path().to_path_buf()).unwrap();

    let provider_reg = ProviderRegistry::new(); // Empty!
    let tool_reg = ToolRegistry::new();

    let agent_loop = AgentLoop::new(config, bus.clone(), session_mgr, provider_reg, tool_reg);
    let _outbound_rx = bus.consume_outbound().await.unwrap();
    let mut event_rx = bus.subscribe_events();

    let agent_handle = tokio::spawn(async move {
        let _ = agent_loop.run().await;
    });

    bus.publish_inbound(make_inbound("This should fail"))
        .await
        .unwrap();

    // Should emit an Error event (no provider)
    let mut saw_error = false;
    for _ in 0..10 {
        match timeout(Duration::from_secs(3), event_rx.recv()).await {
            Ok(Ok(AgentEvent::Started { .. })) => {}
            Ok(Ok(AgentEvent::Error { error, .. })) => {
                assert!(
                    error.contains("No provider") || error.contains("provider"),
                    "Expected provider error, got: {}",
                    error
                );
                saw_error = true;
                break;
            }
            Ok(Ok(AgentEvent::Completed { .. })) => panic!("Should not complete without provider"),
            _ => continue,
        }
    }
    assert!(saw_error, "Expected Error event");

    agent_handle.abort();
}

// ─── Test 7: Tool not found is handled gracefully ────────────────────────────

#[tokio::test]
async fn test_e2e_tool_not_found_graceful() {
    let bus = MessageBus::new();
    let config = make_config();

    let tmp = tempfile::tempdir().unwrap();
    let session_mgr = SessionManager::new(tmp.path().to_path_buf()).unwrap();

    let provider = MockProvider::multi_step(vec![
        // LLM requests a nonexistent tool
        CompletionResponse {
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_missing".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "nonexistent_tool".to_string(),
                    arguments: "{}".to_string(),
                },
            }]),
            usage: None,
            finish_reason: Some("tool_calls".to_string()),
        },
        // Provider sees error result and gives text response
        CompletionResponse {
            content: Some("Sorry, that tool is not available.".to_string()),
            tool_calls: None,
            usage: None,
            finish_reason: Some("stop".to_string()),
        },
    ]);

    let provider_reg = make_provider_registry(provider);
    let tool_reg = ToolRegistry::new(); // Empty — tool not found

    let agent_loop = AgentLoop::new(config, bus.clone(), session_mgr, provider_reg, tool_reg);
    let mut outbound_rx = bus.consume_outbound().await.unwrap();

    let agent_handle = tokio::spawn(async move { agent_loop.run().await.unwrap() });

    bus.publish_inbound(make_inbound("Use nonexistent tool"))
        .await
        .unwrap();

    // Tool error becomes a tool result string; LLM sees it and handles it
    let outbound = timeout(TEST_TIMEOUT, outbound_rx.recv())
        .await
        .expect("Timeout on graceful error response")
        .expect("Channel closed");

    assert_eq!(outbound.content, "Sorry, that tool is not available.");

    agent_handle.abort();
}
