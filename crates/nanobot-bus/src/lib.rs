//! # nanobot-bus
//!
//! Async message bus for decoupling channel I/O from agent processing.

pub mod events;
pub mod queue;

pub use events::*;
pub use queue::MessageBus;
