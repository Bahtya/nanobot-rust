<div align="center">

# nanobot-rs

**A multi-platform AI agent framework built in Rust**

[![CI](https://github.com/Bahtya/nanobot-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/Bahtya/nanobot-rust/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT-blue)](https://github.com/Bahtya/nanobot-rust/blob/main/LICENSE)
[![Crates](https://img.shields.io/badge/crates-16-purple)](./crates)

Fast, streaming-first, and production-ready. Connect Telegram, Discord, and
OpenAI-compatible clients to any LLM provider through a unified agent loop
with built-in memory, skills, and self-evolution.

</div>

---

## Features

- **Multi-platform channels** — Telegram, Discord, OpenAI-compatible HTTP API
- **Streaming responses** — SSE streaming for real-time token delivery
- **Tool system** — shell, web, filesystem, cron, search, message, spawn
- **Agent loop** — context management, memory, hooks, and context compaction
- **Sub-agent spawning** — parallel agent tasks via tokio JoinSet
- **Cron scheduling** — tick-based scheduler with JSON state persistence
- **Health checks** — registry-based checks with auto-restart and exponential backoff
- **Skill system** — TOML manifests, hot-reload, `SkillCompiler`, runtime skill injection
- **Tiered memory** — `MemoryStore` trait with HotStore (L1 in-memory) and WarmStore (L2 LanceDB vectors)
- **Learning & evolution** — `LearningEvent` bus, event processors, prompt assembly from observations
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
  nanobot-core · nanobot-config · nanobot-bus
  nanobot-session · nanobot-security · nanobot-providers
  nanobot-cron · nanobot-heartbeat · nanobot-daemon
  nanobot-memory · nanobot-skill · nanobot-learning
```

## Quick Start

### Build

```bash
cargo build --release
```

### Configure

```bash
nanobot-rs setup
# Edit ~/.nanobot-rs/config.yaml with your API keys
```

### Run

```bash
# Interactive agent (one-shot)
nanobot-rs agent "Summarize the latest commits"

# Start gateway (Telegram + Discord)
nanobot-rs gateway

# Start API server
nanobot-rs serve --port 8080

# Periodic health checking
nanobot-rs heartbeat

# Show system status
nanobot-rs status

# Start as daemon (background, double-fork, PID file + flock)
nanobot-rs daemon start

# Check status (auto-cleans stale PID files from crashed instances)
nanobot-rs daemon status

# Stop gracefully (SIGTERM, configurable grace period)
nanobot-rs daemon stop

# Restart (stop + re-exec)
nanobot-rs daemon restart
```

Environment variable `NANOBOT_RS_HOME` overrides the default config directory
(`~/.nanobot-rs`).

## Configuration

```yaml
# ~/.nanobot-rs/config.yaml

providers:
  openai:
    api_key: ${OPENAI_API_KEY}
    model: gpt-4o
    base_url: https://api.openai.com/v1   # optional: point to any OpenAI-compatible API
  anthropic:
    api_key: ${ANTHROPIC_API_KEY}
    model: claude-sonnet-4-6

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

security:
  network:
    deny:
      - "10.0.0.0/8"
      - "172.16.0.0/12"
      - "192.168.0.0/16"

daemon:
  pid_file: ~/.nanobot-rs/nanobot-rs.pid
  log_dir: ~/.nanobot-rs/logs
  working_directory: /
  grace_period_secs: 30
```

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
| `config migrate` | Migrate Python nanobot config to nanobot-rs format |
| `setup` | Interactive configuration wizard |
| `status` | Show current configuration and system status |
| `daemon start/stop/restart/status` | Native Unix daemon: double-fork, PID file (flock), SIGTERM/SIGINT/SIGHUP, log rotation |

## Crates

| Crate | Description |
|-------|-------------|
| [`nanobot-core`](./crates/nanobot-core) | Error types, constants, core types (`MessageType`, `Platform`) |
| [`nanobot-config`](./crates/nanobot-config) | YAML config loading, schema validation, path resolution |
| [`nanobot-bus`](./crates/nanobot-bus) | Tokio broadcast-based async message bus |
| [`nanobot-session`](./crates/nanobot-session) | SQLite-backed session and conversation store |
| [`nanobot-security`](./crates/nanobot-security) | Network allowlist/denylist, command approval, SSRF protection |
| [`nanobot-providers`](./crates/nanobot-providers) | LLM provider trait — OpenAI-compatible and Anthropic SSE streaming |
| [`nanobot-tools`](./crates/nanobot-tools) | Tool registry + builtins (shell, web, fs, search, cron, spawn, message) |
| [`nanobot-agent`](./crates/nanobot-agent) | Agent loop, context builder, memory, skills, hooks, sub-agents |
| [`nanobot-cron`](./crates/nanobot-cron) | Tick-based cron scheduler with JSON state persistence |
| [`nanobot-heartbeat`](./crates/nanobot-heartbeat) | Health check registry, periodic task monitoring, auto-restart |
| [`nanobot-channels`](./crates/nanobot-channels) | Platform adapters — Telegram, Discord — via `ChannelManager` |
| [`nanobot-api`](./crates/nanobot-api) | OpenAI-compatible HTTP API server (Axum) |
| [`nanobot-daemon`](./crates/nanobot-daemon) | Unix daemon: double-fork, PID file (flock), signal handling, file logging |
| [`nanobot-memory`](./crates/nanobot-memory) | `MemoryStore` trait, HotStore (L1 in-memory), WarmStore/LanceDB (L2 vectors) |
| [`nanobot-skill`](./crates/nanobot-skill) | `Skill` trait, TOML manifests, `SkillRegistry`, `SkillCompiler` |
| [`nanobot-learning`](./crates/nanobot-learning) | `LearningEvent` bus, event processors, prompt assembly |

## Stats

| Metric | Value |
|--------|-------|
| Rust source files | 126 |
| Lines of Rust code | ~62,800 |
| Crates | 16 |
| Minimum Rust version | 1.75 |

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

This project is licensed under the [MIT License](https://github.com/Bahtya/nanobot-rust/blob/main/LICENSE).
