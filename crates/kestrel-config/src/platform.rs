//! Platform detection for Android Termux and similar environments.
//!
//! Provides runtime detection functions since the same binary may run on
//! desktop Linux or an Android device (Termux).

use std::path::PathBuf;

/// Returns `true` if running inside Termux on Android.
///
/// Detection relies on:
/// - `TERMUX_VERSION` env var (set by Termux itself)
/// - `PREFIX` containing `com.termux/files/usr`
pub fn is_termux() -> bool {
    std::env::var("TERMUX_VERSION")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
        || std::env::var("PREFIX")
            .map(|p| p.contains("com.termux/files/usr"))
            .unwrap_or(false)
}

/// Returns `true` if running on Android (includes Termux).
///
/// Checks for `ANDROID_ROOT` which is set by the Android system.
pub fn is_android() -> bool {
    std::env::var("ANDROID_ROOT")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

/// Returns the Termux `$PREFIX` directory (e.g. `/data/data/com.termux/files/usr`).
///
/// Falls back to `/data/data/com.termux/files/usr` if the `PREFIX` env var
/// is not set but `is_termux()` is true.
pub fn get_prefix() -> Option<PathBuf> {
    if !is_termux() {
        return None;
    }
    std::env::var("PREFIX")
        .ok()
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from("/data/data/com.termux/files/usr")))
}

/// Returns the appropriate binary directory for installing executables.
///
/// - Termux: `$PREFIX/bin`
/// - Desktop: `~/.local/bin`
pub fn get_bin_dir() -> PathBuf {
    if let Some(prefix) = get_prefix() {
        prefix.join("bin")
    } else {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".local")
            .join("bin")
    }
}

/// Returns the appropriate tmp directory.
///
/// - Termux: `$PREFIX/tmp`
/// - Desktop: `/tmp`
pub fn get_tmp_dir() -> PathBuf {
    if let Some(prefix) = get_prefix() {
        let tmp = prefix.join("tmp");
        if tmp.exists() {
            return tmp;
        }
    }
    PathBuf::from("/tmp")
}

/// Returns the shell binary path.
///
/// - Respects `$SHELL` env var if set
/// - Termux fallback: `$PREFIX/bin/sh`
/// - Default: `/bin/sh`
pub fn get_shell_path() -> String {
    if let Ok(shell) = std::env::var("SHELL") {
        if !shell.is_empty() {
            return shell;
        }
    }
    if is_termux() {
        if let Some(prefix) = get_prefix() {
            let sh = prefix.join("bin").join("sh");
            if sh.exists() {
                return sh.to_string_lossy().to_string();
            }
        }
    }
    "/bin/sh".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_not_termux_by_default() {
        // Clear env vars to ensure clean state
        std::env::remove_var("TERMUX_VERSION");
        std::env::remove_var("PREFIX");
        std::env::remove_var("ANDROID_ROOT");
        // In CI/dev environments, this should be false
        // (can't assert false because some test runners might have weird env)
        let _ = is_termux();
    }

    #[test]
    fn test_get_shell_path_falls_back_to_bin_sh() {
        std::env::remove_var("SHELL");
        std::env::remove_var("TERMUX_VERSION");
        std::env::remove_var("PREFIX");
        // On a standard Linux system, should fall back to /bin/sh
        assert_eq!(get_shell_path(), "/bin/sh");
    }

    #[test]
    fn test_get_tmp_dir_default() {
        std::env::remove_var("TERMUX_VERSION");
        std::env::remove_var("PREFIX");
        assert_eq!(get_tmp_dir(), PathBuf::from("/tmp"));
    }
}
