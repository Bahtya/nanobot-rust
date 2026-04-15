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
///
/// `port_override`: when `Some`, overrides the port from `config.api.port`.
pub async fn run(config: Config, port_override: Option<u16>, dangerous: bool) -> Result<()> {
    let effective_port = port_override.unwrap_or(config.api.port);
    info!("Starting nanobot API server on port {}...", effective_port);

    let bus = MessageBus::new();
    let home = nanobot_config::paths::get_nanobot_home()?;
    let session_manager = SessionManager::new(home)?;

    // ── Provider registry ─────────────────────────────────────
    let provider_registry = ProviderRegistry::from_config(&config)?;
    info!("Providers: {:?}", provider_registry.provider_names());

    // ── Tool registry ─────────────────────────────────────────
    let tool_registry = nanobot_tools::ToolRegistry::new();
    builtins::register_all_with_config(&tool_registry, builtins::BuiltinsConfig { dangerous });
    info!("Tools: {:?}", tool_registry.tool_names());

    // ── Agent loop (background, for bus-based messages) ───────
    let agent_loop = AgentLoop::new(
        config.clone(),
        bus.clone(),
        session_manager.clone(),
        provider_registry.clone(),
        tool_registry.clone(),
    );

    let _agent_handle = tokio::spawn(async move {
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
        port_override,
    );
    let _api_handle = tokio::spawn(async move {
        if let Err(e) = server.run().await {
            tracing::error!("API server error: {}", e);
        }
    });

    info!("API server running on port {}", effective_port);

    // ── Wait for shutdown signal ──────────────────────────────
    #[cfg(target_family = "unix")]
    {
        loop {
            let sig = nanobot_daemon::signal::wait_for_signal().await;
            match sig {
                nanobot_daemon::signal::ShutdownSignal::Graceful => {
                    info!("Received graceful shutdown signal (SIGTERM)");
                    break;
                }
                nanobot_daemon::signal::ShutdownSignal::Fast => {
                    info!("Received fast shutdown signal (SIGINT)");
                    break;
                }
                nanobot_daemon::signal::ShutdownSignal::Reload => {
                    info!("Received SIGHUP (log rotation placeholder)");
                    continue;
                }
            }
        }
    }

    #[cfg(not(target_family = "unix"))]
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Received shutdown signal");
            }
            _ = _agent_handle => {
                info!("Agent loop exited");
            }
            _ = _api_handle => {
                info!("API server exited");
            }
        }
    }

    Ok(())
}
