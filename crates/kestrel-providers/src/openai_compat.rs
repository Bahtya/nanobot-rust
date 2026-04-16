//! OpenAI-compatible provider — works with any OpenAI API-compatible endpoint.

use async_trait::async_trait;
use kestrel_core::{FunctionCall, MessageRole, ToolCall, Usage};

use crate::base::{
    BoxStream, CompletionChunk, CompletionRequest, CompletionResponse, LlmProvider, ToolCallDelta,
};
use crate::build_client;
use crate::retry::{retry_with_backoff, RetryConfig};
use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::debug;

/// Configuration for an OpenAI-compatible provider.
#[derive(Debug, Clone)]
pub struct OpenAiCompatConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub organization: Option<String>,
    /// When true, bypass proxy for this provider's API endpoint.
    /// Set for domestic APIs (e.g. ZAI, Qwen) that don't need a proxy.
    pub no_proxy: bool,
}

/// Provider for OpenAI-compatible APIs (OpenAI, DeepSeek, Groq, etc.).
pub struct OpenAiCompatProvider {
    config: OpenAiCompatConfig,
    client: Client,
    retry: Arc<RetryConfig>,
}

impl OpenAiCompatProvider {
    pub fn new(config: OpenAiCompatConfig) -> anyhow::Result<Self> {
        let client = build_client(config.no_proxy)?;
        Ok(Self {
            config,
            client,
            retry: Arc::new(RetryConfig::default()),
        })
    }

    /// Create with a custom HTTP client (useful for testing).
    pub fn with_client(config: OpenAiCompatConfig, client: Client) -> Self {
        Self {
            config,
            client,
            retry: Arc::new(RetryConfig::default()),
        }
    }

    /// Create with a custom retry configuration.
    pub fn with_retry(config: OpenAiCompatConfig, retry: RetryConfig) -> anyhow::Result<Self> {
        let client = build_client(config.no_proxy)?;
        Ok(Self {
            config,
            client,
            retry: Arc::new(retry),
        })
    }

    /// Create with a custom HTTP client and retry configuration (useful for testing).
    pub fn with_client_and_retry(
        config: OpenAiCompatConfig,
        client: Client,
        retry: RetryConfig,
    ) -> Self {
        Self {
            config,
            client,
            retry: Arc::new(retry),
        }
    }

    fn build_headers(&self) -> HashMap<String, String> {
        let mut headers = HashMap::new();
        headers.insert(
            "Authorization".to_string(),
            format!("Bearer {}", self.config.api_key),
        );
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        if let Some(org) = &self.config.organization {
            headers.insert("OpenAI-Organization".to_string(), org.clone());
        }
        headers
    }

    fn build_request_body(&self, request: &CompletionRequest) -> serde_json::Value {
        let mut messages = Vec::new();
        for msg in &request.messages {
            let mut m = json!({
                "role": match msg.role {
                    MessageRole::System => "system",
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                    MessageRole::Tool => "tool",
                },
                "content": msg.content,
            });
            if let Some(name) = &msg.name {
                m["name"] = json!(name);
            }
            if let Some(tool_call_id) = &msg.tool_call_id {
                m["tool_call_id"] = json!(tool_call_id);
            }
            if let Some(tool_calls) = &msg.tool_calls {
                m["tool_calls"] = json!(tool_calls);
            }
            messages.push(m);
        }

        let mut body = json!({
            "model": request.model,
            "messages": messages,
        });

        if let Some(max_tokens) = request.max_tokens {
            body["max_tokens"] = json!(max_tokens);
        }
        if let Some(temp) = request.temperature {
            body["temperature"] = json!(temp);
        }
        if let Some(tools) = &request.tools {
            let tool_defs: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = json!(tool_defs);
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
                        let tool_call_deltas = build_openai_tool_call_deltas(&tc_acc);
                        tc_acc.clear();
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

                    let chunk_data: serde_json::Value = match serde_json::from_str(data_str) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    // Extract text delta
                    let delta_text = chunk_data
                        .get("choices")
                        .and_then(|c| c.get(0))
                        .and_then(|c| c.get("delta"))
                        .and_then(|d| d.get("content"))
                        .and_then(|c| c.as_str());

                    // Extract tool call deltas
                    if let Some(tool_calls) = chunk_data
                        .get("choices")
                        .and_then(|c| c.get(0))
                        .and_then(|c| c.get("delta"))
                        .and_then(|d| d.get("tool_calls"))
                        .and_then(|tc| tc.as_array())
                    {
                        for tc in tool_calls {
                            let idx =
                                tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                            let entry = tc_acc
                                .entry(idx)
                                .or_insert_with(|| (String::new(), String::new(), String::new()));
                            if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                                entry.0 = id.to_string();
                            }
                            if let Some(name) = tc
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|n| n.as_str())
                            {
                                entry.1 = name.to_string();
                            }
                            if let Some(args) = tc
                                .get("function")
                                .and_then(|f| f.get("arguments"))
                                .and_then(|a| a.as_str())
                            {
                                entry.2.push_str(args);
                            }
                        }
                    }

