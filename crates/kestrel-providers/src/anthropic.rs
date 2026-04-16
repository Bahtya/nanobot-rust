//! Anthropic provider — native Claude API integration.

use async_trait::async_trait;
use kestrel_core::{FunctionCall, FunctionDefinition, Message, MessageRole, ToolCall, Usage};

use crate::base::{
    BoxStream, CompletionChunk, CompletionRequest, CompletionResponse, LlmProvider, ToolCallDelta,
};
use crate::build_client;
use crate::retry::{retry_with_backoff, RetryConfig};
use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::debug;

/// Anthropic API configuration.
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub api_key: String,
    pub model: String,
    pub api_version: Option<String>,
    pub base_url: Option<String>,
}

/// Anthropic Claude provider using the Messages API.
pub struct AnthropicProvider {
    config: AnthropicConfig,
    client: Client,
    retry: Arc<RetryConfig>,
}

impl AnthropicProvider {
    pub fn new(config: AnthropicConfig) -> anyhow::Result<Self> {
        // Anthropic is a foreign API — never skip proxy.
        let client = build_client(false)?;
        Ok(Self {
            config,
            client,
            retry: Arc::new(RetryConfig::default()),
        })
    }

    /// Create with a custom HTTP client (useful for testing).
    pub fn with_client(config: AnthropicConfig, client: Client) -> Self {
        Self {
            config,
            client,
            retry: Arc::new(RetryConfig::default()),
        }
    }

    /// Create with a custom retry configuration.
    pub fn with_retry(config: AnthropicConfig, retry: RetryConfig) -> anyhow::Result<Self> {
        let client = build_client(false)?;
        Ok(Self {
            config,
            client,
            retry: Arc::new(retry),
        })
    }

    /// Create with a custom HTTP client and retry configuration (useful for testing).
    pub fn with_client_and_retry(
        config: AnthropicConfig,
        client: Client,
        retry: RetryConfig,
    ) -> Self {
        Self {
            config,
            client,
            retry: Arc::new(retry),
        }
    }

    fn base_url(&self) -> &str {
        self.config
            .base_url
            .as_deref()
            .unwrap_or("https://api.anthropic.com")
    }

