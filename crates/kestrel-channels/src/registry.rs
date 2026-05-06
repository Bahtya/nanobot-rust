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
    fn empty() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    fn register_builtin_channels(&mut self) {
        self.register("telegram", || {
            Box::new(platforms::telegram::TelegramChannel::new())
        });
        self.register("discord", || {
            Box::new(platforms::discord::DiscordChannel::new())
        });
        self.register("feishu", || {
            Box::new(platforms::feishu::FeishuChannel::new())
        });
        self.register("websocket", || {
            Box::new(platforms::websocket::WebSocketChannel::new())
        });
        self.register("weixin", || {
            Box::new(platforms::weixin::WeixinChannel::new())
        });
    }

    /// Create a registry with built-in channel factories using process environment.
    pub fn new() -> Self {
        let mut registry = Self::empty();
        registry.register_builtin_channels();
        registry
    }

    /// Create a registry with built-in channel factories using the supplied config.
    pub fn new_with_config(config: &kestrel_config::Config) -> Self {
        let mut registry = Self::empty();

        if let Some(telegram) = config.channels.telegram.clone() {
            let notifications = config.notifications.clone();
            registry.register("telegram", move || {
                Box::new(
                    platforms::telegram::TelegramChannel::new_with_notifications_config(
                        &telegram,
                        &notifications,
                    ),
                )
            });
        } else {
            registry.register("telegram", || {
                Box::new(platforms::telegram::TelegramChannel::new())
            });
        }

        if let Some(discord) = config.channels.discord.clone() {
            registry.register("discord", move || {
                Box::new(platforms::discord::DiscordChannel::new_with_config(
                    &discord,
                ))
            });
        } else {
            registry.register("discord", || {
                Box::new(platforms::discord::DiscordChannel::new())
            });
        }

        registry.register("websocket", || {
            Box::new(platforms::websocket::WebSocketChannel::new())
        });

        if let Some(feishu) = config.channels.feishu.clone() {
            registry.register("feishu", move || {
                Box::new(platforms::feishu::FeishuChannel::new_with_config(&feishu))
            });
        } else {
            registry.register("feishu", || {
                Box::new(platforms::feishu::FeishuChannel::new())
            });
        }

        if let Some(weixin) = config.channels.weixin.clone() {
            registry.register("weixin", move || {
                Box::new(platforms::weixin::WeixinChannel::new_with_config(&weixin))
            });
        } else {
            registry.register("weixin", || {
                Box::new(platforms::weixin::WeixinChannel::new())
            });
        }

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
        assert!(names.contains(&"feishu".to_string()));
        assert!(names.contains(&"websocket".to_string()));
        assert!(names.contains(&"weixin".to_string()));
    }

    #[test]
    fn test_registry_default() {
        let registry = ChannelRegistry::default();
        let names = registry.channel_names();
        assert_eq!(names.len(), 5);
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
    fn test_registry_create_weixin() {
        let registry = ChannelRegistry::new();
        let channel = registry.create_channel("weixin").unwrap();
        assert_eq!(channel.name(), "weixin");
        assert_eq!(channel.platform(), Platform::Weixin);
        assert!(!channel.is_connected());
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
        assert_eq!(names.len(), 5);
        assert!(names.contains(&"custom".to_string()));

        let channel = registry.create_channel("custom").unwrap();
        assert_eq!(channel.name(), "telegram"); // reuses telegram impl
    }
}
