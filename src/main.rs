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
        /// Port to listen on. Overrides config.api.port.
        #[arg(short, long)]
        port: Option<u16>,
    },

    /// Start the heartbeat service (periodic task checking).
    Heartbeat,

    /// Show health check status from the heartbeat service.
    Health,

    /// Cron job management commands.
    Cron {
        #[command(subcommand)]
        subcommand: CronSubcommand,
    },

    /// Configuration management commands.
    Config {
        #[command(subcommand)]
        subcommand: ConfigSubcommand,
    },

    /// Interactive configuration setup.
    Setup,

    /// Show current configuration and status.
    Status,

    /// Daemon management commands (Unix only).
    Daemon {
        #[command(subcommand)]
        subcommand: DaemonSubcommand,
    },
}

#[derive(Subcommand)]
enum CronSubcommand {
    /// List all cron jobs.
    List,

    /// Show status of a specific cron job (by name or ID).
    Status {
        /// Job name or ID.
        name: String,
    },
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

#[derive(Subcommand)]
enum DaemonSubcommand {
    /// Start the daemon (daemonize then run gateway).
    Start,

    /// Stop the running daemon.
    Stop,

    /// Restart the daemon (stop + start).
    Restart,

    /// Check daemon status.
    Status,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // For daemon start, we must fork BEFORE starting the tokio runtime.
    // fork() only copies the calling thread — tokio worker threads are lost.
    // All other commands need the runtime, so we defer its creation.
    let is_daemon_start = matches!(
        &cli.command,
        Commands::Daemon {
            subcommand: DaemonSubcommand::Start
        }
    );

    if !is_daemon_start {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new(&cli.log_level)),
            )
            .init();
    }

    // Load configuration
    let config = nanobot_config::load_config(cli.config.as_deref())?;

    match cli.command {
        Commands::Agent { message } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(commands::agent::run(config, message))?;
        }
        Commands::Gateway { channels } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(commands::gateway::run(config, channels))?;
        }
        Commands::Serve { port } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(commands::serve::run(config, port))?;
        }
        Commands::Heartbeat => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(commands::heartbeat::run(config))?;
        }
        Commands::Health => {
            commands::health::check(&config)?;
        }
        Commands::Cron { subcommand } => match subcommand {
            CronSubcommand::List => {
                commands::cron::list(&config)?;
            }
            CronSubcommand::Status { name } => {
                commands::cron::status(&config, &name)?;
            }
        },
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
        Commands::Daemon { subcommand } => {
            let action = match subcommand {
                DaemonSubcommand::Start => commands::daemon::DaemonAction::Start,
                DaemonSubcommand::Stop => commands::daemon::DaemonAction::Stop,
                DaemonSubcommand::Restart => commands::daemon::DaemonAction::Restart,
                DaemonSubcommand::Status => commands::daemon::DaemonAction::Status,
            };
            match action {
                commands::daemon::DaemonAction::Start => {
                    // Fork happens HERE — before any tokio runtime exists.
                    let _pid_file = commands::daemon::handle_daemon_command(action, config.clone())?
                        .expect("Start always returns Some(PidFile)");
                    // Now start tokio runtime in the daemon process
                    let rt = tokio::runtime::Runtime::new()?;
                    rt.block_on(commands::gateway::run(config, vec![]))?;
                }
                _ => {
                    commands::daemon::handle_daemon_command(action, config)?;
                }
            }
        }
    }

    Ok(())
}
