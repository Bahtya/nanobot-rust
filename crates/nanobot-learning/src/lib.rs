//! # nanobot-learning
//!
//! Event-driven learning system for nanobot-rust.
//!
//! Provides learning event types, an async event bus, event persistence,
//! basic event processors, and prompt assembly for injecting learned
//! context into agent prompts.

pub mod config;
pub mod event;
pub mod processor;
pub mod prompt;
pub mod store;

pub use config::LearningConfig;
pub use event::{ErrorClassification, LearningAction, LearningEvent};
pub use processor::{BasicEventProcessor, LearningEventHandler, ProcessorStats};
pub use prompt::{PromptSection, PromptAssembler};
pub use store::EventStore;
