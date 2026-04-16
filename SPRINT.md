# Sprint 2: Native Daemon — Pingora-Inspired Service Architecture

## Sprint 2 Status: In Progress

**Goal**: Implement native daemon mode for kestrel, inspired by Cloudflare Pingora's Server/Service lifecycle management.

**Reference Architecture**: https://github.com/cloudflare/pingora — Server::new → bootstrap → add_service → run_forever pattern. Key modules: server/mod.rs, server/daemon.rs, services/background.rs, server/transfer_fd.rs.

## Why Pingora?

Pingora solves exactly our problem: a Rust async server that needs to run as a proper Unix daemon with:
- PID file management
- Signal handling (SIGTERM/SIGINT/SIGHUP)
- Graceful shutdown with timeout
- Background service management
- Zero-downtime upgrade via FD passing (future scope)

## Closed-Loop Development Workflow

This sprint uses a single CC agent running a complete closed loop:

```
[Explore] → [Plan] → [Implement] → [Verify] → [Commit] → [PR]
     ↑                                                    |
     └────────── If verify fails, loop back ──────────────┘
```

### Phase 1: Explore (understand before writing)
- Read Pingora's server/daemon.rs, services/mod.rs, services/background.rs
- Read current kestrel gateway.rs, main.rs, heartbeat crate
- Understand the gap: what's missing for daemon mode

### Phase 2: Plan (design before coding)
- Design kestrel-daemon crate structure
- Define DaemonConfig schema
- Map Pingora patterns to kestrel architecture
- Write plan to docs/plan-daemon.md

### Phase 3: Implement (code)
- Create kestrel-daemon crate
- Implement: daemonize, pid_file, signal, logging modules
- Integrate into main.rs CLI (add daemon subcommand)
- Integrate into gateway.rs (replace ctrl_c with signal handler)
- Add DaemonConfig to config schema

### Phase 4: Verify (test everything)
- cargo test --workspace (all existing tests pass)
- cargo clippy --workspace (0 warnings)
- Manual test: daemon start/stop/restart/status
- Manual test: signal handling (SIGTERM graceful shutdown)
- Manual test: PID file lifecycle

## Deliverables

### New Crate: kestrel-daemon
```
crates/kestrel-daemon/
├── Cargo.toml
├── src/
│   ├── lib.rs          # Public API
│   ├── daemonize.rs    # Unix double-fork (Pingora pattern)
│   ├── pid_file.rs     # PID file management with flock
│   ├── signal.rs       # SIGTERM/SIGINT/SIGHUP handling
│   └── logging.rs      # File logging with tracing-appender
```

### Modified Files
- `src/main.rs` — add `daemon` subcommand (start/stop/restart/status)
- `src/commands/gateway.rs` — replace ctrl_c with signal handler, add PID file lifecycle
- `src/commands/serve.rs` — same signal/PID treatment
- `crates/kestrel-config/src/schema.rs` — add DaemonConfig struct
- `Cargo.toml` — add workspace member + dependencies

### Config Addition
```yaml
daemon:
  enabled: false
  pid_file: ~/.kestrel/kestrel.pid
  log_dir: ~/.kestrel/logs
  working_directory: /
```

## Pingora Patterns to Apply

1. **Server facade** — `Server::new(opt).bootstrap().add_service(svc).run_forever()` — adapt for kestrel gateway
2. **Service trait** — each long-running component (channels, heartbeat, api) as a named Service
3. **Shutdown broadcast** — tokio watch channel for shutdown signal propagation (we already have MessageBus, extend it)
4. **Daemonize** — Pingora uses `daemonize` crate, double-fork + PID file + stderr redirect
5. **Execution phases** — Setup → Bootstrap → Running → GracefulShutdown → Terminated
6. **Grace period** — configurable timeout for in-flight requests to complete

## Out of Scope (Future Sprints)
- Zero-downtime upgrade via FD passing (Sprint 3)
- Hot config reload via SIGHUP (Sprint 3)
- Systemd service file generation
- Multi-process architecture

## Pass/Fail Criteria
- [ ] `cargo test --workspace` passes (0 failures)
- [ ] `cargo clippy --workspace` passes (0 warnings)
- [ ] `kestrel daemon start` backgrounds the process with PID file
- [ ] `kestrel daemon stop` sends SIGTERM, process exits gracefully
- [ ] `kestrel daemon status` shows PID and process state
- [ ] `kestrel daemon restart` = stop + start
- [ ] SIGTERM triggers graceful shutdown (channels drain, API completes in-flight)
- [ ] PID file cleaned up on exit (normal and signal)
- [ ] Config `daemon.enabled: true` makes gateway auto-daemonize without subcommand
- [ ] Doc comments on all pub functions
