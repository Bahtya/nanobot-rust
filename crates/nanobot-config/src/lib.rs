//! # nanobot-config
//!
//! Configuration loading and schema for nanobot-rust.
//! Maintains YAML compatibility with the Python nanobot config format.

pub mod loader;
pub mod migration;
pub mod paths;
pub mod python_migrate;
pub mod python_schema;
pub mod schema;
pub mod validate;

pub use loader::load_config;
pub use python_migrate::{migrate_from_python, MigrationReport, MigrationResult};
pub use schema::Config;
pub use validate::{fill_defaults, validate, validate_and_fill, ValidationReport};