                    let finish_reason = chunk_data
                        .get("choices")
                        .and_then(|c| c.get(0))
                        .and_then(|c| c.get("finish_reason"))
                        .and_then(|f| f.as_str());

                    let usage = chunk_data.get("usage").map(|u| Usage {
                        prompt_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()),
                        completion_tokens: u.get("completion_tokens").and_then(|v| v.as_u64()),
                        total_tokens: u.get("total_tokens").and_then(|v| v.as_u64()),
                    });

                    if delta_text.is_some() || finish_reason.is_some() || usage.is_some() {
                        let tool_call_deltas = if finish_reason.is_some() {
                            build_openai_tool_call_deltas(&tc_acc)
                        } else {
                            None
                        };
                        if finish_reason.is_some() {
                            tc_acc.clear();
                        }
                        let _ = tx
                            .send(Ok(CompletionChunk {
                                delta: delta_text.map(String::from),
                                tool_call_deltas,
                                usage,
                                done: finish_reason == Some("stop")
                                    || finish_reason == Some("tool_calls"),
                            }))
                            .await;
                    }
                }
            }
        });

        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
    }
}

fn build_openai_tool_call_deltas(
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

#[derive(Debug, Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiMessage {
    content: Option<String>,
    tool_calls: Option<Vec<OpenAiToolCall>>,
}

#[derive(Debug, Deserialize)]
struct OpenAiToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: OpenAiFunction,
}

#[derive(Debug, Deserialize)]
struct OpenAiFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OpenAiUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

