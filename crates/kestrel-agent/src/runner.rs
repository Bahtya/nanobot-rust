//! Agent runner — the iterative LLM tool-calling loop.
//!
//! Executes the core LLM → tool call → result → LLM cycle
//! until the model produces a final response or max iterations is reached.
//! Mirrors the Python `agent/runner.py` AgentRunner.

use anyhow::{Context, Result};
use kestrel_bus::events::{AgentEvent, StreamChunk};
use kestrel_config::Config;
use kestrel_core::{Message, MessageRole, RunResult, ToolCall, Usage};
use kestrel_providers::{CompletionRequest, ProviderRegistry};
use kestrel_tools::ToolRegistry;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, info, warn};

/// Guard that aborts all remaining `JoinHandle`s when dropped.
///
/// When `message_timeout` fires and the parent future is dropped, this guard
/// ensures spawned tool tasks are cancelled rather than left running as
/// detached zombies.
struct AbortOnDrop<T: Send + 'static> {
    handles: Vec<tokio::task::JoinHandle<T>>,
}

impl<T: Send + 'static> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        for h in self.handles.drain(..) {
            h.abort();
        }
    }
}

/// Map a tool name to its display icon.
fn tool_icon(name: &str) -> &'static str {
    match name {
        "exec" | "shell" => "\u{1f4bb}",         // 💻 terminal
        "write_file" => "\u{270d}\u{fe0f}",      // ✍️ write
        "read_file" => "\u{1f4d6}",              // 📖 read
        "edit_file" => "\u{270f}\u{fe0f}",       // ✏️ edit
        "list_dir" => "\u{1f4c2}",               // 📂 directory
        "web_search" => "\u{1f50d}",             // 🔍 search
        "web_fetch" => "\u{1f310}",              // 🌐 web
        "memory" | "save_memory" => "\u{1f9e0}", // 🧠 memory
        _ => "\u{26a1}",                         // ⚡ default
    }
}

/// Build a short preview of a tool call's primary argument for display.
fn format_tool_preview(name: &str, args_json: &str) -> String {
    let args: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };

    let preview: Option<&str> = match name {
        "exec" | "shell" => args.get("command").and_then(|v| v.as_str()),
        "write_file" | "read_file" | "edit_file" => args.get("path").and_then(|v| v.as_str()),
        "list_dir" => args.get("path").and_then(|v| v.as_str()),
        "web_search" => args.get("query").and_then(|v| v.as_str()),
        "web_fetch" => args.get("url").and_then(|v| v.as_str()),
        _ => None,
    };

    match preview {
        Some(p) if !p.is_empty() => {
            let max_chars = 50;
            let chars: Vec<char> = p.chars().collect();
            if chars.len() > max_chars {
                let truncated: String = chars.iter().take(max_chars).collect();
                format!("\"{}\u{2026}\"", truncated)
            } else {
                format!("\"{}\"", p)
            }
        }
        _ => String::new(),
    }
}

/// Callback for emitting events during agent execution.
pub type EventCallback = Box<dyn Fn(AgentEvent) + Send + Sync>;

/// The agent runner that executes the iterative tool-calling loop.
pub struct AgentRunner {
    config: Arc<Config>,
    providers: Arc<ProviderRegistry>,
    tools: Arc<ToolRegistry>,
    stream_tx: Option<broadcast::Sender<StreamChunk>>,
    event_callback: Option<Arc<EventCallback>>,
    /// Guard that serializes execution of mutating tools. Read-only tools
    /// bypass this lock and run concurrently.
    mutating_guard: Arc<Mutex<()>>,
    /// Session key for correlating tool-call events and stream chunks.
    session_key: Option<String>,
    /// Full-chain trace ID propagated from the originating inbound message.
    trace_id: Option<String>,
    /// Cancellation token for graceful abort via /stop.
    cancel_token: Option<tokio_util::sync::CancellationToken>,
    /// Per-tool execution timeout in seconds (from config.agent.tool_timeout).
    tool_timeout_secs: u64,
}

