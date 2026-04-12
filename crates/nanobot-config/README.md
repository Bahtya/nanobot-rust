# nanobot-config

YAML configuration loading with environment variable expansion and schema migration.

Part of the [nanobot-rust](../..) workspace.

## Overview

Loads and validates the `config.yaml` that drives nanobot behavior. Supports `${VAR}`
and `${VAR:-default}` environment variable substitution inside YAML values, and
automatically migrates older config versions to the current schema. Maintains YAML
compatibility with the Python nanobot config format.

## Key Types

| Type | Description |
|---|---|
| `Config` | Root config: providers, channels, agent defaults, security, heartbeat, dream |
| `ProvidersConfig` | LLM provider entries (Anthropic, OpenAI, DeepSeek, Groq, Ollama, etc.) |
| `ChannelsConfig` | Channel configs (Telegram, Discord, Slack, Matrix, Email, etc.) |
| `AgentDefaults` | Model, temperature, max_tokens, max_iterations, system_prompt, streaming |
| `SecurityConfig` | SSRF whitelist, private IP blocking, blocked networks |
| `HeartbeatConfig` | Enable/disable and interval for periodic task checking |
| `DreamConfig` | Memory consolidation settings |
| `McpServerConfig` | MCP server transport (stdio/sse/http), command, args, env |
| `CustomProviderConfig` | Non-standard LLM endpoints with URL and model patterns |

## Key Functions

- `load_config(path)` -- Load from file (or default path), expand env vars, run migrations
- `save_config(config, path)` -- Serialize config back to YAML
- `expand_env_vars(input)` -- Resolve `${VAR}` / `${VAR:-default}` patterns

## Usage

```rust
use nanobot_config::load_config;
use std::path::Path;

let config = load_config(Some(Path::new("config.yaml")))?;
println!("Model: {}", config.agent.model);
println!("Temperature: {}", config.agent.temperature);

// Env vars in YAML are expanded:
//   api_key: ${ANTHROPIC_API_KEY}
//   model: ${MODEL:-gpt-4o}
```