    /// Convert unified messages to Anthropic format.
    /// Returns (system_prompt, messages) since Anthropic separates system.
    fn convert_messages(&self, messages: &[Message]) -> (Option<String>, Vec<serde_json::Value>) {
        let mut system = None;
        let mut converted = Vec::new();

        for msg in messages {
            match msg.role {
                MessageRole::System => {
                    system = Some(msg.content.clone());
                }
                MessageRole::User | MessageRole::Assistant => {
                    let mut m = json!({
                        "role": match msg.role {
                            MessageRole::User => "user",
                            MessageRole::Assistant => "assistant",
                            _ => "user",
                        },
                        "content": msg.content,
                    });
                    if let Some(tool_calls) = &msg.tool_calls {
                        let content_blocks: Vec<serde_json::Value> =
                            vec![json!({"type": "text", "text": msg.content})];
                        let tool_use_blocks: Vec<serde_json::Value> = tool_calls
                            .iter()
                            .map(|tc| {
                                json!({
                                    "type": "tool_use",
                                    "id": tc.id,
                                    "name": tc.function.name,
                                    "input": serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                                        .unwrap_or(json!({})),
                                })
                            })
                            .collect();
                        m["content"] = json!(content_blocks
                            .into_iter()
                            .chain(tool_use_blocks)
                            .collect::<Vec<_>>());
                    }
                    converted.push(m);
                }
                MessageRole::Tool => {
                    let m = json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": msg.tool_call_id,
                            "content": msg.content,
                        }],
                    });
                    converted.push(m);
                }
            }
        }

        (system, converted)
    }

    fn convert_tools(&self, tools: &[FunctionDefinition]) -> Vec<serde_json::Value> {
        tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect()
    }

    /// Build the request body for the Anthropic API.
    fn build_request_body(&self, request: &CompletionRequest) -> serde_json::Value {
        let (system, messages) = self.convert_messages(&request.messages);

        let mut body = json!({
            "model": request.model,
            "messages": messages,
            "max_tokens": request.max_tokens.unwrap_or(4096),
        });

        if let Some(sys) = system {
            body["system"] = json!(sys);
        }
        if let Some(temp) = request.temperature {
            body["temperature"] = json!(temp);
        }
        if let Some(tools) = &request.tools {
            body["tools"] = json!(self.convert_tools(tools));
        }
        body
    }

    /// Parse SSE lines from byte stream and yield CompletionChunks.
    fn parse_sse_stream(
        byte_stream: impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    ) -> BoxStream {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<CompletionChunk>>(32);

        tokio::spawn(async move {
            use futures::StreamExt;
            let mut stream = byte_stream.boxed();
            let mut buffer = String::new();
            let mut tc_acc: HashMap<usize, (String, String, String)> = HashMap::new();

            while let Some(chunk_result) = stream.next().await {
                let bytes = match chunk_result {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(Err(anyhow::anyhow!("Stream error: {}", e))).await;
                        return;
                    }
                };
                buffer.push_str(&String::from_utf8_lossy(&bytes));

                // Process complete lines
                while let Some(pos) = buffer.find('\n') {
                    let line = buffer[..pos].trim_end().to_string();
                    buffer = buffer[pos + 1..].to_string();

                    if line.is_empty() {
                        continue;
                    }
                    let data_str = line
                        .strip_prefix("data: ")
                        .or_else(|| line.strip_prefix("data:"));
                    let data_str = match data_str {
                        Some(d) => d.trim(),
                        None => continue,
                    };

                    if data_str == "[DONE]" {
                        let tool_call_deltas = build_anthropic_tool_call_deltas(&tc_acc);
                        let _ = tx
                            .send(Ok(CompletionChunk {
                                delta: None,
                                tool_call_deltas,
                                usage: None,
                                done: true,
                            }))
                            .await;
                        return;
                    }

                    let event: serde_json::Value = match serde_json::from_str(data_str) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");

                    match event_type {
                        "content_block_delta" => {
                            // Text delta
                            if let Some(delta_text) = event
                                .get("delta")
                                .and_then(|d| d.get("text"))
                                .and_then(|t| t.as_str())
                            {
                                let _ = tx
                                    .send(Ok(CompletionChunk {
                                        delta: Some(delta_text.to_string()),
                                        tool_call_deltas: None,
                                        usage: None,
                                        done: false,
                                    }))
                                    .await;
                            }
                            // Tool call input delta
                            if let Some(partial_json) = event
                                .get("delta")
                                .and_then(|d| d.get("partial_json"))
                                .and_then(|p| p.as_str())
                            {
                                if let Some(idx) = event.get("index").and_then(|i| i.as_u64()) {
                                    let idx = idx as usize;
                                    let entry = tc_acc.entry(idx).or_insert_with(|| {
                                        (String::new(), String::new(), String::new())
                                    });
                                    entry.2.push_str(partial_json);
                                }
                            }
                        }
                        "content_block_start" => {
                            if let Some(content_block) = event.get("content_block") {
                                if content_block.get("type").and_then(|t| t.as_str())
                                    == Some("tool_use")
                                {
                                    if let Some(idx) = event.get("index").and_then(|i| i.as_u64()) {
                                        let id = content_block
                                            .get("id")
                                            .and_then(|i| i.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let name = content_block
                                            .get("name")
                                            .and_then(|n| n.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        tc_acc.insert(idx as usize, (id, name, String::new()));
                                    }
                                }
                            }
                        }
                        "message_delta" => {
                            let usage = event.get("usage").map(|u| Usage {
                                prompt_tokens: None,
                                completion_tokens: u.get("output_tokens").and_then(|v| v.as_u64()),
                                total_tokens: None,
                            });
                            let tool_call_deltas = build_anthropic_tool_call_deltas(&tc_acc);
                            tc_acc.clear();
                            let _ = tx
                                .send(Ok(CompletionChunk {
                                    delta: None,
                                    tool_call_deltas,
                                    usage,
                                    done: false,
                                }))
                                .await;
                        }
                        _ => {}
                    }
                }
            }
        });

        // Convert mpsc receiver into a stream
        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
    }
}

