//! Agent loop — the main message processing cycle.
//!
//! Consumes InboundMessages from the bus, builds context, runs the agent,
//! and publishes OutboundMessages back. Mirrors the Python `agent/loop.py`.

use crate::context::ContextBuilder;
use crate::hook::CompositeHook;
use crate::runner::AgentRunner;
use anyhow::Result;
use nanobot_bus::events::{AgentEvent, InboundMessage, OutboundMessage, StreamChunk};
use nanobot_bus::MessageBus;
use nanobot_config::Config;
use nanobot_providers::ProviderRegistry;
use nanobot_session::SessionManager;
use nanobot_tools::ToolRegistry;
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

        Self {
            config,
            bus,
            session_manager,
            provider_registry,
            tool_registry,
            hooks,
            running,
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
}
