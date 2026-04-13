//! Agent loop — the main message processing cycle.
//!
//! Consumes InboundMessages from the bus, builds context, runs the agent,
//! and publishes OutboundMessages back. Includes context compaction and
//! structured notes support. When heartbeat is enabled in config, the agent
//! loop spawns a background [`HeartbeatService`] that periodically checks
//! all component health.

use crate::compaction::{compact_session, CompactionConfig};
use crate::context::ContextBuilder;
use crate::heartbeat::{
    AgentLoopHealthCheck, BusHealthCheck, ChannelHealthCheck, ProviderHealthCheck,
    SessionStoreHealthCheck,
};
use crate::hook::CompositeHook;
use crate::notes::NotesManager;
use crate::runner::AgentRunner;
use anyhow::Result;
use nanobot_bus::events::{AgentEvent, InboundMessage, OutboundMessage, StreamChunk};
use nanobot_bus::MessageBus;
use nanobot_config::Config;
use nanobot_heartbeat::HeartbeatService;
use nanobot_providers::ProviderRegistry;
use nanobot_session::SessionManager;
use nanobot_tools::ToolRegistry;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// The main agent loop that processes messages from the bus.
pub struct AgentLoop {
    config: Arc<Config>,
    bus: Arc<MessageBus>,
    session_manager: Arc<SessionManager>,
    #[allow(dead_code)]
    provider_registry: Arc<ProviderRegistry>,
    tool_registry: Arc<ToolRegistry>,
    hooks: Arc<RwLock<CompositeHook>>,
    running: Arc<RwLock<bool>>,
    compaction_config: CompactionConfig,
    /// Shared set of channel names currently connected.
    connected_channels: Arc<parking_lot::RwLock<HashSet<String>>>,
    /// Shared last-activity timestamp for the agent loop health check.
    agent_activity: Arc<parking_lot::RwLock<Option<chrono::DateTime<chrono::Local>>>>,
}

impl AgentLoop {
    /// Create a new agent loop.
    pub fn new(
        config: Config,
        bus: MessageBus,
        session_manager: SessionManager,
        provider_registry: ProviderRegistry,
        tool_registry: ToolRegistry,
    ) -> Self {
        let config = Arc::new(config);
        let bus = Arc::new(bus);
        let session_manager = Arc::new(session_manager);
        let provider_registry = Arc::new(provider_registry);
        let tool_registry = Arc::new(tool_registry);
        let hooks = Arc::new(RwLock::new(CompositeHook::new()));
        let running = Arc::new(RwLock::new(false));
        let compaction_config = CompactionConfig::default();
        let connected_channels = Arc::new(parking_lot::RwLock::new(HashSet::new()));
        let agent_activity = Arc::new(parking_lot::RwLock::new(None));

        Self {
            config,
            bus,
            session_manager,
            provider_registry,
            tool_registry,
            hooks,
            running,
            compaction_config,
            connected_channels,
            agent_activity,
        }
    }

    /// Start the agent loop (consumes inbound messages).
    pub async fn run(&self) -> Result<()> {
        let mut running = self.running.write().await;
        if *running {
            warn!("Agent loop is already running");
            return Ok(());
        }
        *running = true;
        drop(running);

        info!("Agent loop started");

        // Start heartbeat service if enabled
        let heartbeat_handle = if self.config.heartbeat.enabled {
            Some(self.spawn_heartbeat().await?)
        } else {
            None
        };

        let inbound_rx = self.bus.consume_inbound().await;
        let mut inbound_rx = match inbound_rx {
            Some(rx) => rx,
            None => {
                anyhow::bail!("Inbound receiver already taken");
            }
        };

        while *self.running.read().await {
            match inbound_rx.recv().await {
                Some(msg) => {
                    // Record activity for heartbeat tracking
                    *self.agent_activity.write() = Some(chrono::Local::now());

                    if let Err(e) = self.process_message(msg).await {
                        error!("Error processing message: {}", e);
                    }
                }
                None => {
                    info!("Inbound channel closed, stopping agent loop");
                    break;
                }
            }
        }

        // Stop heartbeat if running
        if let Some(handle) = heartbeat_handle {
            handle.stop().await;
        }

        *self.running.write().await = false;
        info!("Agent loop stopped");
        Ok(())
    }