fn build_anthropic_tool_call_deltas(
    tc_acc: &HashMap<usize, (String, String, String)>,
) -> Option<Vec<ToolCallDelta>> {
    if tc_acc.is_empty() {
        return None;
    }
    let mut deltas: Vec<ToolCallDelta> = tc_acc
        .iter()
        .map(|(&idx, (id, name, args))| ToolCallDelta {
            index: idx,
            id: if id.is_empty() {
                None
            } else {
                Some(id.clone())
            },
            function_name: if name.is_empty() {
                None
            } else {
                Some(name.clone())
            },
            function_arguments: if args.is_empty() {
                None
            } else {
                Some(args.clone())
            },
        })
        .collect();
    deltas.sort_by_key(|d| d.index);
    Some(deltas)
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let url = format!("{}/v1/messages", self.base_url());
        let body = self.build_request_body(&request);
        let api_key = self.config.api_key.clone();
        let api_version = self
            .config
            .api_version
            .clone()
            .unwrap_or_else(|| "2023-06-01".to_string());
        let client = self.client.clone();
        let retry_config = self.retry.clone();

        debug!("Anthropic request to {} (model: {})", url, request.model);

        retry_with_backoff(&retry_config, move |_attempt| {
            let url = url.clone();
            let body = body.clone();
            let api_key = api_key.clone();
            let api_version = api_version.clone();
            let client = client.clone();
            async move {
                let resp = client
                    .post(&url)
                    .header("x-api-key", &api_key)
                    .header("anthropic-version", &api_version)
                    .header("content-type", "application/json")
                    .json(&body)
                    .send()
                    .await
                    .with_context(|| "Failed to send Anthropic request")?;

                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("Anthropic API error ({}): {}", status, text);
                }

                let api_resp: serde_json::Value = resp
                    .json()
                    .await
                    .context("Failed to parse Anthropic response")?;

                // Extract text content
                let content =
                    api_resp
                        .get("content")
                        .and_then(|c| c.as_array())
                        .and_then(|blocks| {
                            blocks
                                .iter()
                                .filter_map(|b| {
                                    if b.get("type")?.as_str()? == "text" {
                                        b.get("text")?.as_str().map(String::from)
                                    } else {
                                        None
                                    }
                                })
                                .collect::<Vec<_>>()
                                .into_iter()
                                .next()
                        });

                // Extract tool calls
                let tool_calls = api_resp
                    .get("content")
                    .and_then(|c| c.as_array())
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter_map(|b| {
                                if b.get("type")?.as_str()? == "tool_use" {
                                    Some(ToolCall {
                                        id: b.get("id")?.as_str()?.to_string(),
                                        call_type: "function".to_string(),
                                        function: FunctionCall {
                                            name: b.get("name")?.as_str()?.to_string(),
                                            arguments: serde_json::to_string(
                                                b.get("input").unwrap_or(&json!({})),
                                            )
                                            .unwrap_or_default(),
                                        },
                                    })
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                    })
                    .filter(|tc| !tc.is_empty());

                let usage = api_resp.get("usage").map(|u| Usage {
                    prompt_tokens: u.get("input_tokens").and_then(|v| v.as_u64()),
                    completion_tokens: u.get("output_tokens").and_then(|v| v.as_u64()),
                    total_tokens: None,
                });

                let stop_reason = api_resp
                    .get("stop_reason")
                    .and_then(|v| v.as_str())
                    .map(String::from);

                Ok(CompletionResponse {
                    content,
                    tool_calls,
                    usage,
                    finish_reason: stop_reason,
                })
            }
        })
        .await
    }

    async fn complete_stream(&self, request: CompletionRequest) -> Result<BoxStream> {
        let url = format!("{}/v1/messages", self.base_url());
        let mut body = self.build_request_body(&request);
        body["stream"] = json!(true);
        let api_key = self.config.api_key.clone();
        let api_version = self
            .config
            .api_version
            .clone()
            .unwrap_or_else(|| "2023-06-01".to_string());
        let retry_config = self.retry.clone();

        debug!(
            "Anthropic streaming request to {} (model: {})",
            url, request.model
        );

        retry_with_backoff(&retry_config, move |_attempt| {
            let url = url.clone();
            let body = body.clone();
            let api_key = api_key.clone();
            let api_version = api_version.clone();
            let client = self.client.clone();
            async move {
                let resp = client
                    .post(&url)
                    .header("x-api-key", &api_key)
                    .header("anthropic-version", &api_version)
                    .header("content-type", "application/json")
                    .json(&body)
                    .send()
                    .await
                    .with_context(|| "Failed to send Anthropic streaming request")?;

                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("Anthropic API error ({}): {}", status, text);
                }

                Ok(Self::parse_sse_stream(resp.bytes_stream()))
            }
        })
        .await
    }

    fn supports_model(&self, model: &str) -> bool {
        let lower = model.to_lowercase();
        lower.contains("claude") || lower.contains("anthropic")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_anthropic_config_construction() {
        let config = AnthropicConfig {
            api_key: "sk-test-123".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            api_version: Some("2023-06-01".to_string()),
            base_url: Some("https://custom.api.com".to_string()),
        };
        assert_eq!(config.api_key, "sk-test-123");
        assert_eq!(config.model, "claude-sonnet-4-20250514");
        assert_eq!(config.api_version.as_deref(), Some("2023-06-01"));
        assert_eq!(config.base_url.as_deref(), Some("https://custom.api.com"));
    }

    #[test]
    fn test_anthropic_provider_supports_model() {
        let provider = AnthropicProvider::new(AnthropicConfig {
            api_key: "test".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            api_version: None,
            base_url: None,
        })
        .unwrap();

        assert!(provider.supports_model("claude-3.5-sonnet"));
        assert!(provider.supports_model("claude-3-opus"));
        assert!(provider.supports_model("anthropic-model"));
        assert!(!provider.supports_model("gpt-4"));
        assert!(!provider.supports_model("llama-3"));
    }

    #[test]
    fn test_anthropic_provider_name() {
        let provider = AnthropicProvider::new(AnthropicConfig {
            api_key: "test".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            api_version: None,
            base_url: None,
        })
        .unwrap();
        assert_eq!(provider.name(), "anthropic");
    }

    #[test]
    fn test_convert_messages_system_and_user() {
        let provider = AnthropicProvider::new(AnthropicConfig {
            api_key: "test".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            api_version: None,
            base_url: None,
        })
        .unwrap();

        let messages = vec![
            Message {
                role: MessageRole::System,
                content: "You are helpful".to_string(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                role: MessageRole::User,
                content: "Hello".to_string(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let (system, converted) = provider.convert_messages(&messages);
        assert_eq!(system, Some("You are helpful".to_string()));
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["role"], "user");
        assert_eq!(converted[0]["content"], "Hello");
    }

    #[test]
    fn test_convert_messages_tool_result() {
        let provider = AnthropicProvider::new(AnthropicConfig {
            api_key: "test".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            api_version: None,
            base_url: None,
        })
        .unwrap();

        let messages = vec![Message {
            role: MessageRole::Tool,
            content: "result data".to_string(),
            name: Some("get_weather".to_string()),
            tool_call_id: Some("call_123".to_string()),
            tool_calls: None,
        }];

        let (system, converted) = provider.convert_messages(&messages);
        assert!(system.is_none());
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["role"], "user");
        let content = converted[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "call_123");
    }

    #[test]
    fn test_convert_messages_with_tool_calls() {
        let provider = AnthropicProvider::new(AnthropicConfig {
            api_key: "test".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            api_version: None,
            base_url: None,
        })
        .unwrap();

        let messages = vec![Message {
            role: MessageRole::Assistant,
            content: "Let me check".to_string(),
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_456".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "search".to_string(),
                    arguments: r#"{"query":"test"}"#.to_string(),
                },
            }]),
        }];

        let (system, converted) = provider.convert_messages(&messages);
        assert!(system.is_none());
        assert_eq!(converted.len(), 1);
        let content = converted[0]["content"].as_array().unwrap();
        // Should have text block + tool_use block
        assert!(content.iter().any(|b| b["type"] == "text"));
        assert!(content.iter().any(|b| b["type"] == "tool_use"));
    }

    #[test]
    fn test_convert_tools() {
        let provider = AnthropicProvider::new(AnthropicConfig {
            api_key: "test".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            api_version: None,
            base_url: None,
        })
        .unwrap();

        let tools = vec![FunctionDefinition {
            name: "get_weather".to_string(),
            description: Some("Get weather".to_string()),
            parameters: Some(json!({"type": "object", "properties": {"city": {"type": "string"}}})),
        }];

        let converted = provider.convert_tools(&tools);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["name"], "get_weather");
        assert_eq!(converted[0]["description"], "Get weather");
        assert!(converted[0]["input_schema"].is_object());
    }

    #[test]
    fn test_build_request_body() {
        let provider = AnthropicProvider::new(AnthropicConfig {
            api_key: "test".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            api_version: None,
            base_url: None,
        })
        .unwrap();

        let request = CompletionRequest {
            model: "claude-sonnet-4-20250514".to_string(),
            messages: vec![
                Message {
                    role: MessageRole::System,
                    content: "Be helpful".to_string(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
                Message {
                    role: MessageRole::User,
                    content: "Hi".to_string(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
            ],
            tools: None,
            max_tokens: Some(2048),
            temperature: Some(0.5),
            stream: false,
        };

        let body = provider.build_request_body(&request);
        assert_eq!(body["model"], "claude-sonnet-4-20250514");
        assert_eq!(body["max_tokens"], 2048);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["system"], "Be helpful");
        assert!(body["messages"].is_array());
    }

    #[test]
    fn test_base_url_default() {
        let provider = AnthropicProvider::new(AnthropicConfig {
            api_key: "test".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            api_version: None,
            base_url: None,
        })
        .unwrap();
        assert_eq!(provider.base_url(), "https://api.anthropic.com");
    }

    #[test]
    fn test_base_url_custom() {
        let provider = AnthropicProvider::new(AnthropicConfig {
            api_key: "test".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            api_version: None,
            base_url: Some("https://custom.api.com".to_string()),
        })
        .unwrap();
        assert_eq!(provider.base_url(), "https://custom.api.com");
    }
}
