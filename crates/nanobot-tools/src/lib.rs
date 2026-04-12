//! # nanobot-tools
//!
//! Tool system with trait definition, registry, and built-in tools.

pub mod builtins;
pub mod registry;
pub mod schema;
pub mod trait_def;

pub use registry::ToolRegistry;
pub use trait_def::{Tool, ToolError};
