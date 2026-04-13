# nanobot-rs — Rust AI Agent Framework

## Project Overview
A Rust rewrite of the Python nanobot multi-platform AI agent framework. The binary is `nanobot-rs`. Config directory is `~/.nanobot-rs` (NOT `~/.nanobot`). Env var override: `NANOBOT_RS_HOME`.

## Architecture
Workspace with 12 crates + main binary:

```
nanobot-core      → Error types, constants, core types (MessageType, Platform)
nanobot-config    → YAML config loading, schema, path resolution (~/.nanobot-rs)
nanobot-bus       → Tokio broadcast-based message bus
nanobot-session   → SQLite-backed session/conversation store
nanobot-security  → Network allowlist/denylist, command approval
nanobot-providers → LLM provider abstraction (OpenAI-compat, Anthropic)
nanobot-tools     → Tool registry + builtins (shell, filesystem, web, cron, spawn, search, message)
nanobot-agent     → Agent loop, context management, memory, skills, hooks, subagents
nanobot-cron      → Cron job scheduling service
nanobot-heartbeat → Periodic task/health checking
nanobot-channels  → Platform adapters (Telegram, Discord) via ChannelManager
nanobot-api       → OpenAI-compatible HTTP API server (Axum)
```

## Key Commands
```bash
cargo build --workspace              # Build everything
cargo test --workspace               # Run all tests (222 tests)
cargo clippy --workspace             # Lint (currently 0 warnings)
cargo check                          # Quick compile check
cargo run --bin nanobot-rs -- --help # Run the binary
```

## Code Standards
- Rust edition 2021, MSRV 1.75
- Use `anyhow::Result` for application code, `thiserror` for library error types
- All public functions need doc comments (`///` style)
- Comprehensive tests — aim for >80% coverage on each crate
- Zero clippy warnings (`cargo clippy --workspace` must pass)
- Use `tracing` for logging (not `println!`), with `info!`/`debug!`/`warn!`/`error!`
- Async via `tokio` runtime — use `#[tokio::test]` for async tests
- Serde for serialization — YAML for config, JSON for API
- Follow Rust naming conventions (snake_case for functions/vars, CamelCase for types)

## Design Principles (from Anthropic engineering research)

### Agent Design Patterns
- **Keep it simple**: Start with the simplest solution. Only add complexity when demonstrably needed.
- **Evaluator-Optimizer pattern**: Separate the generator from the evaluator. The agent that writes code should not be the sole judge of its quality.
- **Sub-agent architecture**: Specialized sub-agents handle focused tasks with clean context windows. Return distilled summaries.
- **Brain-Hands-Session decoupling**: Separate the "brain" (LLM + harness) from "hands" (tools/sandbox) from "session" (event log).

### Context Engineering
- **Just-in-time context**: Don't stuff everything into memory. Load what's needed when it's needed.
- **Token efficiency**: Every field in config, every struct member costs memory at inference time. Be intentional.
- **Structured note-taking**: The session store enables persistent memory outside the context window.
- **Compaction**: Support conversation summarization when approaching limits.

### Tool Design (ACI — Agent-Computer Interface)
- **Fewer, better tools**: Consolidate multi-step operations into single tools.
- **Meaningful returns**: Return enough context for decisions without follow-up calls.
- **Token-efficient responses**: Default to concise output, detailed on demand.
- **Clear schemas**: Unambiguous parameter names, helpful error messages that steer agents.
- **Poka-yoke**: Design tools so they can't be easily misused.

### Harness Evolution
- As models improve, simplify the harness — don't just add scaffolding.
- The agent loop should be configurable, not hard-coded to specific model behaviors.
- Context anxiety mitigation (compaction, structured notes) should be pluggable.

## Current Status (2026-04-13)
- 82 .rs files, ~57K lines
- 222 tests passing, 0 compile errors, 0 clippy warnings
- All 12 crates + binary compile and pass tests
- Commands implemented: agent, gateway, serve, heartbeat, setup, status
- Channel adapters: Telegram, Discord (stub implementations)
- Built-in tools: shell, filesystem, web, cron, spawn, search, message
- Provider support: OpenAI-compatible (covers DeepSeek, Groq, OpenRouter), Anthropic

## Next Steps (Priority Order)
1. **Flesh out Telegram channel adapter** — real polling/webhook with proper message handling
2. **Implement gateway wiring** — make `gateway` command actually connect to platforms and route messages through the agent loop
3. **Agent loop integration test** — end-to-end test: message in → agent processes → response out
4. **Discord channel adapter** — real Discord bot connection
5. **Context compaction** — implement conversation summarization for long sessions
6. **Sub-agent spawning** — implement the spawn tool for parallel agent tasks
7. **Cron execution** — make the cron service actually schedule and run jobs
8. **Config migration from Python nanobot** — optional import tool for existing users

## Key Principles (from Garry Tan — "Thin Harness, Fat Skills")

### CLAUDE.md itself should be thin
- This file is a pointer, not an encyclopedia
- Keep under 200 lines — details go in dedicated docs
- The model's attention degrades with too much upfront context

### Skill files = permanent upgrades
- Every repeated task must become a skill file (markdown procedure)
- Skills take parameters like method calls — same process, different inputs
- If you have to do something twice, codify it

### Resolvers = just-in-time context loading
- Don't stuff everything into context upfront
- Load the right doc when the right task appears
- Description fields in skills ARE resolvers

### Latent vs. Deterministic boundary
- Push intelligence UP into skills (judgment, synthesis)
- Push execution DOWN into deterministic code (tests, parsers, SQL)
- NEVER put combinatorial/algorithmic work in latent space
- Tests must be fully deterministic — no LLM output in assertions

### Every feature must be tested before commit
- Write the test first or alongside the implementation
- cargo test --workspace MUST pass before every commit
- cargo clippy --workspace MUST be clean
- Commit and push after each complete feature

## Pitfalls
- DO NOT use `~/.nanobot` — always use `~/.nanobot-rs` (or `NANOBOT_RS_HOME` env)
- DO NOT name the binary `nanobot` — it's `nanobot-rs`
- Tests in `crates/nanobot-config` may touch filesystem — use tempdir pattern
- The bus uses tokio broadcast — receivers need to keep up or handle laggy messages
- Session store uses SQLite — be mindful of concurrent access patterns
- Provider implementations should handle rate limiting (429) with exponential backoff
