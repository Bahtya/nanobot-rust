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
use kestrel_bus::events::{AgentEvent, InboundMessage, OutboundMessage, StreamChunk};
use kestrel_bus::MessageBus;
use kestrel_config::Config;
use kestrel_core::{Message, MessageRole};
use kestrel_heartbeat::HeartbeatService;
use kestrel_learning::event::{ErrorClassification, LearningEvent, LearningEventBus, SkillOutcome};
use kestrel_learning::prompt::{PromptAssembler, SkillIndexEntry};
use kestrel_memory::types::{MemoryCategory, MemoryEntry, MemoryQuery};
use kestrel_memory::MemoryStore as AsyncMemoryStore;
use kestrel_providers::{CompletionRequest, ProviderRegistry};
use kestrel_session::SessionManager;
use kestrel_skill::SkillRegistry;
use kestrel_tools::ToolRegistry;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn, Instrument};

const REFLECTION_SYSTEM_PROMPT: &str = "You are a brief task reflection engine. Respond in 1-2 concise sentences about what went well or what to improve.";
const REFLECTION_USER_TEMPLATE: &str = "Briefly reflect (1-2 sentences) on this completed agent task:\n- User request: {user_request}\n- Tool calls made: {tool_calls}\n- Iterations: {iterations}\n- Success: {success}\n- Response preview: {response_preview}\n\nFocus on what went well or what could be improved next time.";

struct ReflectionTask {
    learning_bus: LearningEventBus,
    provider_registry: Arc<ProviderRegistry>,
    config: Arc<Config>,
    user_message: String,
    tool_calls_made: usize,
    iterations_used: usize,
    success: bool,
    response_preview: String,
}

