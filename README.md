# nanobot-rust

A Rust rewrite of the Python [nanobot](https://github.com/ai-nanobot/nanobot) AI agent framework. Multi-platform, streaming-first, and built for production use.

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                      CLI (clap)                         │
│  agent │ gateway │ serve │ heartbeat │ setup │ status    │
└────┬────────┬────────┬──────────────────────────────────┘
     │        │        │
     ▼        ▼        ▼
┌─────────────────────────────────────────────────────────┐
│                    MessageBus                           │
│          inbound (mpsc) │ outbound (mpsc)               │
│          events (broadcast) │ stream (broadcast)        │
└────┬──────────────────────────────────┬─────────────────┘
     │                                  │
     ▼                                  ▼
┌──────────────┐               ┌────────────────┐
│ Channel      │               │ Agent Loop     │
│ Adapters     │               │ (runner +      │
│              │               │  context)      │
│ - Telegram   │               └───────┬────────┘
│ - Discord    │                       │
│ - API server │              ┌────────┴────────┐
└──────────────┘              │                 │
                        ┌─────┴─────┐    ┌──────┴──────┐
                        │ Providers │    │   Tools     │
                        │           │    │             │
                        │ - OpenAI  │    │ - Shell     │
                        │ - Anthropic│   │ - Web       │
                        │ - DeepSeek│    │ - Filesystem│
                        │ - Groq    │    │ - Search    │
                        │ - Ollama  │    │ - Cron      │
                        └───────────┘    │ - Message   │
                                         └─────────────┘
```

## Crates

| Crate | Description |
|---|---|
| `nanobot-core` | Shared types, constants, error types |
| `nanobot-config` | YAML config loading with env var expansion |
| `nanobot-bus` | Async message bus (mpsc + broadcast) |
| `nanobot-session` | Session management with DashMap + JSONL |
| `nanobot-security` | SSRF protection, URL validation |
| `nanobot-providers` | LLM provider trait + OpenAI/Anthropic SSE streaming |
| `nanobot-tools` | Tool registry + builtins (shell, web, fs, search, cron) |
| `nanobot-agent` | Agent loop, runner, context builder, memory, hooks |
| `nanobot-cron` | Tick-based cron scheduler with JSON persistence |
| `nanobot-heartbeat` | LLM-based periodic task checking |
| `nanobot-channels` | Telegram (long-polling) + Discord (REST + Gateway WebSocket) |
| `nanobot-api` | OpenAI-compatible HTTP API (Axum) |

## Quick Start

### Build

```bash
cargo build --release
```

### Configure

```bash
nanobot setup
# Edit ~/.nanobot/config.yaml with your API keys
```

Example config:

```yaml
providers:
  openai:
    api_key: ${OPENAI_API_KEY}
    model: gpt-4o
  anthropic:
    api_key: ${ANTHROPIC_API_KEY}

channels:
  telegram:
    token: ${TELEGRAM_BOT_TOKEN}
    enabled: true
  discord:
    token: ${DISCORD_BOT_TOKEN}
    enabled: true

agent:
  model: gpt-4o
  temperature: 0.7
  max_tokens: 4096
  streaming: true
```

### Run

```bash
# Interactive agent (one-shot)
nanobot agent "What is the weather in Tokyo?"

# Start gateway (Telegram + Discord + API)
nanobot gateway

# Start API server only
nanobot serve --port 8080

# Periodic task checking
nanobot heartbeat

# Show status
nanobot status
```

## API Server

The `serve` command exposes an OpenAI-compatible API:

```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

## Development

```bash
# Build
cargo build --workspace

# Test (unit + integration)
cargo test --workspace

# Lint
cargo clippy --workspace -- -D warnings

# Format
cargo fmt --all --check
```

## Stats

- **80 Rust source files** across 12 crates + binary
- **272 tests** (unit + integration)
- Zero clippy warnings
- Zero unsafe code

## License

MIT
