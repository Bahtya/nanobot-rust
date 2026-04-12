# nanobot-bus

Async message bus that decouples channel adapters from agent processing.

Part of the [nanobot-rust](../..) workspace.

## Overview

Provides the central communication backbone of nanobot. Channel adapters publish
inbound user messages; the agent loop consumes them, processes them, and publishes
outbound responses. A broadcast channel carries lifecycle events and streaming chunks
to any number of subscribers.

## Key Types

| Type | Description |
|---|---|
| `MessageBus` | Central hub with inbound/outbound mpsc channels + event/stream broadcast |
| `InboundMessage` | Message arriving from a platform (channel, sender, content, media) |
| `OutboundMessage` | Message being sent from the agent back to a platform |
| `StreamChunk` | Real-time streaming text delta with session key and done flag |
| `AgentEvent` | Lifecycle events: Started, StreamingChunk, ToolCall, Completed, Error |

## MessageBus API

- `new()` / `with_capacity(n)` -- Create a bus
- `publish_inbound(msg)` / `consume_inbound()` -- Adapters push, agent takes the receiver
- `publish_outbound(msg)` / `consume_outbound()` -- Agent pushes, channel manager takes
- `emit_event(event)` / `subscribe_events()` -- Broadcast lifecycle events
- `publish_stream_chunk(chunk)` / `subscribe_stream()` -- Broadcast streaming output

## Usage

```rust
use nanobot_bus::MessageBus;
use nanobot_bus::events::InboundMessage;

let bus = MessageBus::new();

// Channel adapter side
bus.publish_inbound(inbound_msg).await?;

// Agent loop side
let mut rx = bus.consume_inbound().await.unwrap();
while let Some(msg) = rx.recv().await {
    // process message...
    bus.publish_outbound(response).await?;
}
```
