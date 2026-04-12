//! Config subcommand — validate config.yaml schema and migrate from Python.

use anyhow::Result;
use nanobot_config::Config;
use std::path::Path;
use tracing::info;

/// Run config validation.
pub fn validate(config: &Config) -> Result<()> {
    info!("Validating configuration...");

    let report = nanobot_config::validate(config);

    if report.is_empty() {
        println!("Configuration is valid. No issues found.");
        return Ok(());
    }

    let num_errors = report.errors().len();
    let num_warnings = report.warnings().len();

    let warnings = report.warnings();
    if !warnings.is_empty() {
        println!("Warnings ({}):", warnings.len());
        for w in &warnings {
            println!("  {}", w);
        }
    }

    let errors = report.errors();
    if !errors.is_empty() {
        println!("Errors ({}):", errors.len());
        for e in &errors {
            println!("  {}", e);
        }
        println!(
            "\nConfiguration has {} error(s). Fix them before running.",
            num_errors
        );
        std::process::exit(1);
    }

    println!(
        "\nConfiguration is valid with {} warning(s).",
        num_warnings
    );
    Ok(())
}

/// Run Python nanobot config migration.
pub fn migrate(from: &Path, dry_run: bool) -> Result<()> {
    info!("Migrating Python nanobot config from: {}", from.display());

    let result = nanobot_config::migrate_from_python(from)?;

    // Print migration report
    if !result.report.mapped.is_empty() {
        println!("Mapped fields ({}):", result.report.mapped.len());
        for field in &result.report.mapped {
            println!("  [OK] {}", field);
        }
    }

    if !result.report.notes.is_empty() {
        println!("\nNotes ({}):", result.report.notes.len());
        for note in &result.report.notes {
            println!("  [NOTE] {}", note);
        }
    }

    if !result.report.unmapped.is_empty() {
        println!("\nUnmapped fields ({}):", result.report.unmapped.len());
        for field in &result.report.unmapped {
            println!("  [SKIP] {}", field);
        }
    }

    if dry_run {
        println!("\n--- Generated config.yaml (dry run) ---\n");
        let yaml = serde_yaml::to_string(&result.config)?;
        println!("{}", yaml);
    } else {
        let config_path = nanobot_config::paths::get_config_path()?;
        println!("\nWriting config to: {}", config_path.display());
        nanobot_config::loader::save_config(&result.config, &config_path)?;
        println!("Migration complete.");
    }

    Ok(())
}
