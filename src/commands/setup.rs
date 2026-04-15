//! Setup command — write the template config and initialize home directories.

use anyhow::{Context, Result};
use nanobot_config::Config;
use std::io::{self, BufRead, Write};
use std::path::Path;

/// Run the setup command.
pub fn run(_config: Config) -> Result<()> {
    let template = Config::default();
    let config_path = nanobot_config::paths::get_config_path()?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();

    run_with_io(&config_path, &template, &mut input, &mut output)
}

fn run_with_io<R: BufRead, W: Write>(
    config_path: &Path,
    template: &Config,
    input: &mut R,
    output: &mut W,
) -> Result<()> {
    writeln!(output, "=== Nanobot Setup ===\n")?;
    writeln!(output, "Template configuration:")?;
    writeln!(output, "  Model: {}", template.agent.model)?;
    writeln!(output, "  Temperature: {}", template.agent.temperature)?;
    writeln!(output, "  Max tokens: {}", template.agent.max_tokens)?;
    writeln!(output, "  Streaming: {}", template.agent.streaming)?;
    writeln!(output)?;

    if config_path.exists() && !confirm_overwrite(config_path, input, output)? {
        writeln!(
            output,
            "Keeping existing config at {}.",
            config_path.display()
        )?;
        return Ok(());
    }

    initialize_home(config_path, template)?;

    writeln!(
        output,
        "Saved template config to: {}",
        config_path.display()
    )?;
    writeln!(
        output,
        "Created default directories: skills, sessions, learning"
    )?;
    writeln!(output, "Setup complete.")?;

    Ok(())
}

fn confirm_overwrite<R: BufRead, W: Write>(
    config_path: &Path,
    input: &mut R,
    output: &mut W,
) -> Result<bool> {
    writeln!(
        output,
        "Config file already exists at {}.",
        config_path.display()
    )?;
    write!(output, "Overwrite with template config? [y/N]: ")?;
    output.flush()?;

    let mut answer = String::new();
    input.read_line(&mut answer)?;

    Ok(answer.trim().eq_ignore_ascii_case("y"))
}

fn initialize_home(config_path: &Path, template: &Config) -> Result<()> {
    let home = config_path
        .parent()
        .context("Config path must have a parent directory")?;

    std::fs::create_dir_all(home)
        .with_context(|| format!("Failed to create config home: {}", home.display()))?;
    nanobot_config::loader::save_config(template, config_path)?;

    for dir in ["skills", "sessions", "learning"] {
        let path = home.join(dir);
        std::fs::create_dir_all(&path)
            .with_context(|| format!("Failed to create directory: {}", path.display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template_yaml() -> String {
        serde_yaml::to_string(&Config::default()).unwrap()
    }

    #[test]
    fn setup_keeps_existing_config_when_user_declines_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.yaml");
        std::fs::write(&config_path, "agent:\n  model: existing-model\n").unwrap();

        let mut input = io::Cursor::new(b"n\n");
        let mut output = Vec::new();

        run_with_io(&config_path, &Config::default(), &mut input, &mut output).unwrap();

        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            "agent:\n  model: existing-model\n"
        );
        assert!(!tmp.path().join("skills").exists());

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Overwrite with template config? [y/N]:"));
        assert!(output.contains("Keeping existing config"));
    }

    #[test]
    fn setup_keeps_existing_config_when_user_presses_enter() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.yaml");
        std::fs::write(&config_path, "agent:\n  model: existing-model\n").unwrap();

        let mut input = io::Cursor::new(b"\n");
        let mut output = Vec::new();

        run_with_io(&config_path, &Config::default(), &mut input, &mut output).unwrap();

        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            "agent:\n  model: existing-model\n"
        );
        assert!(!tmp.path().join("sessions").exists());

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Keeping existing config"));
    }

    #[test]
    fn setup_overwrites_existing_config_and_creates_default_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.yaml");
        std::fs::write(&config_path, "agent:\n  model: existing-model\n").unwrap();

        let mut input = io::Cursor::new(b"y\n");
        let mut output = Vec::new();

        run_with_io(&config_path, &Config::default(), &mut input, &mut output).unwrap();

        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            template_yaml()
        );
        assert!(tmp.path().join("skills").is_dir());
        assert!(tmp.path().join("sessions").is_dir());
        assert!(tmp.path().join("learning").is_dir());

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Saved template config"));
    }

    #[test]
    fn setup_creates_template_config_and_directories_when_config_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.yaml");

        let mut input = io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();

        run_with_io(&config_path, &Config::default(), &mut input, &mut output).unwrap();

        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            template_yaml()
        );
        assert!(tmp.path().join("skills").is_dir());
        assert!(tmp.path().join("sessions").is_dir());
        assert!(tmp.path().join("learning").is_dir());

        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("Overwrite with template config? [y/N]:"));
        assert!(output.contains("Setup complete."));
    }
}
