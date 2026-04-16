//! # kestrel-learning
//!
//! Event-driven learning system for kestrel.
//!
//! Provides learning event types, an async event bus, event persistence,
//! basic event processors, and prompt assembly for injecting learned
//! context into agent prompts.

pub mod config;
pub mod consumer;
pub mod event;
pub mod processor;
pub mod prompt;
pub mod store;

pub use config::LearningConfig;
pub use consumer::LearningConsumer;
pub use event::{ErrorClassification, LearningAction, LearningEvent};
pub use processor::{BasicEventProcessor, LearningEventHandler, ProcessorStats};
pub use prompt::{MemoryFenceEntry, PromptAssembler, PromptSection, SkillIndexEntry, ToolInfo};
pub use store::EventStore;
