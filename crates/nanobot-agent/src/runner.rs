//! Agent runner — the iterative LLM tool-calling loop.
//!
//! Executes the core LLM → tool call → result → LLM cycle
//! until the model produces a final response or max iterations is reached.
//! Mirrors the Python `agent/runner.py` AgentRunner.

use anyhow::{Context, Result};
use nanobot_bus::events::{AgentEvent, StreamChunk};
use nanobot_config::Config;
use nanobot_core::{Message, MessageRole, RunResult, ToolCall, Usage};
use nanobot_providers::{CompletionRequest, ProviderRegistry};
use nanobot_tools::ToolRegistry;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

/// Callback for emitting events during agent execution.
pub type EventCallback = Box<dyn Fn(AgentEvent) + Send + Sync>;

/// The agent runner that executes the iterative tool-calling loop.
pub struct AgentRunner {
    config: Arc<Config>,
    providers: Arc<ProviderRegistry>,
    tools: Arc<ToolRegistry>,
    stream_tx: Option<broadcast::Sender<StreamChunk>>,
    event_callback: Option<Arc<EventCallback>>,
}

impl AgentRunner {
    pub fn new(
        config: Arc<Config>,
        providers: Arc<ProviderRegistry>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        Self {
            config,
            providers,
            tools,
            stream_tx: None,
            event_callback: None,
        }
    }

    /// Set the streaming channel for real-time output.
    pub fn with_stream_tx(mut self, tx: broadcast::Sender<StreamChunk>) -> Self {
        self.stream_tx = Some(tx);
        self
    }

    /// Set a callback for agent lifecycle events (ToolCall, etc.).
    pub fn with_event_callback(mut self, cb: EventCallback) -> Self {
        self.event_callback = Some(Arc::new(cb));
        self
    }

    fn emit_event(&self, event: AgentEvent) {
        if let Some(cb) = &self.event_callback {
            cb(event);
        }
    }

    fn emit_stream_chunk(&self, session_key: &str, content: String, done: bool) {
        if let Some(tx) = &self.stream_tx {
            let _ = tx.send(StreamChunk {
                session_key: session_key.to_string(),
                content,
                done,
            });
        }
    }

    /// Run the agent loop with a system prompt and message history.
    /// Uses streaming if a stream_tx is configured.
    pub async fn run(&self, system_prompt: String, messages: Vec<Message>) -> Result<RunResult> {
        let model = &self.config.agent.model;
        let max_iterations = self.config.agent.max_iterations;
        let temperature = self.config.agent.temperature;
        let max_tokens = self.config.agent.max_tokens;

        let provider = self
            .providers
            .get_provider(model)
            .with_context(|| format!("No provider available for model: {}", model))?;

        // Build initial messages with system prompt
        let mut conversation = vec![Message {
            role: MessageRole::System,
            content: system_prompt,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }];
        conversation.extend(messages);

        let tool_definitions = self.tools.get_definitions();
        let mut total_usage = Usage::default();
        let mut tool_calls_made = 0;

        let use_streaming = self.stream_tx.is_some();

        for iteration in 0..max_iterations {
            debug!("Agent iteration {}/{}", iteration + 1, max_iterations);

            let request = CompletionRequest {
                model: model.clone(),
                messages: conversation.clone(),
                tools: if tool_definitions.is_empty() {
                    None
                } else {
                    Some(tool_definitions.clone())
                },
                max_tokens: Some(max_tokens),
                temperature: Some(temperature),
                stream: use_streaming,
            };

            // Use streaming or non-streaming based on configuration
            let response: nanobot_providers::CompletionResponse = if use_streaming {
                self.complete_streaming(&provider, request).await?.into()
            } else {
                provider
                    .complete(request)
                    .await
                    .with_context(|| "LLM completion failed")?
            };

            // Track usage
            if let Some(usage) = &response.usage {
                total_usage.prompt_tokens = total_usage.prompt_tokens.or(usage.prompt_tokens);
                total_usage.completion_tokens =
                    total_usage.completion_tokens.or(usage.completion_tokens);
                total_usage.total_tokens = total_usage.total_tokens.or(usage.total_tokens);
            }

            // If no tool calls, we're done
            let tool_calls = match response.tool_calls {
                Some(tc) if !tc.is_empty() => tc,
                _ => {
                    let content = response.content.unwrap_or_default();
                    info!(
                        "Agent completed in {} iterations, {} tool calls",
                        iteration + 1,
                        tool_calls_made
                    );
                    return Ok(RunResult {
                        content,
                        usage: total_usage,
                        tool_calls_made,
                        iterations_used: iteration + 1,
                    });
                }
            };

            // Emit tool call events
            for tc in &tool_calls {
                self.emit_event(AgentEvent::ToolCall {
                    session_key: String::new(), // filled by caller
                    tool_name: tc.function.name.clone(),
                    iteration: iteration + 1,
                });
            }

            // Add assistant message with tool calls
            let assistant_msg = Message {
                role: MessageRole::Assistant,
                content: response.content.unwrap_or_default(),
                name: None,
                tool_call_id: None,
                tool_calls: Some(tool_calls.clone()),
            };
            conversation.push(assistant_msg);

            // Execute tool calls concurrently
            let results = self.execute_tools(&tool_calls).await;
            tool_calls_made += tool_calls.len();

            // Add tool results to conversation
            for (tool_call, result) in tool_calls.iter().zip(results) {
                conversation.push(Message {
                    role: MessageRole::Tool,
                    content: result,
                    name: Some(tool_call.function.name.clone()),
                    tool_call_id: Some(tool_call.id.clone()),
                    tool_calls: None,
                });
            }
        }

        warn!("Max iterations ({}) reached", max_iterations);
        Ok(RunResult {
            content: "I've reached the maximum number of iterations. Please continue the conversation if needed.".to_string(),
            usage: total_usage,
            tool_calls_made,
            iterations_used: max_iterations,
        })
    }

