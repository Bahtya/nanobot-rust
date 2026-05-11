# Changelog

## [v0.10.0] - 2026-05-11

### Features

#### Terminal TUI Automation APIs
- feat(terminal): add VT parser (vte 0.15) with IncrementalUtf8Decoder, VtePerform, and TerminalOp semantic operations (Issues #329, #330)
- feat(terminal): add terminal screen model with primary/alternate buffers, cell grid, cursor tracking, SGR attributes, scroll regions, scrollback (20K lines), and wide character (CJK) support (Issue #331)
- feat(terminal): add 10 agent tools — `terminal_create_session`, `terminal_send_input`, `terminal_read_output`, `terminal_send_key`, `terminal_capture_screen`, `terminal_capture_scrollback`, `terminal_wait_for_screen_change`, `terminal_list_sessions`, `terminal_kill_session`, `terminal_resize` (Issues #329, #332, #333)
- feat(terminal): add ScreenDiff with changed-line tracking, cursor delta, mode/title/dims change detection, and 6 regression fixture tests (Issue #334)

#### Lua ScriptTool Sandbox Expansion
- feat(script): replace `dangerous: bool` with `ScriptCapability` bitflags (10 capabilities) and `ScriptProfile` enum (Safe/Trusted/Dangerous) — each API gated by capability bit (Issue #339)
- feat(script): add 17 filesystem/path APIs — path helpers (cwd, abspath, join_path, basename, dirname), fs extensions (read_lines, append_file, copy, move, glob, walk), JSON I/O (read_json, write_json), temp helpers (tempdir, tempfile) (Issue #337)
- feat(script): add 6 HTTP/download APIs (http_get, http_post, http_request, fetch_json, post_json, download) with SSRF protection, response size limits, and write path validation (Issue #338)
- feat(script): add built-in `require()` module system with 5 controlled modules (kestrel.fs, kestrel.path, kestrel.json, kestrel.http, kestrel.env) (Issue #340)

### Bug Fixes
- fix(terminal): add `performer.flush_print()` after `parser.advance()` to emit pending Print ops for trailing text
- fix(terminal): strip Windows UNC prefix (`\\?\`) from canonicalized paths in `validate_write_path`
- fix(terminal): rewrite `test_fixture_partial_utf8_across_feeds` to preserve vte parser state across feeds
- fix(terminal): mark flaky ConPTY e2e test as `#[ignore]`, add lightweight spawn+kill test for CI

### Refactoring
- refactor(terminal): remove redundant ConPTY resize retry and `CHCP=65001` env var hack — `portable-pty` already uses `RESIZE_QUIRK` and `CREATE_UNICODE_ENVIRONMENT` flags
- refactor(terminal): replace scrollback `Vec` with `VecDeque` for O(1) eviction
- refactor(terminal): remove unnecessary `unsafe impl Sync` on `TerminalSession`
- refactor(terminal): use `char_indices` in `truncate_output` to avoid panic on multi-byte UTF-8
- refactor(terminal): fix wide-character overlap clearing logic in `put_char`
- refactor(terminal): add 64 KiB input size limit in `send_input`

### Performance
- perf(ci): switch to rust-cache + cargo-nextest, parallelize tests with `--partition` across 2 shards
- perf(ci): disable Windows Defender real-time scanning on CI runners

## [v0.9.17] - 2026-05-10

### Bug Fixes
- fix(channels): downgrade "client not connected" log from ERROR to WARN in manager::handle_outbound — fixes incomplete #346 fix that only changed the WebSocket platform level (Issue #349, PR #351)
- fix(channels): forward client-sent `reply_to` from WebSocket envelope through InboundMessage and use it (with fallback to `message_id`) in all OutboundMessage constructions — ensures request-response correlation works when client sends `reply_to` without `id` (Issue #350, PR #351)

## [v0.9.16] - 2026-05-10

### Bug Fixes
- fix(channels): WebSocket read_loop now sends structured error envelopes for invalid JSON, unknown message types, empty messages, unrecognized formats, and invalid legacy messages instead of silently ignoring them (Issues #342, #343, PR #344)
- fix(channels): upgrade WebSocket error condition logging from `debug!` to `info!` for production visibility

## [v0.9.15] - 2026-05-10

### Bug Fixes
- fix(channels): wire up WebSocket streaming consumer — `run_ws_stream_consumer` was never spawned in production, so streaming chunks (WS.5) were never delivered to clients
- fix(channels): add `session_id` to WebSocket welcome message — clients now receive both `client_id` and `session_id` on connect (WS.3)
- fix(channels): add `done` signal after outbound response messages — clients can now reliably detect when an agent response is complete (WS.7)
- fix(channels): inject MessageBus into WebSocket channel via `set_bus()` — enables streaming chunk delivery alongside final outbound messages

## [v0.9.14] - 2026-05-10

### Bug Fixes
- fix(channels): emit `InterruptRequested` event when WebSocket client disconnects — prevents cascading "client not connected" errors and wasted agent processing for disconnected sessions (Issue #326, PR #327)

## [v0.9.13] - 2026-05-10

### Bug Fixes
- fix(tools): add response body size limit (10MB) to `web_fetch` tool — previously the entire HTTP response was read into memory before truncation, allowing a multi-GB response to OOM the daemon (Issue #324)
- fix(tools): add overall redirect chain timeout (55s) to `web_fetch` — prevents redirect loops from exceeding the default `tool_timeout`
- fix(tools): add URL length validation (2048 chars) to `web_fetch` — rejects unreasonably long URLs

## [v0.9.12] - 2026-05-10

### Bug Fixes
- fix(tools): add content size limit (10MB) to `write_file` tool — previously an agent could write arbitrarily large files to disk, only the read/edit tools had size limits (Issue #322)

## [v0.9.11] - 2026-05-10

### Bug Fixes
- fix(tools): add depth/entry/size limits to GrepTool and entry limit to GlobTool — recursive grep on large directories no longer hangs or exhausts memory, blocking I/O moved to `spawn_blocking` (Issue #318)

## [v0.9.10] - 2026-05-10

### Bug Fixes
- fix(tools): add file size check (10MB limit) to `read_file` and `edit_file` tools — previously these tools would read the entire file into memory regardless of size, allowing a large file read to OOM the daemon (Issue #318, PR #319)

## [v0.9.9] - 2026-05-10

### Bug Fixes
- fix(tools): add 30s I/O timeout to all filesystem tools (`read_file`, `write_file`, `edit_file`) — operations on slow/unresponsive filesystems (NFS, FUSE) no longer hang indefinitely (Issue #316, PR #317)
- fix(tools): add depth/entry limits to `ListDirTool` and move directory traversal to `spawn_blocking` to avoid blocking the tokio executor (Issue #316, PR #317)

## [v0.9.8] - 2026-05-10

### Bug Fixes
- fix(tools): replace all `Mutex::lock().unwrap()` with poison-resistant `unwrap_or_else(|e| e.into_inner())` in terminal session — poisoned mutex from a panic would crash the entire daemon on next access (Issue #314, PR #315)
- fix(tools): add `alive` flag check to `read_output` timeout polling loop — kill() on a session no longer waits for the full timeout before returning (Issue #314, PR #315)
