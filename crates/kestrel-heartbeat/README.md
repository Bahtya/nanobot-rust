# kestrel-heartbeat

Heartbeat service with two-phase LLM-based task checking.

Part of the [kestrel](../..) workspace.

## Overview

Periodically checks whether the agent has pending tasks or follow-ups that need
attention. Uses a two-phase approach to minimize unnecessary LLM calls:

1. **Phase 1 (lightweight):** Ask the LLM a simple YES/NO -- are there pending tasks?
2. **Phase 2 (full execution):** If YES, run the full `AgentRunner` to handle them.

## Key Types

| Type | Description |
|---|---|
| `HeartbeatService` | Configurable interval, provider/tool registries, session manager |

## HeartbeatService API

- `new(config)` -- Create with defaults (no registries attached)
- `with_registries(config, providers, tools, sessions)` -- Full agent capabilities
- `run()` -- Start the periodic heartbeat loop (blocking)
- `stop()` -- Signal the loop to stop
- `is_running()` -> `bool` -- Check current state

## Configuration

Controlled via `Config.heartbeat`:

```yaml
heartbeat:
  enabled: true
  interval_secs: 1800  # 30 minutes (minimum enforced: 60)
```

## Usage

```rust
use kestrel_heartbeat::HeartbeatService;

let service = HeartbeatService::with_registries(
    config, provider_registry, tool_registry, session_manager,
);

// Run in a background task
tokio::spawn(async move { service.run().await });

// To stop
service.stop().await;
```
