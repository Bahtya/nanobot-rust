//! Channel manager — coordinates multiple channel adapters.

use crate::base::BaseChannel;
use crate::registry::ChannelRegistry;
use anyhow::Result;
use dashmap::DashMap;
use nanobot_bus::events::OutboundMessage;
use nanobot_bus::MessageBus;
use std::sync::Arc;
use tracing::{error, info, warn};

/// Manages multiple channel adapters and routes messages.
pub struct ChannelManager {
    registry: Arc<ChannelRegistry>,
    bus: Arc<MessageBus>,
    running_channels: DashMap<String, Arc<tokio::sync::Mutex<Box<dyn BaseChannel>>>>,
}

impl ChannelManager {
    pub fn new(registry: ChannelRegistry, bus: MessageBus) -> Self {
        Self {
            registry: Arc::new(registry),
            bus: Arc::new(bus),
            running_channels: DashMap::new(),
        }
    }

    /// Start a channel by name.
    pub async fn start_channel(&self, name: &str) -> Result<()> {
        let channel = self.registry.create_channel(name)?;
        let mut channel = channel;

        // Set up message handler
        let bus = self.bus.clone();
        channel.set_message_handler(bus.inbound_sender());

        let connected = channel.connect().await?;
        if connected {
            info!("Channel '{}' connected", name);
            self.running_channels
                .insert(name.to_string(), Arc::new(tokio::sync::Mutex::new(channel)));
        } else {
            warn!("Channel '{}' failed to connect", name);
        }

        Ok(())
    }

    /// Stop a channel by name.
    pub async fn stop_channel(&self, name: &str) -> Result<()> {
        if let Some((_, channel)) = self.running_channels.remove(name) {
            let mut channel = channel.lock().await;
            channel.disconnect().await?;
            info!("Channel '{}' disconnected", name);
        }
        Ok(())
    }

    /// Handle an outbound message from the bus.
    pub async fn handle_outbound(&self, msg: OutboundMessage) {
        let channel_name = msg.channel.to_string();
        match self.running_channels.get(&channel_name) {
            Some(channel) => {
                let channel = channel.lock().await;
                match channel
                    .send_message(&msg.chat_id, &msg.content, msg.reply_to.as_deref())
                    .await
                {
                    Ok(result) => {
                        if !result.success {
                            error!(
                                "Failed to send message to {} via {}: {:?}",
                                msg.chat_id, channel_name, result.error
                            );
                        }
                    }
                    Err(e) => {
                        error!("Error sending message via {}: {}", channel_name, e);
                    }
                }
            }
            None => {
                warn!("No running channel for platform: {}", channel_name);
            }
        }
    }

    /// Start the outbound message consumer.
    pub async fn run_outbound_consumer(&self) {
        let mut rx = match self.bus.consume_outbound().await {
            Some(rx) => rx,
            None => {
                warn!("Outbound receiver already taken");
                return;
            }
        };

        info!("Channel manager outbound consumer started");

        while let Some(msg) = rx.recv().await {
            self.handle_outbound(msg).await;
        }

        info!("Channel manager outbound consumer stopped");
    }

    /// Stop all running channels.
    pub async fn stop_all(&self) {
        let names: Vec<String> = self.running_channel_names();
        for name in names {
            if let Err(e) = self.stop_channel(&name).await {
                error!("Error stopping channel '{}': {}", name, e);
            }
        }
    }

    /// List running channel names.
    pub fn running_channel_names(&self) -> Vec<String> {
        self.running_channels
            .iter()
            .map(|r| r.key().clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ChannelRegistry;

    #[test]
    fn test_manager_new() {
        let registry = ChannelRegistry::new();
        let bus = MessageBus::new();
        let manager = ChannelManager::new(registry, bus);
        assert!(manager.running_channel_names().is_empty());
    }

    #[tokio::test]
    async fn test_manager_stop_nonexistent_channel() {
        let registry = ChannelRegistry::new();
        let bus = MessageBus::new();
        let manager = ChannelManager::new(registry, bus);
        // Stopping a channel that was never started should succeed
        let result = manager.stop_channel("nonexistent").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_manager_running_channel_names_empty() {
        let registry = ChannelRegistry::new();
        let bus = MessageBus::new();
        let manager = ChannelManager::new(registry, bus);
        assert!(manager.running_channel_names().is_empty());
    }

    #[tokio::test]
    async fn test_manager_stop_all_empty() {
        let registry = ChannelRegistry::new();
        let bus = MessageBus::new();
        let manager = ChannelManager::new(registry, bus);
        // stop_all on empty manager should succeed without panic
        manager.stop_all().await;
        assert!(manager.running_channel_names().is_empty());
    }
}
