//! Path resolution for kestrel data directories.
//!
//! Mirrors the Python config/paths.py module for data, media, cron, and workspace paths.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Get the kestrel home directory.
///
/// Priority: `KESTREL_HOME` env var > `~/.kestrel` default.
pub fn get_kestrel_home() -> Result<PathBuf> {
    if let Ok(home) = std::env::var("KESTREL_HOME") {
        Ok(PathBuf::from(home))
    } else {
        let home = dirs::home_dir().context("Could not determine home directory")?;
        Ok(home.join(".kestrel"))
    }
}

/// Get the data directory for the current instance.
pub fn get_data_dir() -> Result<PathBuf> {
    let home = get_kestrel_home()?;
    let data_dir = home.join("data");
    ensure_dir(&data_dir)?;
    Ok(data_dir)
}

/// Get the media storage directory.
pub fn get_media_dir(channel: Option<&str>) -> Result<PathBuf> {
    let home = get_kestrel_home()?;
    let media_dir = match channel {
        Some(ch) => home.join("media").join(ch),
        None => home.join("media"),
    };
    ensure_dir(&media_dir)?;
    Ok(media_dir)
}

/// Get the cron jobs storage directory.
pub fn get_cron_dir() -> Result<PathBuf> {
    let home = get_kestrel_home()?;
    let cron_dir = home.join("cron");
    ensure_dir(&cron_dir)?;
    Ok(cron_dir)
}

/// Get the sessions storage directory.
pub fn get_sessions_dir() -> Result<PathBuf> {
    let home = get_kestrel_home()?;
    let sessions_dir = home.join("sessions");
    ensure_dir(&sessions_dir)?;
    Ok(sessions_dir)
}

/// Get the config file path.
///
/// Default: `~/.kestrel/config.yaml`
pub fn get_config_path() -> Result<PathBuf> {
    let home = get_kestrel_home()?;
    Ok(home.join("config.yaml"))
}

/// Get the memory storage directory.
pub fn get_memory_dir() -> Result<PathBuf> {
    let home = get_kestrel_home()?;
    let memory_dir = home.join("memory");
    ensure_dir(&memory_dir)?;
    Ok(memory_dir)
}

/// Get the skills directory.
pub fn get_skills_dir() -> Result<PathBuf> {
    let home = get_kestrel_home()?;
    get_skills_dir_with_home(&home)
}

/// Get the skills directory using an explicit home path.
///
/// Creates the directory if it does not exist.
pub fn get_skills_dir_with_home(home: &Path) -> Result<PathBuf> {
    let skills_dir = home.join("skills");
    ensure_dir(&skills_dir)?;
    Ok(skills_dir)
}

/// Resolve the workspace path from config or default.
pub fn get_workspace_path(config_workspace: Option<&str>) -> Result<PathBuf> {
    match config_workspace {
        Some(ws) if !ws.is_empty() => Ok(PathBuf::from(ws)),
        _ => {
            let home = get_kestrel_home()?;
            Ok(home.join("workspace"))
        }
    }
}

/// Get the templates directory.
pub fn get_templates_dir() -> Result<PathBuf> {
    let home = get_kestrel_home()?;
    Ok(home.join("templates"))
}

/// Ensure a directory exists, creating it if necessary.
fn ensure_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path)
            .with_context(|| format!("Failed to create directory: {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kestrel_home_default() {
        std::env::remove_var("KESTREL_HOME");
        let home = get_kestrel_home().unwrap();
        assert!(home.to_string_lossy().ends_with(".kestrel"));
    }

    #[test]
    fn test_kestrel_home_env() {
        std::env::set_var("KESTREL_HOME", "/tmp/test-kestrel");
        let home = get_kestrel_home().unwrap();
        assert_eq!(home, PathBuf::from("/tmp/test-kestrel"));
        std::env::remove_var("KESTREL_HOME");
    }

    #[test]
    fn test_config_path_default() {
        std::env::set_var("KESTREL_HOME", "/tmp/test-kestrel-config");
        let path = get_config_path().unwrap();
        assert_eq!(path, PathBuf::from("/tmp/test-kestrel-config/config.yaml"));
        std::env::remove_var("KESTREL_HOME");
    }
}
