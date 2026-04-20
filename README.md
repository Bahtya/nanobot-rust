<div align="center">

<img src="https://d2z0o16i8xm8ak.cloudfront.net/301e77e6-a3c9-457a-9ccb-7f5abc49fd20/2d79d076-4d0a-492b-b595-291c81aa9e1a/kestrel_D8_vertical_poster.png?Policy=eyJTdGF0ZW1lbnQiOlt7IlJlc291cmNlIjoiaHR0cHM6Ly9kMnowbzE2aTh4bThhay5jbG91ZGZyb250Lm5ldC8zMDFlNzdlNi1hM2M5LTQ1N2EtOWNjYi03ZjVhYmM0OWZkMjAvMmQ3OWQwNzYtNGQwYS00OTJiLWI1OTUtMjkxYzgxYWE5ZTFhL2tlc3RyZWxfRDhfdmVydGljYWxfcG9zdGVyLnBuZz8qIiwiQ29uZGl0aW9uIjp7IkRhdGVMZXNzVGhhbiI6eyJBV1M6RXBvY2hUaW1lIjoxNzc2OTMyNDAxfX19XX0_&Signature=sXK0UhrH5luE~XNmIsXJwAi2WwhwkF2JPiKiw3Hrd~dZY9L2Jzt-lda5hC61RNoQ5dRKnZeTCrUbOJcYKdqo7JEHjUktOaoz7DEhMmAcdLaU1MsPezNTapo4fpR2hqfuPE5jI-qDU4Pm~kw2RxS07oFMcg~qr5RYeFEmVADgXVWwxlrzPDu9Qe1dRqUVdMW4s1Hp1nJHt2hDa0cn2nIG3RwUw2N4JR8dlqR5v7A3Fj7d12jn5wvY5vhYZBYNGiReSphDe86xMGKcB4Q0bnQw-MqRiCZExfm4zl6bXQIxseUI~9aC6P78F~YelJBO9DMytQ63VW1zdl81O-S1eNVByw__&Key-Pair-Id=K1BF7XGXAIMYNX&rnd=1776327639373&utm_source=perplexity" alt="Kestrel Agent Logo" width="200" />

# Kestrel Agent

**A fast, streaming-first AI agent framework built in Rust**

