//! Gateway command — start the full nanobot gateway.
//!
//! Wires together: bus, channels, agent loop, session manager,
//! provider registry, tool registry, heartbeat, and API server.

use anyhow::Result;
use nanobot_agent::AgentLoop;
use nanobot_api::ApiServer;
use nanobot_bus::MessageBus;
use nanobot_channels::{ChannelManager, ChannelRegistry};
use nanobot_config::Config;
use nanobot_heartbeat::HeartbeatService;
use nanobot_providers::ProviderRegistry;
use nanobot_session::SessionManager;
use nanobot_tools::builtins;
use tracing::info;

/// Run the gateway — starts all components and connects them via the bus.
pub async fn run(config: Config, channels: Vec<String>) -> Result<()> {
    info!("Starting nanobot gateway...");

    // ── Shared bus ────────────────────────────────────────────
    let bus = MessageBus::new();

    // ── Session manager ───────────────────────────────────────
    let home = nanobot_config::paths::get_nanobot_home()?;
    let session_manager = SessionManager::new(home)?;

    // ── Provider registry ─────────────────────────────────────
    let provider_registry = ProviderRegistry::from_config(&config)?;
    info!("Providers: {:?}", provider_registry.provider_names());

    // ── Tool registry ─────────────────────────────────────────
    let tool_registry = nanobot_tools::ToolRegistry::new();
    builtins::register_all(&tool_registry);
    info!("Tools: {:?}", tool_registry.tool_names());

    // ── Seed channel tokens from config into env vars ─────────
    if let Some(ref tg) = config.channels.telegram {
        if tg.enabled {
            std::env::set_var("TELEGRAM_BOT_TOKEN", &tg.token);
        }
    }
    if let Some(ref dc) = config.channels.discord {
        if dc.enabled {
            std::env::set_var("DISCORD_BOT_TOKEN", &dc.token);
        }
    }

    // ── Channel manager ───────────────────────────────────────
    let channel_registry = ChannelRegistry::new();
    let channel_manager = ChannelManager::new(channel_registry, bus.clone());

    // ── Agent loop ────────────────────────────────────────────
    let agent_loop = AgentLoop::new(
        config.clone(),
        bus.clone(),
        session_manager.clone(),
        provider_registry.clone(),
        tool_registry.clone(),
    );

    // ── Determine which channels to start ─────────────────────
    let channels_to_start = if channels.is_empty() {
        let mut auto = Vec::new();
        if let Some(ref tg) = config.channels.telegram {
            if tg.enabled {
                auto.push("telegram".to_string());
            }
        }
        if let Some(ref dc) = config.channels.discord {
            if dc.enabled {
                auto.push("discord".to_string());
            }
        }
        if auto.is_empty() {
            info!("No channels configured; starting with local-only mode");
        }
        auto
    } else {
        channels
    };

    info!("Starting channels: {:?}", channels_to_start);

    for channel_name in &channels_to_start {
        if let Err(e) = channel_manager.start_channel(channel_name).await {
            tracing::error!("Failed to start channel '{}': {}", channel_name, e);
        }
    }

    // ── Heartbeat service ─────────────────────────────────────
    let heartbeat = HeartbeatService::with_registries(
        config.clone(),
        provider_registry.clone(),
        tool_registry.clone(),
        session_manager.clone(),
    );

    // ── API server (with registries for direct agent access) ──
    let api_port: u16 = 8080;
    let api_server = ApiServer::with_registries(
        config.clone(),
        bus.clone(),
        session_manager,
        provider_registry,
        tool_registry,
        api_port,
    );

    // ── Spawn background tasks ────────────────────────────────
    let agent_handle = tokio::spawn(async move {
        if let Err(e) = agent_loop.run().await {
            tracing::error!("Agent loop error: {}", e);
        }
    });

    let outbound_handle = tokio::spawn(async move {
        channel_manager.run_outbound_consumer().await;
    });

    let heartbeat_enabled = config.heartbeat.enabled;
    let heartbeat_handle = tokio::spawn(async move {
        if heartbeat_enabled {
            if let Err(e) = heartbeat.run().await {
                tracing::error!("Heartbeat error: {}", e);
            }
        } else {
            // Park forever so the select! doesn't fire on a disabled service
            std::future::pending::<()>().await;
        }
    });

    let api_handle = tokio::spawn(async move {
        if let Err(e) = api_server.run().await {
            tracing::error!("API server error: {}", e);
        }
    });

    info!("Gateway is running (API on port {})", api_port);
    info!("Press Ctrl+C to stop");

    // ── Wait for shutdown signal ──────────────────────────────
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Received shutdown signal");
        }
        _ = agent_handle => {
            info!("Agent loop exited");
        }
        _ = outbound_handle => {
            info!("Outbound consumer exited");
        }
        _ = heartbeat_handle => {
            info!("Heartbeat exited");
        }
        _ = api_handle => {
            info!("API server exited");
        }
    }

    info!("Gateway shutting down");
    Ok(())
}
