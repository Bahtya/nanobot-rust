//! # nanobot-config
//!
//! Configuration loading and schema for nanobot-rust.
//! Maintains YAML compatibility with the Python nanobot config format.

pub mod loader;
pub mod migration;
pub mod paths;
pub mod schema;
pub mod validate;

pub use loader::load_config;
pub use schema::Config;
pub use validate::{validate, ValidationReport};
