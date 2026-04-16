# kestrel-core

Shared types, error definitions, and constants used across all kestrel crates.

Part of the [kestrel](../..) workspace.

## Overview

This is the foundational crate that every other kestrel crate depends on. It defines
the common data model -- messages, tool calls, platform identifiers, usage stats --
and a unified error type. It intentionally has no async runtime or external service
dependencies.

## Key Types

| Type | Description |
|---|---|
| `Platform` | Enum of supported chat platforms (Telegram, Discord, Slack, etc.) |
| `Message` | A single conversation message with role, content, and optional tool calls |
| `MessageRole` | System / User / Assistant / Tool |
| `MessageType` | Text, Photo, Video, Audio, Voice, Document, Location, Sticker, Command |
| `ToolCall` / `FunctionCall` | LLM-initiated tool invocation with ID and JSON arguments |
| `ToolDefinition` / `FunctionDefinition` | Schema describing an available tool to the LLM |
| `Usage` | Token usage counters (prompt, completion, total) |
| `RunResult` | Final result of an agent run (content, usage, iterations) |
| `SessionSource` | Identifies where a conversation originates (platform, chat_id, thread) |
| `MediaAttachment` | URL / MIME / caption for media in messages |
| `KestrelError` | Unified error enum (Config, Provider, Tool, Session, Security, etc.) |

## Constants

`VERSION`, `DEFAULT_MAX_ITERATIONS` (50), `DEFAULT_TEMPERATURE` (0.7),
`DEFAULT_MAX_TOKENS` (4096), `DEFAULT_SESSION_HISTORY_LIMIT` (200),
`BUS_CHANNEL_CAPACITY` (1024), `DEFAULT_HEARTBEAT_INTERVAL_SECS` (1800),
`CRON_TICK_INTERVAL_SECS` (30).

## Usage

```rust
use kestrel_core::{Message, MessageRole, Platform, ToolCall, Usage};

let msg = Message {
    role: MessageRole::User,
    content: "What is the weather?".to_string(),
    name: None,
    tool_call_id: None,
    tool_calls: None,
};

let platform = Platform::Telegram;
assert_eq!(platform.as_str(), "telegram");
```
