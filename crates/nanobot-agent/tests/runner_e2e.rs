//! End-to-end agent runner tests using a mock LLM provider.
//!
//! Verifies that the agent loop correctly handles:
//! - Simple text responses (no tool calls)
//! - Tool call → tool result → final response
//! - Max iteration limits

use async_trait::async_trait;
use nanobot_agent::AgentRunner;
use nanobot_config::Config;
use nanobot_core::{FunctionCall, Message, MessageRole, ToolCall, Usage};
use nanobot_providers::base::{
    BoxStream, CompletionChunk, CompletionRequest, CompletionResponse, LlmProvider,
};
use nanobot_tools::ToolRegistry;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Mock provider that returns canned responses
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
    nanobot_tools::builtins::register_all(&registry);
    registry
}

fn make_providers(responses: Vec<CompletionResponse>) -> nanobot_providers::ProviderRegistry {
    let mut registry = nanobot_providers::ProviderRegistry::new();
    registry.register("mock", MockProvider::new(responses));
    registry.set_default("mock");
    registry
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test: Agent returns a simple text response with no tool calls.
#[tokio::test]
async fn test_agent_simple_response() {
    let config = Arc::new(make_config());
    let providers = make_providers(vec![CompletionResponse {
        content: Some("Hello! I am a mock assistant.".to_string()),
        tool_calls: None,
        usage: Some(Usage {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            total_tokens: Some(15),
        }),
        finish_reason: Some("stop".to_string()),
    }]);

    let runner = AgentRunner::new(config, Arc::new(providers), Arc::new(make_tools()));

    let messages = vec![Message {
        role: MessageRole::User,
        content: "Hello!".to_string(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }];

    let result = runner
        .run("You are a helpful assistant.".to_string(), messages)
        .await
        .unwrap();

    assert_eq!(result.content, "Hello! I am a mock assistant.");
    assert_eq!(result.iterations_used, 1);
    assert_eq!(result.tool_calls_made, 0);
}

/// Test: Agent handles a tool call followed by a final response.
#[tokio::test]
async fn test_agent_tool_call_then_response() {
    let config = Arc::new(make_config());
    let providers = make_providers(vec![
        CompletionResponse {
            content: Some(String::new()),
            tool_calls: Some(vec![ToolCall {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "glob".to_string(),
                    arguments: r#"{"pattern":"*.rs"}"#.to_string(),
                },
            }]),
            usage: None,
            finish_reason: Some("tool_calls".to_string()),
        },
        CompletionResponse {
            content: Some("Found 3 Rust files.".to_string()),
            tool_calls: None,
            usage: Some(Usage {
                prompt_tokens: Some(20),
                completion_tokens: Some(10),
                total_tokens: Some(30),
            }),
            finish_reason: Some("stop".to_string()),
        },
    ]);

    let runner = AgentRunner::new(config, Arc::new(providers), Arc::new(make_tools()));

    let messages = vec![Message {
        role: MessageRole::User,
        content: "List Rust files".to_string(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }];

    let result = runner
        .run("You are a helpful assistant.".to_string(), messages)
        .await
        .unwrap();

    assert_eq!(result.content, "Found 3 Rust files.");
    assert_eq!(result.iterations_used, 2);
    assert_eq!(result.tool_calls_made, 1);
}

/// Test: Agent stops at max iterations when the model keeps requesting tool calls.
#[tokio::test]
async fn test_agent_max_iterations() {
    let mut config = make_config();
    config.agent.max_iterations = 3;
    let config = Arc::new(config);

    let providers = make_providers(vec![
        CompletionResponse {
            content: Some(String::new()),
            tool_calls: Some(vec![ToolCall {
                id: "call_loop".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "glob".to_string(),
                    arguments: r#"{"pattern":"*"}"#.to_string(),
                },
            }]),
            usage: None,
            finish_reason: Some("tool_calls".to_string()),
        };
        10
    ]);

    let runner = AgentRunner::new(config, Arc::new(providers), Arc::new(make_tools()));

    let messages = vec![Message {
        role: MessageRole::User,
        content: "Loop forever".to_string(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }];

    let result = runner
        .run("You are a helpful assistant.".to_string(), messages)
        .await
        .unwrap();

    assert_eq!(result.iterations_used, 3);
    assert!(result.content.contains("maximum number of iterations"));
}
