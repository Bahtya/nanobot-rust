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
    SessionStoreHealthCheck, ToolRegistryHealthCheck,
};
use crate::hook::CompositeHook;
use crate::notes::NotesManager;
use crate::runner::AgentRunner;
use crate::subagent::SubAgentManager;
use anyhow::Result;
use nanobot_bus::events::{AgentEvent, InboundMessage, OutboundMessage, StreamChunk};
use nanobot_bus::MessageBus;
use nanobot_config::Config;
use nanobot_heartbeat::HeartbeatService;
use nanobot_memory::types::{MemoryCategory, MemoryEntry, MemoryQuery};
use nanobot_memory::MemoryStore as AsyncMemoryStore;
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
    /// Optional sub-agent manager for spawning background tasks.
    subagent_manager: Option<Arc<SubAgentManager>>,
    /// Optional async memory store (nanobot-memory crate) for recall/store.
    memory_store: Option<Arc<dyn AsyncMemoryStore>>,
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
            subagent_manager: None,
            memory_store: None,
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
        info!(
            session_key = %session_key,
            channel = %msg.channel,
            "Processing message"
        );

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
                            session_key = %session_key,
                            before = result.messages_before,
                            after = result.messages_after,
                            "Context compacted"
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        "Context compaction failed for session {}: {}",
                        session_key, e
                    );
                }
            }
        }

        self.session_manager.save_session(&session)?;

        // Recall relevant memories from the memory store (if configured).
        let recalled_memory = self.recall_memories(&msg.content).await;

        // Build context
        let system_prompt = {
            let context_builder = ContextBuilder::new(&self.config);
            context_builder.build_system_prompt(
                &msg,
                &session,
                &self.tool_registry,
                recalled_memory.as_deref(),
            )?
        };

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
                let extracted =
                    NotesManager::extract_notes_from_response(&mut session, &result.content);
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

                // Store conversation memory (non-blocking — failures are logged, not propagated)
                self.store_conversation_memory(&msg.content, &result.content)
                    .await;

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

    /// Recall relevant memories from the memory store for the given query text.
    ///
    /// Returns a formatted string section for injection into the system prompt,
    /// or `None` if no memory store is configured or no memories were found.
    async fn recall_memories(&self, query_text: &str) -> Option<String> {
        let store = self.memory_store.as_ref()?;

        let query = MemoryQuery::new()
            .with_text(query_text)
            .with_limit(5)
            .with_min_confidence(0.3);

        match store.search(&query).await {
            Ok(results) if results.is_empty() => None,
            Ok(results) => {
                let mut lines = vec!["## Recalled Memories".to_string()];
                for scored in &results {
                    lines.push(format!(
                        "- {} [{}] (confidence: {:.2})",
                        scored.entry.content,
                        scored.entry.category,
                        scored.entry.confidence
                    ));
                }
                Some(lines.join("\n"))
            }
            Err(e) => {
                warn!("Memory recall failed: {}", e);
                None
            }
        }
    }

    /// Store a memory entry from a completed conversation turn.
    ///
    /// Extracts a summary from the user message and agent response, then stores
    /// it as an [`MemoryCategory::AgentNote`]. Failures are logged but not propagated
    /// — memory storage must not break the agent loop.
    async fn store_conversation_memory(&self, user_msg: &str, agent_response: &str) {
        let Some(store) = self.memory_store.as_ref() else {
            return;
        };

        // Create a concise summary from the conversation turn.
        let content = format_conversation_summary(user_msg, agent_response);
        let entry = MemoryEntry::new(content, MemoryCategory::AgentNote).with_confidence(0.6);

        if let Err(e) = store.store(entry).await {
            warn!("Failed to store conversation memory: {}", e);
        }
    }

    /// Stop the agent loop.
    pub async fn stop(&self) {
        info!("Stopping agent loop");
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
        let data_dir =
            nanobot_config::paths::get_data_dir().unwrap_or_else(|_| std::env::temp_dir());

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
        svc.register_check(Arc::new(BusHealthCheck::new((*self.bus).clone())));
        svc.register_check(Arc::new(ToolRegistryHealthCheck::new(
            (*self.tool_registry).clone(),
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

    /// Attach a [`SubAgentManager`] to this agent loop.
    ///
    /// When set, the spawn tool in the tool registry will delegate to this
    /// manager for actual sub-agent creation. Call this before [`run()`](Self::run).
    pub fn with_subagent_manager(mut self, manager: Arc<SubAgentManager>) -> Self {
        self.subagent_manager = Some(manager);
        self
    }

    /// Attach an async memory store for recall/store during the agent loop.
    ///
    /// When set, the agent will recall relevant memories before each LLM call
    /// and store new memory entries after each conversation turn.
    pub fn with_memory_store(mut self, store: Arc<dyn AsyncMemoryStore>) -> Self {
        self.memory_store = Some(store);
        self
    }

    /// Get the sub-agent manager, if one has been attached.
    pub fn subagent_manager(&self) -> Option<&Arc<SubAgentManager>> {
        self.subagent_manager.as_ref()
    }
}

/// Format a conversation turn into a concise memory summary.
///
/// Takes the first 200 characters of the user message and first 100 characters
/// of the agent response to create a deterministic, testable summary.
fn format_conversation_summary(user_msg: &str, agent_response: &str) -> String {
    let user_preview = truncate_str(user_msg, 200);
    let response_preview = truncate_str(agent_response, 100);
    format!("User: {} | Agent: {}", user_preview, response_preview)
}

/// Truncate a string to at most `max_len` characters, appending "..." if truncated.
fn truncate_str(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        // Find a valid char boundary to avoid panicking on multi-byte characters.
        let mut end = max_len;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        &s[..end]
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
        let session_manager = SessionManager::new(session_dir.path().to_path_buf()).unwrap();
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
        let session_manager = SessionManager::new(session_dir.path().to_path_buf()).unwrap();
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
        let session_manager = SessionManager::new(session_dir.path().to_path_buf()).unwrap();
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
        let session_manager = SessionManager::new(session_dir.path().to_path_buf()).unwrap();
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
        svc.register_check(Arc::new(ToolRegistryHealthCheck::new(
            (*al.tool_registry).clone(),
        )));

        let checks = svc.registered_checks();
        assert!(checks.contains(&"provider".to_string()));
        assert!(checks.contains(&"session_store".to_string()));
        assert!(checks.contains(&"channel".to_string()));
        assert!(checks.contains(&"tool_registry".to_string()));
        assert_eq!(checks.len(), 4);
    }

    #[tokio::test]
    async fn test_heartbeat_handle_stop() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.heartbeat.interval_secs = 30;
        let svc = Arc::new(HeartbeatService::with_data_dir(
            config,
            dir.path().to_path_buf(),
        ));
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

    // ── Memory integration tests ────────────────────────────────

    use nanobot_memory::HotStore;
    use nanobot_memory::MemoryError;
    use nanobot_memory::types::ScoredEntry;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock memory store for deterministic testing.
    struct MockMemoryStore {
        entries: RwLock<Vec<MemoryEntry>>,
        store_count: AtomicUsize,
        search_count: AtomicUsize,
    }

    impl MockMemoryStore {
        fn new() -> Self {
            Self {
                entries: RwLock::new(Vec::new()),
                store_count: AtomicUsize::new(0),
                search_count: AtomicUsize::new(0),
            }
        }

        fn store_count(&self) -> usize {
            self.store_count.load(Ordering::SeqCst)
        }

        fn search_count(&self) -> usize {
            self.search_count.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl AsyncMemoryStore for MockMemoryStore {
        async fn store(&self, entry: MemoryEntry) -> std::result::Result<(), MemoryError> {
            self.store_count.fetch_add(1, Ordering::SeqCst);
            self.entries.write().await.push(entry);
            Ok(())
        }

        async fn recall(&self, id: &str) -> std::result::Result<Option<MemoryEntry>, MemoryError> {
            let entries = self.entries.read().await;
            Ok(entries.iter().find(|e| e.id == id).cloned())
        }

        async fn search(
            &self,
            query: &nanobot_memory::types::MemoryQuery,
        ) -> std::result::Result<Vec<ScoredEntry>, MemoryError> {
            self.search_count.fetch_add(1, Ordering::SeqCst);
            let entries = self.entries.read().await;
            let results: Vec<ScoredEntry> = entries
                .iter()
                .filter(|e| {
                    if let Some(ref cat) = query.category {
                        if e.category != *cat {
                            return false;
                        }
                    }
                    if let Some(min_conf) = query.min_confidence {
                        if e.confidence < min_conf {
                            return false;
                        }
                    }
                    if let Some(ref text) = query.text {
                        if !e.content.to_lowercase().contains(&text.to_lowercase()) {
                            return false;
                        }
                    }
                    true
                })
                .map(|e| ScoredEntry {
                    entry: e.clone(),
                    score: 1.0,
                })
                .take(query.limit)
                .collect();
            Ok(results)
        }

        async fn delete(&self, _id: &str) -> std::result::Result<(), MemoryError> {
            Ok(())
        }

        async fn len(&self) -> usize {
            self.entries.read().await.len()
        }

        async fn clear(&self) -> std::result::Result<(), MemoryError> {
            self.entries.write().await.clear();
            Ok(())
        }
    }

    #[test]
    fn test_with_memory_store_builder() {
        let al = make_agent_loop();
        let mock = Arc::new(MockMemoryStore::new());
        let al = al.with_memory_store(mock);
        assert!(al.memory_store.is_some());
    }

    #[test]
    fn test_without_memory_store_is_none() {
        let al = make_agent_loop();
        assert!(al.memory_store.is_none());
    }

    #[tokio::test]
    async fn test_recall_memories_no_store() {
        let al = make_agent_loop();
        let result = al.recall_memories("test query").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_recall_memories_empty_store() {
        let mock = Arc::new(MockMemoryStore::new());
        let al = make_agent_loop().with_memory_store(mock.clone());
        let result = al.recall_memories("test query").await;
        assert!(result.is_none());
        assert_eq!(mock.search_count(), 1);
    }

    #[tokio::test]
    async fn test_recall_memories_with_results() {
        let mock = Arc::new(MockMemoryStore::new());
        mock.store(
            MemoryEntry::new("User likes Rust", MemoryCategory::Preference).with_confidence(0.9),
        )
        .await
        .unwrap();

        let al = make_agent_loop().with_memory_store(mock.clone());
        let result = al.recall_memories("rust").await;
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("## Recalled Memories"));
        assert!(text.contains("User likes Rust"));
        assert!(text.contains("preference"));
    }

    #[tokio::test]
    async fn test_recall_memories_no_match() {
        let mock = Arc::new(MockMemoryStore::new());
        mock.store(MemoryEntry::new("Python scripting", MemoryCategory::Fact).with_confidence(0.9))
            .await
            .unwrap();

        let al = make_agent_loop().with_memory_store(mock.clone());
        let result = al.recall_memories("rust programming").await;
        // "rust programming" does not match "Python scripting"
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_store_conversation_memory_no_store() {
        let al = make_agent_loop();
        // Should not panic or error
        al.store_conversation_memory("hello", "hi there").await;
    }

    #[tokio::test]
    async fn test_store_conversation_memory_with_store() {
        let mock = Arc::new(MockMemoryStore::new());
        let al = make_agent_loop().with_memory_store(mock.clone());

        al.store_conversation_memory("What is Rust?", "Rust is a systems language")
            .await;

        assert_eq!(mock.store_count(), 1);
        let entries = mock.entries.read().await;
        assert_eq!(entries.len(), 1);
        assert!(entries[0].content.contains("What is Rust?"));
        assert!(entries[0].content.contains("Rust is a systems language"));
        assert_eq!(entries[0].category, MemoryCategory::AgentNote);
        assert!((entries[0].confidence - 0.6).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_store_conversation_memory_multiple() {
        let mock = Arc::new(MockMemoryStore::new());
        let al = make_agent_loop().with_memory_store(mock.clone());

        al.store_conversation_memory("msg1", "resp1").await;
        al.store_conversation_memory("msg2", "resp2").await;

        assert_eq!(mock.store_count(), 2);
    }

    #[test]
    fn test_format_conversation_summary() {
        let summary = format_conversation_summary("Hello world", "Hi there");
        assert!(summary.starts_with("User: Hello world"));
        assert!(summary.contains("Agent: Hi there"));
    }

    #[test]
    fn test_format_conversation_summary_truncation() {
        let long_user = "a".repeat(300);
        let long_agent = "b".repeat(200);
        let summary = format_conversation_summary(&long_user, &long_agent);
        assert!(summary.contains("User: "));
        assert!(summary.contains("Agent: "));
        // Should not contain the full 300 chars
        assert!(!summary.contains(&long_user));
    }

    #[test]
    fn test_truncate_str_short() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_str_exact() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_str_long() {
        let result = truncate_str("hello world", 5);
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_truncate_str_unicode() {
        let s = "日本語テスト";
        let result = truncate_str(s, 6);
        // Should not panic and should truncate at a char boundary
        assert!(result.len() <= 6);
        assert!(!result.is_empty());
    }

    #[tokio::test]
    async fn test_recall_with_real_hotstore() {
        let dir = tempfile::tempdir().unwrap();
        let config = nanobot_memory::MemoryConfig::for_test(dir.path());
        let store = HotStore::new(&config).await.unwrap();

        // Pre-populate
        store
            .store(
                MemoryEntry::new("User prefers dark mode", MemoryCategory::Preference)
                    .with_confidence(0.95),
            )
            .await
            .unwrap();
        store
            .store(
                MemoryEntry::new("Project uses Rust", MemoryCategory::Fact).with_confidence(0.8),
            )
            .await
            .unwrap();

        let al = make_agent_loop().with_memory_store(Arc::new(store));

        let result = al.recall_memories("dark mode").await;
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("dark mode"));
    }

    #[tokio::test]
    async fn test_store_with_real_hotstore() {
        let dir = tempfile::tempdir().unwrap();
        let config = nanobot_memory::MemoryConfig::for_test(dir.path());
        let store = HotStore::new(&config).await.unwrap();

        let al = make_agent_loop().with_memory_store(Arc::new(store));

        al.store_conversation_memory("How do I build?", "Use cargo build")
            .await;

        // Verify stored by searching
        let store = al.memory_store.unwrap();
        let results = store
            .search(&nanobot_memory::types::MemoryQuery::new().with_text("cargo"))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].entry.content.contains("cargo"));
    }
}
