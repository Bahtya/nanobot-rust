# nanobot-channels

Channel adapters for chat platforms with a unified trait interface.

Part of the [nanobot-rust](../..) workspace.

## Overview

Provides platform-specific adapters that translate between chat platform APIs and the
nanobot message bus. Currently implements Telegram (long-polling via `getUpdates`) and
Discord (REST API v10 + Gateway WebSocket). The `ChannelManager` coordinates multiple
adapters and routes outbound messages to the correct platform.

## Key Types

| Type | Description |
|---|---|
| `BaseChannel` (trait) | Unified interface: connect, disconnect, send_message, send_typing, send_image |
| `SendResult` | Result of a send: success, message_id, error, retryable |
| `ChatInfo` | Chat metadata (id, name, type, member count) |
| `ChannelRegistry` | Factory registry: maps channel names to constructors |
| `ChannelManager` | Coordinates running channels, routes outbound messages |
| `TelegramChannel` | Telegram Bot API adapter using long-polling |
| `DiscordChannel` | Discord adapter using REST + Gateway WebSocket |

## Channel Lifecycle

1. `ChannelRegistry::new()` registers built-in factories (telegram, discord)
2. `ChannelManager::start_channel(name)` creates an adapter, connects, wires to bus
3. Inbound flow: platform -> adapter -> bus inbound channel
4. Outbound flow: bus outbound channel -> ChannelManager -> adapter -> platform
5. `run_outbound_consumer()` drives the outbound side

## Usage

```rust
use nanobot_channels::{ChannelRegistry, ChannelManager};

let registry = ChannelRegistry::new();  // telegram + discord
let manager = ChannelManager::new(registry, bus);

manager.start_channel("telegram").await?;
manager.start_channel("discord").await?;

// Consume outbound messages and route to platforms
manager.run_outbound_consumer().await;
```
