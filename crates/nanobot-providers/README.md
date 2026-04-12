# nanobot-providers

LLM provider abstraction with multiple backend implementations and SSE streaming.

Part of the [nanobot-rust](../..) workspace.

## Overview

Defines a unified `LlmProvider` trait and implements it for Anthropic (Claude), OpenAI,
and any OpenAI-compatible endpoint (DeepSeek, Groq, OpenRouter, Ollama, etc.).
A `ProviderRegistry` maps model names to the correct provider using keyword matching.

## Key Types

| Type | Description |
|---|---|
| `LlmProvider` (trait) | `name()`, `complete()`, `complete_stream()`, `supports_model()` |
| `CompletionRequest` | Model, messages, tools, max_tokens, temperature, stream flag |
| `CompletionResponse` | Content, tool_calls, usage, finish_reason |
| `CompletionChunk` | Streaming delta: content, tool_call_deltas, usage, done flag |
| `ToolCallDelta` | Incremental tool call assembly (index, id, function name/args) |
| `ProviderRegistry` | Keyword-based model-to-provider routing |
| `AnthropicProvider` | Native Claude Messages API integration |
| `OpenAiCompatProvider` | Generic OpenAI-compatible client (OpenAI, DeepSeek, Groq, etc.) |

## Provider Resolution

Model keywords map to providers: `"claude"` -> anthropic, `"gpt"`/`"o1"` -> openai,
`"deepseek"` -> deepseek, `"llama"`/`"qwen"` -> ollama, etc. Built from `Config` via
`ProviderRegistry::from_config(&config)`.

## Usage

```rust
use nanobot_providers::{ProviderRegistry, CompletionRequest};
use nanobot_core::{Message, MessageRole};

let registry = ProviderRegistry::from_config(&config)?;
let provider = registry.get_provider("claude-sonnet-4-20250514").unwrap();

let request = CompletionRequest {
    model: "claude-sonnet-4-20250514".to_string(),
    messages: vec![Message {
        role: MessageRole::User, content: "Hello".to_string(),
        name: None, tool_call_id: None, tool_calls: None,
    }],
    tools: None, max_tokens: Some(1024),
    temperature: Some(0.7), stream: false,
};

let response = provider.complete(request).await?;
```
