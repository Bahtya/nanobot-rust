//! Gateway command — start the full nanobot gateway.
//!
//! Wires together: bus, channels, agent loop, session manager,
//! provider registry, tool registry, heartbeat, and API server.
//!
//! Supports daemon mode when launched via `nanobot-rs daemon start`.

use std::sync::Arc;

use anyhow::Result;
use nanobot_agent::AgentLoop;
use nanobot_api::ApiServer;
use nanobot_bus::events::AgentEvent;
use nanobot_bus::MessageBus;
use nanobot_channels::{ChannelManager, ChannelRegistry};
use nanobot_config::Config;
use nanobot_heartbeat::HeartbeatService;
use nanobot_memory::{HotStore, MemoryConfig};
use nanobot_providers::ProviderRegistry;
use nanobot_session::SessionManager;
use nanobot_tools::builtins;
use tracing::info;

/// Run the gateway — starts all components and connects them via the bus.
///
/// PID file management is handled by the `daemon start` command, not here.
pub async fn run(config: Config, channels: Vec<String>) -> Result<()> {
    info!("Starting nanobot gateway...");

    // ── Shared bus ────────────────────────────────────────────
    let bus = MessageBus::new();

    // ── Session manager ───────────────────────────────────────
    let home = nanobot_config::paths::get_nanobot_home()?;
    let session_manager = SessionManager::new(home.clone())?;

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

    // ── Channel manager (wrapped in Arc for shared access) ────
    let channel_registry = ChannelRegistry::new();
    let channel_manager = Arc::new(ChannelManager::new(channel_registry, bus.clone()));

    // ── Agent loop ────────────────────────────────────────────
    let agent_loop = {
        let mut al = AgentLoop::new(
            config.clone(),
            bus.clone(),
            session_manager.clone(),
            provider_registry.clone(),
            tool_registry.clone(),
        );

        // Wire memory store (HotStore L1)
        let memory_config = MemoryConfig {
            hot_store_path: home.join("memory").join("hot.jsonl"),
            ..MemoryConfig::default()
        };
        match HotStore::new(&memory_config).await {
            Ok(hot_store) => {
                info!("Memory store initialized (HotStore L1)");
                al = al.with_memory_store(Arc::new(hot_store));
            }
            Err(e) => {
                tracing::warn!("Failed to initialize memory store, continuing without memory: {}", e);
            }
        }

        al
    };

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
    let api_server = ApiServer::with_registries(
        config.clone(),
        bus.clone(),
        session_manager,
        provider_registry,
        tool_registry,
        None,
    );

    // ── Spawn background tasks ────────────────────────────────
    let _agent_handle = tokio::spawn(async move {
        if let Err(e) = agent_loop.run().await {
            tracing::error!("Agent loop error: {}", e);
        }
    });

    let outbound_cm = channel_manager.clone();
    let _outbound_handle = tokio::spawn(async move {
        outbound_cm.run_outbound_consumer().await;
    });

    // ── Typing indicator lifecycle ──────────────────────────────
    let typing_cm = channel_manager.clone();
    let mut typing_event_rx = bus.subscribe_events();
    let _typing_handle = tokio::spawn(async move {
        loop {
            match typing_event_rx.recv().await {
                Ok(AgentEvent::Started { session_key }) => {
                    typing_cm.start_typing(&session_key);
                }
                Ok(AgentEvent::Completed { session_key, .. })
                | Ok(AgentEvent::Error { session_key, .. }) => {
                    typing_cm.stop_typing(&session_key);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("Typing event consumer lagged by {n} messages");
                }
                Err(_) => break,
                _ => {}
            }
        }
    });

    let heartbeat_enabled = config.heartbeat.enabled;
    let _heartbeat_handle = tokio::spawn(async move {
        if heartbeat_enabled {
            if let Err(e) = heartbeat.run().await {
                tracing::error!("Heartbeat error: {}", e);
            }
        } else {
            // Park forever so the select! doesn't fire on a disabled service
            std::future::pending::<()>().await;
        }
    });

    let _api_handle = tokio::spawn(async move {
        if let Err(e) = api_server.run().await {
            tracing::error!("API server error: {}", e);
        }
    });

    info!("Gateway is running (API on port {})", config.api.port);
    info!("Press Ctrl+C to stop");

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
                    info!("Received SIGHUP (log rotation placeholder — not yet implemented)");
                    // Keep running; future sprint will add config reload
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
            _ = _outbound_handle => {
                info!("Outbound consumer exited");
            }
            _ = _heartbeat_handle => {
                info!("Heartbeat exited");
            }
            _ = _api_handle => {
                info!("API server exited");
            }
            _ = _typing_handle => {
                info!("Typing handler exited");
            }
        }
    }

    // ── Graceful shutdown ─────────────────────────────────────
    info!("Stopping all channels...");
    channel_manager.stop_all().await;

    info!("Gateway shutting down");
    Ok(())
}
