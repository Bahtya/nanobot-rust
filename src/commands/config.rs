//! Config subcommand — validate config.yaml schema.

use anyhow::Result;
use nanobot_config::Config;
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
