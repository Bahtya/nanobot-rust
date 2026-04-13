//! Channel manager — coordinates multiple channel adapters.

use crate::base::BaseChannel;
use crate::registry::ChannelRegistry;
use anyhow::Result;
use dashmap::DashMap;
use nanobot_bus::events::{AgentEvent, OutboundMessage};
use nanobot_bus::MessageBus;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

/// Manages multiple channel adapters and routes messages.
pub struct ChannelManager {
    registry: Arc<ChannelRegistry>,
    bus: Arc<MessageBus>,
    running_channels: DashMap<String, Arc<tokio::sync::Mutex<Box<dyn BaseChannel>>>>,
    /// Active periodic typing tasks, keyed by session_key.
    typing_tasks: DashMap<String, JoinHandle<()>>,
}

/// Extract platform name and chat_id from a session_key.
///
/// Format: `"{platform}:{chat_id}:{thread_id?}"`
fn parse_session_key(key: &str) -> Option<(&str, &str)> {
    let mut parts = key.splitn(3, ':');
    let platform = parts.next()?;
    let chat_id = parts.next()?;
    if platform.is_empty() || chat_id.is_empty() {
        return None;
    }
    Some((platform, chat_id))
}

impl ChannelManager {
    pub fn new(registry: ChannelRegistry, bus: MessageBus) -> Self {
        Self {
            registry: Arc::new(registry),
            bus: Arc::new(bus),
            running_channels: DashMap::new(),
            typing_tasks: DashMap::new(),
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

        // Stop typing for this session after the reply is sent.
        let session_key = format!("{}:{}", msg.channel, msg.chat_id);
        self.stop_typing(&session_key);
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

    /// Stop all running channels and cancel typing tasks.
    pub async fn stop_all(&self) {
        // Cancel all periodic typing tasks.
        for entry in self.typing_tasks.iter() {
            entry.value().abort();
        }
        self.typing_tasks.clear();

        let names: Vec<String> = self.running_channel_names();
        for name in names {
            if let Err(e) = self.stop_channel(&name).await {
                error!("Error stopping channel '{}': {}", name, e);
            }
        }
    }

    /// Start a periodic typing indicator for a session.
    ///
    /// Sends one typing action immediately, then repeats every 4 seconds.
    /// Call [`stop_typing`] to cancel.
    pub fn start_typing(&self, session_key: &str) {
        let (platform, chat_id) = match parse_session_key(session_key) {
            Some(p) => p,
            None => {
                warn!("Cannot parse session_key for typing: {session_key}");
                return;
            }
        };

        let channel = match self.running_channels.get(platform) {
            Some(c) => c.clone(),
            None => {
                debug!("No running channel for platform: {platform}");
                return;
            }
        };

        let chat_id_owned = chat_id.to_string();
        let sk = session_key.to_string();

        // Fire one typing action immediately.
        {
            let ch = channel.clone();
            let cid = chat_id_owned.clone();
            tokio::spawn(async move {
                let ch = ch.lock().await;
                let _ = ch.send_typing(&cid).await;
            });
        }

        // Spawn a periodic typing task (every 5 s).
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                let ch = channel.lock().await;
                let _ = ch.send_typing(&chat_id_owned).await;
            }
        });

