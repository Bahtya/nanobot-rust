//! # kestrel
//!
//! A Rust rewrite of the Python kestrel AI agent framework.
//! Features an agent loop, channel system, bus message bus,
//! session management, cron scheduling, heartbeat, and security modules.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

mod commands;

/// Kestrel — multi-platform AI agent framework.
#[derive(Parser)]
#[command(name = "kestrel")]
#[command(version = kestrel_core::VERSION)]
#[command(about = "A multi-platform AI agent framework")]
struct Cli {
    /// Path to config file.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// Disable exec tool sandbox restrictions for trusted environments.
    #[arg(long, global = true, default_value_t = false)]
    dangerous: bool,

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
    Setup {
        #[command(subcommand)]
        subcommand: Option<SetupSubcommand>,
    },

    /// Show current configuration and status.
    Status,

    /// Run comprehensive system diagnostics.
    Doctor,

    /// Daemon management commands (Unix only).
    Daemon {
        #[command(subcommand)]
        subcommand: DaemonSubcommand,
    },

    /// Windows Service management commands (Windows only).
    #[cfg(windows)]
    Service {
        #[command(subcommand)]
        action: ServiceAction,
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
    /// Validate the config.toml schema.
    Validate,

    /// Migrate Python kestrel config to kestrel format.
    Migrate {
        /// Path to Python kestrel config directory (e.g., ~/.kestrel).
        #[arg(long)]
        from: PathBuf,

        /// Dry run: print resulting YAML to stdout instead of writing to file.
        #[arg(long)]
        dry_run: bool,
    },

    /// Import encrypted config from a URL.
    Import {
        /// URL to download the encrypted config file from.
        url: String,

        /// Password for decryption.
        #[arg(long)]
        password: String,
    },

    /// Export current config as an encrypted file.
    Export {
        /// Password for encryption.
        #[arg(long)]
        password: String,

        /// Output file path.
        #[arg(short, long, default_value = "config.toml.enc")]
        output: PathBuf,
    },
}

#[derive(Subcommand)]
enum SetupSubcommand {
    /// Run the WeChat iLink QR onboarding flow directly.
    Weixin,
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

/// Windows Service management actions (Windows only).
#[cfg(windows)]
#[derive(Subcommand)]
enum ServiceAction {
    /// Install kestrel as a Windows Service.
    Install,

    /// Uninstall the Windows Service.
    Uninstall,

    /// Run as a Windows Service (called by SCM internally).
    Run,
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

    if let Commands::Setup { subcommand } = &cli.command {
        return match subcommand {
            Some(SetupSubcommand::Weixin) => commands::setup_weixin::run(),
            None => commands::setup::run(kestrel_config::Config::default()),
        };
    }

    // Load configuration
    let config = kestrel_config::load_config(cli.config.as_deref())?;

    match cli.command {
        Commands::Agent { message } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(commands::agent::run(config, message, cli.dangerous))?;
        }
        Commands::Gateway { channels } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(commands::gateway::run(config, channels, cli.dangerous))?;
        }
        Commands::Serve { port } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(commands::serve::run(config, port, cli.dangerous))?;
        }
        Commands::Heartbeat => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(commands::heartbeat::run(config, cli.dangerous))?;
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
            ConfigSubcommand::Import { url, password } => {
                let rt = tokio::runtime::Runtime::new()?;
                rt.block_on(commands::config::import(&url, &password))?;
            }
            ConfigSubcommand::Export { password, output } => {
                commands::config::export(&config, &password, &output)?;
            }
        },
        Commands::Setup { .. } => unreachable!("setup is handled before config loading"),
        Commands::Status => {
            commands::status::run(&config)?;
        }
        Commands::Doctor => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(commands::doctor::run(&config))?;
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
                    let handles = commands::daemon::handle_daemon_command(action, config.clone())?
                        .expect("Start always returns Some(DaemonHandles)");

                    // Install SIGHUP ignore handler before tokio runtime — closes the
                    // window where default SIGHUP would kill the daemon during startup
                    #[cfg(target_family = "unix")]
                    kestrel_daemon::signal::install_early_sighup_handler();

                    // Now start tokio runtime in the daemon process
                    let rt = tokio::runtime::Runtime::new()?;
                    let result = rt.block_on(commands::gateway::run(config, vec![], cli.dangerous));

                    // Drop log_guard first to flush remaining log lines,
                    // then pid_file releases the flock and cleans up.
                    #[cfg(target_family = "unix")]
                    {
                        drop(handles.comm_log_guard);
                        drop(handles.log_guard);
                        if let Err(e) = handles.pid_file.clean() {
                            eprintln!("Failed to clean PID file: {e}");
                        }
                    }

                    result?;
                }
                _ => {
                    commands::daemon::handle_daemon_command(action, config)?;
                }
            }
        }
        #[cfg(windows)]
        Commands::Service { action } => {
            match action {
                ServiceAction::Install => {
                    kestrel_daemon::windows_service::install_service("kestrel", "Kestrel Agent")?;
                }
                ServiceAction::Uninstall => {
                    kestrel_daemon::windows_service::uninstall_service("kestrel")?;
                }
                ServiceAction::Run => {
                    kestrel_daemon::windows_service::run_as_service(move |ctx| {
                        ctx.report_running()?;

                        let rt = tokio::runtime::Runtime::new()?;
                        let gateway =
                            rt.spawn(commands::gateway::run(config, vec![], cli.dangerous));

                        // Block until SCM sends Stop/Shutdown
                        let _ = ctx.wait_for_shutdown();

                        // Cancel gateway and allow graceful shutdown
                        gateway.abort();
                        let _ = rt.shutdown_timeout(std::time::Duration::from_secs(10));

                        Ok(())
                    })?;
                }
            }
        }
    }

    Ok(())
}
