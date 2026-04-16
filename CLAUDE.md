# kestrel

Rust multi-platform AI agent framework. Binary: `kestrel`. Config: `~/.kestrel`. Env override: `KESTREL_HOME`.

## Rules (non-negotiable)

- **Paths**: `~/.kestrel` (NOT `~/.kestrel`). Binary `kestrel` (NOT `kestrel`).
- **Every commit must pass**: `cargo test --workspace` + `cargo clippy --workspace` = 0 failures, 0 warnings.
- **Every feature needs tests** before commit. Tests are deterministic — no LLM output in assertions.
- **Commit + push** after each complete feature. Don't accumulate uncommitted changes.
- **Doc comments** on all `pub` functions (`///` style).
- **GitHub Issue lifecycle (MANDATORY)**:
  1. At task start: `gh issue comment #N --repo Bahtya/kestrel --body "Starting work. Branch: <branch>"`
  2. After each milestone: `gh issue comment #N --repo Bahtya/kestrel --body "<what was done>"`
  3. After commit+push: `gh issue comment #N --repo Bahtya/kestrel --body "Commit <hash>: <summary>"`
  4. When done: `gh issue close #N --repo Bahtya/kestrel --comment "<final summary>"`
  - Issues are the shared coordination layer. If you skip this, nobody knows what you've done.

## Architecture

16 crates + binary. Read individual crate source for details.

```
kestrel-core      → Types, errors, constants
kestrel-config    → YAML config, schema, paths
kestrel-bus       → Tokio broadcast message bus
kestrel-session   → SQLite conversation store
kestrel-security  → SSRF protection, URL validation
kestrel-providers → LLM providers (OpenAI-compat, Anthropic) with retry
kestrel-tools     → Tool registry + builtins (shell, web, fs, cron, search, spawn, message, skills)
kestrel-agent     → Agent loop, context, compaction, sub-agents
kestrel-cron      → Tick-based scheduler with JSON state
kestrel-heartbeat → Health checks, auto-restart, exponential backoff
kestrel-channels  → Telegram (polling) + Discord (WebSocket) via ChannelManager
kestrel-api       → OpenAI-compatible HTTP API (Axum, SSE streaming)
kestrel-daemon    → Unix daemon (double-fork, PID file, signal handling)
kestrel-memory    → MemoryStore trait, HotStore (L1), WarmStore/LanceDB (L2)
kestrel-skill     → Skill trait, TOML manifests, SkillRegistry, SkillCompiler
kestrel-learning  → LearningEvent bus, event processors, prompt assembly
```

Message flow: `InboundMessage → Bus → AgentLoop → (Provider + Tools) → OutboundMessage → Bus → Channel`
Evolution flow: `LearningEvent → EventBus → Processors → (SkillCreate / MemoryUpdate / PromptAdjust)`

## Commands

`cargo build --workspace` | `cargo test --workspace` | `cargo clippy --workspace` | `cargo fmt --all --check`

## Design Principles

- **Thin harness, fat skills**: Harness = loop, files, context, safety only. Complexity in skill files.
- **Latent vs Deterministic**: Judgment → model. Parsing/validation → code. Never mix.
- **Context engineering**: JIT loading, compaction, structured notes outside context window.
- **Fewer, better tools**: Consolidate operations. Token-efficient returns. Poka-yoke.
- **LanceDB over SQLite FTS5**: Semantic vector search for memory/sessions.
- **TOML over YAML**: Rust-native parsing for skill manifests and config.

## Pitfalls

- Bus uses tokio broadcast — receivers must handle lag or drop messages.
- Provider 429 handling: exponential backoff, not immediate retry.
- Tests touching filesystem: use tempdir pattern.
- daemonize MUST run before tokio runtime — fork kills all threads.
- LanceDB: async API, needs runtime spawn for background index maintenance.
- New crates must be added to workspace Cargo.toml `[workspace] members`.
- kestrel-learning depends on types from kestrel-memory and kestrel-skill — use re-exports or shared types from kestrel-core.

## Research

Six Hats analysis (architecture, specs, risks, design) → `docs/0*.md`
Sprint task breakdowns → `docs/task-*.md`