#[async_trait]
impl LlmProvider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        "openai_compat"
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let url = format!("{}/chat/completions", self.config.base_url);
        let body = self.build_request_body(&request);
        let headers = self.build_headers();
        let client = self.client.clone();
        let retry_config = self.retry.clone();

        debug!(
            "Sending completion request to {} (model: {})",
            url, request.model
        );

        retry_with_backoff(&retry_config, move |_attempt| {
            let url = url.clone();
            let body = body.clone();
            let headers = headers.clone();
            let client = client.clone();
            async move {
                let mut req_builder = client.post(&url);
                for (k, v) in &headers {
                    req_builder = req_builder.header(k.as_str(), v.as_str());
                }

                let resp = req_builder
                    .json(&body)
                    .send()
                    .await
                    .with_context(|| format!("Failed to send request to {}", url))?;

                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("API error ({}): {}", status, text);
                }

                let api_resp: OpenAiResponse =
                    resp.json().await.context("Failed to parse API response")?;

                let choice = api_resp
                    .choices
                    .into_iter()
                    .next()
                    .context("No choices in API response")?;

                let tool_calls = choice.message.tool_calls.map(|tcs| {
                    tcs.into_iter()
                        .map(|tc| ToolCall {
                            id: tc.id,
                            call_type: tc.call_type,
                            function: FunctionCall {
                                name: tc.function.name,
                                arguments: tc.function.arguments,
                            },
                        })
                        .collect()
                });

                Ok(CompletionResponse {
                    content: choice.message.content,
                    tool_calls,
                    usage: api_resp.usage.map(|u| Usage {
                        prompt_tokens: u.prompt_tokens,
                        completion_tokens: u.completion_tokens,
                        total_tokens: u.total_tokens,
                    }),
                    finish_reason: choice.finish_reason,
                })
            }
        })
        .await
    }

    async fn complete_stream(&self, request: CompletionRequest) -> Result<BoxStream> {
        let url = format!("{}/chat/completions", self.config.base_url);
        let mut body = self.build_request_body(&request);
        body["stream"] = json!(true);

        let headers = self.build_headers();
        let retry_config = self.retry.clone();

        debug!(
            "Sending streaming request to {} (model: {})",
            url, request.model
        );

        retry_with_backoff(&retry_config, move |_attempt| {
            let url = url.clone();
            let body = body.clone();
            let headers = headers.clone();
            let client = self.client.clone();
            async move {
                let mut req_builder = client.post(&url);
                for (k, v) in &headers {
                    req_builder = req_builder.header(k.as_str(), v.as_str());
                }

                let resp = req_builder
                    .json(&body)
                    .send()
                    .await
                    .with_context(|| format!("Failed to send streaming request to {}", url))?;

                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("API error ({}): {}", status, text);
                }

                Ok(Self::parse_sse_stream(resp.bytes_stream()))
            }
        })
        .await
    }

    fn supports_model(&self, model: &str) -> bool {
        let _ = model;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kestrel_core::Message;

    #[test]
    fn test_openai_config_construction() {
        let config = OpenAiCompatConfig {
            api_key: "sk-test".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4".to_string(),
            organization: Some("org-123".to_string()),
            no_proxy: false,
        };
        assert_eq!(config.api_key, "sk-test");
        assert_eq!(config.base_url, "https://api.openai.com/v1");
        assert_eq!(config.model, "gpt-4");
        assert_eq!(config.organization.as_deref(), Some("org-123"));
    }

    #[test]
    fn test_openai_provider_supports_model() {
        let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
            api_key: "test".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4".to_string(),
            organization: None,
            no_proxy: false,
        })
        .unwrap();
        assert!(provider.supports_model("gpt-4"));
        assert!(provider.supports_model("claude-3"));
        assert!(provider.supports_model("anything"));
    }

    #[test]
    fn test_openai_provider_name() {
        let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
            api_key: "test".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4".to_string(),
            organization: None,
            no_proxy: false,
        })
        .unwrap();
        assert_eq!(provider.name(), "openai_compat");
    }

    #[test]
    fn test_build_headers() {
        let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
            api_key: "sk-secret".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4".to_string(),
            organization: Some("org-test".to_string()),
            no_proxy: false,
        })
        .unwrap();
        let headers = provider.build_headers();
        assert_eq!(headers.get("Authorization").unwrap(), "Bearer sk-secret");
        assert_eq!(headers.get("Content-Type").unwrap(), "application/json");
        assert_eq!(headers.get("OpenAI-Organization").unwrap(), "org-test");
    }

    #[test]
    fn test_build_request_body_basic() {
        let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
            api_key: "test".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4".to_string(),
            organization: None,
            no_proxy: false,
        })
        .unwrap();

        let request = CompletionRequest {
            model: "gpt-4".to_string(),
            messages: vec![
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
            ],
            tools: None,
            max_tokens: Some(1024),
            temperature: Some(0.7),
            stream: false,
        };

        let body = provider.build_request_body(&request);
        assert_eq!(body["model"], "gpt-4");
        assert_eq!(body["max_tokens"], 1024);
        let temp = body["temperature"].as_f64().unwrap();
        assert!((temp - 0.7).abs() < 0.01);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn test_build_request_body_with_tool_result() {
        let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
            api_key: "test".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4".to_string(),
            organization: None,
            no_proxy: false,
        })
        .unwrap();

        let request = CompletionRequest {
            model: "gpt-4".to_string(),
            messages: vec![Message {
                role: MessageRole::Tool,
                content: "result data".to_string(),
                name: Some("search".to_string()),
                tool_call_id: Some("call_1".to_string()),
                tool_calls: None,
            }],
            tools: None,
            max_tokens: None,
            temperature: None,
            stream: false,
        };

        let body = provider.build_request_body(&request);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "tool");
        assert_eq!(messages[0]["tool_call_id"], "call_1");
        assert_eq!(messages[0]["name"], "search");
    }

    #[test]
    fn test_build_headers_no_organization() {
        let provider = OpenAiCompatProvider::new(OpenAiCompatConfig {
            api_key: "sk-test".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4".to_string(),
            organization: None,
            no_proxy: false,
        })
        .unwrap();
        let headers = provider.build_headers();
        assert_eq!(headers.get("Authorization").unwrap(), "Bearer sk-test");
        assert!(!headers.contains_key("OpenAI-Organization"));
    }
}
