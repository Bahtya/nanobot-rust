# Changelog

## [v0.8.2] - 2026-05-09

### New Features
- feat(setup): review-centric navigation — user can freely pick any step to configure (#278)
  - Setup starts from Review menu, user jumps to any step independently
  - No more forced linear walk through all steps
  - Each step returns to Review after completion
  - "Skip (keep current)" option for each channel step

### Bug Fixes
- fix(setup): remove misleading "Go back" from Provider selection list
- fix(setup): properly track channel status from actual config state

## [v0.8.1] - 2026-05-09

### New Features
- feat(setup): interactive wizard overhaul — back-navigation, Quick/Full setup, input validation, enhanced config summary (#278)
  - State-machine loop with "Go back" on every step (#266)
  - Quick Setup (provider only) vs Full Setup mode (#274)
  - First-run initialization wizard vs update wizard banners (#268)
  - Provider list with recommended markers and grouping (#270)
  - API key format validation, URL scheme check (#272)
  - Enhanced config summary with key first-3...last-4, channel status (#271)
  - Unified channel confirmation prompts (#276)
  - Next Steps guidance after save (#277)
  - Graceful Ctrl+C interruption (#273)

### Bug Fixes
- fix(setup): step numbering duplicate — Step 5 appeared twice (#269)

## [v0.8.0] - 2026-05-08

### New Features
- feat(feishu): WebSocket long-connection mode — persistent outbound connection, no public endpoint needed (PR #253)
- feat(feishu): media send/receive — image, file, audio, video upload and download with CDN integration (PR #254)
- feat(feishu): security and access control — webhook verification token, AES payload decryption, group/DM policy, bot filtering, mention-only mode (PR #255)
- feat(feishu): message deduplication and batching — message_id + content fingerprint dedup, auto-batch rapid messages with smart split detection (PR #256)
- feat(feishu): typing indicator and interactive card message support — typing events, card action handling, message ID tracking (PR #257)
- feat(weixin): media send/receive with AES-256-GCM encryption (PR #258)
- feat(weixin): smart message chunking — automatic split near platform char limit with regex-aware boundary detection (PR #259)
- feat(daemon): Windows Service daemon support with service CLI subcommand (PR #245, #264)
- feat(daemon): cross-platform logging module (PR #265)

### Changes
- ci: add Windows CI matrix for compilation and test verification (PR #262)
- release: add Windows (x86_64-pc-windows-msvc) build target to release workflow

### Bug Fixes
- fix(config): use Windows-compatible default paths in DaemonConfig (PR #261)
- fix(config): add Windows-compatible paths in platform.rs (PR #244)
- fix(tools): use /c flag for cmd.exe on Windows in shell build_command (PR #260)
- fix(binary): cfg-gate Unix-only kestrel_daemon usage (PR #263)
- fix(daemon): gate nix/libc deps to Unix-only for Windows compat (PR #243)
- fix(agent): gate Unix-only heartbeat tests with cfg(unix) (PR #242)

## [v0.7.4] - 2026-05-08

### Bug Fixes
- fix(weixin): harden getUpdates parsing and cursor handling (PR #246)

## [v0.7.3] - 2026-05-08

### Bug Fixes
- fix(weixin): handle non-empty msgs in getUpdates response
- fix(weixin): allow dead_code for iLink compatibility fields

## [v0.7.2] - 2026-05-08

### Bug Fixes
- Weixin iLink polling now tolerates `getupdates` response drift to avoid update fetch failures

## [v0.7.1] - 2026-05-07

### Bug Fixes
- Re-enable the `kestrel setup weixin` subcommand, restore Weixin wizard credential handling, and add Feishu webhook flow test coverage (#230)

## [v0.7.0] - 2026-05-07

### New Features
- Feishu (Lark) channel support with webhook handling, gateway integration, and status visibility (#223)
- Automated Feishu / Lark QR scan-to-create onboarding flow for setup (#225)
- WeChat (Weixin) iLink Bot API channel integration with long-polling, typing indicators, DM/group policy controls, and context-token persistence (#228)
- WeChat setup and operations wired into the product flow: gateway auto-start detection, status output, and setup wizard coverage for manual credentials plus QR onboarding support (#228)
- Feishu / Lark is now discoverable directly from the main `kestrel setup` wizard, with QR auto-onboarding launched inline from the unified setup entry (#229)

### Changes
- `kestrel setup` is now the single setup entry point for Feishu / Lark onboarding; the old `kestrel setup feishu` shortcut was removed (#229)

### Bug Fixes
- Feishu onboarding review fixes: preserve existing proxy settings, avoid sensitive JSON parse leaks, and keep MSRV-compatible dead-code annotations (#226)

## [v0.6.0] - 2026-04-30

### New Features
- Interactive setup wizard (`kestrel setup`) for guided first-time configuration (#220, #221)
- Diagnostic command (`kestrel doctor`) to verify connectivity, config validity, and environment health (#219)
- GLM Coding Plan (智谱) added as built-in provider with deepseek-coder and glm-4 models (#222)
- Config import/export with AES-256-GCM encryption for secure backup (#216)
- Improved Telegram `/models` menu UX — collapse on select, paginate large lists (#218)

### Bug Fixes
- Fix `provider_field!` macro unused warning in release builds
- Fix `handle_status()` missing opencode_go provider list
- Fix token masking for short tokens
- Various clippy and rustfmt compliance fixes

## [v0.5.9] - 2026-04-30

### New Features
- Communication log system with trace_id tracking for HTTP/WS/tool calls (#214)
- Configurable comm_log level and separate comm.log file

## [v0.5.6] - 2026-04-29

### Bug Fixes
- Model/provider routing: fix prefix precedence, reflection prefix stripping, and WebSocket `/models` command (#209)

## [v0.5.5] - 2026-04-29

### New Features
- Two-level model/provider selection with `/models` command in Telegram (#208)
- Model name prefix stripping: `provider/model` format strips prefix before API call (#208)

## [v0.5.4] - 2026-04-29

### New Features
- Real-time tool call streaming with icon + argument preview (#207)

## [v0.5.3] - 2026-04-29

### Bug Fixes
- SSE streaming reliability: separate HTTP client for streaming (no total timeout), preventing reqwest from killing long-lived SSE connections at 30s (#206)
- Match "timed out" (with space) in retryable error detection alongside "timeout" (#206)
- Stream-error quick retry (up to 2 attempts with short backoff) in `complete_streaming` before falling through to agent-loop retry (#206)
- Slow-provider WARN logs when SSE stream takes >5s to connect or has >10s gaps between chunks (#206)
- Deadline-aware retries: skip retry when it would exceed the message timeout budget (#206)

### New Features
- OpenCode Go provider and extensible model selection system (#205)

## [v0.5.1] - 2026-04-29

### New Features
- Add `/settings gateway`, `/settings timeout`, `/settings retry` subcommands for runtime configuration

### Bug Fixes
- Fix merge conflict markers left in `commands.rs`
- Clean up stale sessions on timeout, add deadline-aware retries (#204)

## [v0.5.0] - 2026-04-29

### New Features
- Add OpenRouter deepseek-v4-flash model support
- WebSocket local command dispatch: `/settings`, `/help`, `/status`, `/menu`, `/history`, `/skill`, `/validate` are now handled locally instead of forwarding to LLM
- Text-based model switching commands over WebSocket: `/settings model`, `/settings model next`, `/settings model <name>`

### Bug Fixes
- Fix `sanitize_error_for_user` infinite loop that caused CI Test job to hang — replaced `while let + find` pattern with `regex::replace_all`
- Error sanitization now properly strips `user_id` from upstream API error responses sent over WebSocket
- Fix UX: add progress feedback for long tasks and interrupt-replan for busy sessions (#186, #187)
- Register `settings_view` callback handler for pagination in Telegram channel
- Fix Telegram test assertions for `settings_view` handler (handler_count 3 → 4)

### Performance
- Eliminate 22× `sleep(150/200ms)` in websocket tests — replaced with `wait_for_client_count()` event-driven waiting
- Reduce streaming test sleep from 150ms to 20ms
- Reduce integration test sleeps from 100/200ms to 10/20ms
- Remove redundant CI Build job (Test job already compiles all code)
- Unify CI cache keys between clippy and test jobs

### CI/CD
- Add conditional disk space cleanup (only triggers when usage > 80%) to prevent runner "No space left on device" errors
- Temporarily disable slow websocket integration tests in CI (#198) — can re-enable with `cargo test -- --ignored`

### Cleanup
- Remove `TEST_REVIEW.md` from repo root
- Remove accidentally committed binary files (`kestrel`, `kestrel-x86_64-linux.tar.gz`)
- Delete `SerialTest` dependency, replace with async-safe test patterns

## [v0.4.6] - 2026-04-28

- Initial release with Telegram, Discord, and WebSocket channels
- OpenRouter and multi-provider LLM support
- Agent loop with streaming, heartbeat, and session management