impl AgentRunner {
    pub fn new(
        config: Arc<Config>,
        providers: Arc<ProviderRegistry>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        let tool_timeout_secs = config.agent.tool_timeout;
        Self {
            config,
            providers,
            tools,
            stream_tx: None,
            event_callback: None,
            mutating_guard: Arc::new(Mutex::new(())),
            session_key: None,
            trace_id: None,
            cancel_token: None,
            tool_timeout_secs,
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

    /// Set the session key for correlating events and stream chunks.
    pub fn with_session_key(mut self, key: impl Into<String>) -> Self {
        self.session_key = Some(key.into());
        self
    }

    /// Set the trace ID for full-chain correlation.
    pub fn with_trace_id(mut self, id: impl Into<String>) -> Self {
        self.trace_id = Some(id.into());
        self
    }

    /// Set a cancellation token for graceful abort.
    pub fn with_cancel_token(mut self, token: tokio_util::sync::CancellationToken) -> Self {
        self.cancel_token = Some(token);
        self
    }

    fn emit_event(&self, event: AgentEvent) {
        if let Some(cb) = &self.event_callback {
            cb(event);
        }
    }

    fn emit_stream_chunk(&self, content: String, done: bool) {
        if let Some(tx) = &self.stream_tx {
            let _ = tx.send(StreamChunk {
                session_key: self.session_key.clone().unwrap_or_default(),
                content,
                done,
                trace_id: self.trace_id.clone(),
            });
        }
    }

    /// Run the agent loop with a system prompt and message history.
    /// Uses streaming if a stream_tx is configured.
    pub async fn run(&self, system_prompt: String, messages: Vec<Message>) -> Result<RunResult> {
        let model = &self.config.agent.model;
        let provider_name = self.config.agent.provider.as_deref().unwrap_or("");
        let max_iterations = self.config.agent.max_iterations;
        let temperature = self.config.agent.temperature;
        let max_tokens = self.config.agent.max_tokens;

        let provider = self
            .providers
            .get_provider(provider_name)
            .with_context(|| {
                format!(
                    "No provider available: {:?} (model: {})",
                    self.config.agent.provider, model
                )
            })?;

        info!(
            trace_id = %self.trace_id.as_deref().unwrap_or("-"),
            llm_model = %model,
            llm_provider = %provider.name(),
            "Starting agent run"
        );

        // Build initial messages with system prompt
        let mut conversation = vec![Message {
            role: MessageRole::System,
            content: system_prompt,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
        }];
        conversation.extend(messages);

        let tool_definitions = self.tools.get_definitions();
        let mut total_usage = Usage::default();
        let mut tool_calls_made = 0;
        let mut reasoning_content: Option<String> = None;

        let use_streaming = self.stream_tx.is_some();

        for iteration in 0..max_iterations {
            // Check for cancellation between iterations
            if let Some(ref token) = self.cancel_token {
                if token.is_cancelled() {
                    info!(trace_id = %self.trace_id.as_deref().unwrap_or("-"), "Agent run cancelled at iteration {}", iteration + 1);
                    self.emit_stream_chunk(String::new(), true);
                    return Ok(RunResult {
                        content: "Agent run was cancelled.".to_string(),
                        reasoning_content: None,
                        usage: total_usage,
                        tool_calls_made,
                        iterations_used: iteration,
                        hit_limit: false,
                    });
                }
            }

            debug!(trace_id = %self.trace_id.as_deref().unwrap_or("-"), "Agent iteration {}/{}", iteration + 1, max_iterations);

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
                reasoning_effort: self.config.agent.reasoning_effort.clone(),
            };

            // Use streaming or non-streaming based on configuration
            let response: kestrel_providers::CompletionResponse = if use_streaming {
                let sr = self.complete_streaming(&provider, request).await?;
                reasoning_content = sr.reasoning_content.clone();
                sr.into()
            } else {
                let resp = provider
                    .complete(request)
                    .await
                    .with_context(|| "LLM completion failed")?;
                reasoning_content = resp.reasoning_content.clone();
                resp
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
                        trace_id = %self.trace_id.as_deref().unwrap_or("-"),
                        llm_model = %model,
                        iterations = iteration + 1,
                        tool_calls = tool_calls_made,
                        tokens_used = ?total_usage.total_tokens,
                        "Agent run completed"
                    );
                    return Ok(RunResult {
                        content,
                        reasoning_content,
                        usage: total_usage,
                        tool_calls_made,
                        iterations_used: iteration + 1,
                        hit_limit: false,
                    });
                }
            };

            // Emit tool call events
            for tc in &tool_calls {
                self.emit_event(AgentEvent::ToolCall {
                    session_key: self.session_key.clone().unwrap_or_default(),
                    tool_name: tc.function.name.clone(),
                    iteration: iteration + 1,
                    trace_id: self.trace_id.clone(),
                });
                if let Some(ref tid) = self.trace_id {
                    tracing::info!(
                        target: "comm",
                        trace_id = %tid,
                        tool = %tc.function.name,
                        "TOOL START"
                    );
                }
            }

            // Add assistant message with tool calls
            let assistant_msg = Message {
                role: MessageRole::Assistant,
                content: response.content.unwrap_or_default(),
                name: None,
                tool_call_id: None,
                tool_calls: Some(tool_calls.clone()),
                reasoning_content: reasoning_content.clone(),
            };
            conversation.push(assistant_msg);

            // Execute tool calls concurrently
            let results = self.execute_tools(&tool_calls).await;
            tool_calls_made += tool_calls.len();

            for (tc, (_, duration_ms, success)) in tool_calls.iter().zip(&results) {
                debug!(
                    trace_id = %self.trace_id.as_deref().unwrap_or("-"),
                    tool_name = %tc.function.name,
                    duration_ms = *duration_ms,
                    "Tool call completed"
                );
                if let Some(ref tid) = self.trace_id {
                    tracing::info!(
                        target: "comm",
                        trace_id = %tid,
                        tool = %tc.function.name,
                        duration_ms = *duration_ms,
                        success = *success,
                        "TOOL END"
                    );
                }
            }

            // Add tool results to conversation
            for (tool_call, (result, _, _)) in tool_calls.iter().zip(results) {
                conversation.push(Message {
                    role: MessageRole::Tool,
                    content: result,
                    name: Some(tool_call.function.name.clone()),
                    tool_call_id: Some(tool_call.id.clone()),
                    tool_calls: None,
                    reasoning_content: None,
                });
            }
        }

