//! End-to-end pipeline integration test.
//!
//! Verifies the full message flow:
//!   InboundMessage → AgentLoop → LLM (mock) → tool call → response → OutboundMessage
//!
//! Uses a mock LLM provider and real bus/session/tools infrastructure.

use async_trait::async_trait;
use kestrel_agent::AgentLoop;
use kestrel_bus::events::{AgentEvent, InboundMessage};
use kestrel_bus::MessageBus;
use kestrel_config::Config;
use kestrel_core::{FunctionCall, MessageType, Platform, SessionSource, ToolCall, Usage};
use kestrel_providers::base::{
    BoxStream, CompletionChunk, CompletionRequest, CompletionResponse, LlmProvider, ToolCallDelta,
};
use kestrel_session::SessionManager;
use kestrel_tools::ToolRegistry;
use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// Mock provider — returns a sequence of canned responses
// ---------------------------------------------------------------------------

struct MockProvider {
    responses: Vec<CompletionResponse>,
    call_count: AtomicUsize,
}

impl MockProvider {
    fn new(responses: Vec<CompletionResponse>) -> Self {
        Self {
            responses,
            call_count: AtomicUsize::new(0),
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
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        let resp = self
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

        // Convert tool_calls to tool_call_deltas for streaming consumers
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

fn make_providers(responses: Vec<CompletionResponse>) -> kestrel_providers::ProviderRegistry {
    let mut registry = kestrel_providers::ProviderRegistry::new();
    registry.register("mock", MockProvider::new(responses));
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
        trace_id: None,
        reply_to: None,
        timestamp: chrono::Local::now(),
    }
}

// ---------------------------------------------------------------------------
// Pipeline tests
// ---------------------------------------------------------------------------

/// Full pipeline: inbound → agent → LLM → outbound (simple response, no tools)
#[tokio::test]
async fn test_pipeline_simple_response() {
    let config = make_config();
    let bus = MessageBus::new();
    let tmp = tempfile::tempdir().unwrap();
    let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();
    let providers = make_providers(vec![CompletionResponse {
        content: Some("Hello from the agent!".to_string()),
        tool_calls: None,
        usage: Some(Usage {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            total_tokens: Some(15),
        }),
        finish_reason: Some("stop".to_string()),
    }]);
    let tools = make_tools();

    let agent_loop = AgentLoop::new(config, bus.clone(), session_manager, providers, tools);

    // Take the outbound receiver before starting
    let mut outbound_rx = bus.consume_outbound().await.unwrap();

    // Start agent loop in background
    let agent_handle = tokio::spawn(async move {
        agent_loop.run().await.unwrap();
    });

    // Send an inbound message
    bus.publish_inbound(make_inbound("Hi there!"))
        .await
        .unwrap();

    // Wait for the outbound response
    let outbound = tokio::time::timeout(std::time::Duration::from_secs(5), outbound_rx.recv())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(outbound.channel, Platform::Telegram);
    assert_eq!(outbound.chat_id, "chat42");
    assert_eq!(outbound.content, "Hello from the agent!");
    assert_eq!(outbound.reply_to.as_deref(), Some("msg1"));

    // Stop agent loop
    bus.publish_inbound(make_inbound("")) // won't be processed, just to unblock if needed
        .await
        .ok();
    agent_handle.abort();
}

/// Pipeline: inbound → agent → LLM → tool call → tool result → final response → outbound
#[tokio::test]
async fn test_pipeline_with_tool_call() {
    let config = make_config();
    let bus = MessageBus::new();
    let tmp = tempfile::tempdir().unwrap();
    let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();

    // First LLM call: request a tool call (glob)
    // Second LLM call: return the final answer
    let providers = make_providers(vec![
        CompletionResponse {
            content: Some(String::new()),
            tool_calls: Some(vec![ToolCall {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "glob".to_string(),
                    arguments: r#"{"pattern":"*.rs","path":"."}"#.to_string(),
                },
            }]),
            usage: None,
            finish_reason: Some("tool_calls".to_string()),
        },
        CompletionResponse {
            content: Some("Found several Rust files in the workspace.".to_string()),
            tool_calls: None,
            usage: Some(Usage {
                prompt_tokens: Some(30),
                completion_tokens: Some(10),
                total_tokens: Some(40),
            }),
            finish_reason: Some("stop".to_string()),
        },
    ]);

    let tools = make_tools();

    let agent_loop = AgentLoop::new(config, bus.clone(), session_manager, providers, tools);

    let mut outbound_rx = bus.consume_outbound().await.unwrap();

    let agent_handle = tokio::spawn(async move {
        agent_loop.run().await.unwrap();
    });

    bus.publish_inbound(make_inbound("List Rust files"))
        .await
        .unwrap();

    let outbound = tokio::time::timeout(std::time::Duration::from_secs(10), outbound_rx.recv())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        outbound.content,
        "Found several Rust files in the workspace."
    );
    assert_eq!(outbound.channel, Platform::Telegram);

