//! Platform detection for Android Termux and similar environments.
//!
//! Provides runtime detection functions since the same binary may run on
//! desktop Linux or an Android device (Termux).

use std::path::PathBuf;
use std::sync::OnceLock;

/// Fallback home directory on Termux if `dirs::home_dir()` returns `None`.
pub const TERMUX_HOME_FALLBACK: &str = "/data/data/com.termux/files/home";

/// Fallback Termux prefix if `$PREFIX` env var is not set.
const TERMUX_PREFIX_FALLBACK: &str = "/data/data/com.termux/files/usr";

/// Returns `true` if running inside Termux on Android.
///
/// Detection relies on:
/// - `TERMUX_VERSION` env var (set by Termux itself)
/// - `PREFIX` containing `com.termux/files/usr`
///
/// Result is cached after first call.
pub fn is_termux() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var("TERMUX_VERSION")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
            || std::env::var("PREFIX")
                .map(|p| p.contains("com.termux/files/usr"))
                .unwrap_or(false)
    })
}

/// Returns `true` if running on Android (includes Termux).
///
/// Checks for `ANDROID_ROOT` which is set by the Android system.
/// Result is cached after first call.
pub fn is_android() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var("ANDROID_ROOT")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    })
}

/// Returns the Termux `$PREFIX` directory (e.g. `/data/data/com.termux/files/usr`).
///
/// Falls back to `TERMUX_PREFIX_FALLBACK` if the `PREFIX` env var
/// is not set but `is_termux()` is true.
pub fn get_prefix() -> Option<PathBuf> {
    if !is_termux() {
        return None;
    }
    std::env::var("PREFIX")
        .ok()
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(TERMUX_PREFIX_FALLBACK)))
}

/// Returns a POSIX-compatible shell path for use in command wrappers.
///
/// Always returns a POSIX sh — either `/bin/sh` or `$PREFIX/bin/sh` on Termux.
/// This should be used for `ulimit` wrappers and other POSIX shell constructs,
/// NOT for interactive shell resolution (use `get_shell_path()` for that).
pub fn get_posix_sh() -> String {
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

/// Returns the appropriate binary directory for installing executables.
///
/// - Termux: `$PREFIX/bin`
/// - Desktop: `~/.local/bin`
pub fn get_bin_dir() -> Option<PathBuf> {
    if let Some(prefix) = get_prefix() {
        Some(prefix.join("bin"))
    } else {
        dirs::home_dir().map(|h| h.join(".local").join("bin"))
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

/// Returns the interactive shell path.
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
    fn test_constants_are_valid_paths() {
        assert!(TERMUX_HOME_FALLBACK.starts_with('/'));
        assert!(TERMUX_PREFIX_FALLBACK.starts_with('/'));
        assert!(TERMUX_HOME_FALLBACK.contains("termux"));
        assert!(TERMUX_PREFIX_FALLBACK.contains("termux"));
    }

    #[test]
    fn test_get_tmp_dir_default() {
        assert_eq!(get_tmp_dir(), PathBuf::from("/tmp"));
    }

    #[test]
    fn test_get_shell_path_is_nonempty() {
        let path = get_shell_path();
        assert!(!path.is_empty());
    }

    #[test]
    fn test_get_posix_sh_returns_posix_path() {
        let sh = get_posix_sh();
        assert!(!sh.is_empty());
        assert!(sh.ends_with("/sh"));
    }

    #[test]
    fn test_get_bin_dir_returns_some() {
        assert!(get_bin_dir().is_some());
    }
}
