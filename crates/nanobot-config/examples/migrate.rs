//! Standalone Python nanobot config migration tool.
//!
//! Reads a Python nanobot `config.json` (plus per-channel configs) and converts
//! it to nanobot-rs `config.yaml`.
//!
//! # Usage
//!
//! ```sh
//! # Dry run — print YAML to stdout
//! cargo run -p nanobot-config --example migrate -- --from ~/.nanobot --dry-run
//!
//! # Write to auto-detected config path (~/.nanobot-rs/config.yaml)
//! cargo run -p nanobot-config --example migrate -- --from ~/.nanobot
//!
//! # Write to a specific output file
//! cargo run -p nanobot-config --example migrate -- --from ~/.nanobot --output ./config.yaml
//! ```

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

/// Migrate Python nanobot config to nanobot-rs YAML format.
#[derive(Parser)]
#[command(name = "migrate", about = "Migrate Python nanobot config to nanobot-rs format")]
struct Cli {
    /// Path to Python nanobot config directory (e.g. ~/.nanobot).
    #[arg(long)]
    from: PathBuf,

    /// Output path for config.yaml.
    /// Defaults to auto-detected path (~/.nanobot-rs/config.yaml).
    #[arg(long)]
    output: Option<PathBuf>,

    /// Dry run: print YAML to stdout instead of writing to file.
    #[arg(long)]
    dry_run: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let from = &cli.from;
    anyhow::ensure!(
        from.exists(),
        "Source directory does not exist: {}",
        from.display()
    );

    eprintln!("Migrating Python nanobot config from: {}", from.display());

    let opts = nanobot_config::MigrationOptions {
        dry_run: cli.dry_run,
        output_file: cli.output.clone(),
        ..Default::default()
    };

    let result = nanobot_config::migrate_from_python(from, &opts)
        .context("Failed to migrate Python config")?;

    // Print migration report to stderr so stdout stays clean for --dry-run
    print_report(&result.report);

    let yaml = serde_yaml::to_string(&result.config)
        .context("Failed to serialize config to YAML")?;

    if cli.dry_run {
        println!("{}", yaml);
    } else {
        let output_path = match cli.output {
            Some(ref p) => p.clone(),
            None => nanobot_config::paths::get_config_path()?,
        };
        // Ensure parent directory exists
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
        }
        std::fs::write(&output_path, &yaml)
            .with_context(|| format!("Failed to write config to {}", output_path.display()))?;
        eprintln!("\nConfig written to: {}", output_path.display());
    }

    Ok(())
}

/// Print a human-readable migration report to stderr.
fn print_report(report: &nanobot_config::MigrationReport) {
    if !report.mapped.is_empty() {
        eprintln!("\nMapped fields ({}):", report.mapped.len());
        for field in &report.mapped {
            eprintln!("  [OK] {}", field);
        }
    }

    if !report.notes.is_empty() {
        eprintln!("\nNotes ({}):", report.notes.len());
        for note in &report.notes {
            eprintln!("  [NOTE] {}", note);
        }
    }

    if !report.unmapped.is_empty() {
        eprintln!("\nUnmapped fields ({}):", report.unmapped.len());
        for field in &report.unmapped {
            eprintln!("  [SKIP] {}", field);
        }
    }

    eprintln!(
        "\nSummary: {} mapped, {} unmapped, {} notes",
        report.mapped.len(),
        report.unmapped.len(),
        report.notes.len()
    );
}
