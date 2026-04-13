# nanobot-rs

Rust multi-platform AI agent framework. Binary: `nanobot-rs`. Config: `~/.nanobot-rs`. Env override: `NANOBOT_RS_HOME`.

## Rules (non-negotiable)

- **Paths**: `~/.nanobot-rs` (NOT `~/.nanobot`). Binary `nanobot-rs` (NOT `nanobot`).
- **Every commit must pass**: `cargo test --workspace` + `cargo clippy --workspace` = 0 failures, 0 warnings.
- **Every feature needs tests** before commit. Tests are deterministic — no LLM output in assertions.
- **Commit + push** after each complete feature. Don't accumulate uncommitted changes.
- **Doc comments** on all `pub` functions (`///` style).

## Architecture

12 crates + binary. Read individual crate source for details.

```
nanobot-core      → Types, errors, constants
nanobot-config    → YAML config, schema, paths
nanobot-bus       → Tokio broadcast message bus
nanobot-session   → SQLite conversation store
nanobot-security  → SSRF protection, URL validation
nanobot-providers → LLM providers (OpenAI-compat, Anthropic) with retry
nanobot-tools     → Tool registry + builtins (shell, web, fs, cron, search, spawn, message, skills)
nanobot-agent     → Agent loop, context, memory, compaction, sub-agents
nanobot-cron      → Tick-based scheduler with JSON state
nanobot-heartbeat → Health checks, auto-restart, exponential backoff
nanobot-channels  → Telegram (polling) + Discord (WebSocket) via ChannelManager
nanobot-api       → OpenAI-compatible HTTP API (Axum, SSE streaming)
```

Message flow: `InboundMessage → Bus → AgentLoop → (Provider + Tools) → OutboundMessage → Bus → Channel`

## Commands

`cargo build --workspace` | `cargo test --workspace` | `cargo clippy --workspace` | `cargo fmt --all --check`

CLI subcommands: `agent`, `gateway`, `serve`, `heartbeat`, `health`, `setup`, `status`, `config validate`, `config migrate`

## Design Principles (pointers, not duplication)

- **Thin harness, fat skills**: See Garry Tan's article. Harness = 4 things only (loop, files, context, safety). Complexity goes in skill files.
- **Latent vs Deterministic**: Judgment/synthesis → model (latent). Parsing/validation/counting → code (deterministic). Never mix them up.
- **Context engineering**: JIT loading, compaction, structured notes outside context window. See Anthropic's blog.
- **Fewer, better tools**: Consolidate operations. Token-efficient returns. Poka-yoke.

## Pitfalls

- Bus uses tokio broadcast — receivers must handle lag or drop messages.
- Session store uses SQLite — concurrent access needs care.
- Provider 429 handling: exponential backoff, not immediate retry.
- Tests touching filesystem: use tempdir pattern.
