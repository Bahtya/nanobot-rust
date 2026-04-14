# Daemon Mode Implementation Plan

## Module Structure: `crates/nanobot-daemon/`

```
crates/nanobot-daemon/
├── Cargo.toml
└── src/
    ├── lib.rs         — pub mod daemonize, pid_file, signal, logging
    ├── daemonize.rs   — Daemonizer struct: double-fork via nix crate
    ├── pid_file.rs    — PidFile: flock-based atomic PID file lock
    ├── signal.rs      — SignalHandler: tokio::signal::unix for SIGTERM/SIGINT/SIGHUP
    └── logging.rs     — setup_file_logging: tracing-appender non-blocking writer
```

## API Surface

### daemonize.rs
- `pub struct Daemonizer { working_dir, pid_file, log_file }`
- `pub fn daemonize(config: &DaemonConfig) -> Result<()>` — double-fork, setsid, chdir, umask, redirect stdio

### pid_file.rs
- `pub struct PidFile { path, file }` — holds locked file handle
- `pub fn create(path: &str) -> Result<PidFile>` — create + flock(LOCK_EX|LOCK_NB) + write PID
- `pub fn read_pid(path: &str) -> Result<Option<i32>>` — read PID from file (no lock)
- `pub fn clean(self) -> Result<()>` — drop lock + unlink file (consume on exit)

### signal.rs
- `pub enum ShutdownSignal { Graceful, Fast, Reload }`
- `pub async fn wait_for_signal() -> ShutdownSignal` — tokio::signal::unix for SIGTERM/SIGINT/SIGHUP
- `pub fn send_sigterm(pid: i32) -> Result<()>` — for `daemon stop`

### logging.rs
- `pub fn setup_file_logging(log_dir: &str, level: &str) -> Result<WorkerGuard>` — tracing-appender non-blocking

## Integration Points

### main.rs
- Add `Daemon` subcommand with `DaemonAction` enum (Start, Stop, Restart, Status)
- `daemon start`: call `nanobot_daemon::daemonize::daemonize()` BEFORE `#[tokio::main]` enters
- `daemon stop`: read PID, send SIGTERM, wait
- `daemon restart`: stop + start
- `daemon status`: read PID, check `/proc/{pid}` exists

### gateway.rs
- Replace `tokio::signal::ctrl_c()` with `nanobot_daemon::signal::wait_for_signal()`
- Handle `Graceful` (SIGTERM) and `Fast` (SIGINT) shutdown signals
- Log `Reload` (SIGHUP) as placeholder
- Create PidFile on start, clean on exit

### serve.rs
- Same signal/PID treatment as gateway.rs

## Config Schema Changes

```rust
// In crates/nanobot-config/src/schema.rs:
pub struct DaemonConfig {
    pub enabled: bool,              // default false
    pub pid_file: String,           // default ~/.nanobot-rs/nanobot-rs.pid
    pub log_dir: String,            // default ~/.nanobot-rs/logs
    pub working_directory: String,  // default "/"
    pub grace_period_secs: u64,     // default 30
}
```

Add `daemon: DaemonConfig` field to `Config` struct.

## Dependencies

- `nix` (for fork, setsid, flock, kill — manual double-fork, no daemonize crate)
- `tracing-appender` (for non-blocking file writer)
- `tokio` with `signal` feature (already in workspace)

## Test Plan

- `pid_file.rs`: test create/read/clean with tempdir, test double-lock fails
- `daemonize.rs`: test Daemonizer::new builds correct struct (can't test actual fork in unit tests)
- `signal.rs`: test ShutdownSignal enum construction
- `logging.rs`: test setup creates log directory and returns guard
- `schema.rs`: test DaemonConfig defaults, test YAML parse with daemon section
