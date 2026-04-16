//! Agent command — start interactive local agent.

use anyhow::Result;
use kestrel_agent::AgentLoop;
use kestrel_bus::MessageBus;
use kestrel_config::Config;
use kestrel_core::{MessageType, Platform};
use kestrel_providers::ProviderRegistry;
use kestrel_session::SessionManager;
use kestrel_tools::builtins;
use tracing::info;

/// Run the interactive agent.
pub async fn run(config: Config, initial_message: Option<String>, dangerous: bool) -> Result<()> {
    info!("Starting kestrel agent...");

    // Initialize shared components
    let bus = MessageBus::new();
    let home = kestrel_config::paths::get_kestrel_home()?;
    let session_manager = SessionManager::new(home)?;
    let provider_registry = ProviderRegistry::from_config(&config)?;
    let tool_registry = kestrel_tools::ToolRegistry::new();
    builtins::register_all_with_config(&tool_registry, builtins::BuiltinsConfig { dangerous });

    info!("Providers: {:?}", provider_registry.provider_names());
    info!("Tools: {:?}", tool_registry.tool_names());

    // Build the agent loop
    let agent_loop = AgentLoop::new(
        config,
        bus.clone(),
        session_manager,
        provider_registry,
        tool_registry,
    );

    if let Some(msg) = initial_message {
        // One-shot: process a single message
        let inbound = kestrel_bus::events::InboundMessage {
            channel: Platform::Local,
            sender_id: "user".to_string(),
            chat_id: "local".to_string(),
            content: msg,
            media: vec![],
            metadata: Default::default(),
            source: None,
            message_type: MessageType::Text,
            message_id: None,
            reply_to: None,
            timestamp: chrono::Local::now(),
        };

        // Spawn a task to consume outbound messages
        let (reply_tx, mut reply_rx) = tokio::sync::mpsc::channel::<String>(4);
        let reply_bus = bus.clone();
        let _out_listener = tokio::spawn(async move {
            if let Some(mut rx) = reply_bus.consume_outbound().await {
                while let Some(msg) = rx.recv().await {
                    if reply_tx.send(msg.content).await.is_err() {
                        break;
                    }
                }
            }
        });

        agent_loop.process_message(inbound).await?;

        // Wait for the response
        match tokio::time::timeout(std::time::Duration::from_secs(120), reply_rx.recv()).await {
            Ok(Some(reply)) => println!("{}", reply),
            Ok(None) => tracing::warn!("No response received"),
            Err(_) => tracing::warn!("Timed out waiting for agent response"),
        }
    } else {
        // Daemon mode: run the full agent loop
        agent_loop.run().await?;
    }

    Ok(())
}
