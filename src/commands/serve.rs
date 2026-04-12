//! Serve command — start the OpenAI-compatible API server.
//!
//! Starts the API server with a background agent loop and
//! pre-built provider/tool registries for direct agent processing.

use anyhow::Result;
use nanobot_agent::AgentLoop;
use nanobot_api::ApiServer;
use nanobot_bus::MessageBus;
use nanobot_config::Config;
use nanobot_providers::ProviderRegistry;
use nanobot_session::SessionManager;
use nanobot_tools::builtins;
use tracing::info;

/// Run the API server.
pub async fn run(config: Config, port: u16) -> Result<()> {
    info!("Starting nanobot API server on port {}...", port);

    let bus = MessageBus::new();
    let home = nanobot_config::paths::get_nanobot_home()?;
    let session_manager = SessionManager::new(home)?;

    // ── Provider registry ─────────────────────────────────────
    let provider_registry = ProviderRegistry::from_config(&config)?;
    info!("Providers: {:?}", provider_registry.provider_names());

    // ── Tool registry ─────────────────────────────────────────
    let tool_registry = nanobot_tools::ToolRegistry::new();
    builtins::register_all(&tool_registry);
    info!("Tools: {:?}", tool_registry.tool_names());

    // ── Agent loop (background, for bus-based messages) ───────
    let agent_loop = AgentLoop::new(
        config.clone(),
        bus.clone(),
        session_manager.clone(),
        provider_registry.clone(),
        tool_registry.clone(),
    );

    let agent_handle = tokio::spawn(async move {
        if let Err(e) = agent_loop.run().await {
            tracing::error!("Agent loop error: {}", e);
        }
    });

    // ── API server (with registries for direct agent access) ──
    let server = ApiServer::with_registries(
        config,
        bus,
        session_manager,
        provider_registry,
        tool_registry,
        port,
    );
    let api_handle = tokio::spawn(async move {
        if let Err(e) = server.run().await {
            tracing::error!("API server error: {}", e);
        }
    });

    info!("API server running on port {}", port);

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received shutdown signal");
        }
        _ = agent_handle => {
            info!("Agent loop exited");
        }
        _ = api_handle => {
            info!("API server exited");
        }
    }

    Ok(())
}