        // Cancel any previous task for the same session.
        if let Some(old) = self.typing_tasks.insert(sk, handle) {
            old.abort();
        }
    }

    /// Stop the periodic typing indicator for a session.
    pub fn stop_typing(&self, session_key: &str) {
        if let Some((_, handle)) = self.typing_tasks.remove(session_key) {
            handle.abort();
        }
    }

    /// Run an event listener that starts/stops typing based on agent lifecycle.
    ///
    /// - `AgentEvent::Started` → start typing for that session.
    /// - `AgentEvent::Completed` / `AgentEvent::Error` → stop typing.
    ///
    /// Call this once at startup (e.g. alongside `run_outbound_consumer`).
    pub async fn run_typing_on_events(&self) {
        let mut rx = self.bus.subscribe_events();
        info!("Channel manager typing event listener started");

        loop {
            match rx.recv().await {
                Ok(event) => match &event {
                    AgentEvent::Started { session_key } => {
                        debug!("Typing started for session: {session_key}");
                        self.start_typing(session_key);
                    }
                    AgentEvent::Completed { session_key, .. } => {
                        debug!("Typing stopped for session: {session_key}");
                        self.stop_typing(session_key);
                    }
                    AgentEvent::Error { session_key, .. } => {
                        debug!("Typing stopped (error) for session: {session_key}");
                        self.stop_typing(session_key);
                    }
                    _ => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!("Typing event listener lagged by {n} events");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    info!("Typing event listener: event bus closed");
                    break;
                }
            }
        }
    }

    /// Send a reaction emoji on a specific platform channel.
    pub async fn send_reaction_for_chat(
        &self,
        platform: &str,
        chat_id: &str,
        message_id: &str,
        emoji: &str,
    ) {
        if let Some(channel) = self.running_channels.get(platform) {
            let channel = channel.lock().await;
            if let Err(e) = channel.send_reaction(chat_id, message_id, emoji).await {
                warn!("Failed to send reaction: {e}");
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

    // -----------------------------------------------------------------------
    // Tests for parse_session_key helper
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_session_key_simple() {
        let (platform, chat_id) = parse_session_key("telegram:123").unwrap();
        assert_eq!(platform, "telegram");
        assert_eq!(chat_id, "123");
    }

    #[test]
    fn test_parse_session_key_with_thread() {
        let (platform, chat_id) = parse_session_key("telegram:-100:42").unwrap();
        assert_eq!(platform, "telegram");
        assert_eq!(chat_id, "-100");
    }

    #[test]
    fn test_parse_session_key_negative_chat_id() {
        let (platform, chat_id) = parse_session_key("discord:-999888").unwrap();
        assert_eq!(platform, "discord");
        assert_eq!(chat_id, "-999888");
    }

    #[test]
    fn test_parse_session_key_empty() {
        assert!(parse_session_key("").is_none());
    }

    #[test]
    fn test_parse_session_key_no_colon() {
        assert!(parse_session_key("telegram").is_none());
    }

    #[test]
    fn test_parse_session_key_empty_platform() {
        assert!(parse_session_key(":123").is_none());
    }

    #[test]
    fn test_parse_session_key_empty_chat_id() {
        assert!(parse_session_key("telegram:").is_none());
    }

    // -----------------------------------------------------------------------
    // Tests for typing indicator lifecycle
    // -----------------------------------------------------------------------

    #[test]
    fn test_start_typing_no_running_channel() {
        let registry = ChannelRegistry::new();
        let bus = MessageBus::new();
        let manager = ChannelManager::new(registry, bus);
        // Should silently do nothing — no panic.
        manager.start_typing("telegram:123");
        assert!(manager.typing_tasks.is_empty());
    }

    #[test]
    fn test_stop_typing_nonexistent() {
        let registry = ChannelRegistry::new();
        let bus = MessageBus::new();
        let manager = ChannelManager::new(registry, bus);
        // Should silently do nothing — no panic.
        manager.stop_typing("telegram:nonexistent");
    }

    #[test]
    fn test_start_typing_invalid_session_key() {
        let registry = ChannelRegistry::new();
        let bus = MessageBus::new();
        let manager = ChannelManager::new(registry, bus);
        manager.start_typing("invalid-no-colon");
        assert!(manager.typing_tasks.is_empty());
    }

    #[tokio::test]
    async fn test_send_reaction_for_chat_no_channel() {
        let registry = ChannelRegistry::new();
        let bus = MessageBus::new();
        let manager = ChannelManager::new(registry, bus);
        // Should silently do nothing — no panic.
        manager
            .send_reaction_for_chat("telegram", "123", "456", "👀")
            .await;
    }

    // -----------------------------------------------------------------------
    // Tests for event-driven typing
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_typing_started_on_agent_started_event() {
        let registry = ChannelRegistry::new();
        let bus = MessageBus::new();
        let manager = ChannelManager::new(registry, bus.clone());

        bus.emit_event(AgentEvent::Started {
            session_key: "telegram:123".to_string(),
        });

        let mgr = Arc::new(manager);
        let mgr_clone = mgr.clone();
        let handle = tokio::spawn(async move {
            mgr_clone.run_typing_on_events().await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // No running channel → typing task not created, but no panic.
        assert!(mgr.typing_tasks.is_empty());
        handle.abort();
    }

    #[tokio::test]
    async fn test_typing_stopped_on_agent_completed_event() {
        let registry = ChannelRegistry::new();
        let bus = MessageBus::new();
        let manager = ChannelManager::new(registry, bus.clone());

        bus.emit_event(AgentEvent::Completed {
            session_key: "telegram:123".to_string(),
            iterations: 1,
            tool_calls: 0,
        });

        let mgr = Arc::new(manager);
        let mgr_clone = mgr.clone();
        let handle = tokio::spawn(async move {
            mgr_clone.run_typing_on_events().await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(mgr.typing_tasks.is_empty());
        handle.abort();
    }

    #[tokio::test]
    async fn test_typing_stopped_on_agent_error_event() {
        let registry = ChannelRegistry::new();
        let bus = MessageBus::new();
        let manager = ChannelManager::new(registry, bus.clone());

        bus.emit_event(AgentEvent::Error {
            session_key: "discord:456".to_string(),
            error: "timeout".to_string(),
        });

        let mgr = Arc::new(manager);
        let mgr_clone = mgr.clone();
        let handle = tokio::spawn(async move {
            mgr_clone.run_typing_on_events().await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(mgr.typing_tasks.is_empty());
        handle.abort();
    }

    #[tokio::test]
    async fn test_handle_outbound_stops_typing() {
        let registry = ChannelRegistry::new();
        let bus = MessageBus::new();
        let manager = ChannelManager::new(registry, bus);

        let msg = OutboundMessage {
            channel: nanobot_core::Platform::Telegram,
            chat_id: "123".to_string(),
            content: "reply".to_string(),
            reply_to: None,
            media: vec![],
            metadata: Default::default(),
        };
        manager.handle_outbound(msg).await;
        assert!(manager.typing_tasks.is_empty());
    }
}
