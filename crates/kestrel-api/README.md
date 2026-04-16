# kestrel-api

OpenAI-compatible HTTP API server built on Axum.

Part of the [kestrel](../..) workspace.

## Overview

Exposes kestrel's agent capabilities via an OpenAI-compatible REST API so that existing
tools and integrations targeting the `/v1/chat/completions` endpoint can use kestrel as
a drop-in backend.

## Endpoints

| Method | Path | Description |
|---|---|---|
| POST | `/v1/chat/completions` | Chat completion (runs the agent, returns response) |
| GET | `/v1/models` | List available models (returns configured agent model) |
| GET | `/health` | Health check (status + version) |

## Key Types

| Type | Description |
|---|---|
| `ApiServer` | Holds shared state, builds the Axum router, starts the listener |
| `AppState` | Shared state: config, bus, session_manager, provider/tool registries |
| `ChatCompletionRequest` | OpenAI-compatible request: model, messages, temperature, max_tokens |
| `ChatCompletionResponse` | OpenAI-compatible response: id, model, choices, usage |
| `ApiMessage` | A single message with role and content |

## Usage

```rust
use kestrel_api::ApiServer;

let server = ApiServer::with_registries(
    config, bus, session_manager,
    provider_registry, tool_registry, Some(8080),
);

server.run().await?;
```

### curl Example

```bash
curl -X POST http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```
