//! # nanobot-channels
//!
//! Channel system — base trait, manager, registry, and platform implementations.

pub mod base;
pub mod manager;
pub mod platforms;
pub mod registry;

pub use base::BaseChannel;
pub use manager::ChannelManager;
pub use registry::ChannelRegistry;
