# nanobot-rs

Rust multi-platform AI agent framework. Binary: `nanobot-rs`. Config: `~/.nanobot-rs`. Env override: `NANOBOT_RS_HOME`.

## Rules (non-negotiable)

- **Paths**: `~/.nanobot-rs` (NOT `~/.nanobot`). Binary `nanobot-rs` (NOT `nanobot`).
- **Every commit must pass**: `cargo test --workspace` + `cargo clippy --workspace` = 0 failures, 0 warnings.
- **Every feature needs tests** before commit. Tests are deterministic — no LLM output in assertions.
- **Commit + push** after each complete feature. Don't accumulate uncommitted changes.
- **Doc comments** on all `pub` functions (`///` style).

## Architecture

15 crates + binary. Read individual crate source for details.

```
nanobot-core      → Types, errors, constants
nanobot-config    → YAML config, schema, paths
nanobot-bus       → Tokio broadcast message bus
nanobot-session   → SQLite conversation store
nanobot-security  → SSRF protection, URL validation
nanobot-providers → LLM providers (OpenAI-compat, Anthropic) with retry
nanobot-tools     → Tool registry + builtins (shell, web, fs, cron, search, spawn, message, skills)
nanobot-agent     → Agent loop, context, compaction, sub-agents
nanobot-cron      → Tick-based scheduler with JSON state
nanobot-heartbeat → Health checks, auto-restart, exponential backoff
nanobot-channels  → Telegram (polling) + Discord (WebSocket) via ChannelManager
nanobot-api       → OpenAI-compatible HTTP API (Axum, SSE streaming)
nanobot-daemon    → Unix daemon (double-fork, PID file, signal handling)
nanobot-memory    → MemoryStore trait, HotStore (L1), WarmStore/LanceDB (L2)
nanobot-skill     → Skill trait, TOML manifests, SkillRegistry, SkillCompiler
nanobot-learning  → LearningEvent bus, event processors, prompt assembly
```

Message flow: `InboundMessage → Bus → AgentLoop → (Provider + Tools) → OutboundMessage → Bus → Channel`
Evolution flow: `LearningEvent → EventBus → Processors → (SkillCreate / MemoryUpdate / PromptAdjust)`

## Commands

`cargo build --workspace` | `cargo test --workspace` | `cargo clippy --workspace` | `cargo fmt --all --check`

CLI subcommands: `agent`, `gateway`, `serve`, `heartbeat`, `health`, `setup`, `status`, `config validate`, `config migrate`

## Design Principles

- **Thin harness, fat skills**: Harness = loop, files, context, safety only. Complexity in skill files.
- **Latent vs Deterministic**: Judgment → model. Parsing/validation → code. Never mix.
- **Context engineering**: JIT loading, compaction, structured notes outside context window.
- **Fewer, better tools**: Consolidate operations. Token-efficient returns. Poka-yoke.
- **LanceDB over SQLite FTS5**: Semantic vector search replaces keyword full-text search for memory/sessions.
- **TOML over YAML**: Rust-native parsing for skill manifests and config.

## Sprint 3: Self-Evolution MVP ✅ DONE

Merged to main. 3 new crates: nanobot-memory (50 tests), nanobot-skill (68 tests), nanobot-learning (34 tests).

## Sprint 4: Integration — Wire Self-Evolution into AgentLoop

**Goal**: Connect memory, skill, learning crates to the agent loop so self-evolution actually runs.

**2 parallel agents (memory+skill first, learning depends on both):**

| Agent | Branch | Task | Scope | Issue |
|-------|--------|------|-------|-------|
| cc-s4-memory | feat/s4-memory-wire | MemoryStore recall/store in AgentLoop | nanobot-agent, nanobot-memory, gateway | #12 |
| cc-s4-skill | feat/s4-skill-wire | SkillRegistry matching + prompt injection | nanobot-agent, nanobot-skill, gateway | #13 |
| cc-s4-learn | feat/s4-learning-wire | LearningEvent emission + PromptAssembler (AFTER #12+#13) | nanobot-agent, nanobot-learning, gateway | #14 |

**Integration points** (all in `crates/nanobot-agent/src/`):
- AgentLoop::new() — accept MemoryStore, SkillRegistry, EventBus as params
- Before LLM call — recall memory + match skills → inject into prompt
- After tool execution — emit LearningEvent → processors update confidence
- gateway.rs — create instances, wire into AgentLoop

## Research References (Six Hats Analysis)

Six Hat analysis documents at `/tmp/hats/` contain deep Hermes source analysis + nanobot-rust migration specs. Key sections:

| Hat | File | Key Sections |
|-----|------|-------------|
| 🔵 Blue | `01-blue-hat-architecture.md` | §2 Self-evolution loop, §3 KEPA engine, §5 Memory system |
| ⚪ White | `02-white-hat-specification.md` | §1 Memory data model, §2 Skill data model, §3 Self-review, §6 Tool system |
| 🔴 Red | `03-red-hat-critique.md` | §1 Design smells, §4 Migration difficulty ranking |
| ⚫ Black | `04-black-hat-risks.md` | §5 Python→Rust pitfalls, §6 Migration safeguards |
| 🟡 Yellow | `05-yellow-hat-value.md` | §2 MVP definition, §7 Implementation order, §6 2-week plan |
| 🟢 Green | `06-green-hat-design.md` | §1 Skill architecture, §2 Event-driven learning, §3 Memory layers, §8 3-phase plan |

**Phase plan** (Green Hat §8): Phase 1 MVP (2-3wk) → Phase 2 Core (4-6wk) → Phase 3 Advanced (6-8wk)

## Pitfalls

- Bus uses tokio broadcast — receivers must handle lag or drop messages.
- Provider 429 handling: exponential backoff, not immediate retry.
- Tests touching filesystem: use tempdir pattern.
- daemonize MUST run before tokio runtime — fork kills all threads.
- LanceDB: async API, needs runtime spawn for background index maintenance.
- New crates must be added to workspace Cargo.toml `[workspace] members`.
- nanobot-learning depends on types from nanobot-memory and nanobot-skill — use re-exports or shared types from nanobot-core.