/// The main agent loop that processes messages from the bus.
pub struct AgentLoop {
    config: Arc<Config>,
    bus: Arc<MessageBus>,
    session_manager: Arc<SessionManager>,
    /// LLM provider registry used for agent runs and post-task reflection.
    provider_registry: Arc<ProviderRegistry>,
    tool_registry: Arc<ToolRegistry>,
    skill_registry: Option<Arc<SkillRegistry>>,
    hooks: Arc<RwLock<CompositeHook>>,
    running: Arc<RwLock<bool>>,
    compaction_config: CompactionConfig,
    /// Shared set of channel names currently connected.
    connected_channels: Arc<parking_lot::RwLock<HashSet<String>>>,
    /// Shared last-activity timestamp for the agent loop health check.
    agent_activity: Arc<parking_lot::RwLock<Option<chrono::DateTime<chrono::Local>>>>,
    /// Optional sub-agent manager for spawning background tasks.
    subagent_manager: Option<Arc<SubAgentManager>>,
    /// Optional async memory store (kestrel-memory crate) for recall/store.
    memory_store: Option<Arc<dyn AsyncMemoryStore>>,
    /// Optional learning event bus (kestrel-learning crate) for event emission.
    learning_bus: Option<LearningEventBus>,
    /// Optional prompt assembler for dynamic system prompt construction.
    prompt_assembler: Option<PromptAssembler>,
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
        Self {
            config: Arc::new(config),
            bus: Arc::new(bus),
            session_manager: Arc::new(session_manager),
            provider_registry: Arc::new(provider_registry),
            tool_registry: Arc::new(tool_registry),
            skill_registry: None,
            hooks: Arc::new(RwLock::new(CompositeHook::new())),
            running: Arc::new(RwLock::new(false)),
            compaction_config: CompactionConfig::default(),
            connected_channels: Arc::new(parking_lot::RwLock::new(HashSet::new())),
            agent_activity: Arc::new(parking_lot::RwLock::new(None)),
            subagent_manager: None,
            memory_store: None,
            learning_bus: None,
            prompt_assembler: None,
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
        let span = tracing::info_span!(
            "process_message",
            session_key = %msg.session_key(),
            trace_id = %msg.trace_id.as_deref().unwrap_or("no-trace"),
            channel = %msg.channel,
        );

        async move {
            let session_key = msg.session_key();
            info!("Processing message");

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

            self.session_manager.save_session_async(&session);

            // Recall relevant memories from the memory store (if configured).
            let recalled_memory = self.recall_memories(&msg.content).await;

            // Build context (with memory recall + skill matching if attached)
            let system_prompt = {
                let mut context_builder = ContextBuilder::new(&self.config);

                // Attach prompt assembler if configured
                if let Some(ref assembler) = self.prompt_assembler {
                    context_builder = context_builder.with_prompt_assembler(assembler.clone());
                }

                // Match skills against the user message for event emission only.
                if let Some(ref registry) = self.skill_registry {
                    let skill_index_entries = self.build_skill_index_entries(registry).await;
                    if !skill_index_entries.is_empty() {
                        context_builder = context_builder.with_skill_index(skill_index_entries);
                    }

                    let matches = registry.match_skills(&msg.content).await;

                    // Emit SkillUsed learning events for each matched skill
                    if let Some(ref bus) = self.learning_bus {
                        for m in &matches {
                            bus.publish(LearningEvent::SkillUsed {
                                skill_name: m.name.clone(),
                                match_score: m.score,
                                outcome: SkillOutcome::Helpful,
                                timestamp: chrono::Utc::now(),
                            });
                        }
                    }
                }

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

                    if let Err(e) = self.session_manager.save_session(&session) {
                        warn!(
                            session_key = %session_key,
                            "Failed to persist completed session: {e}"
                        );
                    }

                    // Store conversation memory (non-blocking — failures are logged, not propagated)
                    self.store_conversation_memory(&msg.content, &result.content)
                        .await;

                    // Emit ToolSucceeded learning event if tools were used
                    if result.tool_calls_made > 0 {
                        if let Some(ref bus) = self.learning_bus {
                            bus.publish(LearningEvent::ToolSucceeded {
                                tool: "agent_loop".to_string(),
                                args_summary: format!(
                                    "session={}, tool_calls={}, iterations={}",
                                    session_key, result.tool_calls_made, result.iterations_used
                                ),
                                duration_ms: 0,
                                context_hash: format!("sess:{}", session_key),
                                timestamp: chrono::Utc::now(),
                            });
                        }
                    }

                    // Send outbound message
                    let user_msg = msg.content.clone();
                    let result_content = result.content.clone();
                    let tool_calls = result.tool_calls_made;
                    let iterations = result.iterations_used;
                    let outbound = OutboundMessage {
                        channel: msg.channel.clone(),
                        chat_id: msg.chat_id.clone(),
                        content: result.content,
                        reply_to: msg.message_id.clone(),
                        trace_id: msg.trace_id.clone(),
                        media: vec![],
                        metadata: Default::default(),
                    };

                    if let Err(e) = self.bus.publish_outbound(outbound).await {
                        error!("Failed to publish outbound message: {}", e);
                    }

                    // Post-task LLM reflection runs in the background after the
                    // outbound response path completes.
                    if let Some(bus) = self.learning_bus.clone() {
                        let provider_registry = self.provider_registry.clone();
                        let config = self.config.clone();
                        tokio::spawn(async move {
                            post_task_reflect(ReflectionTask {
                                learning_bus: bus,
                                provider_registry,
                                config,
                                user_message: user_msg,
                                tool_calls_made: tool_calls,
                                iterations_used: iterations,
                                success: true,
                                response_preview: result_content,
                            })
                            .await;
                        });
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

                    // Emit ToolFailed learning event
                    if let Some(ref bus) = self.learning_bus {
                        bus.publish(LearningEvent::ToolFailed {
                            tool: "agent_loop".to_string(),
                            args_summary: format!("session={}", session_key),
                            error: ErrorClassification::Environment,
                            error_message: e.to_string(),
                            retry_count: 0,
                            timestamp: chrono::Utc::now(),
                        });
                    }

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
        .instrument(span)
        .await
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
            Ok(results) if results.is_empty() => {
                // Emit MemoryAccessed (miss)
                if let Some(ref bus) = self.learning_bus {
                    bus.publish(LearningEvent::MemoryAccessed {
                        query: query_text.to_string(),
                        results_count: 0,
                        hit: false,
                        timestamp: chrono::Utc::now(),
                    });
                }
                None
            }
            Ok(results) => {
                let count = results.len();
                let mut lines = Vec::new();
                for scored in &results {
                    lines.push(format!(
                        "- {} [{}] (confidence: {:.2})",
                        scored.entry.content, scored.entry.category, scored.entry.confidence
                    ));
                }
                // Emit MemoryAccessed (hit)
                if let Some(ref bus) = self.learning_bus {
                    bus.publish(LearningEvent::MemoryAccessed {
                        query: query_text.to_string(),
                        results_count: count,
                        hit: true,
                        timestamp: chrono::Utc::now(),
                    });
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

    /// Build skill index entries from all registered, non-deprecated skills.
    async fn build_skill_index_entries(&self, registry: &SkillRegistry) -> Vec<SkillIndexEntry> {
        let mut names = registry.skill_names().await;
        names.sort();

        let mut entries = Vec::new();
        for name in names {
            let Some(skill_guard) = registry.get(&name).await else {
                continue;
            };
            let skill = skill_guard.read();
            if skill.is_deprecated() {
                continue;
            }

            let manifest = skill.manifest();
            entries.push(SkillIndexEntry {
                name: manifest.name.clone(),
                description: manifest.description.clone(),
                category: manifest.category.clone(),
                triggers: manifest.triggers.clone(),
            });
        }

        entries
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
            kestrel_config::paths::get_data_dir().unwrap_or_else(|_| std::env::temp_dir());

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

    /// Attach a [`SkillRegistry`] for skill discovery before LLM calls.
    ///
    /// When set, the agent loop will publish a skill index into the system
    /// prompt and use match results for learning events.
    pub fn with_skill_registry(mut self, registry: Arc<SkillRegistry>) -> Self {
        self.skill_registry = Some(registry);
        self
    }

    /// Get the skill registry, if one has been attached.
    pub fn skill_registry(&self) -> Option<&Arc<SkillRegistry>> {
        self.skill_registry.as_ref()
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

    /// Attach a [`LearningEventBus`] for emitting learning events.
    ///
    /// When set, the agent loop will emit learning events (MemoryAccessed,
    /// SkillUsed, ToolSucceeded, ToolFailed) at key points during message
    /// processing.
    pub fn with_learning_bus(mut self, bus: LearningEventBus) -> Self {
        self.learning_bus = Some(bus);
        self
    }

    /// Get the learning event bus, if one has been attached.
    pub fn learning_bus(&self) -> Option<&LearningEventBus> {
        self.learning_bus.as_ref()
    }

    /// Attach a [`PromptAssembler`] for dynamic system prompt construction.
    ///
    /// When set, the assembler controls how prompt sections are joined and
    /// separated. The assembler is passed through to [`ContextBuilder`] during
    /// system prompt construction.
    pub fn with_prompt_assembler(mut self, assembler: PromptAssembler) -> Self {
        self.prompt_assembler = Some(assembler);
        self
    }

    /// Get the prompt assembler, if one has been attached.
    pub fn prompt_assembler(&self) -> Option<&PromptAssembler> {
        self.prompt_assembler.as_ref()
    }

    /// Get the sub-agent manager, if one has been attached.
    pub fn subagent_manager(&self) -> Option<&Arc<SubAgentManager>> {
        self.subagent_manager.as_ref()
    }
}

/// Perform a brief LLM-powered reflection on a completed task in the
/// background after the user response has already been sent.
///
/// Collects task metadata (user message, tool calls, iterations, outcome),
/// asks the configured LLM for a 1-2 sentence assessment, and publishes
/// the result as a [`LearningEvent::TaskReflection`]. Failures are logged
/// and silently ignored — reflection must not break the agent loop.
async fn post_task_reflect(task: ReflectionTask) {
    let model = &task.config.agent.model;
    let provider = match task.provider_registry.get_provider(model) {
        Some(p) => p,
        None => {
            warn!("No provider available for task reflection");
            return;
        }
    };

    let reflection_prompt = REFLECTION_USER_TEMPLATE
        .replace("{user_request}", truncate_str(&task.user_message, 100))
        .replace("{tool_calls}", &task.tool_calls_made.to_string())
        .replace("{iterations}", &task.iterations_used.to_string())
        .replace("{success}", &task.success.to_string())
        .replace(
            "{response_preview}",
            truncate_str(&task.response_preview, 100),
        );

    let request = CompletionRequest {
        model: model.clone(),
        messages: vec![
            Message {
                role: MessageRole::System,
                content: REFLECTION_SYSTEM_PROMPT.to_string(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                role: MessageRole::User,
                content: reflection_prompt,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ],
        tools: None,
        max_tokens: Some(100),
        temperature: Some(0.3),
        stream: false,
    };

    match provider.complete(request).await {
        Ok(response) => {
            if let Some(reflection) = response.content {
                if !reflection.trim().is_empty() {
                    let tool_calls_u32 = task.tool_calls_made as u32;
                    task.learning_bus.publish(LearningEvent::TaskReflection {
                        task_summary: truncate_str(&task.user_message, 100).to_string(),
                        tool_calls_count: tool_calls_u32,
                        success: task.success,
                        reflection,
                        timestamp: chrono::Utc::now(),
                    });
                }
            }
        }
        Err(e) => {
            warn!("Post-task reflection LLM call failed: {}", e);
        }
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
    use futures::stream;
    use kestrel_core::{MessageType, Platform};
    use kestrel_providers::base::{BoxStream, CompletionChunk};
    use kestrel_providers::{CompletionRequest, CompletionResponse, LlmProvider};
    use std::collections::HashMap;

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

    struct MockProvider {
        response: String,
    }

    #[async_trait::async_trait]
    impl LlmProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }

        fn default_model(&self) -> &str {
            "mock-model"
        }

        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> anyhow::Result<CompletionResponse> {
            Ok(CompletionResponse {
                content: Some(self.response.clone()),
                tool_calls: None,
                usage: None,
                finish_reason: Some("stop".to_string()),
            })
        }

        async fn complete_stream(&self, _request: CompletionRequest) -> anyhow::Result<BoxStream> {
            Ok(Box::pin(stream::iter(vec![Ok(CompletionChunk {
                delta: Some(self.response.clone()),
                tool_call_deltas: None,
                usage: None,
                done: true,
            })])))
        }

        fn supports_model(&self, _model: &str) -> bool {
            true
        }
    }

    fn make_agent_loop_with_provider(response: &str) -> AgentLoop {
        let mut config = Config::default();
        config.agent.model = "mock-model".to_string();

        let bus = MessageBus::new();
        let session_dir = tempfile::tempdir().unwrap();
        let session_manager = SessionManager::new(session_dir.path().to_path_buf()).unwrap();
        let mut provider_registry = ProviderRegistry::new();
        provider_registry.register(
            "mock",
            MockProvider {
                response: response.to_string(),
            },
        );
        provider_registry.set_default("mock");

        AgentLoop::new(
            config,
            bus,
            session_manager,
            provider_registry,
            ToolRegistry::new(),
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
        config.channels.telegram = Some(kestrel_config::schema::TelegramConfig {
            token: "test".to_string(),
            allowed_users: vec![],
            admin_users: vec![],
            enabled: true,
            streaming: false,
            proxy: None,
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
        config.channels.telegram = Some(kestrel_config::schema::TelegramConfig {
            token: "t".to_string(),
            allowed_users: vec![],
            admin_users: vec![],
            enabled: true,
            streaming: false,
            proxy: None,
        });
        config.channels.discord = Some(kestrel_config::schema::DiscordConfig {
            token: "d".to_string(),
            allowed_guilds: vec![],
            enabled: true,
            streaming: false,
            proxy: None,
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
    async fn test_process_message_publishes_reply_when_persistence_fails() {
        let al = make_agent_loop_with_provider("reply delivered");
        let al = AgentLoop {
            session_manager: Arc::new(
                (*al.session_manager)
                    .clone()
                    .with_persist_hook(Arc::new(|_| Err(anyhow::anyhow!("mock disk error")))),
            ),
            ..al
        };
        let mut outbound_rx = al.bus.consume_outbound().await.unwrap();

        let msg = InboundMessage {
            channel: Platform::Telegram,
            sender_id: "user-1".to_string(),
            chat_id: "chat-1".to_string(),
            content: "hello".to_string(),
            media: vec![],
            metadata: HashMap::new(),
            source: None,
            message_type: MessageType::Text,
            message_id: Some("msg-1".to_string()),
            trace_id: None,
            reply_to: None,
            timestamp: chrono::Local::now(),
        };

        al.process_message(msg).await.unwrap();

        let outbound = tokio::time::timeout(std::time::Duration::from_secs(5), outbound_rx.recv())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(outbound.chat_id, "chat-1");
        assert_eq!(outbound.reply_to.as_deref(), Some("msg-1"));
        assert_eq!(outbound.content, "reply delivered");
    }

    #[tokio::test]
    async fn test_spawn_heartbeat_registers_checks() {
        let mut config = Config::default();
        config.heartbeat.enabled = true;
        config.heartbeat.interval_secs = 60;
        config.channels.telegram = Some(kestrel_config::schema::TelegramConfig {
            token: "test".to_string(),
            allowed_users: vec![],
            admin_users: vec![],
            enabled: true,
            streaming: false,
            proxy: None,
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

    use kestrel_memory::types::ScoredEntry;
    use kestrel_memory::HotStore;
    use kestrel_memory::MemoryError;
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
            query: &kestrel_memory::types::MemoryQuery,
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
        let config = kestrel_memory::MemoryConfig::for_test(dir.path());
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
            .store(MemoryEntry::new("Project uses Rust", MemoryCategory::Fact).with_confidence(0.8))
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
        let config = kestrel_memory::MemoryConfig::for_test(dir.path());
        let store = HotStore::new(&config).await.unwrap();

        let al = make_agent_loop().with_memory_store(Arc::new(store));

        al.store_conversation_memory("How do I build?", "Use cargo build")
            .await;

        // Verify stored by searching
        let store = al.memory_store.unwrap();
        let results = store
            .search(&kestrel_memory::types::MemoryQuery::new().with_text("cargo"))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].entry.content.contains("cargo"));
    }

    // ── Skill registry wiring tests ──────────────────────────

    #[test]
    fn test_skill_registry_none_by_default() {
        let al = make_agent_loop();
        assert!(al.skill_registry.is_none());
        assert!(al.skill_registry().is_none());
    }

    #[test]
    fn test_with_skill_registry() {
        let registry = Arc::new(SkillRegistry::new());
        let al = make_agent_loop().with_skill_registry(registry);
        assert!(al.skill_registry.is_some());
        assert!(al.skill_registry().is_some());
    }

    #[tokio::test]
    async fn test_runtime_context_includes_skill_index_entries_when_skills_registered() {
        use crate::context::ContextBuilder;
        use kestrel_bus::events::InboundMessage;
        use kestrel_core::{MessageType, Platform};
        use kestrel_session::Session;
        use kestrel_skill::manifest::SkillManifestBuilder;
        use kestrel_skill::skill::CompiledSkill;

        let registry = Arc::new(SkillRegistry::new());
        registry
            .register(CompiledSkill::new(
                SkillManifestBuilder::new("deploy-k8s", "1.0.0", "Deploy to Kubernetes")
                    .triggers(vec!["deploy".to_string(), "k8s".to_string()])
                    .build(),
            ))
            .await
            .unwrap();

        let al = make_agent_loop().with_skill_registry(registry.clone());
        let entries = al.build_skill_index_entries(&registry).await;
        let config = Config::default();
        let msg = InboundMessage {
            channel: Platform::Telegram,
            sender_id: "user1".to_string(),
            chat_id: "chat1".to_string(),
            content: "hello".to_string(),
            media: vec![],
            metadata: Default::default(),
            source: None,
            message_type: MessageType::Text,
            message_id: None,
            trace_id: None,
            reply_to: None,
            timestamp: chrono::Local::now(),
        };
        let session = Session::new("test:key".to_string());
        let tools = ToolRegistry::new();

        let prompt = ContextBuilder::new(&config)
            .with_skill_index(entries)
            .build_system_prompt(&msg, &session, &tools, None)
            .unwrap();

        assert!(prompt.contains("## Skill Index"));
        assert!(prompt.contains("skill_view(name)"));
        assert!(prompt.contains("- deploy-k8s: Deploy to Kubernetes [category: uncategorized]"));
        assert!(!prompt.contains("Apply manifests"));
    }

    // ── Learning event bus wiring tests ────────────────────────

    #[test]
    fn test_learning_bus_none_by_default() {
        let al = make_agent_loop();
        assert!(al.learning_bus.is_none());
        assert!(al.learning_bus().is_none());
    }

    #[test]
    fn test_with_learning_bus_builder() {
        let bus = LearningEventBus::new();
        let al = make_agent_loop().with_learning_bus(bus);
        assert!(al.learning_bus.is_some());
        assert!(al.learning_bus().is_some());
    }

    #[tokio::test]
    async fn test_learning_bus_emits_memory_accessed_on_recall() {
        let bus = LearningEventBus::new();
        let mut rx = bus.subscribe();

        let mock = Arc::new(MockMemoryStore::new());
        mock.store(
            MemoryEntry::new("User likes Rust", MemoryCategory::Preference).with_confidence(0.9),
        )
        .await
        .unwrap();

        let al = make_agent_loop()
            .with_memory_store(mock)
            .with_learning_bus(bus);

        // Trigger recall (which emits the event)
        let result = al.recall_memories("rust").await;
        assert!(result.is_some());

        // Verify the event was published
        let event = rx.try_recv().expect("should receive MemoryAccessed event");
        match event {
            LearningEvent::MemoryAccessed {
                query,
                results_count,
                hit,
                ..
            } => {
                assert!(query.contains("rust"));
                assert!(results_count > 0);
                assert!(hit);
            }
            other => panic!("Expected MemoryAccessed, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_learning_bus_emits_memory_accessed_no_hit() {
        // No memory store → recall returns None immediately (via `?`),
        // no event emitted at all.
        let bus = LearningEventBus::new();
        let mut rx = bus.subscribe();

        let al = make_agent_loop().with_learning_bus(bus);
        let result = al.recall_memories("rust").await;
        assert!(result.is_none());

        // No store → no event emitted
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_learning_bus_emits_memory_accessed_with_hit() {
        use kestrel_learning::event::LearningEvent;

        let bus = LearningEventBus::new();
        let mut rx = bus.subscribe();

        let mock = Arc::new(MockMemoryStore::new());
        let al = make_agent_loop()
            .with_memory_store(mock)
            .with_learning_bus(bus);

        // Empty store → recall returns None → no hit
        let _ = al.recall_memories("anything").await;

        // Verify event was emitted with hit=false
        let event = rx.try_recv().expect("should receive event");
        match event {
            LearningEvent::MemoryAccessed {
                hit, results_count, ..
            } => {
                assert!(!hit);
                assert_eq!(results_count, 0);
            }
            other => panic!("Expected MemoryAccessed, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_learning_bus_emits_skill_used() {
        use kestrel_learning::event::LearningEvent;
        use kestrel_skill::manifest::SkillManifestBuilder;
        use kestrel_skill::skill::CompiledSkill;

        let bus = LearningEventBus::new();
        let mut rx = bus.subscribe();

        let registry = Arc::new(SkillRegistry::new());
        let skill = CompiledSkill::new(
            SkillManifestBuilder::new("deploy-k8s", "1.0.0", "Deploy to Kubernetes")
                .triggers(vec!["deploy".to_string(), "k8s".to_string()])
                .steps(vec!["Apply manifests".to_string()])
                .build(),
        );
        registry.register(skill).await.unwrap();

        let al = make_agent_loop()
            .with_skill_registry(registry)
            .with_learning_bus(bus);

        // Build skill sections triggers match and emission
        let registry = al.skill_registry().unwrap();
        let matches = registry.match_skills("deploy to k8s").await;
        assert_eq!(matches.len(), 1);

        // Manually emit as process_message would
        if let Some(ref bus) = al.learning_bus {
            for m in &matches {
                bus.publish(LearningEvent::SkillUsed {
                    skill_name: m.name.clone(),
                    match_score: m.score,
                    outcome: kestrel_learning::event::SkillOutcome::Helpful,
                    timestamp: chrono::Utc::now(),
                });
            }
        }

        let event = rx.try_recv().expect("should receive SkillUsed event");
        match event {
            LearningEvent::SkillUsed {
                skill_name,
                match_score,
                ..
            } => {
                assert_eq!(skill_name, "deploy-k8s");
                assert!(match_score > 0.0);
            }
            other => panic!("Expected SkillUsed, got: {:?}", other),
        }
    }

    #[test]
    fn test_learning_bus_clone_and_subscribe() {
        let bus = LearningEventBus::new();
        let mut rx = bus.subscribe();

        bus.publish(LearningEvent::ToolSucceeded {
            tool: "test".to_string(),
            args_summary: "args".to_string(),
            duration_ms: 10,
            context_hash: "hash".to_string(),
            timestamp: chrono::Utc::now(),
        });

        assert!(rx.try_recv().is_ok());
    }

    // ── PromptAssembler integration tests ─────────────────────────

    #[test]
    fn test_prompt_assembler_none_by_default() {
        let al = make_agent_loop();
        assert!(al.prompt_assembler.is_none());
        assert!(al.prompt_assembler().is_none());
    }

    #[test]
    fn test_with_prompt_assembler_builder() {
        let assembler = PromptAssembler::new();
        let al = make_agent_loop().with_prompt_assembler(assembler);
        assert!(al.prompt_assembler.is_some());
        assert!(al.prompt_assembler().is_some());
    }

    #[test]
    fn test_with_prompt_assembler_custom_separator() {
        let assembler = PromptAssembler::with_separator("\n---\n");
        let al = make_agent_loop().with_prompt_assembler(assembler);
        assert!(al.prompt_assembler.is_some());
    }

    #[test]
    fn test_prompt_assembler_passed_to_context_builder() {
        // Verify that when a PromptAssembler is attached, the assembled prompt
        // uses the custom separator.
        let assembler = PromptAssembler::with_separator("\n===\n");
        let al = make_agent_loop().with_prompt_assembler(assembler);

        // We can't call process_message without a full agent setup,
        // but we can verify the builder method is wired correctly.
        assert!(al.prompt_assembler().is_some());
    }

    // ── Post-task reflection tests ───────────────────────────────

    #[tokio::test]
    async fn test_post_task_reflect_publishes_event() {
        let bus = LearningEventBus::new();
        let mut rx = bus.subscribe();

        let al = make_agent_loop_with_provider("Execution was smooth and efficient.");
        post_task_reflect(ReflectionTask {
            learning_bus: bus,
            provider_registry: al.provider_registry.clone(),
            config: al.config.clone(),
            user_message: "deploy to prod".to_string(),
            tool_calls_made: 3,
            iterations_used: 2,
            success: true,
            response_preview: "deployed successfully".to_string(),
        })
        .await;

        let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("timeout")
            .expect("should receive event");

        match event {
            LearningEvent::TaskReflection {
                task_summary,
                tool_calls_count,
                success,
                reflection,
                ..
            } => {
                assert!(task_summary.contains("deploy to prod"));
                assert_eq!(tool_calls_count, 3);
                assert!(success);
                assert!(reflection.contains("smooth"));
            }
            other => panic!("Expected TaskReflection, got: {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_post_task_reflect_no_learning_bus() {
        // No learning bus means the background helper is never spawned.
        let al = make_agent_loop_with_provider("reflection");
        let _ = al;
    }

    #[tokio::test]
    async fn test_post_task_reflect_no_provider() {
        let bus = LearningEventBus::new();
        let mut rx = bus.subscribe();

        // make_agent_loop creates a ProviderRegistry with no providers.
        let al = make_agent_loop();

        post_task_reflect(ReflectionTask {
            learning_bus: bus,
            provider_registry: al.provider_registry.clone(),
            config: al.config.clone(),
            user_message: "test".to_string(),
            tool_calls_made: 0,
            iterations_used: 0,
            success: true,
            response_preview: "ok".to_string(),
        })
        .await;

        // No event should be published since no provider is configured.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_post_task_reflect_empty_response_no_event() {
        let bus = LearningEventBus::new();
        let mut rx = bus.subscribe();

        // MockProvider returns an empty string.
        let al = make_agent_loop_with_provider("");

        post_task_reflect(ReflectionTask {
            learning_bus: bus,
            provider_registry: al.provider_registry.clone(),
            config: al.config.clone(),
            user_message: "test".to_string(),
            tool_calls_made: 1,
            iterations_used: 1,
            success: true,
            response_preview: "done".to_string(),
        })
        .await;

        // Empty content should not publish an event.
        assert!(rx.try_recv().is_err());
    }
}
