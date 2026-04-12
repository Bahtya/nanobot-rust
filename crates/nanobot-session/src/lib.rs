//! # nanobot-session
//!
//! Session management with JSONL persistence and history truncation.

pub mod manager;
pub mod store;
pub mod types;

pub use manager::SessionManager;
pub use types::*;
