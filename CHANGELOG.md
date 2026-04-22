# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1] - 2026-04-22

### Fixed

- Fix macOS x86_64 release build by disabling AVX512 in `.cargo/config.toml` (#135)

## [0.2.0] - 2026-04-22

### Added

- Self-evolution observability: learning event consumers, action outcome tracking, and processor metrics (#125)
- JSONL schema versioning for HotStore persistence with backward-compatible migration (#123)
- Word-boundary text search precision with `\b` regex patterns and ReDoS mitigation (#122)
- XML memory isolation with `<memory-context>` tags and budget-aware truncation (#120)
- Deferred writes for HotStore with auto-flush threshold and `Drop` best-effort flush (#120)
- Token budget control for memory injection (`MEMORY_CHAR_BUDGET` at 2200/1375 chars) (#120)
- Entry-level memory budget skipping — no more broken mid-entry truncation (#133)
- Configurable memory char budgets via `MemoryConfig` (backward compatible defaults) (#133)
- LanceDB predicate input validation to prevent injection (#132)
- WarmStore write serialization with `tokio::sync::Mutex` (#132)
- Security scanner limitation documentation (#134)

### Fixed

- XML escaping of memory content to prevent prompt injection via `</memory-context>` (#126)
- Session persistence race condition in e2e tests (#124)
- Clippy warnings and dead code cleanup
- `mark_dirty()` error handling in HotStore auto-flush (#126)

### Changed

- Removed expired documentation: Six Hats analysis, sprint plans, and task files

## [0.1.0] - 2025-04-20

### Added

- Complete Rust workspace with 16 crates: core, config, bus, session, security, providers, tools, agent, cron, heartbeat, channels, api, daemon, memory, skill, learning
- Agent loop with context compaction, structured notes, and sub-agent spawning
- OpenAI-compatible HTTP API with SSE streaming, auth middleware, and request logging
- Telegram channel adapter with polling, inline keyboards, typing indicators, read receipts, and callback routing
- Discord channel adapter with WebSocket gateway, RESUME reconnection, typing, and read receipts
- WebSocket channel adapter for browser-based chat with rich protocol and streaming
- LLM providers (OpenAI-compatible, Anthropic) with retry, circuit breaker, and exponential backoff
- Tool registry with built-in tools: shell, web, filesystem, cron, search, spawn, message, skills
- LanceDB-backed persistent memory store with HotStore (L1) and WarmStore (L2)
- Skill system with TOML manifests, SkillRegistry, SkillCompiler, and hot-reload
- Learning event bus with processors for skill creation, memory updates, and prompt adjustment
- Prompt assembler for enriched context with tool guidance, memory fences, and skill index
- Cron scheduler with tick-based execution and JSON state persistence
- Heartbeat service with health checks, auto-restart, and exponential backoff
- Unix daemon mode with double-fork, PID file, and signal handling
- Config validation with cross-field checks, channel-provider validation, and env-var support
- Python-to-Rust config migration tool with YAML input, validation, and dry-run
- Context window budget manager with smart pruning and overflow events
- Security module with SSRF protection, URL validation, and sandboxed exec
- Mutating tool serialization for deterministic agent execution
- GitHub Actions CI with format, clippy, build, test, security audit, and merge conflict detection
- Progressive disclosure and dynamic skill commands with evolution triggers
- Config-driven HTTP/SOCKS5 proxy support for Telegram and Discord
- Online notification on Telegram channel connect
- /menu, /settings, /history, /help, /status, /validate, /reset commands across channels

### Changed

- Renamed project from nanobot-rust to kestrel
- Replaced WarmStore HashMap with LanceDB persistent vector backend
- Simplified CLAUDE.md to thin harness with pointers only
- Enhanced README with build deps, API docs, and config reference
- Updated CI workflow with separate jobs, cargo-audit, and improved caching

### Fixed

- Resolved CI failures from skill_loader compile error and rustls-webpki security update
- Fixed provider 503 retry with dedicated aggressive backoff strategy
- Fixed clippy dead_code warnings and hardened unwrap calls
- Fixed flaky test_handle_callback_settings_streaming_toggle
- Fixed cron sort to use sort_by_key for descending order
- Fixed unnecessary u64 cast in retry jitter calculation
- Fixed tool argument parse errors to surface instead of defaulting to empty JSON
- Fixed session regression tests after note store redesign
- Fixed Discord Gateway intents and heartbeat cleanup
- Fixed heartbeat to use configured model name in provider health check
- Fixed learning: removed dead code, added post-task reflection, and validated empty instructions
- Fixed skill mutation lock and disk-first writes
- Fixed HotStore LRU eviction to O(1) performance
- Fixed agent wiring: skill index into runtime and ProcessorStats version tag
- Fixed health checks wired into gateway startup path
- Fixed cargo fmt after PR merges
- Removed legacy memory.rs, unified on kestrel-memory crate

[Unreleased]: https://github.com/Bahtya/kestrel-agent/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/Bahtya/kestrel-agent/releases/tag/v0.2.0
[0.1.0]: https://github.com/Bahtya/kestrel-agent/releases/tag/v0.1.0