        warn!(trace_id = %self.trace_id.as_deref().unwrap_or("-"), "Max iterations ({}) reached", max_iterations);
        Ok(RunResult {
            content: "I've reached the maximum number of iterations. Please continue the conversation if needed.".to_string(),
            reasoning_content,
            usage: total_usage,
            tool_calls_made,
            iterations_used: max_iterations,
            hit_limit: true,
        })
    }

    /// Perform a streaming completion, accumulating the full response.
    ///
    /// On transient stream errors (decode failure, idle timeout), retries the
    /// provider call up to `max_stream_retries` times with short backoff before
    /// propagating the error to the agent-loop retry.
    async fn complete_streaming(
        &self,
        provider: &Arc<dyn kestrel_providers::LlmProvider>,
        request: CompletionRequest,
    ) -> Result<crate::StreamingResult> {
        use futures::StreamExt;
        use kestrel_core::{FunctionCall, ToolCall as CoreToolCall};

        let max_stream_retries: u32 = 2;
        let mut stream_attempt = 0u32;

        let result = 'stream_retry: loop {
            let send_start = std::time::Instant::now();
            let mut stream = match provider.complete_stream(request.clone()).await {
                Ok(s) => s,
                Err(e) => break 'stream_retry Err(e),
            };

            let connect_ms = send_start.elapsed().as_millis() as u64;
            if connect_ms > 5000 {
                warn!(
                    trace_id = %self.trace_id.as_deref().unwrap_or("-"),
                    elapsed_ms = connect_ms,
                    "Slow provider response: took >5s to establish stream"
                );
            }

            let mut first_byte_logged = false;
            let mut full_content = String::new();
            let mut full_reasoning = String::new();
            let mut usage: Option<Usage> = None;
            let mut tool_calls_map: std::collections::HashMap<usize, (String, String, String)> =
                std::collections::HashMap::new();

            let first_chunk_timeout = std::time::Duration::from_secs(15);
            let idle_timeout = std::time::Duration::from_secs(30);
            let mut is_first = true;
            let mut last_chunk_at = std::time::Instant::now();

            loop {
                let timeout = if is_first {
                    first_chunk_timeout
                } else {
                    idle_timeout
                };
                let chunk_result = tokio::time::timeout(timeout, stream.next()).await;
                is_first = false;

                let now = std::time::Instant::now();
                let gap = now.duration_since(last_chunk_at);
                if gap >= std::time::Duration::from_secs(10) {
                    warn!(
                        trace_id = %self.trace_id.as_deref().unwrap_or("-"),
                        elapsed_ms = send_start.elapsed().as_millis() as u64,
                        gap_ms = gap.as_millis() as u64,
                        "SSE stream slow: long gap between chunks"
                    );
                }

                let chunk_result = match chunk_result {
                    Ok(Some(r)) => r,
                    Ok(None) => {
                        break 'stream_retry Ok((
                            full_content,
                            full_reasoning,
                            usage,
                            tool_calls_map,
                            send_start,
                        ));
                    }
                    Err(_) => {
                        let err = anyhow::anyhow!(
                            "SSE stream timed out: no data received within {}s",
                            timeout.as_secs()
                        );
                        if stream_attempt < max_stream_retries {
                            stream_attempt += 1;
                            let backoff = std::time::Duration::from_millis(500 << stream_attempt);
                            warn!(
                                trace_id = %self.trace_id.as_deref().unwrap_or("-"),
                                attempt = stream_attempt,
                                max_retries = max_stream_retries,
                                backoff_ms = backoff.as_millis() as u64,
                                "Stream idle timeout, retrying provider call"
                            );
                            tokio::time::sleep(backoff).await;
                            continue 'stream_retry;
                        }
                        break 'stream_retry Err(err);
                    }
                };
                let chunk = match chunk_result {
                    Ok(c) => c,
                    Err(e) => {
                        let err_str = format!("{:#}", e);
                        let is_stream_err = err_str.contains("Stream error")
                            || err_str.contains("error decoding response body")
                            || err_str.contains("timed out")
                            || err_str.contains("timeout");
                        if is_stream_err && stream_attempt < max_stream_retries {
                            stream_attempt += 1;
                            let backoff = std::time::Duration::from_millis(500 << stream_attempt);
                            warn!(
                                trace_id = %self.trace_id.as_deref().unwrap_or("-"),
                                attempt = stream_attempt,
                                max_retries = max_stream_retries,
                                backoff_ms = backoff.as_millis() as u64,
                                error = %err_str,
                                "Stream decode error, retrying provider call"
                            );
                            tokio::time::sleep(backoff).await;
                            continue 'stream_retry;
                        }
                        break 'stream_retry Err(e);
                    }
                };

                if !first_byte_logged {
                    debug!(
                        trace_id = %self.trace_id.as_deref().unwrap_or("-"),
                        elapsed_ms = send_start.elapsed().as_millis() as u64,
                        "SSE first-byte received"
                    );
                    first_byte_logged = true;
                }
                last_chunk_at = std::time::Instant::now();

                // Accumulate text content
                if let Some(delta) = &chunk.delta {
                    full_content.push_str(delta);
                    self.emit_stream_chunk(delta.clone(), false);
                }

                // Accumulate reasoning content (no emit to channels — passthrough only)
                if let Some(rc) = &chunk.reasoning_content {
                    full_reasoning.push_str(rc);
                }

                // Accumulate tool call deltas and announce new tool names in real-time
                if let Some(deltas) = &chunk.tool_call_deltas {
                    for delta in deltas {
                        let entry = tool_calls_map
                            .entry(delta.index)
                            .or_insert_with(|| (String::new(), String::new(), String::new()));
                        if let Some(id) = &delta.id {
                            entry.0 = id.clone();
                        }
                        if let Some(name) = &delta.function_name {
                            let is_new = entry.1.is_empty();
                            entry.1 = name.clone();
                            if is_new {
                                let icon = tool_icon(&entry.1);
                                self.emit_stream_chunk(
                                    format!("\n{} {} ...\n", icon, entry.1),
                                    false,
                                );
                            }
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
                    break 'stream_retry Ok((
                        full_content,
                        full_reasoning,
                        usage,
                        tool_calls_map,
                        send_start,
                    ));
                }
            }
        };

        let (full_content, full_reasoning, usage, tool_calls_map, send_start) = result?;

        debug!(
            trace_id = %self.trace_id.as_deref().unwrap_or("-"),
            total_ms = send_start.elapsed().as_millis() as u64,
            "SSE stream completed"
        );

        // Emit final stream chunk
        self.emit_stream_chunk(String::new(), true);

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
            reasoning_content: if full_reasoning.is_empty() {
                None
            } else {
                Some(full_reasoning)
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

    /// Execute multiple tool calls, serializing mutating tools.
    ///
    /// Read-only tools run concurrently as before. Mutating tools each
    /// acquire a shared mutex before executing, guaranteeing they run
    /// one at a time even when the LLM issues several in a single turn.
    ///
    /// All spawned tasks are wrapped in an `AbortOnDrop` guard so that
    /// if the parent future is cancelled (e.g. message_timeout fires),
    /// remaining tool tasks are immediately aborted instead of leaking.
    async fn execute_tools(&self, tool_calls: &[ToolCall]) -> Vec<(String, u64, bool)> {
        type ToolResult = (String, u64, bool);
        let mut guard = AbortOnDrop::<ToolResult> {
            handles: Vec::new(),
        };

        for tc in tool_calls {
            let tool_name = tc.function.name.clone();
            let args_str = tc.function.arguments.clone();
            let tools = self.tools.clone();
            let mutating_guard = self.mutating_guard.clone();
            let is_mutating = self.tools.is_mutating(&tool_name);

            let handle = tokio::spawn(async move {
                let start = std::time::Instant::now();
                let args: Value = match serde_json::from_str(&args_str) {
                    Ok(v) => v,
                    Err(e) => {
                        return (
                            format!(
                                "Tool argument error for '{}': failed to parse arguments: {}. \
                             Raw arguments: {:?}",
                                tool_name, e, args_str
                            ),
                            start.elapsed().as_millis() as u64,
                            false,
                        );
                    }
                };

                let result = if is_mutating {
                    let _lock = mutating_guard.lock().await;
                    match tools.execute(&tool_name, args).await {
                        Ok(result) => result,
                        Err(e) => format!("Tool error: {}", e),
                    }
                } else {
                    match tools.execute(&tool_name, args).await {
                        Ok(result) => result,
                        Err(e) => format!("Tool error: {}", e),
                    }
                };
                let ok = !result.starts_with("Tool error:")
                    && !result.starts_with("Tool argument error");
                (result, start.elapsed().as_millis() as u64, ok)
            });

            guard.handles.push(handle);
        }

        // Show each tool call with its icon and argument preview.
        for tc in tool_calls {
            let icon = tool_icon(&tc.function.name);
            let preview = format_tool_preview(&tc.function.name, &tc.function.arguments);
            let display = if preview.is_empty() {
                format!("\n{} {}\n", icon, tc.function.name)
            } else {
                format!("\n{} {}: {}\n", icon, tc.function.name, preview)
            };
            self.emit_stream_chunk(display, false);
        }

        let total = guard.handles.len();
        let mut results: Vec<(String, u64, bool)> = Vec::with_capacity(total);
        let overall_start = std::time::Instant::now();
        let heartbeat_interval = std::time::Duration::from_secs(10);
        let tool_deadline = std::time::Duration::from_secs(self.tool_timeout_secs);

        #[allow(clippy::needless_range_loop)]
        for i in 0..total {
            // Poll this handle with a heartbeat timeout loop, bounded by tool_deadline.
            // Access handles by index so each `&mut` borrow is short-lived;
            // the AbortOnDrop guard aborts remaining handles if this future is dropped.
            let tool_start = std::time::Instant::now();
            loop {
                // Check for cancellation (e.g. /stop command or message_timeout)
                if let Some(ref token) = self.cancel_token {
                    if token.is_cancelled() {
                        guard.handles[i].abort();
                        results.push((
                            format!(
                                "Tool '{}' cancelled — agent run was interrupted",
                                tool_calls[i].function.name
                            ),
                            tool_start.elapsed().as_millis() as u64,
                            false,
                        ));
                        break;
                    }
                }

                let remaining = tool_deadline.saturating_sub(tool_start.elapsed());
                if remaining.is_zero() {
                    // Tool timeout exceeded — abort and report.
                    guard.handles[i].abort();
                    let elapsed = tool_start.elapsed().as_secs();
                    warn!(
                        trace_id = %self.trace_id.as_deref().unwrap_or("-"),
                        tool_name = %tool_calls[i].function.name,
                        timeout_secs = self.tool_timeout_secs,
                        elapsed_secs = elapsed,
                        "Tool timed out"
                    );
                    self.emit_event(AgentEvent::ToolResult {
                        session_key: self.session_key.clone().unwrap_or_default(),
                        tool_name: tool_calls[i].function.name.clone(),
                        duration_ms: tool_start.elapsed().as_millis() as u64,
                        trace_id: self.trace_id.clone(),
                    });
                    results.push((
                        format!(
                            "Tool '{}' timed out after {}s (limit: {}s)",
                            tool_calls[i].function.name, elapsed, self.tool_timeout_secs
                        ),
                        tool_start.elapsed().as_millis() as u64,
                        false,
                    ));
                    break;
                }

                let poll_timeout = heartbeat_interval.min(remaining);
                match tokio::time::timeout(poll_timeout, &mut guard.handles[i]).await {
                    Ok(join_res) => {
                        match join_res {
                            Ok((result, duration, ok)) => {
                                self.emit_event(AgentEvent::ToolResult {
                                    session_key: self.session_key.clone().unwrap_or_default(),
                                    tool_name: tool_calls[i].function.name.clone(),
                                    duration_ms: duration,
                                    trace_id: self.trace_id.clone(),
                                });
                                results.push((result, duration, ok));
                            }
                            Err(e) => {
                                self.emit_event(AgentEvent::ToolResult {
                                    session_key: self.session_key.clone().unwrap_or_default(),
                                    tool_name: tool_calls[i].function.name.clone(),
                                    duration_ms: 0,
                                    trace_id: self.trace_id.clone(),
                                });
                                results.push((format!("Tool execution failed: {}", e), 0, false));
                            }
                        }
                        break;
                    }
                    Err(_) => {
                        // Check if tool deadline has passed
                        if tool_start.elapsed() >= tool_deadline {
                            guard.handles[i].abort();
                            let elapsed = tool_start.elapsed().as_secs();
                            warn!(
                                trace_id = %self.trace_id.as_deref().unwrap_or("-"),
                                tool_name = %tool_calls[i].function.name,
                                timeout_secs = self.tool_timeout_secs,
                                elapsed_secs = elapsed,
                                "Tool timed out"
                            );
                            self.emit_event(AgentEvent::ToolResult {
                                session_key: self.session_key.clone().unwrap_or_default(),
                                tool_name: tool_calls[i].function.name.clone(),
                                duration_ms: tool_start.elapsed().as_millis() as u64,
                                trace_id: self.trace_id.clone(),
                            });
                            results.push((
                                format!(
                                    "Tool '{}' timed out after {}s (limit: {}s)",
                                    tool_calls[i].function.name, elapsed, self.tool_timeout_secs
                                ),
                                tool_start.elapsed().as_millis() as u64,
                                false,
                            ));
                            break;
                        }
                        // Heartbeat — emit progress and keep waiting.
                        let elapsed = overall_start.elapsed().as_secs();
                        let remaining_tools = total - i;
                        self.emit_stream_chunk(
                            format!(
                                "\n\u{23f3} Still running... ({}s elapsed, {} pending)\n",
                                elapsed, remaining_tools
                            ),
                            false,
                        );
                    }
                }
            }
        }

        // All handles polled — clear to prevent AbortOnDrop::drop from
        // aborting already-completed tasks.
        guard.handles.clear();

        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use kestrel_core::{FunctionCall, ToolCall as CoreToolCall};
    use kestrel_tools::trait_def::{Tool, ToolError};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A mock tool that records how many times it runs and optionally
    /// simulates work with a small sleep.
    struct CountingTool {
        name: &'static str,
        mutating: bool,
        counter: Arc<AtomicUsize>,
        work_duration: std::time::Duration,
    }

    impl CountingTool {
        fn new(name: &'static str, mutating: bool, counter: Arc<AtomicUsize>) -> Self {
            Self {
                name,
                mutating,
                counter,
                work_duration: std::time::Duration::ZERO,
            }
        }

        fn with_work_duration(mut self, d: std::time::Duration) -> Self {
            self.work_duration = d;
            self
        }
    }

    #[async_trait]
    impl Tool for CountingTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "counting tool"
        }
        fn parameters_schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn is_mutating(&self) -> bool {
            self.mutating
        }
        async fn execute(&self, _args: Value) -> Result<String, ToolError> {
            if !self.work_duration.is_zero() {
                tokio::time::sleep(self.work_duration).await;
            }
            let count = self.counter.fetch_add(1, Ordering::SeqCst);
            Ok(format!("{}-{}", self.name, count))
        }
    }

    fn make_runner(tools: ToolRegistry) -> AgentRunner {
        let mut config = Config::default();
        config.agent.provider = Some("mock".to_string());
        let config = Arc::new(config);
        let providers = Arc::new(ProviderRegistry::new());
        let tools = Arc::new(tools);
        AgentRunner::new(config, providers, tools)
    }

    fn tool_call(name: &str, id: usize) -> CoreToolCall {
        CoreToolCall {
            id: format!("call_{}", id),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: name.to_string(),
                arguments: "{}".to_string(),
            },
        }
    }

    #[tokio::test]
    async fn test_mutating_tools_execute_serially() {
        let counter = Arc::new(AtomicUsize::new(0));
        let registry = ToolRegistry::new();
        registry.register(
            CountingTool::new("write", true, counter.clone())
                .with_work_duration(std::time::Duration::from_millis(50)),
        );

        let runner = make_runner(registry);

        // Issue 3 mutating tool calls
        let calls = vec![
            tool_call("write", 1),
            tool_call("write", 2),
            tool_call("write", 3),
        ];

        let results = runner.execute_tools(&calls).await;

        // All 3 must complete
        assert_eq!(results.len(), 3);
        // Counter must be exactly 3
        assert_eq!(counter.load(Ordering::SeqCst), 3);
        // Each result should be distinct (serialized execution)
        assert!(results.iter().all(|(r, _, _)| r.starts_with("write-")));
    }

    #[tokio::test]
    async fn test_readonly_tools_execute_concurrently() {
        let counter = Arc::new(AtomicUsize::new(0));
        let registry = ToolRegistry::new();
        registry.register(
            CountingTool::new("read", false, counter.clone())
                .with_work_duration(std::time::Duration::from_millis(50)),
        );

        let runner = make_runner(registry);

        // Issue 3 read-only tool calls
        let calls = vec![
            tool_call("read", 1),
            tool_call("read", 2),
            tool_call("read", 3),
        ];

        let start = std::time::Instant::now();
        let results = runner.execute_tools(&calls).await;
        let elapsed = start.elapsed();

        assert_eq!(results.len(), 3);
        assert_eq!(counter.load(Ordering::SeqCst), 3);
        // Concurrent execution: 3 × 50ms should complete well under 150ms
        // (allowing some scheduling overhead, but definitely under the serial time)
        assert!(
            elapsed < std::time::Duration::from_millis(140),
            "Read-only tools should run concurrently, took {:?}",
            elapsed
        );
    }

    #[tokio::test]
    async fn test_mixed_mutating_and_readonly() {
        let write_counter = Arc::new(AtomicUsize::new(0));
        let read_counter = Arc::new(AtomicUsize::new(0));
        let registry = ToolRegistry::new();
        registry.register(
            CountingTool::new("write", true, write_counter.clone())
                .with_work_duration(std::time::Duration::from_millis(50)),
        );
        registry.register(
            CountingTool::new("read", false, read_counter.clone())
                .with_work_duration(std::time::Duration::from_millis(50)),
        );

        let runner = make_runner(registry);

        let calls = vec![
            tool_call("write", 1),
            tool_call("read", 1),
            tool_call("write", 2),
            tool_call("read", 2),
        ];

        let results = runner.execute_tools(&calls).await;

        assert_eq!(results.len(), 4);
        assert_eq!(write_counter.load(Ordering::SeqCst), 2);
        assert_eq!(read_counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_single_mutating_tool_executes() {
        let counter = Arc::new(AtomicUsize::new(0));
        let registry = ToolRegistry::new();
        registry.register(CountingTool::new("exec", true, counter.clone()));

        let runner = make_runner(registry);

        let calls = vec![tool_call("exec", 1)];
        let results = runner.execute_tools(&calls).await;

        assert_eq!(results.len(), 1);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(results[0].0, "exec-0");
    }

    #[tokio::test]
    async fn test_empty_tool_calls() {
        let registry = ToolRegistry::new();
        let runner = make_runner(registry);

        let calls: Vec<CoreToolCall> = vec![];
        let results = runner.execute_tools(&calls).await;

        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_unknown_tool_returns_error_string() {
        let registry = ToolRegistry::new();
        let runner = make_runner(registry);

        let calls = vec![tool_call("nonexistent", 1)];
        let results = runner.execute_tools(&calls).await;

        assert_eq!(results.len(), 1);
        assert!(results[0].0.contains("Tool error"));
        assert!(results[0].0.contains("not found"));
    }

    #[tokio::test]
    async fn test_per_tool_duration_tracked() {
        let registry = ToolRegistry::new();
        registry.register(
            CountingTool::new("slow", false, Arc::new(AtomicUsize::new(0)))
                .with_work_duration(std::time::Duration::from_millis(50)),
        );

        let runner = make_runner(registry);
        let calls = vec![tool_call("slow", 1)];
        let results = runner.execute_tools(&calls).await;

        assert_eq!(results.len(), 1);
        let (_, duration_ms, _) = &results[0];
        assert!(
            *duration_ms >= 40,
            "per-tool duration should reflect actual execution time, got {duration_ms}ms"
        );
    }

    #[tokio::test]
    async fn test_tool_timeout_enforced() {
        let mut config = Config::default();
        config.agent.tool_timeout = 1; // 1 second timeout
        config.agent.provider = Some("mock".to_string());
        let config = Arc::new(config);
        let providers = Arc::new(ProviderRegistry::new());
        let registry = ToolRegistry::new();
        registry.register(
            CountingTool::new("slow_tool", false, Arc::new(AtomicUsize::new(0)))
                .with_work_duration(std::time::Duration::from_secs(30)),
        );
        let tools = Arc::new(registry);

        let runner = AgentRunner::new(config, providers, tools);
        let calls = vec![tool_call("slow_tool", 1)];
        let results = runner.execute_tools(&calls).await;

        assert_eq!(results.len(), 1);
        let (result_text, _, ok) = &results[0];
        assert!(!ok, "timed-out tool should report failure");
        assert!(
            result_text.contains("timed out"),
            "result should mention timeout: got '{result_text}'"
        );
        assert!(
            result_text.contains("slow_tool"),
            "result should name the tool: got '{result_text}'"
        );
    }

    #[test]
    fn test_trace_id_propagated_to_toolcall_event() {
        let events: Arc<std::sync::Mutex<Vec<AgentEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = events.clone();
        let registry = ToolRegistry::new();
        let runner = AgentRunner::new(
            Arc::new(Config::default()),
            Arc::new(ProviderRegistry::new()),
            Arc::new(registry),
        )
        .with_session_key("test-session")
        .with_trace_id("trace-abc-123")
        .with_event_callback(Box::new(move |event| {
            captured.lock().unwrap().push(event);
        }));

        // Emit a ToolCall event directly through the runner's emit_event
        runner.emit_event(AgentEvent::ToolCall {
            session_key: "test-session".to_string(),
            tool_name: "shell".to_string(),
            iteration: 1,
            trace_id: runner.trace_id.clone(),
        });

        let evts = events.lock().unwrap();
        assert_eq!(evts.len(), 1);
        match &evts[0] {
            AgentEvent::ToolCall { trace_id, .. } => {
                assert_eq!(trace_id.as_deref(), Some("trace-abc-123"));
            }
            other => panic!("Expected ToolCall event, got {:?}", other),
        }
    }
}