    agent_handle.abort();
}

/// Pipeline: verify events are emitted during processing
#[tokio::test]
async fn test_pipeline_events_emitted() {
    let config = make_config();
    let bus = MessageBus::new();
    let tmp = tempfile::tempdir().unwrap();
    let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();
    let providers = make_providers(vec![CompletionResponse {
        content: Some("Done!".to_string()),
        tool_calls: None,
        usage: None,
        finish_reason: Some("stop".to_string()),
    }]);
    let tools = make_tools();

    let agent_loop = AgentLoop::new(config, bus.clone(), session_manager, providers, tools);

    let mut events_rx = bus.subscribe_events();
    let mut outbound_rx = bus.consume_outbound().await.unwrap();

    let agent_handle = tokio::spawn(async move {
        agent_loop.run().await.unwrap();
    });

    bus.publish_inbound(make_inbound("test")).await.unwrap();

    // Wait for completion
    let _outbound = tokio::time::timeout(std::time::Duration::from_secs(5), outbound_rx.recv())
        .await
        .unwrap()
        .unwrap();

    // Check that Started event was emitted
    let started = tokio::time::timeout(std::time::Duration::from_secs(1), events_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(started, AgentEvent::Started { .. }));

    // Check that Completed event was emitted
    let completed = tokio::time::timeout(std::time::Duration::from_secs(1), events_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(completed, AgentEvent::Completed { .. }));

    agent_handle.abort();
}

/// Pipeline: multiple messages in sequence
#[tokio::test]
async fn test_pipeline_multiple_messages() {
    let config = make_config();
    let bus = MessageBus::new();
    let tmp = tempfile::tempdir().unwrap();
    let session_manager = SessionManager::new(tmp.path().to_path_buf()).unwrap();

    // Return different responses for each call
    let providers = make_providers(vec![
        CompletionResponse {
            content: Some("First response".to_string()),
            tool_calls: None,
            usage: None,
            finish_reason: Some("stop".to_string()),
        },
        CompletionResponse {
            content: Some("Second response".to_string()),
            tool_calls: None,
            usage: None,
            finish_reason: Some("stop".to_string()),
        },
    ]);

    let tools = make_tools();
    let agent_loop = AgentLoop::new(config, bus.clone(), session_manager, providers, tools);

    let mut outbound_rx = bus.consume_outbound().await.unwrap();

    let agent_handle = tokio::spawn(async move {
        agent_loop.run().await.unwrap();
    });

    // First message
    bus.publish_inbound(make_inbound("msg1")).await.unwrap();

    let out1 = tokio::time::timeout(std::time::Duration::from_secs(5), outbound_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(out1.content, "First response");

    // Second message
    bus.publish_inbound(make_inbound("msg2")).await.unwrap();

    let out2 = tokio::time::timeout(std::time::Duration::from_secs(5), outbound_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(out2.content, "Second response");

    agent_handle.abort();
}

/// Pipeline: agent error handling when provider fails
#[tokio::test]
async fn test_pipeline_provider_error_handled() {
    struct FailingProvider;
    #[async_trait]
    impl LlmProvider for FailingProvider {
        fn name(&self) -> &str {
            "failing"
        }
        fn default_model(&self) -> &str {
            "mock-model"
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
    let agent_loop = AgentLoop::new(config, bus.clone(), session_manager, providers, tools);

    let mut events_rx = bus.subscribe_events();

    let agent_handle = tokio::spawn(async move {
        agent_loop.run().await.unwrap();
    });

    bus.publish_inbound(make_inbound("test")).await.unwrap();

    // Should get a Started event
    let started = tokio::time::timeout(std::time::Duration::from_secs(5), events_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(started, AgentEvent::Started { .. }));

    // Should get an Error event (provider failed)
    let error_event = tokio::time::timeout(std::time::Duration::from_secs(5), events_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(error_event, AgentEvent::Error { .. }));

    agent_handle.abort();
}