    /// Perform a streaming completion, accumulating the full response.
    async fn complete_streaming(
        &self,
        provider: &Arc<dyn nanobot_providers::LlmProvider>,
        request: CompletionRequest,
    ) -> Result<crate::StreamingResult> {
        use futures::StreamExt;
        use nanobot_core::{FunctionCall, ToolCall as CoreToolCall};

        let mut stream = provider.complete_stream(request).await?;

        let mut full_content = String::new();
        let mut usage: Option<Usage> = None;
        let mut tool_calls_map: std::collections::HashMap<usize, (String, String, String)> =
            std::collections::HashMap::new();

        while let Some(chunk_result) = stream.next().await {
            let chunk = chunk_result?;

            // Accumulate text content
            if let Some(delta) = &chunk.delta {
                full_content.push_str(delta);
                // Emit streaming chunk
                self.emit_stream_chunk("", delta.clone(), false);
            }

            // Accumulate tool call deltas
            if let Some(deltas) = &chunk.tool_call_deltas {
                for delta in deltas {
                    let entry = tool_calls_map
                        .entry(delta.index)
                        .or_insert_with(|| (String::new(), String::new(), String::new()));
                    if let Some(id) = &delta.id {
                        entry.0 = id.clone();
                    }
                    if let Some(name) = &delta.function_name {
                        entry.1 = name.clone();
                    }
                    if let Some(args) = &delta.function_arguments {
                        entry.2.push_str(args);
                    }
                }
            }

            // Capture usage from final chunks
            if chunk.usage.is_some() {
                usage = chunk.usage.clone();
            }

            if chunk.done {
                break;
            }
        }

        // Emit final stream chunk
        self.emit_stream_chunk("", String::new(), true);

        // Build tool calls from accumulated deltas
        let mut tool_calls_list: Vec<(usize, CoreToolCall)> = tool_calls_map
            .into_iter()
            .map(|(idx, (id, name, args))| {
                (
                    idx,
                    CoreToolCall {
                        id,
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name,
                            arguments: args,
                        },
                    },
                )
            })
            .collect();
        tool_calls_list.sort_by_key(|(idx, _)| *idx);
        let tool_calls: Vec<CoreToolCall> = tool_calls_list.into_iter().map(|(_, tc)| tc).collect();

        Ok(crate::StreamingResult {
            content: if full_content.is_empty() && tool_calls.is_empty() {
                None
            } else {
                Some(full_content)
            },
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            usage,
            finish_reason: None,
        })
    }

    /// Execute multiple tool calls concurrently.
    async fn execute_tools(&self, tool_calls: &[ToolCall]) -> Vec<String> {
        let mut handles = Vec::new();

        for tc in tool_calls {
            let tool_name = tc.function.name.clone();
            let args_str = tc.function.arguments.clone();
            let tools = self.tools.clone();

            let handle = tokio::spawn(async move {
                let args: Value =
                    serde_json::from_str(&args_str).unwrap_or(Value::Object(Default::default()));

                match tools.execute(&tool_name, args).await {
                    Ok(result) => result,
                    Err(e) => format!("Tool error: {}", e),
                }
            });

            handles.push(handle);
        }

        let mut results = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(result) => results.push(result),
                Err(e) => results.push(format!("Tool execution failed: {}", e)),
            }
        }

        results
    }
}