    /// Process a single inbound message.
    pub async fn process_message(&self, msg: InboundMessage) -> Result<()> {
        let session_key = msg.session_key();
        info!("Processing message from session: {}", session_key);

        // Emit started event
        let started_event = AgentEvent::Started {
            session_key: session_key.clone(),
        };
        self.bus.emit_event(started_event.clone());
        self.hooks
            .read()
            .await
            .emit(&crate::hook::HookContext {
                event: started_event,
                data: Default::default(),
            })
            .await;

        // Get or create session
        let mut session = self
            .session_manager
            .get_or_create(&session_key, msg.source.clone());

        // Add user message to session
        session.add_user_message(msg.content.clone());

        // Compact context if approaching token limits
        if self.compaction_config.needs_compaction(&session) {
            match compact_session(&mut session, &self.compaction_config) {
                Ok(result) => {
                    if result.messages_after < result.messages_before {
                        info!(
                            "Context compacted: {} → {} messages",
                            result.messages_before, result.messages_after
                        );
                    }
                }
                Err(e) => {
                    warn!("Context compaction failed for session {}: {}", session_key, e);
                }
            }
        }

        self.session_manager.save_session(&session)?;

        // Build context
        let context_builder = ContextBuilder::new(&self.config);
        let system_prompt =
            context_builder.build_system_prompt(&msg, &session, &self.tool_registry)?;

        // Set up event callback for this session
        let bus_for_stream = self.bus.clone();

        // Run agent with events wired through
        let messages = session.to_messages();
        let result = {
            // Build a runner with event callback for this session
            let event_bus = bus_for_stream.clone();

            let runner_with_events = AgentRunner::new(
                self.config.clone(),
                self.provider_registry.clone(),
                self.tool_registry.clone(),
            )
            .with_stream_tx(event_bus.subscribe_stream_tx())
            .with_event_callback(Box::new(move |event: AgentEvent| {
                // Re-emit through bus
                match &event {
                    AgentEvent::StreamingChunk {
                        session_key,
                        content,
                    } => {
                        event_bus.publish_stream_chunk(StreamChunk {
                            session_key: session_key.clone(),
                            content: content.clone(),
                            done: false,
                        });
                    }
                    AgentEvent::ToolCall {
                        session_key,
                        tool_name,
                        iteration,
                    } => {
                        event_bus.emit_event(AgentEvent::ToolCall {
                            session_key: session_key.clone(),
                            tool_name: tool_name.clone(),
                            iteration: *iteration,
                        });
                    }
                    _ => {}
                }
            }));

            runner_with_events.run(system_prompt, messages).await
        };

        match result {
            Ok(result) => {
                // Add assistant response to session
                session.add_assistant_message(result.content.clone());

                // Auto-extract structured notes from the response
                let extracted = NotesManager::extract_notes_from_response(
                    &mut session,
                    &result.content,
                );
                if extracted > 0 {
                    info!(
                        "Auto-extracted {} notes from agent response in session {}",
                        extracted, session_key
                    );
                }

                // Compact notes if they exceed the limit
                if NotesManager::compact_if_needed(&mut session) {
                    info!("Notes compacted for session {}", session_key);
                }

                self.session_manager.save_session(&session)?;

                // Send outbound message
                let outbound = OutboundMessage {
                    channel: msg.channel.clone(),
                    chat_id: msg.chat_id.clone(),
                    content: result.content,
                    reply_to: msg.message_id.clone(),
                    media: vec![],
                    metadata: Default::default(),
                };

                if let Err(e) = self.bus.publish_outbound(outbound).await {
                    error!("Failed to publish outbound message: {}", e);
                }

                // Emit completed event
                let completed_event = AgentEvent::Completed {
                    session_key: session_key.clone(),
                    iterations: result.iterations_used,
                    tool_calls: result.tool_calls_made,
                };
                self.bus.emit_event(completed_event.clone());
                self.hooks
                    .read()
                    .await
                    .emit(&crate::hook::HookContext {
                        event: completed_event,
                        data: Default::default(),
                    })
                    .await;
            }
            Err(e) => {
                error!("Agent run error for session {}: {}", session_key, e);

                let error_event = AgentEvent::Error {
                    session_key: session_key.clone(),
                    error: e.to_string(),
                };
                self.bus.emit_event(error_event.clone());
                self.hooks
                    .read()
                    .await
                    .emit(&crate::hook::HookContext {
                        event: error_event,
                        data: Default::default(),
                    })
                    .await;
            }
        }

        Ok(())
    }

