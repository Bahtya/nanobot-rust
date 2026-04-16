//! Channel registry — discovers and creates channel adapters.

use crate::base::BaseChannel;
use crate::platforms;
use anyhow::{Context, Result};
use std::collections::HashMap;

/// Registry for channel adapters.
pub struct ChannelRegistry {
    factories: HashMap<String, Box<dyn Fn() -> Box<dyn BaseChannel> + Send + Sync>>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            factories: HashMap::new(),
        };

        // Register built-in channels
        registry.register("telegram", || {
            Box::new(platforms::telegram::TelegramChannel::new())
        });
        registry.register("discord", || {
            Box::new(platforms::discord::DiscordChannel::new())
        });
        registry.register("websocket", || {
            Box::new(platforms::websocket::WebSocketChannel::new())
        });

        registry
    }

    /// Register a channel factory.
    pub fn register(
        &mut self,
        name: &str,
        factory: impl Fn() -> Box<dyn BaseChannel> + Send + Sync + 'static,
    ) {
        self.factories.insert(name.to_string(), Box::new(factory));
    }

    /// Create a new channel instance by name.
    pub fn create_channel(&self, name: &str) -> Result<Box<dyn BaseChannel>> {
        self.factories
            .get(name)
            .map(|f| f())
            .with_context(|| format!("Unknown channel: {}", name))
    }

    /// List all registered channel names.
    pub fn channel_names(&self) -> Vec<String> {
        self.factories.keys().cloned().collect()
    }
}

impl Default for ChannelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kestrel_core::Platform;

    #[test]
    fn test_registry_new_has_builtin_channels() {
        let registry = ChannelRegistry::new();
        let names = registry.channel_names();
        assert!(names.contains(&"telegram".to_string()));
        assert!(names.contains(&"discord".to_string()));
        assert!(names.contains(&"websocket".to_string()));
    }

    #[test]
    fn test_registry_default() {
        let registry = ChannelRegistry::default();
        let names = registry.channel_names();
        assert_eq!(names.len(), 3);
    }

    #[test]
    fn test_registry_create_known_channel() {
        let registry = ChannelRegistry::new();
        let channel = registry.create_channel("telegram").unwrap();
        assert_eq!(channel.name(), "telegram");
        assert_eq!(channel.platform(), Platform::Telegram);
        assert!(!channel.is_connected());
    }

    #[test]
    fn test_registry_create_discord() {
        let registry = ChannelRegistry::new();
        let channel = registry.create_channel("discord").unwrap();
        assert_eq!(channel.name(), "discord");
        assert_eq!(channel.platform(), Platform::Discord);
    }

    #[test]
    fn test_registry_create_unknown() {
        let registry = ChannelRegistry::new();
        let result = registry.create_channel("slack");
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.to_string().contains("Unknown channel"));
    }

    #[test]
    fn test_registry_register_custom() {
        let mut registry = ChannelRegistry::new();
        // Register a custom channel (just reuse telegram for testing)
        registry.register("custom", || {
            Box::new(platforms::telegram::TelegramChannel::new())
        });
        let names = registry.channel_names();
        assert_eq!(names.len(), 4);
        assert!(names.contains(&"custom".to_string()));

        let channel = registry.create_channel("custom").unwrap();
        assert_eq!(channel.name(), "telegram"); // reuses telegram impl
    }
}