[![CI](https://github.com/Bahtya/kestrel-agent/actions/workflows/ci.yml/badge.svg)](https://github.com/Bahtya/kestrel-agent/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT-blue)](https://github.com/Bahtya/kestrel-agent/blob/main/LICENSE)
[![Crates](https://img.shields.io/badge/crates-16-purple)](./crates)

A fast, streaming-first AI agent framework built in Rust — connect any platform
to any LLM with built-in memory, skills, and self-evolution.

</div>

---

## Features

- **Multi-platform channels** — Telegram, Discord, WebSocket, OpenAI-compatible HTTP API
- **Streaming responses** — SSE streaming for real-time token delivery
- **Tool system** — shell, web, filesystem, cron, search, message, spawn
- **Agent loop** — context management, memory, hooks, and context compaction
- **Sub-agent spawning** — parallel agent tasks via tokio JoinSet
- **Cron scheduling** — tick-based scheduler with JSON state persistence
- **Health checks** — registry-based checks with auto-restart and exponential backoff
- **Skill system** — TOML manifests, hot-reload, `SkillCompiler`, runtime skill injection
- **Tiered memory** — `MemoryStore` trait with HotStore (L1 in-memory) and WarmStore (L2 LanceDB vectors)
- **Learning & evolution** — `LearningEvent` bus, event processors, prompt assembly from observations
- **Unified TraceID** — cross-channel trace IDs (`kst_{channel}_{id}`) for end-to-end request tracking
- **Provider resilience** — automatic retry with exponential backoff on 429s
- **SSRF protection** — network allowlist/denylist, URL validation, sandboxed exec
- **Native daemon mode** — double-fork daemonization, PID file with flock, signal handling (SIGTERM/SIGINT/SIGHUP), graceful shutdown with log flushing, log rotation (daily)

## Architecture

```
                          ┌──────────────────────────────┐
                          │         CLI (clap)           │
                          │  agent · gateway · serve ·   │
                          │  daemon · heartbeat · setup · │
                          │  status                      │
                          └──────────────┬───────────────┘
                                         │
                 ┌───────────────────────┼───────────────────────┐
                 │                       │                       │
         ┌───────▼──────┐    ┌──────────▼──────────┐   ┌───────▼──────┐
         │   Telegram   │    │      Gateway        │   │  API Server  │
         │  (polling)   │    │  (ChannelManager)   │   │   (Axum)     │
         └───────┬──────┘    └──────────┬──────────┘   └───────┬──────┘
         ┌───────┴──────┐              │                       │
         │   Discord    │              │                       │
         │ (WebSocket)  │              │                       │
         └───────┬──────┘              │                       │
         ┌───────┴──────┐              │                       │
         │   WebSocket  │              │                       │
         │   (server)   │              │                       │
         └───────┬──────┘              │                       │
                 │                     │                       │
                 └─────────┬───────────┘───────────────────────┘
                           │
                  InboundMessage │ Bus (tokio broadcast)
                           │
                  ┌────────▼────────┐
                  │    Agent Loop    │
                  │  ┌────────────┐ │
                  │  │  Context   │ │
                  │  │  Memory    │ │
                  │  │  Skills    │ │
                  │  │  Hooks     │ │
                  │  └─────┬──────┘ │
                  └────────┼────────┘
                           │
              ┌────────────┼────────────┐
              │            │            │
      ┌───────▼──────┐ ┌──▼───────┐ ┌──▼────────────┐
      │  Providers   │ │  Tools   │ │  Sub-agents   │
      │              │ │          │ │               │
      │  · OpenAI    │ │  · shell │ │  · parallel   │
      │  · Anthropic │ │  · web   │ │    spawning   │
      │  · DeepSeek  │ │  · fs    │ │  · isolated   │
      │  · Groq      │ │  · cron  │ │    contexts   │
      │  · Ollama    │ │  · search│ │               │
      └──────────────┘ │  · spawn │ └───────────────┘
                       └──────────┘
                           │
                  OutboundMessage │ Bus
                           │
                  ┌────────▼────────┐
                  │   Channel →     │
                  │   User Response │
                  └─────────────────┘

  ── Evolution Layer ────────────────────────────────────
  LearningEvent → EventBus → Processors → (SkillCreate / MemoryUpdate / PromptAdjust)

  ── Foundation Layer ───────────────────────────────────
  kestrel-core · kestrel-config · kestrel-bus
  kestrel-session · kestrel-security · kestrel-providers
  kestrel-cron · kestrel-heartbeat · kestrel-daemon
  kestrel-memory · kestrel-skill · kestrel-learning
```

## Quick Start

### Prerequisites

**Rust** 1.75+ and **protobuf-compiler** (required by LanceDB).

```bash
# Fedora / RHEL
sudo dnf install protobuf-compiler gcc

# Ubuntu / Debian
sudo apt install protobuf-compiler build-essential
```

Optional — [mold linker](https://github.com/rui314/mold) for faster release linking:

```bash
# Fedora
sudo dnf install mold
# Then add to .cargo/config.toml:
# [target.x86_64-unknown-linux-gnu]
# linker = "clang"
# rustflags = ["-C", "link-arg=-fuse-ld=mold"]
```

The project ships a `.cargo/config.toml` with profile optimizations (thin LTO, dependency pre-optimization in dev mode, symbol stripping in release).

### Build

```bash
cargo build --release
```

### Configure

```bash
kestrel setup
# Edit ~/.kestrel/config.yaml with your API keys
```

### Run

```bash
# Interactive agent (one-shot)
kestrel agent "Summarize the latest commits"

# Start gateway (Telegram + Discord + WebSocket)
kestrel gateway

# Start API server
kestrel serve --port 8080

# Periodic health checking
kestrel heartbeat

# Show system status
kestrel status

# Start as daemon (background, double-fork, PID file + flock)
kestrel daemon start

# Check status (auto-cleans stale PID files from crashed instances)
kestrel daemon status

# Stop gracefully (SIGTERM, configurable grace period)
kestrel daemon stop

# Restart (stop + re-exec)
kestrel daemon restart
```

Environment variable `KESTREL_HOME` overrides the default config directory
(`~/.kestrel`).

## Configuration

```yaml
# ~/.kestrel/config.yaml

providers:
  openai:
    api_key: ${OPENAI_API_KEY}
    model: gpt-4o
    base_url: https://api.openai.com/v1   # optional: point to any OpenAI-compatible API
  anthropic:
    api_key: ${ANTHROPIC_API_KEY}
    model: claude-sonnet-4-6
  openrouter:
    api_key: ${OPENROUTER_API_KEY}
    model: anthropic/claude-sonnet-4-6
  ollama:
    base_url: http://localhost:11434/v1
    model: llama3
  deepseek:
    api_key: ${DEEPSEEK_API_KEY}
    model: deepseek-chat
  groq:
    api_key: ${GROQ_API_KEY}
    model: llama-3.3-70b-versatile
  gemini:
    api_key: ${GEMINI_API_KEY}
    model: gemini-2.0-flash
  # no_proxy: true              # skip proxy for domestic APIs (e.g. ZAI, Qwen)

channels:
  telegram:
    token: ${TELEGRAM_BOT_TOKEN}
    allowed_users: ["123456789"]         # optional: restrict to user IDs
    admin_users: ["123456789"]           # optional: admin user IDs
    enabled: true
    streaming: false                     # telegram doesn't support token-by-token
    proxy: ""                            # optional: http/socks5 proxy URL
  discord:
    token: ${DISCORD_BOT_TOKEN}
    allowed_guilds: ["111222333"]        # optional: restrict to guild IDs
    enabled: true
  websocket:
    enabled: true
    listen_addr: "127.0.0.1:8090"
    auth:
      required: true
      token: "my-secret"
    max_clients: 100
    max_message_size: 1048576

agent:
  model: gpt-4o
  temperature: 0.7
  max_tokens: 4096
  max_iterations: 50                    # tool loop limit
  streaming: true
  tool_timeout: 120                     # seconds per tool execution
  system_prompt: "You are a helpful AI assistant."  # optional override
  workspace: /tmp/workspace             # optional: default working directory

dream:                                  # memory consolidation
  enabled: true
  interval_secs: 7200                   # consolidate every 2h
  model: gpt-4o-mini                    # optional: use cheaper model

heartbeat:
  enabled: false
  interval_secs: 1800

cron:
  enabled: false
  state_file: ~/.kestrel/cron_state.json
  tick_secs: 60

security:
  block_private_ips: true               # block RFC1918 by default
  ssrf_whitelist: []                    # allowed IP ranges for outbound
  blocked_networks: []                  # additional blocked ranges

api:
  host: 0.0.0.0
  port: 8080
  allowed_origins: ["*"]                # CORS origins
  max_body_size: 10485760               # 10 MB

daemon:
  pid_file: ~/.kestrel/kestrel.pid
  log_dir: ~/.kestrel/logs
  working_directory: /
  grace_period_secs: 30

# optional: custom system prompt additions
custom_instructions: "Always respond in English."

# optional: agent identity
name: "Kestrel"

# optional: MCP tool servers
# mcp_servers:
#   filesystem:
#     transport: stdio
#     command: "mcp-filesystem"
#     args: ["--root", "/data"]

# optional: custom provider endpoints
# custom_providers:
#   - name: my_provider
#     base_url: https://my-api.com/v1
#     api_key: key123
#     model_patterns: ["my-model"]

notifications:
  online_notify: true
  # notify_chat_id: "-1001234567890"    # which chat receives the ping
  online_message: "Kestrel v{version} online — {channel} connected"
```

Environment variables in values (`${VAR}`) are expanded at load time.

## CLI Commands

| Command | Description |
|---------|-------------|
| `agent` | Interactive agent — send a message and get a response |
| `gateway` | Start the gateway — connect to Telegram, Discord, etc. |
| `serve` | OpenAI-compatible HTTP API server (Axum) |
| `heartbeat` | Periodic health checking with auto-restart |
| `health` | Show health check status |
| `cron list` | List all cron jobs |
| `cron status` | Show status of a specific cron job |
| `config validate` | Validate the config.yaml schema |
| `config migrate` | Migrate Python kestrel config to kestrel format |
| `setup` | Interactive configuration wizard |
| `status` | Show current configuration and system status |
| `daemon start/stop/restart/status` | Native Unix daemon: double-fork, PID file (flock), SIGTERM/SIGINT/SIGHUP, log rotation |

## Crates

| Crate | Description |
|-------|-------------|
| [`kestrel-core`](./crates/kestrel-core) | Error types, constants, core types (`MessageType`, `Platform`) |
| [`kestrel-config`](./crates/kestrel-config) | YAML config loading, schema validation, path resolution |
| [`kestrel-bus`](./crates/kestrel-bus) | Tokio broadcast-based async message bus |
| [`kestrel-session`](./crates/kestrel-session) | SQLite-backed session and conversation store |
| [`kestrel-security`](./crates/kestrel-security) | Network allowlist/denylist, command approval, SSRF protection |
| [`kestrel-providers`](./crates/kestrel-providers) | LLM provider trait — OpenAI-compatible and Anthropic SSE streaming |
| [`kestrel-tools`](./crates/kestrel-tools) | Tool registry + builtins (shell, web, fs, search, cron, spawn, message) |
| [`kestrel-agent`](./crates/kestrel-agent) | Agent loop, context builder, memory, skills, hooks, sub-agents |
| [`kestrel-cron`](./crates/kestrel-cron) | Tick-based cron scheduler with JSON state persistence |
| [`kestrel-heartbeat`](./crates/kestrel-heartbeat) | Health check registry, periodic task monitoring, auto-restart |
| [`kestrel-channels`](./crates/kestrel-channels) | Platform adapters — Telegram, Discord, WebSocket — via `ChannelManager` |
| [`kestrel-api`](./crates/kestrel-api) | OpenAI-compatible HTTP API server (Axum) |
| [`kestrel-daemon`](./crates/kestrel-daemon) | Unix daemon: double-fork, PID file (flock), signal handling, file logging |
| [`kestrel-memory`](./crates/kestrel-memory) | `MemoryStore` trait, HotStore (L1 in-memory), WarmStore/LanceDB (L2 vectors) |
| [`kestrel-skill`](./crates/kestrel-skill) | `Skill` trait, TOML manifests, `SkillRegistry`, `SkillCompiler` |
| [`kestrel-learning`](./crates/kestrel-learning) | `LearningEvent` bus, event processors, prompt assembly |

## Stats

| Metric | Value |
|--------|-------|
| Rust source files | 151 |
| Lines of Rust code | ~105,200 |
| Crates | 16 |
| Minimum Rust version | 1.75 |

## Build Performance

Tested on v0.1.1 (AMD Ryzen 7 6800U, 8GB RAM, Fedora 45, rustc 1.94.1):

| Metric | Value |
|--------|-------|
| Clean build (release) | 7m 52s |
| Crates compiled | 487 |
| Binary size (stripped) | 17M |
| Linker | mold 2.41.0 via clang |
| LTO | thin |
| Parallel jobs | 4 (RAM-limited) |

## API

kestrel exposes an OpenAI-compatible HTTP API. Start with `kestrel serve`:

```bash
kestrel serve --port 8080
```

### List models

```bash
curl http://localhost:8080/v1/models
```

### Chat completion (non-streaming)

```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o",
    "messages": [
      {"role": "system", "content": "You are a helpful assistant."},
      {"role": "user", "content": "What is 2+2?"}
    ],
    "temperature": 0.7,
    "max_tokens": 256
  }'
```

### Chat completion (SSE streaming)

```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-4o",
    "messages": [{"role": "user", "content": "Hello"}],
    "stream": true
  }'
```

### Health and readiness

```bash
curl http://localhost:8080/health    # detailed health snapshot
curl http://localhost:8080/ready     # 200=ready, 503=not ready (Kubernetes probe)
```

### Endpoints

| Endpoint | Method | Auth | Description |
|----------|--------|------|-------------|
| `/v1/chat/completions` | POST | Bearer token (if configured) | OpenAI-compatible chat (streaming + non-streaming) |
| `/v1/models` | GET | No | List available models |
| `/health` | GET | No | Health check with component details |
| `/ready` | GET | No | Readiness probe (200/503) |

## Troubleshooting

<details>
<summary><strong>Build error: <code>protoc not found</code></strong></summary>

LanceDB requires the protobuf compiler.

```bash
# Fedora / RHEL
sudo dnf install protobuf-compiler

# Ubuntu / Debian
sudo apt install protobuf-compiler
```
</details>

<details>
<summary><strong>Build error: <code>linker 'mold' not found</code></strong></summary>

Remove or update the linker override in `.cargo/config.toml`. The shipped config does not require mold — this only happens if you added the optional mold config without installing it.
</details>

<details>
<summary><strong><code>kestrel serve</code> fails with "address already in use"</strong></summary>

Another process is using port 8080. Either stop it or override the port:

```bash
kestrel serve --port 9090
# or set in config:
# api:
#   port: 9090
```
</details>

<details>
<summary><strong>Clippy warnings on <code>cargo clippy --workspace</code></strong></summary>

Run `cargo clippy --workspace --fix` to auto-fix trivial issues. For persistent warnings, ensure you're on Rust 1.75+ and all dependencies are up to date (`cargo update`).
</details>

## Development

```bash
# Build everything
cargo build --workspace

# Run all tests
cargo test --workspace

# Lint (must pass with 0 warnings)
cargo clippy --workspace -- -D warnings

# Format check
cargo fmt --all --check

# Quick compile check
cargo check
```

## Design Principles

- **Thin harness, fat skills** — Harness handles the loop, files, context, and safety. Complexity lives in skill files.
- **Latent vs deterministic** — Judgment goes to the model; parsing and validation stay in code. Never mix the two.
- **Context engineering** — JIT loading, compaction, and structured notes to stay within the context window.
- **Fewer, better tools** — Consolidated operations with token-efficient returns and poka-yoke defaults.
- **LanceDB over SQLite FTS5** — Semantic vector search for memory and session recall.
- **TOML over YAML** — Rust-native parsing for skill manifests and configuration.

## Contributing

1. Fork the repository and create a feature branch.
2. Ensure `cargo test --workspace` and `cargo clippy --workspace` pass with zero warnings.
3. Add tests for any new functionality — assertions must be deterministic (no LLM output in test expectations).
4. Add `///` doc comments on all `pub` functions.
5. Open a pull request against `main`.

CI runs format checks, clippy, build, tests, and a security audit on every push.

## License

This project is licensed under the [MIT License](https://github.com/Bahtya/kestrel-agent/blob/main/LICENSE).