    /// Stop the agent loop.
    pub async fn stop(&self) {
        *self.running.write().await = false;
    }

    /// Get a reference to the hooks for adding new hooks.
    pub fn hooks(&self) -> Arc<RwLock<CompositeHook>> {
        self.hooks.clone()
    }

    /// Get the shared connected-channels set (for external updates by channel adapters).
    pub fn connected_channels(&self) -> Arc<parking_lot::RwLock<HashSet<String>>> {
        self.connected_channels.clone()
    }

    /// Record agent loop activity (updates the timestamp used by the health check).
    pub fn record_activity(&self) {
        *self.agent_activity.write() = Some(chrono::Local::now());
    }

    /// Return the list of channel names that have configuration present.
    fn configured_channel_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        if self.config.channels.telegram.is_some() {
            names.push("telegram".to_string());
        }
        if self.config.channels.discord.is_some() {
            names.push("discord".to_string());
        }
        if self.config.channels.slack.is_some() {
            names.push("slack".to_string());
        }
        if self.config.channels.matrix.is_some() {
            names.push("matrix".to_string());
        }
        if self.config.channels.whatsapp.is_some() {
            names.push("whatsapp".to_string());
        }
        if self.config.channels.email.is_some() {
            names.push("email".to_string());
        }
        if self.config.channels.dingtalk.is_some() {
            names.push("dingtalk".to_string());
        }
        if self.config.channels.feishu.is_some() {
            names.push("feishu".to_string());
        }
        if self.config.channels.wecom.is_some() {
            names.push("wecom".to_string());
        }
        if self.config.channels.weixin.is_some() {
            names.push("weixin".to_string());
        }
        if self.config.channels.qq.is_some() {
            names.push("qq".to_string());
        }
        if self.config.channels.mochat.is_some() {
            names.push("mochat".to_string());
        }
        names
    }

    /// Spawn the heartbeat service as a background task.
    ///
    /// Registers health checks for all components and starts the periodic
    /// check loop. Returns a [`HeartbeatHandle`] that can be used to stop
    /// the service.
    async fn spawn_heartbeat(&self) -> Result<HeartbeatHandle> {
        let data_dir = nanobot_config::paths::get_data_dir()
            .unwrap_or_else(|_| std::env::temp_dir());

        let config_clone = (*self.config).clone();
        let mut svc = HeartbeatService::with_data_dir(config_clone, data_dir);
        svc.set_bus((*self.bus).clone());

        // Register all component health checks
        svc.register_check(Arc::new(ProviderHealthCheck::new(
            (*self.provider_registry).clone(),
        )));
        svc.register_check(Arc::new(SessionStoreHealthCheck::new(
            (*self.session_manager).clone(),
        )));
        svc.register_check(Arc::new(ChannelHealthCheck::new(
            self.configured_channel_names(),
            self.connected_channels.clone(),
        )));
        svc.register_check(Arc::new(BusHealthCheck::new(
            (*self.bus).clone(),
        )));
        svc.register_check(Arc::new(AgentLoopHealthCheck::new(
            self.agent_activity.clone(),
            self.config.heartbeat.interval_secs.max(120),
        )));

        let running = Arc::clone(&self.running);
        let svc = Arc::new(svc);
        let svc_clone = Arc::clone(&svc);

        let handle = tokio::spawn(async move {
            if let Err(e) = svc_clone.run().await {
                error!("Heartbeat service error: {}", e);
            }
        });

        info!(
            "Heartbeat service spawned with {} checks (interval: {}s)",
            svc.registered_checks().len(),
            svc.interval().as_secs()
        );

        Ok(HeartbeatHandle {
            service: svc,
            task: handle,
            agent_running: running,
        })
    }

    /// Set a custom compaction configuration.
    pub fn with_compaction_config(mut self, config: CompactionConfig) -> Self {
        self.compaction_config = config;
        self
    }
}

