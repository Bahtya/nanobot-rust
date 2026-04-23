//! Native Unix daemon support for kestrel.
//!
//! Provides daemonization (double-fork), PID file management with `flock`,
//! async signal handling via `tokio::signal::unix`, and file-based logging
//! with `tracing-appender`.
//!
//! Inspired by Cloudflare Pingora's `Server` architecture:
//! - daemonize runs **before** the tokio runtime starts (fork kills threads)
//! - PID file uses `flock(LOCK_EX|LOCK_NB)` for atomic locking
//! - Signal handlers use async `tokio::signal::unix`, NOT `libc signal()`
//!
//! All public APIs are gated on `cfg(target_family = "unix")`.

#[cfg(target_family = "unix")]
pub mod audit;
#[cfg(target_family = "unix")]
pub mod daemonize;
#[cfg(target_family = "unix")]
pub mod logging;
#[cfg(target_family = "unix")]
pub mod pid_file;
#[cfg(target_family = "unix")]
pub mod signal;
