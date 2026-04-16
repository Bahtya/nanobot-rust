//! End-to-end agent runner tests using a mock LLM provider.
//!
//! Verifies that the agent loop correctly handles:
//! - Simple text responses (no tool calls)
//! - Tool call → tool result → final response
//! - Max iteration limits

use async_trait::async_trait;
use kestrel_agent::AgentRunner;
use kestrel_config::Config;
use kestrel_core::{FunctionCall, Message, MessageRole, ToolCall, Usage};
use kestrel_providers::base::{
    BoxStream, CompletionChunk, CompletionRequest, CompletionResponse, LlmProvider,
};
use kestrel_tools::ToolRegistry;
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
    kestrel_tools::builtins::register_all(&registry);
    registry
}

fn make_providers(responses: Vec<CompletionResponse>) -> kestrel_providers::ProviderRegistry {
    let mut registry = kestrel_providers::ProviderRegistry::new();
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

/// Test: Malformed tool arguments produce a descriptive error, not silent empty JSON.
/// The agent should see the parse error and be able to self-correct.
#[tokio::test]
async fn test_agent_tool_call_malformed_args_returns_error() {
    let config = Arc::new(make_config());

    // First LLM response: call a tool with malformed JSON arguments
    // Second LLM response: final text (the agent sees the error and replies)
    let providers = make_providers(vec![
        CompletionResponse {
            content: Some(String::new()),
            tool_calls: Some(vec![ToolCall {
                id: "call_malformed".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "glob".to_string(),
                    arguments: "not valid json {{{".to_string(),
                },
            }]),
            usage: None,
            finish_reason: Some("tool_calls".to_string()),
        },
        CompletionResponse {
            content: Some("I see the error, let me retry.".to_string()),
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
        content: "List files".to_string(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }];

    let result = runner
        .run("You are a helpful assistant.".to_string(), messages)
        .await
        .unwrap();

    // The agent should have completed (not crashed) and made 1 tool call
    assert_eq!(result.iterations_used, 2);
    assert_eq!(result.tool_calls_made, 1);
    // The final response should reflect the agent seeing the error
    assert_eq!(result.content, "I see the error, let me retry.");
}

/// Test: Tool argument error messages include the tool name and raw arguments.
/// We use a two-iteration mock to capture what the agent saw as the tool result.
#[tokio::test]
async fn test_agent_tool_arg_error_includes_details() {
    use std::sync::Mutex;

    // Shared capture buffer for inspecting what the agent received
    let captured_messages: Arc<Mutex<Option<Vec<Message>>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured_messages.clone();

    struct CaptureProvider {
        responses: Vec<CompletionResponse>,
        call_count: AtomicUsize,
        captured: Arc<Mutex<Option<Vec<Message>>>>,
    }

    impl CaptureProvider {
        fn new(
            responses: Vec<CompletionResponse>,
            captured: Arc<Mutex<Option<Vec<Message>>>>,
        ) -> Self {
            Self {
                responses,
                call_count: AtomicUsize::new(0),
                captured,
            }
        }
    }

    #[async_trait]
    impl LlmProvider for CaptureProvider {
        fn name(&self) -> &str {
            "capture"
        }

        async fn complete(&self, request: CompletionRequest) -> anyhow::Result<CompletionResponse> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            // On the second call (after tool execution), capture the messages
            if idx == 1 {
                let mut guard = self.captured.lock().unwrap();
                *guard = Some(request.messages.clone());
            }
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

    let malformed_args = "{broken json";
    let tool_name = "glob";

    let capture = CaptureProvider::new(
        vec![
            CompletionResponse {
                content: Some(String::new()),
                tool_calls: Some(vec![ToolCall {
                    id: "call_err".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: tool_name.to_string(),
                        arguments: malformed_args.to_string(),
                    },
                }]),
                usage: None,
                finish_reason: Some("tool_calls".to_string()),
            },
            CompletionResponse {
                content: Some("Done.".to_string()),
                tool_calls: None,
                usage: None,
                finish_reason: Some("stop".to_string()),
            },
        ],
        captured_clone,
    );

    let mut provider_reg = kestrel_providers::ProviderRegistry::new();
    provider_reg.register("capture", capture);
    provider_reg.set_default("capture");

    let mut config = make_config();
    config.agent.model = "mock-model".to_string();
    let runner = AgentRunner::new(
        Arc::new(config),
        Arc::new(provider_reg),
        Arc::new(make_tools()),
    );

    let messages = vec![Message {
        role: MessageRole::User,
        content: "List files".to_string(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }];

    let result = runner
        .run("You are a helpful assistant.".to_string(), messages)
        .await
        .unwrap();

    assert_eq!(result.iterations_used, 2);

    // Check that the tool result message contains the error details
    let captured = captured_messages.lock().unwrap();
    let msgs = captured.as_ref().unwrap();

    // Find the Tool message in the captured conversation
    let tool_msg = msgs
        .iter()
        .find(|m| matches!(m.role, MessageRole::Tool))
        .unwrap();
    let content = &tool_msg.content;

    // The error should mention the tool name, the parse failure, and the raw args
    assert!(
        content.contains(tool_name),
        "Error should mention tool name '{}', got: {}",
        tool_name,
        content
    );
    assert!(
        content.contains("failed to parse arguments"),
        "Error should mention parse failure, got: {}",
        content
    );
    assert!(
        content.contains(malformed_args),
        "Error should include raw arguments, got: {}",
        content
    );
}
