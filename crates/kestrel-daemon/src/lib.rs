//! Native daemon support for kestrel.
//!
//! Provides platform-specific background service management:
//!
//! **Unix**: daemonization (double-fork), PID file management with `flock`,
//! async signal handling via `tokio::signal::unix`, and file-based logging
//! with `tracing-appender`.
//!
//! **Windows**: Windows Service registration via `windows-service` crate,
//! PID file management with `fs4` file locking, and Ctrl+C signal handling.
//!
//! Inspired by Cloudflare Pingora's `Server` architecture:
//! - daemonize runs **before** the tokio runtime starts (fork kills threads)
//! - PID file uses `flock(LOCK_EX|LOCK_NB)` for atomic locking (Unix)
//! - Signal handlers use async `tokio::signal::unix`, NOT `libc signal()` (Unix)
//!
//! Unix-only modules are gated on `cfg(target_family = "unix")`.
//! Windows-only modules are gated on `cfg(target_family = "windows")`.

#[cfg(target_family = "unix")]
pub mod audit;
#[cfg(target_family = "unix")]
pub mod daemonize;
pub mod logging;
#[cfg(target_family = "unix")]
pub mod pid_file;
#[cfg(target_family = "unix")]
pub mod signal;

#[cfg(target_family = "windows")]
pub mod windows_service;
