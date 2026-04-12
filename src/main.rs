//! # nanobot-rust
//!
//! A Rust rewrite of the Python nanobot AI agent framework.
//! Features an agent loop, channel system, bus message bus,
//! session management, cron scheduling, heartbeat, and security modules.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

mod commands;

/// Nanobot — multi-platform AI agent framework.
#[derive(Parser)]
#[command(name = "nanobot-rs")]
#[command(version = nanobot_core::VERSION)]
#[command(about = "A multi-platform AI agent framework")]
struct Cli {
    /// Path to config file.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// Log level (trace, debug, info, warn, error).
    #[arg(short, long, global = true, default_value = "info")]
    log_level: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the interactive agent (local mode).
    Agent {
        /// Initial message to send to the agent.
        message: Option<String>,
    },

    /// Start the gateway (connect to chat platforms).
    Gateway {
        /// Channel to start (e.g., "telegram", "discord").
        /// If omitted, auto-detects from config.
        channels: Vec<String>,
    },

    /// Start the OpenAI-compatible API server.
    Serve {
        /// Port to listen on.
        #[arg(short, long, default_value = "8080")]
        port: u16,
    },

    /// Start the heartbeat service (periodic task checking).
    Heartbeat,

    /// Configuration management commands.
    Config {
        #[command(subcommand)]
        subcommand: ConfigSubcommand,
    },

    /// Interactive configuration setup.
    Setup,

    /// Show current configuration and status.
    Status,
}

#[derive(Subcommand)]
enum ConfigSubcommand {
    /// Validate the config.yaml schema.
    Validate,

    /// Migrate Python nanobot config to nanobot-rs format.
    Migrate {
        /// Path to Python nanobot config directory (e.g., ~/.nanobot).
        #[arg(long)]
        from: PathBuf,

        /// Dry run: print resulting YAML to stdout instead of writing to file.
        #[arg(long)]
        dry_run: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cli.log_level)),
        )
        .init();

    // Load configuration
    let config = nanobot_config::load_config(cli.config.as_deref())?;

    match cli.command {
        Commands::Agent { message } => {
            commands::agent::run(config, message).await?;
        }
        Commands::Gateway { channels } => {
            commands::gateway::run(config, channels).await?;
        }
        Commands::Serve { port } => {
            commands::serve::run(config, port).await?;
        }
        Commands::Heartbeat => {
            commands::heartbeat::run(config).await?;
        }
        Commands::Config { subcommand } => match subcommand {
            ConfigSubcommand::Validate => {
                commands::config::validate(&config)?;
            }
            ConfigSubcommand::Migrate { from, dry_run } => {
                commands::config::migrate(&from, dry_run)?;
            }
        },
        Commands::Setup => {
            commands::setup::run(config)?;
        }
        Commands::Status => {
            commands::status::run(&config)?;
        }
    }

    Ok(())
}
