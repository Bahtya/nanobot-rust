//! Base LLM provider trait and shared types.

use async_trait::async_trait;
use futures::Stream;
use kestrel_core::{FunctionDefinition, Message, Usage};
use serde::{Deserialize, Serialize};
use std::pin::Pin;

/// Boxed stream type for streaming completions.
pub type BoxStream = Pin<Box<dyn Stream<Item = Result<CompletionChunk, anyhow::Error>> + Send>>;

/// Trait for LLM providers.
///
/// Each provider (OpenAI, Anthropic, etc.) implements this trait
/// to provide a unified interface for completions.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// The provider name (e.g., "openai", "anthropic").
    fn name(&self) -> &str;

    /// Returns the provider's configured default model name.
    fn default_model(&self) -> &str;

    /// Perform a non-streaming completion.
    async fn complete(&self, request: CompletionRequest) -> anyhow::Result<CompletionResponse>;

    /// Perform a streaming completion.
    async fn complete_stream(&self, request: CompletionRequest) -> anyhow::Result<BoxStream>;

    /// Check if this provider supports a given model.
    fn supports_model(&self, model: &str) -> bool;
}

/// A completion request sent to an LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// Model identifier.
    pub model: String,

    /// Conversation messages.
    pub messages: Vec<Message>,

    /// Available tools for the model to call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<FunctionDefinition>>,

    /// Maximum tokens for the response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,

    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    /// Whether to stream the response.
    #[serde(default)]
    pub stream: bool,
}

/// A non-streaming completion response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionResponse {
    /// The text content of the response.
    pub content: Option<String>,

    /// Tool calls requested by the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<kestrel_core::ToolCall>>,

    /// Token usage statistics.
    #[serde(default)]
    pub usage: Option<Usage>,

    /// Why the model stopped generating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// A single chunk in a streaming response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionChunk {
    /// Incremental content delta.
    #[serde(default)]
    pub delta: Option<String>,

    /// Tool call deltas (for streaming tool calls).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_deltas: Option<Vec<ToolCallDelta>>,

    /// Token usage (usually in the last chunk).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,

    /// Whether this is the final chunk.
    #[serde(default)]
    pub done: bool,
}

/// A delta for a streaming tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallDelta {
    /// Index of the tool call.
    pub index: usize,

    /// Tool call ID (only in first chunk).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Function name delta.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_name: Option<String>,

    /// Function arguments delta.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub function_arguments: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kestrel_core::{FunctionDefinition, Message, MessageRole, Usage};

    #[test]
    fn test_completion_request_serde() {
        let req = CompletionRequest {
            model: "gpt-4".to_string(),
            messages: vec![Message {
                role: MessageRole::User,
                content: "hello".to_string(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            tools: Some(vec![FunctionDefinition {
                name: "test".to_string(),
                description: Some("a test tool".to_string()),
                parameters: None,
            }]),
            max_tokens: Some(1024),
            temperature: Some(0.7),
            stream: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: CompletionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.model, "gpt-4");
        assert_eq!(back.messages.len(), 1);
        assert_eq!(back.tools.as_ref().unwrap().len(), 1);
        assert_eq!(back.max_tokens, Some(1024));
        assert_eq!(back.temperature, Some(0.7));
        assert!(!back.stream);
    }

    #[test]
    fn test_completion_response_serde() {
        let resp = CompletionResponse {
            content: Some("hi there".to_string()),
            tool_calls: None,
            usage: Some(Usage {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                total_tokens: Some(15),
            }),
            finish_reason: Some("stop".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: CompletionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.content.as_deref(), Some("hi there"));
        assert!(back.tool_calls.is_none());
        assert_eq!(back.usage.unwrap().total_tokens, Some(15));
        assert_eq!(back.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn test_completion_chunk_default() {
        let chunk = CompletionChunk {
            delta: None,
            tool_call_deltas: None,
            usage: None,
            done: false,
        };
        assert!(chunk.delta.is_none());
        assert!(chunk.tool_call_deltas.is_none());
        assert!(chunk.usage.is_none());
        assert!(!chunk.done);
    }

    #[test]
    fn test_tool_call_delta_construction() {
        let delta = ToolCallDelta {
            index: 0,
            id: Some("call_123".to_string()),
            function_name: Some("get_weather".to_string()),
            function_arguments: Some("{\"city\":".to_string()),
        };
        assert_eq!(delta.index, 0);
        assert_eq!(delta.id.as_deref(), Some("call_123"));
        assert_eq!(delta.function_name.as_deref(), Some("get_weather"));
        assert_eq!(delta.function_arguments.as_deref(), Some("{\"city\":"));
    }
}