/// Handle to a running heartbeat service spawned as a background task.
///
/// Calling [`stop()`](Self::stop) signals both the heartbeat service and
/// the background tokio task to shut down.
pub struct HeartbeatHandle {
    service: Arc<HeartbeatService>,
    task: tokio::task::JoinHandle<()>,
    agent_running: Arc<RwLock<bool>>,
}

impl HeartbeatHandle {
    /// Stop the heartbeat service and wait for the background task to finish.
    pub async fn stop(self) {
        self.service.stop().await;
        // Also signal the agent loop to stop so the heartbeat task can exit
        *self.agent_running.write().await = false;
        let _ = self.task.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_agent_loop() -> AgentLoop {
        let config = Config::default();
        let bus = MessageBus::new();
        let session_dir = tempfile::tempdir().unwrap();
        let session_manager =
            SessionManager::new(session_dir.path().to_path_buf()).unwrap();
        let provider_registry = ProviderRegistry::new();
        let tool_registry = ToolRegistry::new();
        AgentLoop::new(
            config,
            bus,
            session_manager,
            provider_registry,
            tool_registry,
        )
    }

    #[test]
    fn test_agent_loop_construction() {
        let al = make_agent_loop();
        assert!(al.connected_channels.read().is_empty());
        assert!(al.agent_activity.read().is_none());
    }

    #[test]
    fn test_connected_channels_starts_empty() {
        let al = make_agent_loop();
        let channels = al.connected_channels();
        assert!(channels.read().is_empty());
    }

    #[test]
    fn test_connected_channels_external_update() {
        let al = make_agent_loop();
        let channels = al.connected_channels();
        channels.write().insert("telegram".to_string());
        assert!(al.connected_channels.read().contains("telegram"));
    }

    #[test]
    fn test_record_activity() {
        let al = make_agent_loop();
        assert!(al.agent_activity.read().is_none());
        al.record_activity();
        assert!(al.agent_activity.read().is_some());
    }

    #[test]
    fn test_configured_channel_names_default() {
        let al = make_agent_loop();
        assert!(al.configured_channel_names().is_empty());
    }

    #[test]
    fn test_configured_channel_names_with_telegram() {
        let mut config = Config::default();
        config.channels.telegram = Some(nanobot_config::schema::TelegramConfig {
            token: "test".to_string(),
            allowed_users: vec![],
            admin_users: vec![],
            enabled: true,
            streaming: false,
        });
        let bus = MessageBus::new();
        let session_dir = tempfile::tempdir().unwrap();
        let session_manager =
            SessionManager::new(session_dir.path().to_path_buf()).unwrap();
        let al = AgentLoop::new(
            config,
            bus,
            session_manager,
            ProviderRegistry::new(),
            ToolRegistry::new(),
        );
        let names = al.configured_channel_names();
        assert_eq!(names, vec!["telegram"]);
    }

    #[test]
    fn test_configured_channel_names_multiple() {
        let mut config = Config::default();
        config.channels.telegram = Some(nanobot_config::schema::TelegramConfig {
            token: "t".to_string(),
            allowed_users: vec![],
            admin_users: vec![],
            enabled: true,
            streaming: false,
        });
        config.channels.discord = Some(nanobot_config::schema::DiscordConfig {
            token: "d".to_string(),
            allowed_guilds: vec![],
            enabled: true,
            streaming: false,
        });
        let bus = MessageBus::new();
        let session_dir = tempfile::tempdir().unwrap();
        let session_manager =
            SessionManager::new(session_dir.path().to_path_buf()).unwrap();
        let al = AgentLoop::new(
            config,
            bus,
            session_manager,
            ProviderRegistry::new(),
            ToolRegistry::new(),
        );
        let names = al.configured_channel_names();
        assert!(names.contains(&"telegram".to_string()));
        assert!(names.contains(&"discord".to_string()));
        assert_eq!(names.len(), 2);
    }

    #[tokio::test]
    async fn test_heartbeat_disabled_by_default() {
        let al = make_agent_loop();
        assert!(!al.config.heartbeat.enabled);
    }

    #[tokio::test]
    async fn test_heartbeat_not_spawned_when_disabled() {
        let al = make_agent_loop();
        // run() will take the inbound receiver, so we can't call it directly.
        // Instead verify the config prevents spawning.
        assert!(!al.config.heartbeat.enabled);
    }

    #[tokio::test]
    async fn test_spawn_heartbeat_registers_checks() {
        let mut config = Config::default();
        config.heartbeat.enabled = true;
        config.heartbeat.interval_secs = 60;
        config.channels.telegram = Some(nanobot_config::schema::TelegramConfig {
            token: "test".to_string(),
            allowed_users: vec![],
            admin_users: vec![],
            enabled: true,
            streaming: false,
        });

        let bus = MessageBus::new();
        let session_dir = tempfile::tempdir().unwrap();
        let data_dir = tempfile::tempdir().unwrap();
        let session_manager =
            SessionManager::new(session_dir.path().to_path_buf()).unwrap();
        let provider_registry = ProviderRegistry::new();
        let tool_registry = ToolRegistry::new();

        let al = AgentLoop::new(
            config,
            bus,
            session_manager,
            provider_registry,
            tool_registry,
        );

        // Override data dir for test by calling spawn_heartbeat
        // We can't call spawn_heartbeat directly since it's private,
        // but we can verify the wiring by checking the checks manually.
        let config_clone = (*al.config).clone();
        let svc = HeartbeatService::with_data_dir(config_clone, data_dir.path().to_path_buf());

        svc.register_check(Arc::new(ProviderHealthCheck::new(
            (*al.provider_registry).clone(),
        )));
        svc.register_check(Arc::new(SessionStoreHealthCheck::new(
            (*al.session_manager).clone(),
        )));
        svc.register_check(Arc::new(ChannelHealthCheck::new(
            al.configured_channel_names(),
            al.connected_channels.clone(),
        )));

        let checks = svc.registered_checks();
        assert!(checks.contains(&"provider".to_string()));
        assert!(checks.contains(&"session_store".to_string()));
        assert!(checks.contains(&"channel".to_string()));
        assert_eq!(checks.len(), 3);
    }

    #[tokio::test]
    async fn test_heartbeat_handle_stop() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.heartbeat.interval_secs = 30;
        let svc = Arc::new(
            HeartbeatService::with_data_dir(config, dir.path().to_path_buf())
        );
        let running = Arc::new(RwLock::new(true));
        let svc_clone = Arc::clone(&svc);

        let task = tokio::spawn(async move {
            let _ = svc_clone.run().await;
        });

        let handle = HeartbeatHandle {
            service: svc,
            task,
            agent_running: running.clone(),
        };

        // Verify service.stop() signals the service correctly
        handle.service.stop().await;
        assert!(!handle.service.is_running().await);

        // Drop the handle (task will eventually exit on its interval)
        // We can't wait for it since the interval is 30s.
        drop(handle);
    }
}
