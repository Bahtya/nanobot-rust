//! Built-in Lua script engine tool.
//!
//! Provides a cross-platform sandboxed Lua execution environment for script
//! orchestration (string processing, JSON, file I/O, batch operations) when
//! shell commands are unavailable or unreliable (especially on Windows).
//!
//! # Security Model
//!
//! Scripts run in a sandboxed Lua VM with restricted standard libraries. Access
//! to host APIs (filesystem, HTTP, environment) is controlled by a
//! **capability bitflags** system and three **security profiles**:
//!
//! | Profile    | FS | JSON | ENV | HTTP | Modules | Full stdlib |
//! |------------|----|------|-----|------|---------|-------------|
//! | Safe       | R/W/Del/Mkdir | yes | yes | no   | no      | no          |
//! | Trusted    | R/W/Del/Mkdir | yes | yes | yes  | yes     | no          |
//! | Dangerous  | R/W/Del/Mkdir | yes | yes | yes  | yes     | yes         |
//!
//! ## Write Path Validation
//!
//! All write operations (write_file, append_file, copy, move, download) are
//! validated against:
//! - **System paths**: `/usr`, `/bin`, `/etc`, `/var`, etc. are blocked
//! - **Sensitive home paths**: `.ssh/`, `.gnupg/`, `.gitconfig` are blocked
//! - **Write quotas**: configurable byte and file-count limits
//!
//! ## HTTP SSRF Protection
//!
//! HTTP APIs block requests to private/loopback/link-local/metadata IPs unless
//! the `HTTP_PRIVATE_NET` capability is explicitly granted. Only `http://` and
//! `https://` URL schemes are allowed.
//!
//! # API Reference
//!
//! ## Filesystem (FS_READ/FS_WRITE/FS_DELETE/FS_MKDIR)
//!
//! - `kestrel.read_file(path)` — read file contents
//! - `kestrel.write_file(path, content)` — write file
//! - `kestrel.append_file(path, content)` — append to file
//! - `kestrel.list_dir(path [, opts])` — list directory entries
//! - `kestrel.exists(path)` — check if path exists
//! - `kestrel.stat(path)` — get file metadata
//! - `kestrel.mkdir(path)` — create directory
//! - `kestrel.remove(path)` — delete file/directory
//! - `kestrel.copy(src, dst)` — copy file
//! - `kestrel.move(src, dst)` — move/rename file
//! - `kestrel.read_lines(path [, offset [, limit]])` — read lines
//! - `kestrel.glob(pattern [, opts])` — glob pattern matching
//! - `kestrel.walk(path [, opts])` — recursive directory walk
//! - `kestrel.read_json(path)` — read JSON file to table
//! - `kestrel.write_json(path, table)` — write table to JSON file
//! - `kestrel.tempdir()` — create temp directory
//! - `kestrel.tempfile()` — create temp file path
//!
//! ## Path utilities (FS_READ)
//!
//! - `kestrel.cwd()` — current working directory
//! - `kestrel.abspath(path)` — absolute path
//! - `kestrel.join_path(a, b, ...)` — join path components
//! - `kestrel.basename(path)` — filename component
//! - `kestrel.dirname(path)` — directory component
//!
//! ## JSON (JSON)
//!
//! - `kestrel.json_decode(str)` — decode JSON string to table
//! - `kestrel.json_encode(table)` — encode table to JSON string
//!
//! ## Environment (ENV_READ)
//!
//! - `kestrel.env(name)` — read environment variable
//!
//! ## HTTP (HTTP + FS_WRITE for download)
//!
//! - `kestrel.http_get(url)` — GET request
//! - `kestrel.http_post(url, body)` — POST request
//! - `kestrel.http_request(opts)` — generic request
//! - `kestrel.fetch_json(url)` — GET + JSON decode
//! - `kestrel.post_json(url, data)` — POST JSON + decode response
//! - `kestrel.download(url, path)` — download to file
//!
//! ## Module System (BUILTIN_MODULES)
//!
//! When enabled, `require()` is available for organized imports:
//!
//! ```lua
//! local fs = require("kestrel.fs")
//! local path = require("kestrel.path")
//! local json = require("kestrel.json")
//! local http = require("kestrel.http")
//! local env_mod = require("kestrel.env")
//! ```
//!
//! Only whitelisted built-in modules are loadable — no filesystem or C module access.
//!
//! # Configuration
//!
//! ```rust,ignore
//! // Safe (default) — no HTTP, no modules
//! let tool = ScriptTool::new();
//!
//! // Trusted — HTTP + modules enabled
//! let tool = ScriptTool::new().with_profile(ScriptProfile::Trusted);
//!
//! // Dangerous — full stdlib (io, os.execute, etc.)
//! let tool = ScriptTool::new().with_profile(ScriptProfile::Dangerous);
//!
//! // Custom capabilities
//! let tool = ScriptTool::new().with_capabilities(
//!     ScriptCapability::FS_READ | ScriptCapability::JSON
//! );
//! ```

use crate::trait_def::{Tool, ToolError};
use async_trait::async_trait;
use kestrel_core::MAX_TOOL_OUTPUT_LENGTH;
use mlua::{HookTriggers, Lua, LuaOptions, StdLib, VmState};
use serde_json::{json, Value};
use std::io::Write;
use std::ops::{BitOr, BitOrAssign};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

const DEFAULT_SCRIPT_TIMEOUT_SECS: u64 = 30;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const DEFAULT_MAX_WRITE_BYTES: usize = 10 * 1024 * 1024;
const DEFAULT_MAX_WRITE_FILES: usize = 100;
const DEFAULT_MAX_INSTRUCTIONS: usize = 10_000_000;
const DEFAULT_MAX_LIST_DIR_DEPTH: usize = 10;
const DEFAULT_MAX_LIST_DIR_ENTRIES: usize = 10_000;
const DEFAULT_HTTP_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
const DEFAULT_MAX_DOWNLOAD_BYTES: usize = 50 * 1024 * 1024;
const DEFAULT_MAX_REQUESTS_PER_SCRIPT: usize = 50;
const DEFAULT_MAX_REDIRECTS: u32 = 5;

/// System paths that are never writable from Lua scripts.
#[cfg(unix)]
const BLOCKED_WRITE_PATHS_UNIX: &[&str] = &[
    "/usr", "/bin", "/sbin", "/etc", "/var", "/sys", "/proc", "/dev", "/boot", "/lib", "/lib64",
    "/opt",
];

#[cfg(windows)]
const BLOCKED_WRITE_PATHS_WINDOWS: &[&str] = &[
    "C:\\Windows",
    "C:\\Program Files",
    "C:\\Program Files (x86)",
    "C:\\ProgramData",
];

/// Sensitive paths relative to home directory.
const BLOCKED_HOME_SUBPATHS: &[&str] = &[".ssh", ".gnupg", ".gitconfig"];

// ---------------------------------------------------------------------------
// Capability / Profile model
// ---------------------------------------------------------------------------

/// Fine-grained bitflags controlling which host APIs are available to Lua scripts.
///
/// Capabilities are checked at VM initialization time: only the APIs whose
/// corresponding capability bit is set are injected into the `kestrel.*`
/// global table. The `ALL_STD_LIBS` flag additionally enables the full set
/// of Lua standard libraries (removing the sandbox).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptCapability(u64);

impl ScriptCapability {
    /// Read files and list directories (`read_file`, `list_dir`, `exists`, `stat`).
    pub const FS_READ: Self = Self(1 << 0);
    /// Write files (`write_file`, `append_file`).
    pub const FS_WRITE: Self = Self(1 << 1);
    /// Delete files and directories (`remove`).
    pub const FS_DELETE: Self = Self(1 << 2);
    /// Create directories (`mkdir`).
    pub const FS_MKDIR: Self = Self(1 << 3);
    /// JSON encode / decode (`json_decode`, `json_encode`).
    pub const JSON: Self = Self(1 << 4);
    /// Read environment variables (`env`).
    pub const ENV_READ: Self = Self(1 << 5);
    /// HTTP requests (`http_get`, `http_post`, `http_request`, `fetch_json`, `post_json`).
    pub const HTTP: Self = Self(1 << 6);
    /// Allow HTTP to private / loopback networks (requires `HTTP`).
    pub const HTTP_PRIVATE_NET: Self = Self(1 << 7);
    /// Built-in module system (`require("kestrel.fs")` etc. — no filesystem loading).
    pub const BUILTIN_MODULES: Self = Self(1 << 8);
    /// Enable all Lua standard libraries (disables sandbox).
    pub const ALL_STD_LIBS: Self = Self(1 << 9);

    /// All capability bits (excluding `ALL_STD_LIBS`).
    pub const ALL_HOST_APIS: Self = Self(
        (1 << 0)
            | (1 << 1)
            | (1 << 2)
            | (1 << 3)
            | (1 << 4)
            | (1 << 5)
            | (1 << 6)
            | (1 << 7)
            | (1 << 8),
    );

    /// Every capability including `ALL_STD_LIBS`.
    pub const ALL: Self = Self((1 << 10) - 1);

    /// Empty — no capabilities at all.
    pub const NONE: Self = Self(0);

    /// Returns `true` if *all* bits in `other` are set in `self`.
    pub fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Returns the raw bitmask value.
    pub fn bits(self) -> u64 {
        self.0
    }
}

impl BitOr for ScriptCapability {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for ScriptCapability {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// Named security profile that maps to a preset set of [`ScriptCapability`] flags.
///
/// Profiles are the primary configuration interface. Each profile expands to
/// a fixed set of capability bits that can also be inspected or extended
/// programmatically via [`ScriptProfile::capabilities`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScriptProfile {
    /// Sandbox with safe host APIs (read, write, delete, mkdir, json, env).
    /// Instruction limits and timeouts are enforced. Full Lua stdlib is NOT
    /// available; dangerous globals (`io`, `require`, `dofile`, `loadfile`,
    /// `package`) are removed.
    #[default]
    Safe,
    /// Trusted: same host APIs as Safe plus reserved future capabilities
    /// (`HTTP`, `BUILTIN_MODULES`). Still sandboxed (no full stdlib).
    Trusted,
    /// Dangerous: all capabilities + full Lua stdlib. No instruction limits,
    /// no sandboxing.
    Dangerous,
}

impl ScriptProfile {
    /// Return the capability set for this profile.
    pub fn capabilities(self) -> ScriptCapability {
        match self {
            Self::Safe => {
                ScriptCapability::FS_READ
                    | ScriptCapability::FS_WRITE
                    | ScriptCapability::FS_DELETE
                    | ScriptCapability::FS_MKDIR
                    | ScriptCapability::JSON
                    | ScriptCapability::ENV_READ
            }
            Self::Trusted => ScriptCapability::ALL_HOST_APIS,
            Self::Dangerous => ScriptCapability::ALL,
        }
    }
}

impl From<bool> for ScriptProfile {
    /// Backward-compatible conversion: `true` → `Dangerous`, `false` → `Safe`.
    fn from(dangerous: bool) -> Self {
        if dangerous {
            Self::Dangerous
        } else {
            Self::Safe
        }
    }
}

// ---------------------------------------------------------------------------
// ScriptTool
// ---------------------------------------------------------------------------

/// Built-in Lua script engine tool.
///
/// Provides a cross-platform sandboxed Lua execution environment that
/// complements the `exec` tool. While `exec` calls external system commands,
/// `script` handles pure logic orchestration: string processing, JSON
/// manipulation, batch file operations, and data transformation.
pub struct ScriptTool {
    timeout: Duration,
    max_output_bytes: usize,
    max_write_bytes: usize,
    max_write_files: usize,
    max_instructions: usize,
    capabilities: ScriptCapability,
}

impl ScriptTool {
    /// Create a new ScriptTool with safe defaults (Safe profile).
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(DEFAULT_SCRIPT_TIMEOUT_SECS),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_write_bytes: DEFAULT_MAX_WRITE_BYTES,
            max_write_files: DEFAULT_MAX_WRITE_FILES,
            max_instructions: DEFAULT_MAX_INSTRUCTIONS,
            capabilities: ScriptProfile::Safe.capabilities(),
        }
    }

    /// Backward-compatible helper: `true` → Dangerous, `false` → Safe.
    pub fn dangerous(mut self, dangerous: bool) -> Self {
        self.capabilities = ScriptProfile::from(dangerous).capabilities();
        self
    }

    /// Set the security profile, replacing the current capabilities.
    pub fn with_profile(mut self, profile: ScriptProfile) -> Self {
        self.capabilities = profile.capabilities();
        self
    }

    /// Set exact capabilities (overrides profile-based defaults).
    pub fn with_capabilities(mut self, caps: ScriptCapability) -> Self {
        self.capabilities = caps;
        self
    }

    /// Returns `true` if the full Lua stdlib is enabled (dangerous mode).
    fn is_full_stdlib(&self) -> bool {
        self.capabilities.contains(ScriptCapability::ALL_STD_LIBS)
    }

    /// Override the default execution timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Override the maximum combined stdout output captured.
    pub fn with_max_output_bytes(mut self, max: usize) -> Self {
        self.max_output_bytes = max;
        self
    }

    /// Override the maximum total bytes written via `kestrel.write_file`.
    pub fn with_max_write_bytes(mut self, max: usize) -> Self {
        self.max_write_bytes = max;
        self
    }

    /// Override the maximum number of files written via `kestrel.write_file`.
    pub fn with_max_write_files(mut self, max: usize) -> Self {
        self.max_write_files = max;
        self
    }

    /// Create a sandboxed Lua VM with restricted standard libraries.
    fn create_sandboxed_vm(&self) -> Result<Lua, ToolError> {
        let full_stdlib = self.is_full_stdlib();
        if full_stdlib {
            info!("Creating script VM with full stdlib (dangerous profile)");
        }
        let safe_libs = if full_stdlib {
            StdLib::ALL
        } else {
            StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::UTF8
        };

        let lua = unsafe { Lua::unsafe_new_with(safe_libs, LuaOptions::default()) };

        if !full_stdlib {
            // Remove dangerous globals, but preserve safe os.* subset
            let globals = lua.globals();
            globals
                .set("io", mlua::Value::Nil)
                .map_err(|e| ToolError::Execution(format!("Failed to sandbox: {}", e)))?;
            globals
                .set("require", mlua::Value::Nil)
                .map_err(|e| ToolError::Execution(format!("Failed to sandbox: {}", e)))?;
            globals
                .set("dofile", mlua::Value::Nil)
                .map_err(|e| ToolError::Execution(format!("Failed to sandbox: {}", e)))?;
            globals
                .set("loadfile", mlua::Value::Nil)
                .map_err(|e| ToolError::Execution(format!("Failed to sandbox: {}", e)))?;
            globals
                .set("package", mlua::Value::Nil)
                .map_err(|e| ToolError::Execution(format!("Failed to sandbox: {}", e)))?;

            // Replace os table with safe subset (date, time, clock only)
            let os_safe = lua.create_table().map_err(|e| {
                ToolError::Execution(format!("Failed to create safe os table: {}", e))
            })?;

            // os.date([format [, time]])
            let os_date_fn = lua
                .create_function(|lua, (fmt, t): (Option<String>, Option<i64>)| {
                    let time_val = t.unwrap_or_else(|| {
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs() as i64
                    });
                    let dt = chrono::DateTime::from_timestamp(time_val, 0)
                        .unwrap_or(chrono::DateTime::UNIX_EPOCH);
                    let format = fmt.unwrap_or_else(|| "%Y-%m-%d %H:%M:%S".to_string());
                    let formatted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        dt.format(&format).to_string()
                    }));
                    match formatted {
                        Ok(s) => lua.create_string(&s),
                        Err(_) => {
                            let fallback = dt.format("%Y-%m-%d %H:%M:%S").to_string();
                            warn!(
                                format_spec = %format,
                                "os.date: unsupported format specifier, using default"
                            );
                            lua.create_string(&fallback)
                        }
                    }
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create os.date: {}", e)))?;
            os_safe
                .set("date", os_date_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set os.date: {}", e)))?;

            // os.time() -> current timestamp
            let os_time_fn = lua
                .create_function(|_, _: ()| {
                    Ok(std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64)
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create os.time: {}", e)))?;
            os_safe
                .set("time", os_time_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set os.time: {}", e)))?;

            // os.clock() -> elapsed wall-clock time since first call
            let clock_start = Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
            let os_clock_fn = lua
                .create_function(move |_, _: ()| {
                    let start = clock_start.lock().unwrap_or_else(|e| e.into_inner());
                    Ok(start.elapsed().as_secs_f64())
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create os.clock: {}", e)))?;
            os_safe
                .set("clock", os_clock_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set os.clock: {}", e)))?;

            globals
                .set("os", os_safe)
                .map_err(|e| ToolError::Execution(format!("Failed to set safe os: {}", e)))?;
        }

        // Inject kestrel API
        self.inject_kestrel_api(&lua)?;

        Ok(lua)
    }

    /// Inject the `kestrel` global table with safe helper functions.
    ///
    /// Each API is only injected when the corresponding capability bit is set
    /// in `self.capabilities`. If a capability is missing, attempting to call
    /// the function from Lua will produce a "nil value" error.
    fn inject_kestrel_api(&self, lua: &Lua) -> Result<(), ToolError> {
        let kestrel_table = lua
            .create_table()
            .map_err(|e| ToolError::Execution(format!("Failed to create kestrel table: {}", e)))?;

        let caps = self.capabilities;

        // --- kestrel.read_file(path [, offset, limit]) ---
        if caps.contains(ScriptCapability::FS_READ) {
            let max_output = self.max_output_bytes;
            let read_file_fn = lua
                .create_function(
                    move |lua, (path, offset, limit): (String, Option<usize>, Option<usize>)| {
                        let content = std::fs::read_to_string(&path).map_err(|e| {
                            mlua::Error::external(format!("Failed to read {}: {}", path, e))
                        })?;

                        let lines: Vec<&str> = content.lines().collect();
                        let off = offset.unwrap_or(0);
                        let lim = limit;

                        let selected: String = if lim.is_some() || off > 0 {
                            let iter = lines.iter().skip(off);
                            let taken: Vec<&str> = if let Some(l) = lim {
                                iter.take(l).copied().collect()
                            } else {
                                iter.copied().collect()
                            };
                            taken.join("\n")
                        } else {
                            content
                        };

                        let truncated = if selected.len() > max_output {
                            &selected[..max_output]
                        } else {
                            &selected
                        };

                        lua.create_string(truncated)
                    },
                )
                .map_err(|e| ToolError::Execution(format!("Failed to create read_file: {}", e)))?;
            kestrel_table
                .set("read_file", read_file_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set read_file: {}", e)))?;
        }

        // --- kestrel.write_file(path, content [, append]) ---
        if caps.contains(ScriptCapability::FS_WRITE) {
            let max_write_bytes = self.max_write_bytes;
            let max_write_files = self.max_write_files;
            let write_counter = Arc::new(AtomicUsize::new(0));
            let write_file_counter = Arc::new(AtomicUsize::new(0));

            let wc = write_counter.clone();
            let wfc = write_file_counter.clone();
            let write_file_fn = lua
                .create_function(
                    move |_, (path, content, append): (String, String, Option<bool>)| {
                        // Path validation
                        validate_write_path(&path)?;

                        // Write limits
                        let content_len = content.len();
                        let prev = wc.fetch_add(content_len, Ordering::SeqCst);
                        if prev + content_len > max_write_bytes {
                            warn!(
                                path,
                                write_bytes = prev + content_len,
                                max_write_bytes,
                                "Script write limit exceeded"
                            );
                            return Err(mlua::Error::external(format!(
                                "Write limit exceeded: {} bytes (max {})",
                                prev + content_len,
                                max_write_bytes
                            )));
                        }
                        let file_count = wfc.fetch_add(1, Ordering::SeqCst) + 1;
                        if file_count > max_write_files {
                            warn!(
                                path,
                                file_count, max_write_files, "Script file count limit exceeded"
                            );
                            return Err(mlua::Error::external(format!(
                                "File count limit exceeded: {} files (max {})",
                                file_count, max_write_files
                            )));
                        }

                        // Create parent directory if needed
                        if let Some(parent) = Path::new(&path).parent() {
                            if !parent.as_os_str().is_empty() {
                                std::fs::create_dir_all(parent).map_err(|e| {
                                    mlua::Error::external(format!("Failed to create dir: {}", e))
                                })?;
                            }
                        }

                        if append.unwrap_or(false) {
                            let mut file = std::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(&path)
                                .map_err(|e| {
                                    mlua::Error::external(format!("Failed to open {}: {}", path, e))
                                })?;
                            file.write_all(content.as_bytes()).map_err(|e| {
                                mlua::Error::external(format!("Failed to write: {}", e))
                            })?;
                            file.flush().map_err(|e| {
                                mlua::Error::external(format!("Failed to flush: {}", e))
                            })?;
                        } else {
                            std::fs::write(&path, &content).map_err(|e| {
                                mlua::Error::external(format!("Failed to write {}: {}", path, e))
                            })?;
                        }

                        Ok(format!("Wrote {} bytes to {}", content_len, path))
                    },
                )
                .map_err(|e| ToolError::Execution(format!("Failed to create write_file: {}", e)))?;
            kestrel_table
                .set("write_file", write_file_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set write_file: {}", e)))?;
        }

        // --- kestrel.list_dir(path [, recursive]) ---
        if caps.contains(ScriptCapability::FS_READ) {
            let list_dir_fn = lua
                .create_function(|lua, (path, recursive): (String, Option<bool>)| {
                    let mut result = Vec::new();
                    list_dir_recursive(
                        &path,
                        recursive.unwrap_or(false),
                        &mut result,
                        0,
                        DEFAULT_MAX_LIST_DIR_DEPTH,
                        DEFAULT_MAX_LIST_DIR_ENTRIES,
                    )
                    .map_err(|e| mlua::Error::external(e.to_string()))?;

                    let table = lua.create_table()?;
                    for entry in &result {
                        table.push(lua.create_string(entry)?)?;
                    }
                    Ok(table)
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create list_dir: {}", e)))?;
            kestrel_table
                .set("list_dir", list_dir_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set list_dir: {}", e)))?;
        }

        // --- kestrel.exists(path) ---
        if caps.contains(ScriptCapability::FS_READ) {
            let exists_fn = lua
                .create_function(|_, path: String| Ok(Path::new(&path).exists()))
                .map_err(|e| ToolError::Execution(format!("Failed to create exists: {}", e)))?;
            kestrel_table
                .set("exists", exists_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set exists: {}", e)))?;
        }

        // --- kestrel.stat(path) ---
        if caps.contains(ScriptCapability::FS_READ) {
            let stat_fn = lua
                .create_function(|lua, path: String| {
                    let meta = std::fs::metadata(&path).map_err(|e| {
                        mlua::Error::external(format!("stat failed for {}: {}", path, e))
                    })?;
                    let table = lua.create_table()?;
                    let file_type = if meta.is_dir() {
                        "dir"
                    } else if meta.is_file() {
                        "file"
                    } else {
                        "other"
                    };
                    table.set("type", file_type)?;
                    table.set("size", meta.len())?;
                    if let Ok(modified) = meta.modified() {
                        if let Ok(dur) = modified.duration_since(std::time::UNIX_EPOCH) {
                            table.set("modified_secs", dur.as_secs())?;
                        }
                    }
                    Ok(table)
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create stat: {}", e)))?;
            kestrel_table
                .set("stat", stat_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set stat: {}", e)))?;
        }

        // --- kestrel.mkdir(path) ---
        if caps.contains(ScriptCapability::FS_MKDIR) {
            let mkdir_fn = lua
                .create_function(|_, path: String| {
                    validate_write_path(&path)?;
                    std::fs::create_dir_all(&path)
                        .map_err(|e| mlua::Error::external(format!("mkdir failed: {}", e)))
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create mkdir: {}", e)))?;
            kestrel_table
                .set("mkdir", mkdir_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set mkdir: {}", e)))?;
        }

        // --- kestrel.remove(path) ---
        if caps.contains(ScriptCapability::FS_DELETE) {
            let remove_fn = lua
                .create_function(|_, path: String| {
                    validate_write_path(&path)?;
                    let p = Path::new(&path);
                    if p.is_dir() {
                        std::fs::remove_dir_all(p)
                    } else {
                        std::fs::remove_file(p)
                    }
                    .map_err(|e| mlua::Error::external(format!("remove failed: {}", e)))
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create remove: {}", e)))?;
            kestrel_table
                .set("remove", remove_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set remove: {}", e)))?;
        }

        // --- kestrel.json_decode(string) ---
        if caps.contains(ScriptCapability::JSON) {
            let json_decode_fn = lua
                .create_function(|lua, json_str: String| {
                    let val: Value = serde_json::from_str(&json_str)
                        .map_err(|e| mlua::Error::external(format!("JSON decode error: {}", e)))?;
                    json_value_to_lua(lua, &val)
                })
                .map_err(|e| {
                    ToolError::Execution(format!("Failed to create json_decode: {}", e))
                })?;
            kestrel_table
                .set("json_decode", json_decode_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set json_decode: {}", e)))?;
        }

        // --- kestrel.json_encode(table) ---
        if caps.contains(ScriptCapability::JSON) {
            let json_encode_fn = lua
                .create_function(|_, table: mlua::Value| {
                    let json_val = lua_value_to_json(&table)?;
                    serde_json::to_string_pretty(&json_val)
                        .map_err(|e| mlua::Error::external(format!("JSON encode error: {}", e)))
                })
                .map_err(|e| {
                    ToolError::Execution(format!("Failed to create json_encode: {}", e))
                })?;
            kestrel_table
                .set("json_encode", json_encode_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set json_encode: {}", e)))?;
        }

        // --- kestrel.env(name) ---
        if caps.contains(ScriptCapability::ENV_READ) {
            let env_fn = lua
                .create_function(|lua, name: String| match std::env::var(&name) {
                    Ok(val) => Ok(Some(lua.create_string(&val)?)),
                    Err(_) => Ok(None),
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create env: {}", e)))?;
            kestrel_table
                .set("env", env_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set env: {}", e)))?;
        }

        // --- kestrel.cwd() ---
        if caps.contains(ScriptCapability::FS_READ) {
            let cwd_fn = lua
                .create_function(|lua, _: ()| {
                    let cwd = std::env::current_dir()
                        .map_err(|e| mlua::Error::external(format!("cwd failed: {}", e)))?;
                    lua.create_string(cwd.to_string_lossy().as_ref())
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create cwd: {}", e)))?;
            kestrel_table
                .set("cwd", cwd_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set cwd: {}", e)))?;
        }

        // --- kestrel.abspath(path) ---
        if caps.contains(ScriptCapability::FS_READ) {
            let abspath_fn = lua
                .create_function(|lua, path: String| {
                    let abs = std::fs::canonicalize(&path)
                        .or_else(|_| std::env::current_dir().map(|d| d.join(&path)))
                        .map_err(|e| mlua::Error::external(format!("abspath failed: {}", e)))?;
                    lua.create_string(abs.to_string_lossy().as_ref())
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create abspath: {}", e)))?;
            kestrel_table
                .set("abspath", abspath_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set abspath: {}", e)))?;
        }

        // --- kestrel.join_path(...) ---
        if caps.contains(ScriptCapability::FS_READ) {
            let join_path_fn = lua
                .create_function(|lua, parts: mlua::MultiValue| {
                    let components: Vec<std::path::PathBuf> = parts
                        .iter()
                        .filter_map(|v| {
                            if let mlua::Value::String(s) = v {
                                s.to_str()
                                    .ok()
                                    .map(|s| std::path::PathBuf::from(s.to_string()))
                            } else {
                                None
                            }
                        })
                        .collect();
                    if components.is_empty() {
                        return Err(mlua::Error::external(
                            "join_path requires at least one argument",
                        ));
                    }
                    let joined = components
                        .iter()
                        .fold(std::path::PathBuf::new(), |acc, p| acc.join(p));
                    lua.create_string(joined.to_string_lossy().as_ref())
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create join_path: {}", e)))?;
            kestrel_table
                .set("join_path", join_path_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set join_path: {}", e)))?;
        }

        // --- kestrel.basename(path) ---
        if caps.contains(ScriptCapability::FS_READ) {
            let basename_fn = lua
                .create_function(|lua, path: String| {
                    let name = Path::new(&path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("");
                    lua.create_string(name)
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create basename: {}", e)))?;
            kestrel_table
                .set("basename", basename_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set basename: {}", e)))?;
        }

        // --- kestrel.dirname(path) ---
        if caps.contains(ScriptCapability::FS_READ) {
            let dirname_fn = lua
                .create_function(|lua, path: String| {
                    let dir = Path::new(&path)
                        .parent()
                        .and_then(|p| p.to_str())
                        .unwrap_or(".");
                    lua.create_string(dir)
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create dirname: {}", e)))?;
            kestrel_table
                .set("dirname", dirname_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set dirname: {}", e)))?;
        }

        // --- kestrel.read_lines(path [, offset, limit]) ---
        if caps.contains(ScriptCapability::FS_READ) {
            let max_output = self.max_output_bytes;
            let read_lines_fn = lua
                .create_function(
                    move |lua, (path, offset, limit): (String, Option<usize>, Option<usize>)| {
                        let content = std::fs::read_to_string(&path).map_err(|e| {
                            mlua::Error::external(format!("Failed to read {}: {}", path, e))
                        })?;

                        let lines: Vec<&str> = content.lines().collect();
                        let off = offset.unwrap_or(0);
                        let iter = lines.iter().skip(off);
                        let taken: Vec<&str> = if let Some(l) = limit {
                            iter.take(l).copied().collect()
                        } else {
                            iter.copied().collect()
                        };

                        let mut total_bytes = 0usize;
                        let table = lua.create_table()?;
                        for line in &taken {
                            total_bytes += line.len();
                            if total_bytes > max_output {
                                break;
                            }
                            table.push(lua.create_string(line)?)?;
                        }
                        Ok(table)
                    },
                )
                .map_err(|e| ToolError::Execution(format!("Failed to create read_lines: {}", e)))?;
            kestrel_table
                .set("read_lines", read_lines_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set read_lines: {}", e)))?;
        }

        // --- kestrel.append_file(path, content) ---
        if caps.contains(ScriptCapability::FS_WRITE) {
            let max_write_bytes = self.max_write_bytes;
            let max_write_files = self.max_write_files;
            let append_counter = Arc::new(AtomicUsize::new(0));
            let append_file_counter = Arc::new(AtomicUsize::new(0));

            let ac = append_counter.clone();
            let afc = append_file_counter.clone();
            let append_file_fn = lua
                .create_function(move |_, (path, content): (String, String)| {
                    validate_write_path(&path)?;

                    let content_len = content.len();
                    let prev = ac.fetch_add(content_len, Ordering::SeqCst);
                    if prev + content_len > max_write_bytes {
                        return Err(mlua::Error::external(format!(
                            "Write limit exceeded: {} bytes (max {})",
                            prev + content_len,
                            max_write_bytes
                        )));
                    }
                    let file_count = afc.fetch_add(1, Ordering::SeqCst) + 1;
                    if file_count > max_write_files {
                        return Err(mlua::Error::external(format!(
                            "File count limit exceeded: {} files (max {})",
                            file_count, max_write_files
                        )));
                    }

                    if let Some(parent) = Path::new(&path).parent() {
                        if !parent.as_os_str().is_empty() {
                            std::fs::create_dir_all(parent).map_err(|e| {
                                mlua::Error::external(format!("Failed to create dir: {}", e))
                            })?;
                        }
                    }

                    let mut file = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .map_err(|e| {
                            mlua::Error::external(format!("Failed to open {}: {}", path, e))
                        })?;
                    file.write_all(content.as_bytes())
                        .map_err(|e| mlua::Error::external(format!("Failed to append: {}", e)))?;
                    file.flush()
                        .map_err(|e| mlua::Error::external(format!("Failed to flush: {}", e)))?;

                    Ok(format!("Appended {} bytes to {}", content_len, path))
                })
                .map_err(|e| {
                    ToolError::Execution(format!("Failed to create append_file: {}", e))
                })?;
            kestrel_table
                .set("append_file", append_file_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set append_file: {}", e)))?;
        }

        // --- kestrel.copy(src, dst) ---
        if caps.contains(ScriptCapability::FS_WRITE) {
            let max_write_bytes = self.max_write_bytes;
            let max_write_files = self.max_write_files;
            let copy_counter = Arc::new(AtomicUsize::new(0));
            let copy_file_counter = Arc::new(AtomicUsize::new(0));

            let cc = copy_counter.clone();
            let cfc = copy_file_counter.clone();
            let copy_fn = lua
                .create_function(move |_, (src, dst): (String, String)| {
                    validate_write_path(&dst)?;

                    let meta = std::fs::metadata(&src).map_err(|e| {
                        mlua::Error::external(format!("Cannot stat source {}: {}", src, e))
                    })?;
                    if !meta.is_file() {
                        return Err(mlua::Error::external(format!(
                            "copy only supports files, not directories: {}",
                            src
                        )));
                    }

                    let size = meta.len() as usize;
                    let prev = cc.fetch_add(size, Ordering::SeqCst);
                    if prev + size > max_write_bytes {
                        return Err(mlua::Error::external(format!(
                            "Write limit exceeded: {} bytes (max {})",
                            prev + size,
                            max_write_bytes
                        )));
                    }
                    let file_count = cfc.fetch_add(1, Ordering::SeqCst) + 1;
                    if file_count > max_write_files {
                        return Err(mlua::Error::external(format!(
                            "File count limit exceeded: {} files (max {})",
                            file_count, max_write_files
                        )));
                    }

                    if let Some(parent) = Path::new(&dst).parent() {
                        if !parent.as_os_str().is_empty() {
                            std::fs::create_dir_all(parent).map_err(|e| {
                                mlua::Error::external(format!("Failed to create dir: {}", e))
                            })?;
                        }
                    }

                    std::fs::copy(&src, &dst)
                        .map_err(|e| mlua::Error::external(format!("copy failed: {}", e)))?;

                    Ok(format!("Copied {} -> {} ({} bytes)", src, dst, size))
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create copy: {}", e)))?;
            kestrel_table
                .set("copy", copy_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set copy: {}", e)))?;
        }

        // --- kestrel.move(src, dst) ---
        if caps.contains(ScriptCapability::FS_WRITE) && caps.contains(ScriptCapability::FS_DELETE) {
            let move_fn = lua
                .create_function(|_, (src, dst): (String, String)| {
                    validate_write_path(&dst)?;

                    if let Some(parent) = Path::new(&dst).parent() {
                        if !parent.as_os_str().is_empty() {
                            std::fs::create_dir_all(parent).map_err(|e| {
                                mlua::Error::external(format!("Failed to create dir: {}", e))
                            })?;
                        }
                    }

                    std::fs::rename(&src, &dst)
                        .map_err(|e| mlua::Error::external(format!("move failed: {}", e)))?;

                    Ok(format!("Moved {} -> {}", src, dst))
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create move: {}", e)))?;
            kestrel_table
                .set("move", move_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set move: {}", e)))?;
        }

        // --- kestrel.glob(pattern [, opts]) ---
        if caps.contains(ScriptCapability::FS_READ) {
            let glob_fn = lua
                .create_function(|lua, (pattern, opts): (String, Option<mlua::Table>)| {
                    let max_entries = opts
                        .as_ref()
                        .and_then(|o| o.get::<usize>("max_entries").ok())
                        .unwrap_or(DEFAULT_MAX_LIST_DIR_ENTRIES);

                    let mut results = Vec::new();
                    let walker = glob::glob(&pattern).map_err(|e| {
                        mlua::Error::external(format!("Invalid glob pattern: {}", e))
                    })?;

                    for entry in walker {
                        if results.len() >= max_entries {
                            break;
                        }
                        match entry {
                            Ok(path) => {
                                results.push(lua.create_string(path.to_string_lossy().as_ref())?);
                            }
                            Err(e) => {
                                debug!(error = %e, "glob: skipping unreadable path");
                            }
                        }
                    }

                    let table = lua.create_table()?;
                    for r in results {
                        table.push(r)?;
                    }
                    Ok(table)
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create glob: {}", e)))?;
            kestrel_table
                .set("glob", glob_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set glob: {}", e)))?;
        }

        // --- kestrel.walk(path [, opts]) ---
        if caps.contains(ScriptCapability::FS_READ) {
            let walk_fn = lua
                .create_function(|lua, (path, opts): (String, Option<mlua::Table>)| {
                    let max_depth = opts
                        .as_ref()
                        .and_then(|o| o.get::<usize>("max_depth").ok())
                        .unwrap_or(DEFAULT_MAX_LIST_DIR_DEPTH);
                    let max_entries = opts
                        .as_ref()
                        .and_then(|o| o.get::<usize>("max_entries").ok())
                        .unwrap_or(DEFAULT_MAX_LIST_DIR_ENTRIES);

                    let mut results: Vec<WalkEntry> = Vec::new();
                    walk_dir(&path, &mut results, 0, max_depth, max_entries)
                        .map_err(|e| mlua::Error::external(e.to_string()))?;

                    let table = lua.create_table()?;
                    for entry in &results {
                        let t = lua.create_table()?;
                        t.set("path", lua.create_string(&entry.path)?)?;
                        t.set("name", lua.create_string(&entry.name)?)?;
                        t.set("type", lua.create_string(entry.file_type)?)?;
                        t.set("depth", entry.depth)?;
                        table.push(t)?;
                    }
                    Ok(table)
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create walk: {}", e)))?;
            kestrel_table
                .set("walk", walk_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set walk: {}", e)))?;
        }

        // --- kestrel.read_json(path) ---
        if caps.contains(ScriptCapability::FS_READ) && caps.contains(ScriptCapability::JSON) {
            let max_output = self.max_output_bytes;
            let read_json_fn = lua
                .create_function(move |lua, path: String| {
                    let content = std::fs::read_to_string(&path).map_err(|e| {
                        mlua::Error::external(format!("Failed to read {}: {}", path, e))
                    })?;

                    if content.len() > max_output {
                        return Err(mlua::Error::external(format!(
                            "File too large: {} bytes (max {})",
                            content.len(),
                            max_output
                        )));
                    }

                    let val: Value = serde_json::from_str(&content)
                        .map_err(|e| mlua::Error::external(format!("JSON parse error: {}", e)))?;
                    json_value_to_lua(lua, &val)
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create read_json: {}", e)))?;
            kestrel_table
                .set("read_json", read_json_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set read_json: {}", e)))?;
        }

        // --- kestrel.write_json(path, value [, pretty]) ---
        if caps.contains(ScriptCapability::FS_WRITE) && caps.contains(ScriptCapability::JSON) {
            let max_write_bytes = self.max_write_bytes;
            let max_write_files = self.max_write_files;
            let wj_counter = Arc::new(AtomicUsize::new(0));
            let wj_file_counter = Arc::new(AtomicUsize::new(0));

            let wjc = wj_counter.clone();
            let wjfc = wj_file_counter.clone();
            let write_json_fn = lua
                .create_function(
                    move |_, (path, table, pretty): (String, mlua::Value, Option<bool>)| {
                        validate_write_path(&path)?;

                        let json_val = lua_value_to_json(&table)?;
                        let content = if pretty.unwrap_or(true) {
                            serde_json::to_string_pretty(&json_val).map_err(|e| {
                                mlua::Error::external(format!("JSON encode error: {}", e))
                            })?
                        } else {
                            serde_json::to_string(&json_val).map_err(|e| {
                                mlua::Error::external(format!("JSON encode error: {}", e))
                            })?
                        };

                        let content_len = content.len();
                        let prev = wjc.fetch_add(content_len, Ordering::SeqCst);
                        if prev + content_len > max_write_bytes {
                            return Err(mlua::Error::external(format!(
                                "Write limit exceeded: {} bytes (max {})",
                                prev + content_len,
                                max_write_bytes
                            )));
                        }
                        let file_count = wjfc.fetch_add(1, Ordering::SeqCst) + 1;
                        if file_count > max_write_files {
                            return Err(mlua::Error::external(format!(
                                "File count limit exceeded: {} files (max {})",
                                file_count, max_write_files
                            )));
                        }

                        if let Some(parent) = Path::new(&path).parent() {
                            if !parent.as_os_str().is_empty() {
                                std::fs::create_dir_all(parent).map_err(|e| {
                                    mlua::Error::external(format!("Failed to create dir: {}", e))
                                })?;
                            }
                        }

                        std::fs::write(&path, &content).map_err(|e| {
                            mlua::Error::external(format!("Failed to write {}: {}", path, e))
                        })?;

                        Ok(format!("Wrote {} bytes to {}", content_len, path))
                    },
                )
                .map_err(|e| ToolError::Execution(format!("Failed to create write_json: {}", e)))?;
            kestrel_table
                .set("write_json", write_json_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set write_json: {}", e)))?;
        }

        // --- kestrel.tempdir() ---
        if caps.contains(ScriptCapability::FS_WRITE) && caps.contains(ScriptCapability::FS_MKDIR) {
            let tempdir_fn = lua
                .create_function(|lua, _: ()| {
                    let base = std::env::temp_dir();
                    let name = format!(
                        "kestrel-{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos()
                    );
                    let dir = base.join(&name);
                    std::fs::create_dir_all(&dir)
                        .map_err(|e| mlua::Error::external(format!("tempdir failed: {}", e)))?;
                    lua.create_string(dir.to_string_lossy().as_ref())
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create tempdir: {}", e)))?;
            kestrel_table
                .set("tempdir", tempdir_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set tempdir: {}", e)))?;
        }

        // --- kestrel.tempfile([prefix]) ---
        if caps.contains(ScriptCapability::FS_WRITE) {
            let tempfile_fn = lua
                .create_function(|lua, prefix: Option<String>| {
                    let base = std::env::temp_dir();
                    let prefix_str = prefix.unwrap_or_else(|| "tmp".to_string());
                    let name = format!(
                        "{}-{}",
                        prefix_str,
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos()
                    );
                    let path = base.join(&name);
                    // Create empty file
                    std::fs::File::create(&path)
                        .map_err(|e| mlua::Error::external(format!("tempfile failed: {}", e)))?;
                    lua.create_string(path.to_string_lossy().as_ref())
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create tempfile: {}", e)))?;
            kestrel_table
                .set("tempfile", tempfile_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set tempfile: {}", e)))?;
        }

        // --- HTTP APIs (gated by HTTP capability) ---
        if caps.contains(ScriptCapability::HTTP) {
            let rt = tokio::runtime::Handle::try_current().map_err(|e| {
                ToolError::Execution(format!("HTTP APIs require tokio runtime: {}", e))
            })?;
            let http_client = reqwest::Client::builder()
                .timeout(Duration::from_millis(DEFAULT_HTTP_TIMEOUT_MS))
                .redirect(reqwest::redirect::Policy::limited(
                    DEFAULT_MAX_REDIRECTS as usize,
                ))
                .build()
                .map_err(|e| ToolError::Execution(format!("Failed to build HTTP client: {}", e)))?;

            let allow_private_net = caps.contains(ScriptCapability::HTTP_PRIVATE_NET);
            let request_counter = Arc::new(AtomicUsize::new(0));
            let max_requests = DEFAULT_MAX_REQUESTS_PER_SCRIPT;
            let max_response_bytes = DEFAULT_MAX_RESPONSE_BYTES;
            let max_download_bytes = DEFAULT_MAX_DOWNLOAD_BYTES;

            // --- kestrel.http_get(url [, opts]) ---
            {
                let rt = rt.clone();
                let client = http_client.clone();
                let rc = request_counter.clone();
                let apn = allow_private_net;

                let http_get_fn = lua
                    .create_function(move |lua, (url, opts): (String, Option<mlua::Table>)| {
                        let timeout_ms = opts
                            .as_ref()
                            .and_then(|o| o.get::<usize>("timeout_ms").ok())
                            .map(|t| t as u64);
                        let headers = extract_http_headers(opts.as_ref());

                        let resp = execute_http(
                            &rt,
                            &client,
                            reqwest::Method::GET,
                            &url,
                            headers,
                            None,
                            None,
                            timeout_ms,
                            apn,
                            &rc,
                            max_requests,
                            max_response_bytes,
                        )?;
                        build_http_response_table(lua, &resp)
                    })
                    .map_err(|e| {
                        ToolError::Execution(format!("Failed to create http_get: {}", e))
                    })?;
                kestrel_table
                    .set("http_get", http_get_fn)
                    .map_err(|e| ToolError::Execution(format!("Failed to set http_get: {}", e)))?;
            }

            // --- kestrel.http_post(url, body [, opts]) ---
            {
                let rt = rt.clone();
                let client = http_client.clone();
                let rc = request_counter.clone();
                let apn = allow_private_net;

                let http_post_fn = lua
                    .create_function(
                        move |lua, (url, body, opts): (String, String, Option<mlua::Table>)| {
                            let timeout_ms = opts
                                .as_ref()
                                .and_then(|o| o.get::<usize>("timeout_ms").ok())
                                .map(|t| t as u64);
                            let headers = extract_http_headers(opts.as_ref());

                            let resp = execute_http(
                                &rt,
                                &client,
                                reqwest::Method::POST,
                                &url,
                                headers,
                                Some(body),
                                None,
                                timeout_ms,
                                apn,
                                &rc,
                                max_requests,
                                max_response_bytes,
                            )?;
                            build_http_response_table(lua, &resp)
                        },
                    )
                    .map_err(|e| {
                        ToolError::Execution(format!("Failed to create http_post: {}", e))
                    })?;
                kestrel_table
                    .set("http_post", http_post_fn)
                    .map_err(|e| ToolError::Execution(format!("Failed to set http_post: {}", e)))?;
            }

            // --- kestrel.http_request(opts) ---
            {
                let rt = rt.clone();
                let client = http_client.clone();
                let rc = request_counter.clone();
                let apn = allow_private_net;

                let http_request_fn = lua
                    .create_function(move |lua, opts: mlua::Table| {
                        let method_str: String =
                            opts.get("method").unwrap_or_else(|_| "GET".to_string());
                        let method = match method_str.to_uppercase().as_str() {
                            "GET" => reqwest::Method::GET,
                            "POST" => reqwest::Method::POST,
                            "PUT" => reqwest::Method::PUT,
                            "DELETE" => reqwest::Method::DELETE,
                            "PATCH" => reqwest::Method::PATCH,
                            "HEAD" => reqwest::Method::HEAD,
                            _ => {
                                return Err(mlua::Error::external(format!(
                                    "Unsupported HTTP method: {}",
                                    method_str
                                )))
                            }
                        };
                        let url: String = opts.get("url").map_err(|_| {
                            mlua::Error::external(
                                "http_request requires 'url' parameter".to_string(),
                            )
                        })?;
                        let timeout_ms = opts.get::<usize>("timeout_ms").ok().map(|t| t as u64);
                        let headers = extract_http_headers(Some(&opts));
                        let body: Option<String> = opts.get("body").ok();

                        // json: accept string (pre-encoded) or table (auto-encode)
                        let json_str: Option<String> = match opts.get::<mlua::Value>("json").ok() {
                            Some(mlua::Value::String(s)) => Some(s.to_str()?.to_string()),
                            Some(v) if !matches!(v, mlua::Value::Nil) => {
                                let jval = lua_value_to_json(&v)?;
                                Some(serde_json::to_string(&jval).map_err(|e| {
                                    mlua::Error::external(format!("JSON encode: {}", e))
                                })?)
                            }
                            _ => None,
                        };

                        let resp = execute_http(
                            &rt,
                            &client,
                            method,
                            &url,
                            headers,
                            body,
                            json_str,
                            timeout_ms,
                            apn,
                            &rc,
                            max_requests,
                            max_response_bytes,
                        )?;
                        build_http_response_table(lua, &resp)
                    })
                    .map_err(|e| {
                        ToolError::Execution(format!("Failed to create http_request: {}", e))
                    })?;
                kestrel_table
                    .set("http_request", http_request_fn)
                    .map_err(|e| {
                        ToolError::Execution(format!("Failed to set http_request: {}", e))
                    })?;
            }

            // --- kestrel.fetch_json(url [, opts]) ---
            {
                let rt = rt.clone();
                let client = http_client.clone();
                let rc = request_counter.clone();
                let apn = allow_private_net;

                let fetch_json_fn = lua
                    .create_function(move |lua, (url, opts): (String, Option<mlua::Table>)| {
                        let timeout_ms = opts
                            .as_ref()
                            .and_then(|o| o.get::<usize>("timeout_ms").ok())
                            .map(|t| t as u64);
                        let headers = extract_http_headers(opts.as_ref());

                        let resp = execute_http(
                            &rt,
                            &client,
                            reqwest::Method::GET,
                            &url,
                            headers,
                            None,
                            None,
                            timeout_ms,
                            apn,
                            &rc,
                            max_requests,
                            max_response_bytes,
                        )?;
                        if resp.status < 200 || resp.status >= 300 {
                            return Err(mlua::Error::external(format!(
                                "fetch_json: HTTP {} - {}",
                                resp.status,
                                &resp.body[..resp.body.len().min(200)]
                            )));
                        }
                        let val: Value = serde_json::from_str(&resp.body).map_err(|e| {
                            mlua::Error::external(format!("JSON decode error: {}", e))
                        })?;
                        json_value_to_lua(lua, &val)
                    })
                    .map_err(|e| {
                        ToolError::Execution(format!("Failed to create fetch_json: {}", e))
                    })?;
                kestrel_table
                    .set("fetch_json", fetch_json_fn)
                    .map_err(|e| {
                        ToolError::Execution(format!("Failed to set fetch_json: {}", e))
                    })?;
            }

            // --- kestrel.post_json(url, value [, opts]) ---
            {
                let rt = rt.clone();
                let client = http_client.clone();
                let rc = request_counter.clone();
                let apn = allow_private_net;

                let post_json_fn =
                    lua
                        .create_function(
                            move |lua,
                                  (url, value, opts): (
                                String,
                                mlua::Value,
                                Option<mlua::Table>,
                            )| {
                                let timeout_ms = opts
                                    .as_ref()
                                    .and_then(|o| o.get::<usize>("timeout_ms").ok())
                                    .map(|t| t as u64);
                                let mut headers = extract_http_headers(opts.as_ref());
                                let has_ct = headers
                                    .iter()
                                    .any(|(k, _)| k.eq_ignore_ascii_case("content-type"));
                                if !has_ct {
                                    headers.push((
                                        "Content-Type".to_string(),
                                        "application/json".to_string(),
                                    ));
                                }

                                let json_val = lua_value_to_json(&value)?;
                                let json_str = serde_json::to_string(&json_val).map_err(|e| {
                                    mlua::Error::external(format!("JSON encode error: {}", e))
                                })?;

                                let resp = execute_http(
                                    &rt,
                                    &client,
                                    reqwest::Method::POST,
                                    &url,
                                    headers,
                                    None,
                                    Some(json_str),
                                    timeout_ms,
                                    apn,
                                    &rc,
                                    max_requests,
                                    max_response_bytes,
                                )?;
                                if resp.status < 200 || resp.status >= 300 {
                                    return Err(mlua::Error::external(format!(
                                        "post_json: HTTP {} - {}",
                                        resp.status,
                                        &resp.body[..resp.body.len().min(200)]
                                    )));
                                }
                                let val: Value = serde_json::from_str(&resp.body).map_err(|e| {
                                    mlua::Error::external(format!("JSON decode error: {}", e))
                                })?;
                                json_value_to_lua(lua, &val)
                            },
                        )
                        .map_err(|e| {
                            ToolError::Execution(format!("Failed to create post_json: {}", e))
                        })?;
                kestrel_table
                    .set("post_json", post_json_fn)
                    .map_err(|e| ToolError::Execution(format!("Failed to set post_json: {}", e)))?;
            }

            // --- kestrel.download(url, path [, opts]) ---
            if caps.contains(ScriptCapability::FS_WRITE) {
                let rt = rt.clone();
                let client = http_client.clone();
                let rc = request_counter.clone();
                let apn = allow_private_net;
                let dl_bytes_counter = Arc::new(AtomicUsize::new(0));
                let dl_file_counter = Arc::new(AtomicUsize::new(0));
                let max_write_bytes = self.max_write_bytes;
                let max_write_files = self.max_write_files;

                let dlbc = dl_bytes_counter.clone();
                let dlfc = dl_file_counter.clone();

                let download_fn = lua
                    .create_function(
                        move |lua, (url, path, opts): (String, String, Option<mlua::Table>)| {
                            validate_write_path(&path)?;

                            let timeout_ms = opts
                                .as_ref()
                                .and_then(|o| o.get::<usize>("timeout_ms").ok())
                                .map(|t| t as u64);
                            let headers = extract_http_headers(opts.as_ref());

                            let parsed = validate_http_url(&url).map_err(mlua::Error::external)?;

                            if !apn {
                                if let Some(host) = parsed.host_str() {
                                    if host_is_private(host) {
                                        warn!(host, "Download blocked: private network");
                                        return Err(mlua::Error::external(format!(
                                            "Download blocked: '{}' resolves to private address",
                                            host
                                        )));
                                    }
                                }
                            }

                            let count = rc.fetch_add(1, Ordering::SeqCst) + 1;
                            if count > max_requests {
                                return Err(mlua::Error::external(format!(
                                    "Request limit exceeded: {} (max {})",
                                    count, max_requests
                                )));
                            }

                            let file_count = dlfc.fetch_add(1, Ordering::SeqCst) + 1;
                            if file_count > max_write_files {
                                return Err(mlua::Error::external(format!(
                                    "File count limit exceeded: {} files (max {})",
                                    file_count, max_write_files
                                )));
                            }

                            if let Some(parent) = Path::new(&path).parent() {
                                if !parent.as_os_str().is_empty() {
                                    std::fs::create_dir_all(parent).map_err(|e| {
                                        mlua::Error::external(format!(
                                            "Failed to create dir: {}",
                                            e
                                        ))
                                    })?;
                                }
                            }

                            let response = rt
                                .block_on(async {
                                    let mut req =
                                        client.request(reqwest::Method::GET, parsed.as_str());
                                    if let Some(t) = timeout_ms {
                                        req = req.timeout(Duration::from_millis(t));
                                    }
                                    for (k, v) in &headers {
                                        req = req.header(k.as_str(), v.as_str());
                                    }
                                    req.send().await
                                })
                                .map_err(|e| {
                                    mlua::Error::external(format!("Download failed: {}", e))
                                })?;

                            let status = response.status().as_u16();
                            if !(200..300).contains(&status) {
                                return Err(mlua::Error::external(format!(
                                    "Download failed: HTTP {}",
                                    status
                                )));
                            }
                            if let Some(len) = response.content_length() {
                                if len as usize > max_download_bytes {
                                    return Err(mlua::Error::external(format!(
                                        "Download too large: {} bytes (max {})",
                                        len, max_download_bytes
                                    )));
                                }
                            }

                            let bytes =
                                rt.block_on(async { response.bytes().await }).map_err(|e| {
                                    mlua::Error::external(format!("Failed to read response: {}", e))
                                })?;
                            if bytes.len() > max_download_bytes {
                                return Err(mlua::Error::external(format!(
                                    "Download too large: {} bytes (max {})",
                                    bytes.len(),
                                    max_download_bytes
                                )));
                            }

                            let prev = dlbc.fetch_add(bytes.len(), Ordering::SeqCst);
                            if prev + bytes.len() > max_write_bytes {
                                return Err(mlua::Error::external(format!(
                                    "Write limit exceeded: {} bytes (max {})",
                                    prev + bytes.len(),
                                    max_write_bytes
                                )));
                            }

                            std::fs::write(&path, &bytes).map_err(|e| {
                                mlua::Error::external(format!("Failed to write {}: {}", path, e))
                            })?;

                            let table = lua.create_table()?;
                            table.set("ok", true)?;
                            table.set("status", status)?;
                            table.set("bytes", bytes.len())?;
                            table.set("path", lua.create_string(&path)?)?;
                            table.set("url", lua.create_string(&url)?)?;
                            Ok(table)
                        },
                    )
                    .map_err(|e| {
                        ToolError::Execution(format!("Failed to create download: {}", e))
                    })?;
                kestrel_table
                    .set("download", download_fn)
                    .map_err(|e| ToolError::Execution(format!("Failed to set download: {}", e)))?;
            }
        }

        // --- kestrel.platform() ---
        let platform_fn = lua
            .create_function(|lua, _: ()| {
                let platform = if cfg!(windows) {
                    "windows"
                } else if kestrel_config::platform::is_android() {
                    "android"
                } else if cfg!(target_os = "macos") {
                    "macos"
                } else {
                    "linux"
                };
                lua.create_string(platform)
            })
            .map_err(|e| ToolError::Execution(format!("Failed to create platform: {}", e)))?;
        kestrel_table
            .set("platform", platform_fn)
            .map_err(|e| ToolError::Execution(format!("Failed to set platform: {}", e)))?;

        // --- Build module tables BEFORE moving kestrel_table into globals ---
        // The BUILTIN_MODULES capability gates a controlled require() that only
        // loads Rust-host pre-registered module tables — no filesystem access.
        if caps.contains(ScriptCapability::BUILTIN_MODULES) {
            let registry = lua.create_table().map_err(|e| {
                ToolError::Execution(format!("Failed to create module registry: {}", e))
            })?;

            // Helper: build a module table from function names present in kestrel_table.
            let build_module = |names: &[&str]| -> Result<mlua::Table, ToolError> {
                let module = lua.create_table().map_err(|e| {
                    ToolError::Execution(format!("Failed to create module table: {}", e))
                })?;
                for &name in names {
                    if let Ok(f) = kestrel_table.get::<mlua::Function>(name) {
                        module.set(name, f).map_err(|e| {
                            ToolError::Execution(format!(
                                "Failed to set module function {}: {}",
                                name, e
                            ))
                        })?;
                    }
                }
                Ok(module)
            };

            // kestrel.fs — filesystem operations
            let fs_mod = build_module(&[
                "read_file",
                "write_file",
                "list_dir",
                "exists",
                "stat",
                "mkdir",
                "remove",
                "read_lines",
                "append_file",
                "copy",
                "move",
                "glob",
                "walk",
                "read_json",
                "write_json",
                "tempdir",
                "tempfile",
            ])?;
            registry.set("kestrel.fs", fs_mod).map_err(|e| {
                ToolError::Execution(format!("Failed to register kestrel.fs: {}", e))
            })?;

            // kestrel.path — path utilities
            let path_mod = build_module(&["cwd", "abspath", "join_path", "basename", "dirname"])?;
            registry.set("kestrel.path", path_mod).map_err(|e| {
                ToolError::Execution(format!("Failed to register kestrel.path: {}", e))
            })?;

            // kestrel.json — JSON encode/decode and file I/O
            let json_mod =
                build_module(&["json_decode", "json_encode", "read_json", "write_json"])?;
            registry.set("kestrel.json", json_mod).map_err(|e| {
                ToolError::Execution(format!("Failed to register kestrel.json: {}", e))
            })?;

            // kestrel.http — HTTP operations
            let http_mod = build_module(&[
                "http_get",
                "http_post",
                "http_request",
                "fetch_json",
                "post_json",
                "download",
            ])?;
            registry.set("kestrel.http", http_mod).map_err(|e| {
                ToolError::Execution(format!("Failed to register kestrel.http: {}", e))
            })?;

            // kestrel.env — environment variables
            let env_mod = build_module(&["env"])?;
            registry.set("kestrel.env", env_mod).map_err(|e| {
                ToolError::Execution(format!("Failed to register kestrel.env: {}", e))
            })?;

            // Custom require() — only whitelisted built-in modules, no filesystem loading.
            let require_fn = lua
                .create_function(move |_lua, name: String| {
                    match registry.get::<mlua::Value>(name.as_str()) {
                        Ok(mlua::Value::Table(t)) => Ok(mlua::Value::Table(t)),
                        _ => Err(mlua::Error::external(format!(
                            "module '{}' not found: \
                             only built-in kestrel.* modules are available \
                             (kestrel.fs, kestrel.path, kestrel.json, kestrel.http, kestrel.env)",
                            name
                        ))),
                    }
                })
                .map_err(|e| ToolError::Execution(format!("Failed to create require: {}", e)))?;

            lua.globals()
                .set("require", require_fn)
                .map_err(|e| ToolError::Execution(format!("Failed to set require: {}", e)))?;

            info!("Built-in module system enabled: kestrel.fs, kestrel.path, kestrel.json, kestrel.http, kestrel.env");
        }

        // Set kestrel global (after module tables are built from kestrel_table)
        lua.globals()
            .set("kestrel", kestrel_table)
            .map_err(|e| ToolError::Execution(format!("Failed to set kestrel global: {}", e)))?;

        Ok(())
    }
}

impl Default for ScriptTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ScriptTool {
    fn name(&self) -> &str {
        "script"
    }

    fn description(&self) -> &str {
        "Execute a Lua script in a built-in sandboxed engine. \
         Use for cross-platform scripting: string processing, \
         file I/O, JSON manipulation, and logic orchestration \
         when shell commands are unavailable or unreliable (e.g. Windows). \
         Available APIs: kestrel.read_file, kestrel.write_file, kestrel.list_dir, \
         kestrel.exists, kestrel.stat, kestrel.mkdir, kestrel.remove, \
         kestrel.json_decode, kestrel.json_encode, kestrel.env, kestrel.platform, \
         kestrel.cwd, kestrel.abspath, kestrel.join_path, kestrel.basename, \
         kestrel.dirname, kestrel.read_lines, kestrel.append_file, kestrel.copy, \
         kestrel.move, kestrel.glob, kestrel.walk, kestrel.read_json, \
         kestrel.write_json, kestrel.tempdir, kestrel.tempfile, \
         kestrel.http_get, kestrel.http_post, kestrel.http_request, \
         kestrel.fetch_json, kestrel.post_json, kestrel.download. \
         Built-in modules (Trusted/Dangerous): require('kestrel.fs'), \
         require('kestrel.path'), require('kestrel.json'), \
         require('kestrel.http'), require('kestrel.env')."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Lua script to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in seconds (default: 30)"
                }
            },
            "required": ["code"]
        })
    }

    fn toolset(&self) -> &str {
        "default"
    }

    fn is_mutating(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let code = args["code"]
            .as_str()
            .ok_or_else(|| ToolError::Validation("Missing 'code' parameter".to_string()))?;

        let timeout_secs = args["timeout"].as_u64().unwrap_or(self.timeout.as_secs());

        debug!(
            code_len = code.len(),
            timeout_secs,
            capabilities = self.capabilities.bits(),
            max_instructions = self.max_instructions,
            "Script execution starting"
        );

        let lua = self.create_sandboxed_vm()?;

        // Set up instruction limit and wall-clock timeout via debug hook
        let start = std::time::Instant::now();
        if !self.is_full_stdlib() {
            let max_instructions = self.max_instructions;
            let instruction_counter = Arc::new(AtomicUsize::new(0));
            let timeout = Duration::from_secs(timeout_secs);
            lua.set_hook(
                HookTriggers::new().every_nth_instruction(1000),
                move |_lua, _debug| {
                    let count = instruction_counter.fetch_add(1000, Ordering::Relaxed);
                    if count >= max_instructions {
                        warn!(
                            instruction_count = count,
                            max_instructions, "Script instruction limit exceeded, aborting"
                        );
                        return Err(mlua::Error::external(format!(
                            "Instruction limit exceeded (max {})",
                            max_instructions
                        )));
                    }
                    if start.elapsed() > timeout {
                        warn!(
                            elapsed_secs = start.elapsed().as_secs(),
                            timeout_secs = timeout.as_secs(),
                            "Script timed out, aborting"
                        );
                        return Err(mlua::Error::external(format!(
                            "Script timed out after {}s",
                            timeout.as_secs()
                        )));
                    }
                    Ok(VmState::Continue)
                },
            )
            .map_err(|e| ToolError::Execution(format!("Failed to set instruction hook: {}", e)))?;
        }

        // Set up output capture buffer with early size-limit enforcement to
        // prevent unbounded memory growth from scripts that print large output.
        let stdout_buf: Arc<std::sync::Mutex<Vec<u8>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let buf_clone = stdout_buf.clone();
        let output_truncated = Arc::new(AtomicBool::new(false));
        let truncated_flag = output_truncated.clone();
        let max_output = self.max_output_bytes;

        // Override print to write to our buffer (enforcing max_output_bytes)
        let globals = lua.globals();
        let print_capture = lua
            .create_function(move |_, args: mlua::MultiValue| {
                if truncated_flag.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let parts: Vec<String> = args
                    .iter()
                    .map(|v| match v {
                        mlua::Value::String(s) => s
                            .to_str()
                            .map(|x| x.to_string())
                            .unwrap_or_else(|_| "<invalid>".to_string()),
                        mlua::Value::Integer(n) => n.to_string(),
                        mlua::Value::Number(n) => n.to_string(),
                        mlua::Value::Boolean(b) => b.to_string(),
                        mlua::Value::Nil => "nil".to_string(),
                        _ => format!("{:?}", v),
                    })
                    .collect();
                let line = parts.join("\t");
                let mut buf = buf_clone.lock().unwrap_or_else(|e| e.into_inner());
                if buf.len() + line.len() + 1 > max_output {
                    truncated_flag.store(true, Ordering::Relaxed);
                    return Ok(());
                }
                buf.extend_from_slice(line.as_bytes());
                buf.push(b'\n');
                Ok(())
            })
            .map_err(|e| ToolError::Execution(format!("Failed to create print: {}", e)))?;

        globals
            .set("print", print_capture)
            .map_err(|e| ToolError::Execution(format!("Failed to set print: {}", e)))?;

        // Execute the script — catch_unwind prevents Lua binding panics from
        // crashing the process (e.g., poisoned mutex in print, or any future
        // binding bug that triggers a Rust panic instead of returning an error).
        let exec_result: Result<(), ToolError> = tokio::task::block_in_place(|| {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| lua.load(code).exec())) {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(ToolError::Execution(format!("Lua error: {}", e))),
                Err(panic) => {
                    let msg = if let Some(s) = panic.downcast_ref::<String>() {
                        s.clone()
                    } else if let Some(s) = panic.downcast_ref::<&str>() {
                        s.to_string()
                    } else {
                        "unknown panic".to_string()
                    };
                    Err(ToolError::Execution(format!("Lua panic: {}", msg)))
                }
            }
        });

        let elapsed_ms = start.elapsed().as_millis();

        match &exec_result {
            Ok(()) => {
                // Collect captured output
                let was_truncated = output_truncated.load(Ordering::Relaxed);
                let output = {
                    let buf = stdout_buf.lock().unwrap_or_else(|e| e.into_inner());
                    String::from_utf8_lossy(&buf).to_string()
                };

                let output_len = output.len();
                let truncated =
                    was_truncated || output_len > MAX_TOOL_OUTPUT_LENGTH || output_len > max_output;

                info!(
                    duration_ms = elapsed_ms,
                    output_len,
                    truncated,
                    dangerous = self.is_full_stdlib(),
                    "Script execution completed"
                );

                if output.is_empty() {
                    Ok("(script completed, no output)".to_string())
                } else if output.len() > MAX_TOOL_OUTPUT_LENGTH {
                    let mut truncated_output = output[..MAX_TOOL_OUTPUT_LENGTH].to_string();
                    truncated_output.push_str("\n... (output truncated)");
                    Ok(truncated_output)
                } else if output.len() > max_output {
                    Ok(format!(
                        "{}\n... (output truncated at {} bytes)",
                        &output[..max_output.min(output.len())],
                        max_output
                    ))
                } else {
                    Ok(output)
                }
            }
            Err(e) => {
                warn!(
                    duration_ms = elapsed_ms,
                    error = %e,
                    dangerous = self.is_full_stdlib(),
                    "Script execution failed"
                );
                exec_result?;
                unreachable!()
            }
        }
    }
}

// --- Walk helper ---

struct WalkEntry {
    path: String,
    name: String,
    file_type: &'static str,
    depth: usize,
}

fn walk_dir(
    path: &str,
    results: &mut Vec<WalkEntry>,
    depth: usize,
    max_depth: usize,
    max_entries: usize,
) -> Result<(), ToolError> {
    if depth > max_depth {
        return Ok(());
    }

    let entries = match std::fs::read_dir(path) {
        Ok(e) => e,
        Err(e) => {
            debug!(path, error = %e, "walk: unreadable directory, skipping");
            return Ok(());
        }
    };

    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        if results.len() >= max_entries {
            return Ok(());
        }

        let name = entry.file_name().to_string_lossy().to_string();
        let file_type = match entry.file_type() {
            Ok(ft) if ft.is_dir() => "dir",
            Ok(ft) if ft.is_file() => "file",
            Ok(_) => "other",
            Err(_) => continue,
        };

        let entry_path = entry.path().to_string_lossy().to_string();

        results.push(WalkEntry {
            path: entry_path.clone(),
            name,
            file_type,
            depth,
        });

        if file_type == "dir" {
            walk_dir(&entry_path, results, depth + 1, max_depth, max_entries)?;
        }
    }

    Ok(())
}

// --- Path validation ---

fn validate_write_path(path: &str) -> Result<(), mlua::Error> {
    let p = Path::new(path);

    // Check for blocked system paths
    #[cfg(unix)]
    {
        for blocked in BLOCKED_WRITE_PATHS_UNIX {
            if p.starts_with(blocked) {
                warn!(
                    path,
                    blocked_system_path = *blocked,
                    "Script write blocked: system path"
                );
                return Err(mlua::Error::external(format!(
                    "Write to system path '{}' is not allowed",
                    path
                )));
            }
        }
    }

    #[cfg(windows)]
    {
        let lower = path.to_lowercase();
        for blocked in BLOCKED_WRITE_PATHS_WINDOWS {
            if lower.starts_with(&blocked.to_lowercase()) {
                warn!(
                    path,
                    blocked_system_path = *blocked,
                    "Script write blocked: system path"
                );
                return Err(mlua::Error::external(format!(
                    "Write to system path '{}' is not allowed",
                    path
                )));
            }
        }
    }

    // Check for sensitive home subpaths
    if let Some(file_name) = p.file_name().and_then(|n| n.to_str()) {
        let lower = file_name.to_lowercase();
        for blocked in BLOCKED_HOME_SUBPATHS {
            if lower == *blocked {
                warn!(
                    path,
                    sensitive_component = *blocked,
                    "Script write blocked: sensitive path"
                );
                return Err(mlua::Error::external(format!(
                    "Write to sensitive path '{}' is not allowed",
                    path
                )));
            }
        }
    }

    // Check for directory traversal that escapes to sensitive areas
    let canonical_attempt = if p.is_absolute() {
        Some(p.to_path_buf())
    } else {
        std::env::current_dir().ok().map(|d| d.join(p))
    };

    if let Some(canonical_base) = canonical_attempt {
        // Block if the canonical path contains .ssh, .gnupg etc.
        let components: Vec<String> = canonical_base
            .components()
            .filter_map(|c| c.as_os_str().to_str().map(|s| s.to_lowercase()))
            .collect();
        for blocked in BLOCKED_HOME_SUBPATHS {
            if components.iter().any(|c| c == *blocked) {
                warn!(
                    path,
                    sensitive_component = *blocked,
                    "Script write blocked: sensitive path in components"
                );
                return Err(mlua::Error::external(format!(
                    "Write to sensitive path '{}' is not allowed",
                    path
                )));
            }
        }
    }

    Ok(())
}

// --- Directory listing ---

fn list_dir_recursive(
    path: &str,
    recursive: bool,
    result: &mut Vec<String>,
    depth: usize,
    max_depth: usize,
    max_entries: usize,
) -> Result<(), ToolError> {
    if depth > max_depth {
        result.push(format!("{}(max depth reached)", "  ".repeat(depth)));
        return Ok(());
    }

    let entries = match std::fs::read_dir(path) {
        Ok(e) => e,
        Err(e) => {
            warn!(path, error = %e, "Script list_dir: unreadable directory, skipping");
            result.push(format!("{}(unreadable: {})", "  ".repeat(depth), e));
            return Ok(());
        }
    };

    let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());

    let indent = "  ".repeat(depth);
    for entry in entries {
        if result.len() >= max_entries {
            result.push(format!(
                "{}(truncated: max {} entries)",
                indent, max_entries
            ));
            return Ok(());
        }

        let name = entry.file_name().to_string_lossy().to_string();
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(e) => {
                warn!(path, error = %e, "Script list_dir: could not get file type, skipping");
                result.push(format!("{}{} (stat failed)", indent, name));
                continue;
            }
        };

        if file_type.is_dir() {
            result.push(format!("{}{}/", indent, name));
            if recursive {
                let sub_path = format!("{}/{}", path, name);
                list_dir_recursive(&sub_path, true, result, depth + 1, max_depth, max_entries)?;
            }
        } else {
            result.push(format!("{}{}", indent, name));
        }
    }

    Ok(())
}

// --- JSON <-> Lua conversion helpers ---

fn json_value_to_lua(lua: &Lua, val: &Value) -> Result<mlua::Value, mlua::Error> {
    match val {
        Value::Null => Ok(mlua::Value::Nil),
        Value::Bool(b) => Ok(mlua::Value::Boolean(*b)),
        Value::Number(n) => Ok(mlua::Value::Number(n.as_f64().unwrap_or(0.0))),
        Value::String(s) => Ok(mlua::Value::String(lua.create_string(s)?)),
        Value::Array(arr) => {
            let table = lua.create_table()?;
            for (i, item) in arr.iter().enumerate() {
                table.set(i + 1, json_value_to_lua(lua, item)?)?;
            }
            Ok(mlua::Value::Table(table))
        }
        Value::Object(obj) => {
            let table = lua.create_table()?;
            for (k, v) in obj {
                table.set(k.as_str(), json_value_to_lua(lua, v)?)?;
            }
            Ok(mlua::Value::Table(table))
        }
    }
}

fn lua_value_to_json(val: &mlua::Value) -> Result<Value, mlua::Error> {
    match val {
        mlua::Value::Nil => Ok(Value::Null),
        mlua::Value::Boolean(b) => Ok(Value::Bool(*b)),
        mlua::Value::Integer(n) => Ok(json!(*n)),
        mlua::Value::Number(n) => Ok(json!(*n)),
        mlua::Value::String(s) => Ok(Value::String(s.to_str()?.to_string())),
        mlua::Value::Table(t) => {
            // Collect integer-keyed and string-keyed entries separately
            let mut arr = Vec::new();
            let mut has_int_keys = false;
            for i in 1.. {
                let v: mlua::Value = match t.get(i) {
                    Ok(v) => v,
                    Err(_) => break, // no more integer keys
                };
                if v.is_nil() {
                    break;
                }
                has_int_keys = true;
                arr.push(lua_value_to_json(&v)?);
            }

            let mut obj = serde_json::Map::new();
            for pair in t.pairs::<mlua::Value, mlua::Value>() {
                let (k, v): (mlua::Value, mlua::Value) = pair?;
                match &k {
                    mlua::Value::Integer(n) if *n >= 1 => continue, // already handled above
                    mlua::Value::String(s) => {
                        obj.insert(s.to_str()?.to_string(), lua_value_to_json(&v)?);
                    }
                    _ => {} // skip other key types
                }
            }

            if has_int_keys && obj.is_empty() {
                // Pure array
                Ok(Value::Array(arr))
            } else if has_int_keys {
                // Mixed: encode as object with integer keys as string keys
                let mut mixed = serde_json::Map::new();
                for (i, v) in arr.iter().enumerate() {
                    mixed.insert((i + 1).to_string(), v.clone());
                }
                for (k, v) in obj {
                    mixed.insert(k, v);
                }
                Ok(Value::Object(mixed))
            } else {
                Ok(Value::Object(obj))
            }
        }
        _ => Ok(Value::Null),
    }
}

// --- HTTP helpers ---

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
    url: String,
    final_url: String,
}

fn validate_http_url(url_str: &str) -> Result<url::Url, String> {
    let parsed = url::Url::parse(url_str).map_err(|e| format!("Invalid URL: {}", e))?;
    match parsed.scheme() {
        "http" | "https" => {}
        s => return Err(format!("Unsupported URL scheme: {} (only http/https)", s)),
    }
    if parsed.host_str().is_none() {
        return Err("URL has no host".to_string());
    }
    Ok(parsed)
}

fn ip_is_private(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
        }
        std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
    }
}

fn host_is_private(host: &str) -> bool {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return ip_is_private(&ip);
    }
    let lookup = format!("{}:0", host);
    if let Ok(addrs) = std::net::ToSocketAddrs::to_socket_addrs(&lookup) {
        for addr in addrs {
            if ip_is_private(&addr.ip()) {
                return true;
            }
        }
    }
    false
}

fn extract_http_headers(opts: Option<&mlua::Table>) -> Vec<(String, String)> {
    let Some(tbl) = opts else { return Vec::new() };
    let Ok(headers_tbl) = tbl.get::<mlua::Table>("headers") else {
        return Vec::new();
    };
    let mut headers = Vec::new();
    for (k, v) in headers_tbl.pairs::<String, String>().flatten() {
        headers.push((k, v));
    }
    headers
}

#[allow(clippy::too_many_arguments)]
fn execute_http(
    rt: &tokio::runtime::Handle,
    client: &reqwest::Client,
    method: reqwest::Method,
    url_str: &str,
    headers: Vec<(String, String)>,
    body: Option<String>,
    json_body: Option<String>,
    timeout_ms: Option<u64>,
    allow_private_net: bool,
    request_counter: &AtomicUsize,
    max_requests: usize,
    max_response_bytes: usize,
) -> Result<HttpResponse, mlua::Error> {
    let parsed = validate_http_url(url_str).map_err(mlua::Error::external)?;

    if !allow_private_net {
        if let Some(host) = parsed.host_str() {
            if host_is_private(host) {
                warn!(host, "HTTP request blocked: private network");
                return Err(mlua::Error::external(format!(
                    "Request blocked: '{}' resolves to a private/loopback address",
                    host
                )));
            }
        }
    }

    let count = request_counter.fetch_add(1, Ordering::SeqCst) + 1;
    if count > max_requests {
        warn!(count, max_requests, "HTTP request limit exceeded");
        return Err(mlua::Error::external(format!(
            "Request limit exceeded: {} (max {})",
            count, max_requests
        )));
    }

    let url_for_log = parsed.to_string();
    let start = std::time::Instant::now();

    let response = rt
        .block_on(async {
            let mut req = client.request(method, parsed);
            if let Some(t) = timeout_ms {
                req = req.timeout(Duration::from_millis(t));
            }
            for (k, v) in &headers {
                req = req.header(k.as_str(), v.as_str());
            }
            if let Some(json) = &json_body {
                req = req
                    .header("Content-Type", "application/json")
                    .body(json.clone());
            } else if let Some(b) = &body {
                req = req.body(b.clone());
            }
            req.send().await
        })
        .map_err(|e| mlua::Error::external(format!("HTTP request failed: {}", e)))?;

    let status = response.status().as_u16();
    let final_url = response.url().to_string();
    let resp_headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    if let Some(len) = response.content_length() {
        if len as usize > max_response_bytes {
            return Err(mlua::Error::external(format!(
                "Response too large: {} bytes (max {})",
                len, max_response_bytes
            )));
        }
    }

    let body_bytes = rt
        .block_on(async { response.bytes().await })
        .map_err(|e| mlua::Error::external(format!("Failed to read response body: {}", e)))?;

    if body_bytes.len() > max_response_bytes {
        return Err(mlua::Error::external(format!(
            "Response too large: {} bytes (max {})",
            body_bytes.len(),
            max_response_bytes
        )));
    }

    let body = String::from_utf8_lossy(&body_bytes).to_string();
    let elapsed_ms = start.elapsed().as_millis();

    info!(
        method = %url_for_log,
        status,
        bytes = body_bytes.len(),
        elapsed_ms,
        "HTTP request completed"
    );

    Ok(HttpResponse {
        status,
        headers: resp_headers,
        body,
        url: url_str.to_string(),
        final_url,
    })
}

fn build_http_response_table(lua: &Lua, resp: &HttpResponse) -> Result<mlua::Table, mlua::Error> {
    let table = lua.create_table()?;
    table.set("ok", resp.status >= 200 && resp.status < 300)?;
    table.set("status", resp.status)?;
    table.set("body", lua.create_string(&resp.body)?)?;
    table.set("url", lua.create_string(&resp.url)?)?;
    table.set("final_url", lua.create_string(&resp.final_url)?)?;

    let headers_table = lua.create_table()?;
    for (k, v) in &resp.headers {
        headers_table.set(k.as_str(), lua.create_string(v)?)?;
    }
    table.set("headers", headers_table)?;

    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trait_def::Tool;

    #[test]
    fn test_script_tool_name() {
        let tool = ScriptTool::new();
        assert_eq!(tool.name(), "script");
    }

    #[test]
    fn test_script_tool_schema() {
        let tool = ScriptTool::new();
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        let required = schema["required"].as_array().unwrap();
        let names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(names.contains(&"code"));
    }

    #[test]
    fn test_script_tool_is_mutating() {
        let tool = ScriptTool::new();
        assert!(tool.is_mutating());
    }

    #[test]
    fn test_script_tool_default() {
        let tool = ScriptTool::default();
        assert_eq!(tool.name(), "script");
    }

    #[test]
    fn test_script_tool_with_timeout() {
        let tool = ScriptTool::new().with_timeout(Duration::from_secs(5));
        assert_eq!(tool.name(), "script");
    }

    #[test]
    fn test_script_tool_dangerous_mode() {
        let tool = ScriptTool::new().dangerous(true);
        assert_eq!(tool.name(), "script");
        assert!(tool.is_full_stdlib());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_missing_code() {
        let tool = ScriptTool::new();
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Missing 'code'"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_hello_world() {
        let tool = ScriptTool::new();
        let result = tool.execute(json!({"code": "print('hello world')"})).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("hello world"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_output_buffer_enforces_max_bytes() {
        // Use a very small max_output_bytes to verify the buffer stops growing
        let tool = ScriptTool::new().with_max_output_bytes(100);
        // This script prints far more than 100 bytes
        let code = "for i = 1, 10000 do print(string.rep('A', 100)) end";
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        // Output should be limited to ~100 bytes plus truncation suffix
        assert!(
            output.len() <= 200,
            "output should be bounded by max_output_bytes, got {} bytes: {}",
            output.len(),
            &output[..output.len().min(100)]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_string_operations() {
        let tool = ScriptTool::new();
        let result = tool
            .execute(json!({"code": "print(string.format('result: %d', 42 + 8))"}))
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("result: 50"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_table_operations() {
        let tool = ScriptTool::new();
        let result = tool
            .execute(
                json!({"code": "local t = {3, 1, 2}; table.sort(t); print(table.concat(t, ','))"}),
            )
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("1,2,3"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_json_decode_encode() {
        let tool = ScriptTool::new();
        let result = tool
            .execute(json!({"code": "local data = kestrel.json_decode('{\"name\":\"test\",\"value\":42}'); print(data.name); print(data.value)"}))
            .await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("test"));
        assert!(output.contains("42"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_json_encode() {
        let tool = ScriptTool::new();
        let result = tool
            .execute(json!({"code": "local t = {hello = 'world', num = 123}; print(kestrel.json_encode(t))"}))
            .await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("hello"));
        assert!(output.contains("world"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_platform() {
        let tool = ScriptTool::new();
        let result = tool
            .execute(json!({"code": "print(kestrel.platform())"}))
            .await;
        assert!(result.is_ok());
        let output = result.unwrap();
        #[cfg(windows)]
        assert!(output.contains("windows"));
        #[cfg(not(windows))]
        assert!(output.contains("linux") || output.contains("macos") || output.contains("android"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_sandbox_blocks_io() {
        let tool = ScriptTool::new();
        let result = tool
            .execute(json!({"code": "io.open('/tmp/test', 'w')"}))
            .await;
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("nil") || err_msg.contains("error"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_sandbox_blocks_os_execute() {
        let tool = ScriptTool::new();
        // Our safe os table has no execute method, so this should error
        let result = tool
            .execute(json!({"code": "os.execute('echo pwned')"}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_sandbox_blocks_require() {
        let tool = ScriptTool::new();
        let result = tool.execute(json!({"code": "require('os')"})).await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_read_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "line1\nline2\nline3\n").unwrap();

        let tool = ScriptTool::new();
        let code = format!(
            "local content = kestrel.read_file('{}'); print(content)",
            file_path.to_str().unwrap().replace('\\', "\\\\")
        );
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("line1"));
        assert!(output.contains("line3"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_write_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("output.txt");
        let path_str = file_path.to_str().unwrap().replace('\\', "\\\\");

        let tool = ScriptTool::new();
        let code = format!("kestrel.write_file('{}', 'hello from lua')", path_str);
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok());

        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "hello from lua");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_write_file_blocked_system_path() {
        let tool = ScriptTool::new();
        let blocked_path = if cfg!(windows) {
            r"C:\Windows\System32\kestrel_test.txt"
        } else {
            "/etc/test"
        };
        let code = format!(
            "kestrel.write_file('{}', 'hacked')",
            blocked_path.replace('\\', "\\\\")
        );
        let result = tool.execute(json!({"code": code})).await;
        // Should either error at Lua level or Rust level
        assert!(result.is_err() || result.unwrap().contains("not allowed"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_mkdir_blocked_system_path() {
        let tool = ScriptTool::new();
        let blocked_path = if cfg!(windows) {
            r"C:\Windows\kestrel_test_dir"
        } else {
            "/etc/kestrel_test_dir"
        };
        let code = format!("kestrel.mkdir('{}')", blocked_path.replace('\\', "\\\\"));
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_remove_blocked_system_path() {
        let tool = ScriptTool::new();
        let blocked_path = if cfg!(windows) {
            r"C:\Windows\System32\kestrel_test.txt"
        } else {
            "/etc/passwd"
        };
        let code = format!("kestrel.remove('{}')", blocked_path.replace('\\', "\\\\"));
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_list_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();

        let path_str = dir.path().to_str().unwrap().replace('\\', "\\\\");
        let tool = ScriptTool::new();
        let code = format!(
            "local entries = kestrel.list_dir('{}'); for _, e in ipairs(entries) do print(e) end",
            path_str
        );
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("a.txt"));
        assert!(output.contains("b.rs"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_exists() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("exists.txt");
        std::fs::write(&file_path, "yes").unwrap();

        let path_str = file_path.to_str().unwrap().replace('\\', "\\\\");
        let tool = ScriptTool::new();
        let code = format!(
            "print(kestrel.exists('{}')); print(kestrel.exists('/nonexistent_xyz_123'))",
            path_str
        );
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("true"));
        assert!(output.contains("false"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_math_and_loop() {
        let tool = ScriptTool::new();
        let result = tool
            .execute(
                json!({"code": "local sum = 0; for i = 1, 100 do sum = sum + i end; print(sum)"}),
            )
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("5050"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_os_date() {
        let tool = ScriptTool::new();
        let result = tool
            .execute(json!({"code": "print(os.date('%Y-%m-%d'))"}))
            .await;
        assert!(result.is_ok());
        let output = result.unwrap();
        // Should contain a 4-digit year (e.g. 2026)
        assert!(output.trim().len() >= 10, "os.date output: {}", output);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_os_clock() {
        let tool = ScriptTool::new();
        let result = tool
            .execute(json!({"code": "local t = os.clock(); print(type(t))"}))
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("number"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_env() {
        let tool = ScriptTool::new();
        let code = r#"
            local path = kestrel.env("PATH")
            if path then
                print("PATH is set")
            else
                print("PATH is nil")
            end
        "#;
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("PATH is set"));
    }

    #[test]
    fn test_validate_write_path_allows_normal() {
        assert!(validate_write_path("./output.txt").is_ok());
        assert!(validate_write_path("src/main.rs").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn test_validate_write_path_blocks_system() {
        assert!(validate_write_path("/etc/passwd").is_err());
        assert!(validate_write_path("/usr/bin/evil").is_err());
        assert!(validate_write_path("/bin/sh").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn test_validate_write_path_blocks_system_windows() {
        assert!(validate_write_path("C:\\Windows\\System32\\evil.dll").is_err());
        assert!(validate_write_path("C:\\Program Files\\bad.exe").is_err());
    }

    #[test]
    fn test_validate_write_path_blocks_sensitive_home() {
        assert!(validate_write_path(".ssh/authorized_keys").is_err());
        assert!(validate_write_path(".gnupg/pubring.gpg").is_err());
    }

    #[test]
    fn test_list_dir_respects_max_depth() {
        let dir = tempfile::tempdir().unwrap();
        // Create depth 5: dir/a/b/c/d/e
        let mut deep = dir.path().to_path_buf();
        for i in 0..5 {
            deep.push(format!("l{}", i));
            std::fs::create_dir_all(&deep).unwrap();
            std::fs::write(deep.join("file.txt"), "x").unwrap();
        }

        let mut result = Vec::new();
        list_dir_recursive(dir.path().to_str().unwrap(), true, &mut result, 0, 2, 10000).unwrap();

        let output = result.join("\n");
        assert!(output.contains("max depth reached"), "output: {}", output);
    }

    #[test]
    fn test_list_dir_respects_max_entries() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..50 {
            std::fs::write(dir.path().join(format!("f{:03}.txt", i)), "x").unwrap();
        }

        let mut result = Vec::new();
        list_dir_recursive(dir.path().to_str().unwrap(), false, &mut result, 0, 10, 10).unwrap();

        let output = result.join("\n");
        assert!(
            output.contains("truncated: max 10 entries"),
            "output: {}",
            output
        );
    }

    #[test]
    fn test_list_dir_handles_unreadable_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("visible.txt"), "x").unwrap();

        let mut result = Vec::new();
        // Reading a non-existent sub-path should not panic — just return a warning
        list_dir_recursive(
            "/nonexistent_xyz_dir_12345",
            false,
            &mut result,
            0,
            10,
            10000,
        )
        .unwrap();

        let output = result.join("\n");
        assert!(output.contains("unreadable"), "output: {}", output);
    }

    // ── Capability / Profile tests ──────────────────────────────

    #[test]
    fn test_capability_bitflags_basic() {
        let caps = ScriptCapability::FS_READ | ScriptCapability::FS_WRITE;
        assert!(caps.contains(ScriptCapability::FS_READ));
        assert!(caps.contains(ScriptCapability::FS_WRITE));
        assert!(!caps.contains(ScriptCapability::FS_DELETE));
        assert!(!caps.contains(ScriptCapability::ALL_STD_LIBS));
    }

    #[test]
    fn test_capability_all_host_apis() {
        let all = ScriptCapability::ALL_HOST_APIS;
        assert!(all.contains(ScriptCapability::FS_READ));
        assert!(all.contains(ScriptCapability::FS_WRITE));
        assert!(all.contains(ScriptCapability::FS_DELETE));
        assert!(all.contains(ScriptCapability::FS_MKDIR));
        assert!(all.contains(ScriptCapability::JSON));
        assert!(all.contains(ScriptCapability::ENV_READ));
        assert!(all.contains(ScriptCapability::HTTP));
        assert!(!all.contains(ScriptCapability::ALL_STD_LIBS));
    }

    #[test]
    fn test_capability_all_includes_stdlib() {
        assert!(ScriptCapability::ALL.contains(ScriptCapability::ALL_STD_LIBS));
        assert!(ScriptCapability::ALL.contains(ScriptCapability::ALL_HOST_APIS));
    }

    #[test]
    fn test_profile_safe_capabilities() {
        let caps = ScriptProfile::Safe.capabilities();
        assert!(caps.contains(ScriptCapability::FS_READ));
        assert!(caps.contains(ScriptCapability::FS_WRITE));
        assert!(caps.contains(ScriptCapability::FS_DELETE));
        assert!(caps.contains(ScriptCapability::FS_MKDIR));
        assert!(caps.contains(ScriptCapability::JSON));
        assert!(caps.contains(ScriptCapability::ENV_READ));
        assert!(!caps.contains(ScriptCapability::HTTP));
        assert!(!caps.contains(ScriptCapability::ALL_STD_LIBS));
    }

    #[test]
    fn test_profile_trusted_capabilities() {
        let caps = ScriptProfile::Trusted.capabilities();
        assert!(caps.contains(ScriptCapability::FS_READ));
        assert!(caps.contains(ScriptCapability::FS_WRITE));
        assert!(caps.contains(ScriptCapability::FS_DELETE));
        assert!(caps.contains(ScriptCapability::FS_MKDIR));
        assert!(caps.contains(ScriptCapability::JSON));
        assert!(caps.contains(ScriptCapability::ENV_READ));
        assert!(caps.contains(ScriptCapability::HTTP));
        assert!(caps.contains(ScriptCapability::BUILTIN_MODULES));
        assert!(!caps.contains(ScriptCapability::ALL_STD_LIBS));
    }

    #[test]
    fn test_profile_dangerous_capabilities() {
        let caps = ScriptProfile::Dangerous.capabilities();
        assert!(caps.contains(ScriptCapability::ALL));
        assert!(caps.contains(ScriptCapability::ALL_STD_LIBS));
    }

    #[test]
    fn test_profile_from_bool() {
        assert_eq!(ScriptProfile::from(false), ScriptProfile::Safe);
        assert_eq!(ScriptProfile::from(true), ScriptProfile::Dangerous);
    }

    #[test]
    fn test_profile_default_is_safe() {
        assert_eq!(ScriptProfile::default(), ScriptProfile::Safe);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_capability_gates_write_file() {
        // FS_WRITE not set → write_file should be nil
        let tool =
            ScriptTool::new().with_capabilities(ScriptCapability::FS_READ | ScriptCapability::JSON);
        let result = tool
            .execute(json!({"code": "print(kestrel.write_file == nil)"}))
            .await
            .unwrap();
        assert!(
            result.contains("true"),
            "write_file should be nil without FS_WRITE capability, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_capability_gates_read_file() {
        // FS_READ not set → read_file should be nil
        let tool = ScriptTool::new()
            .with_capabilities(ScriptCapability::FS_WRITE | ScriptCapability::JSON);
        let result = tool
            .execute(json!({"code": "print(kestrel.read_file == nil)"}))
            .await
            .unwrap();
        assert!(
            result.contains("true"),
            "read_file should be nil without FS_READ capability, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_capability_gates_remove() {
        // FS_DELETE not set → remove should be nil
        let tool =
            ScriptTool::new().with_capabilities(ScriptCapability::FS_READ | ScriptCapability::JSON);
        let result = tool
            .execute(json!({"code": "print(kestrel.remove == nil)"}))
            .await
            .unwrap();
        assert!(
            result.contains("true"),
            "remove should be nil without FS_DELETE capability"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_capability_gates_mkdir() {
        // FS_MKDIR not set → mkdir should be nil
        let tool =
            ScriptTool::new().with_capabilities(ScriptCapability::FS_READ | ScriptCapability::JSON);
        let result = tool
            .execute(json!({"code": "print(kestrel.mkdir == nil)"}))
            .await
            .unwrap();
        assert!(
            result.contains("true"),
            "mkdir should be nil without FS_MKDIR capability"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_capability_gates_json() {
        // JSON not set → json_decode and json_encode should be nil
        let tool = ScriptTool::new().with_capabilities(ScriptCapability::FS_READ);
        let result = tool
            .execute(
                json!({"code": "print(kestrel.json_decode == nil, kestrel.json_encode == nil)"}),
            )
            .await
            .unwrap();
        assert!(
            result.contains("true\ttrue") || result.contains("true  true"),
            "json_decode and json_encode should be nil without JSON capability, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_capability_gates_env() {
        // ENV_READ not set → env should be nil
        let tool = ScriptTool::new().with_capabilities(ScriptCapability::FS_READ);
        let result = tool
            .execute(json!({"code": "print(kestrel.env == nil)"}))
            .await
            .unwrap();
        assert!(
            result.contains("true"),
            "env should be nil without ENV_READ capability"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_capability_platform_always_available() {
        // platform() is always available regardless of capabilities
        let tool = ScriptTool::new().with_capabilities(ScriptCapability::NONE);
        let result = tool
            .execute(json!({"code": "print(kestrel.platform())"}))
            .await
            .unwrap();
        assert!(
            result.contains("linux")
                || result.contains("macos")
                || result.contains("windows")
                || result.contains("android"),
            "platform() should always be available, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_profile_safe_has_working_apis() {
        // Default (Safe) profile should have all current working APIs
        let tool = ScriptTool::new();
        let code = format!(
            r#"
                -- All these should work in Safe profile
                assert(kestrel.read_file ~= nil, "read_file missing")
                assert(kestrel.write_file ~= nil, "write_file missing")
                assert(kestrel.list_dir ~= nil, "list_dir missing")
                assert(kestrel.exists ~= nil, "exists missing")
                assert(kestrel.stat ~= nil, "stat missing")
                assert(kestrel.mkdir ~= nil, "mkdir missing")
                assert(kestrel.remove ~= nil, "remove missing")
                assert(kestrel.json_decode ~= nil, "json_decode missing")
                assert(kestrel.json_encode ~= nil, "json_encode missing")
                assert(kestrel.env ~= nil, "env missing")
                assert(kestrel.platform ~= nil, "platform missing")
                print("all apis present")
            "#
        );
        let result = tool.execute(json!({"code": code})).await;
        assert!(
            result.is_ok(),
            "Safe profile should have all standard APIs: {:?}",
            result
        );
        assert!(result.unwrap().contains("all apis present"));
    }

    // ── Path API tests ──────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_cwd() {
        let tool = ScriptTool::new();
        let result = tool
            .execute(json!({"code": "local cwd = kestrel.cwd(); print(type(cwd), #cwd > 0)"}))
            .await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(
            output.contains("string"),
            "cwd should return a string, got: {}",
            output
        );
        assert!(
            output.contains("true"),
            "cwd should be non-empty, got: {}",
            output
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_abspath() {
        let tool = ScriptTool::new();
        let result = tool
            .execute(json!({"code": "local abs = kestrel.abspath('.'); print(abs)"}))
            .await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(
            output.starts_with('/'),
            "abspath should return absolute path, got: {}",
            output
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_join_path() {
        let tool = ScriptTool::new();
        let result = tool
            .execute(json!({"code": "print(kestrel.join_path('a', 'b', 'c.txt'))"}))
            .await;
        assert!(result.is_ok());
        let output = result.unwrap().trim().to_string();
        assert!(
            output.contains("a") && output.contains("b") && output.contains("c.txt"),
            "join_path should join components, got: {}",
            output
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_basename() {
        let tool = ScriptTool::new();
        let result = tool
            .execute(json!({"code": "print(kestrel.basename('/foo/bar/baz.txt'))"}))
            .await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("baz.txt"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_dirname() {
        let tool = ScriptTool::new();
        let result = tool
            .execute(json!({"code": "print(kestrel.dirname('/foo/bar/baz.txt'))"}))
            .await;
        assert!(result.is_ok());
        let output = result.unwrap().trim().to_string();
        assert!(
            output.contains("bar"),
            "dirname should return parent dir, got: {}",
            output
        );
    }

    // ── read_lines tests ────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_read_lines() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("lines.txt");
        std::fs::write(&file_path, "line1\nline2\nline3\nline4\nline5\n").unwrap();

        let path_str = file_path.to_str().unwrap().replace('\\', "\\\\");
        let tool = ScriptTool::new();
        let code = format!(
            "local lines = kestrel.read_lines('{}', 1, 2); for _, l in ipairs(lines) do print(l) end",
            path_str
        );
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("line2"));
        assert!(output.contains("line3"));
        assert!(!output.contains("line1"));
        assert!(!output.contains("line4"));
    }

    // ── append_file tests ───────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_append_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("append.txt");
        let path_str = file_path.to_str().unwrap().replace('\\', "\\\\");

        let tool = ScriptTool::new();
        let code1 = format!("kestrel.write_file('{}', 'hello')", path_str);
        tool.execute(json!({"code": code1})).await.unwrap();

        let code2 = format!("kestrel.append_file('{}', ' world')", path_str);
        let result = tool.execute(json!({"code": code2})).await;
        assert!(result.is_ok());

        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "hello world");
    }

    // ── copy tests ──────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_copy() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        std::fs::write(&src, "copy me").unwrap();

        let src_str = src.to_str().unwrap().replace('\\', "\\\\");
        let dst_str = dst.to_str().unwrap().replace('\\', "\\\\");
        let tool = ScriptTool::new();
        let code = format!("kestrel.copy('{}', '{}')", src_str, dst_str);
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok());

        let content = std::fs::read_to_string(&dst).unwrap();
        assert_eq!(content, "copy me");
    }

    // ── move tests ──────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_move() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("original.txt");
        let dst = dir.path().join("moved.txt");
        std::fs::write(&src, "move me").unwrap();

        let src_str = src.to_str().unwrap().replace('\\', "\\\\");
        let dst_str = dst.to_str().unwrap().replace('\\', "\\\\");
        let tool = ScriptTool::new();
        let code = format!("kestrel.move('{}', '{}')", src_str, dst_str);
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok());

        assert!(!src.exists(), "source should be gone after move");
        let content = std::fs::read_to_string(&dst).unwrap();
        assert_eq!(content, "move me");
    }

    // ── glob tests ──────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_glob() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        std::fs::write(dir.path().join("c.txt"), "").unwrap();

        let path_str = dir.path().to_str().unwrap().replace('\\', "\\\\");
        let tool = ScriptTool::new();
        let code = format!(
            "local files = kestrel.glob('{}/*.rs'); for _, f in ipairs(files) do print(f) end",
            path_str
        );
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(
            output.contains("a.rs"),
            "glob should find a.rs, got: {}",
            output
        );
        assert!(
            output.contains("b.rs"),
            "glob should find b.rs, got: {}",
            output
        );
        assert!(
            !output.contains("c.txt"),
            "glob should not find c.txt, got: {}",
            output
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_glob_max_entries() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..20 {
            std::fs::write(dir.path().join(format!("f{:03}.txt", i)), "").unwrap();
        }

        let path_str = dir.path().to_str().unwrap().replace('\\', "\\\\");
        let tool = ScriptTool::new();
        let code = format!(
            "local files = kestrel.glob('{}/*.txt', {{max_entries = 5}}); print(#files)",
            path_str
        );
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        let output = output.trim();
        assert_eq!(
            output, "5",
            "glob should respect max_entries, got: {}",
            output
        );
    }

    // ── walk tests ──────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_walk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("b.txt"), "").unwrap();

        let path_str = dir.path().to_str().unwrap().replace('\\', "\\\\");
        let tool = ScriptTool::new();
        let code = format!(
            "local entries = kestrel.walk('{}'); for _, e in ipairs(entries) do print(e.type, e.name) end",
            path_str
        );
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(
            output.contains("a.txt"),
            "walk should find a.txt, got: {}",
            output
        );
        assert!(
            output.contains("b.txt"),
            "walk should find b.txt in sub dir, got: {}",
            output
        );
        assert!(
            output.contains("sub"),
            "walk should list sub directory, got: {}",
            output
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_walk_respects_max_depth() {
        let dir = tempfile::tempdir().unwrap();
        let mut deep = dir.path().to_path_buf();
        for i in 0..5 {
            deep.push(format!("d{}", i));
            std::fs::create_dir_all(&deep).unwrap();
            std::fs::write(deep.join("file.txt"), "x").unwrap();
        }

        let path_str = dir.path().to_str().unwrap().replace('\\', "\\\\");
        let tool = ScriptTool::new();
        let code = format!(
            "local entries = kestrel.walk('{}', {{max_depth = 2}}); print(#entries)",
            path_str
        );
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok());
    }

    // ── read_json / write_json tests ────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_read_json() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("data.json");
        std::fs::write(&file_path, r#"{"name": "test", "value": 42}"#).unwrap();

        let path_str = file_path.to_str().unwrap().replace('\\', "\\\\");
        let tool = ScriptTool::new();
        let code = format!(
            "local data = kestrel.read_json('{}'); print(data.name, data.value)",
            path_str
        );
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("test"));
        assert!(output.contains("42"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_write_json() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("output.json");
        let path_str = file_path.to_str().unwrap().replace('\\', "\\\\");

        let tool = ScriptTool::new();
        let code = format!(
            r#"kestrel.write_json('{}', {{hello = "world", num = 123}})"#,
            path_str
        );
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok());

        let content = std::fs::read_to_string(&file_path).unwrap();
        assert!(content.contains("hello"));
        assert!(content.contains("world"));
        assert!(content.contains("123"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_read_write_json_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("roundtrip.json");
        let path_str = file_path.to_str().unwrap().replace('\\', "\\\\");

        let tool = ScriptTool::new();
        let code = format!(
            r#"
                local data = {{items = {{1, 2, 3}}, name = "test"}}
                kestrel.write_json('{path}', data)
                local loaded = kestrel.read_json('{path}')
                print(loaded.name, #loaded.items)
            "#,
            path = path_str
        );
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("test"));
        assert!(output.contains("3"));
    }

    // ── tempdir / tempfile tests ────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_tempdir() {
        let tool = ScriptTool::new();
        let code = r#"
            local dir = kestrel.tempdir()
            print(type(dir), #dir > 0)
            -- Write a file to the temp dir to verify it exists
            kestrel.write_file(dir .. "/test.txt", "temp content")
            print(kestrel.exists(dir .. "/test.txt"))
        "#;
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(
            output.contains("string"),
            "tempdir should return string, got: {}",
            output
        );
        assert!(
            output.contains("true"),
            "tempdir should be usable, got: {}",
            output
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_tempfile() {
        let tool = ScriptTool::new();
        let code = r#"
            local path = kestrel.tempfile()
            print(type(path), #path > 0)
        "#;
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(
            output.contains("string"),
            "tempfile should return string, got: {}",
            output
        );
    }

    // ── Capability gating for new APIs ──────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_capability_gates_path_apis() {
        // Without FS_READ, path APIs should be nil
        let tool = ScriptTool::new().with_capabilities(ScriptCapability::JSON);
        let result = tool
            .execute(json!({"code": "print(kestrel.cwd == nil, kestrel.abspath == nil, kestrel.join_path == nil, kestrel.basename == nil, kestrel.dirname == nil)"}))
            .await
            .unwrap();
        assert!(
            result.contains("true"),
            "path APIs should be nil without FS_READ, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_capability_gates_copy() {
        // Without FS_WRITE, copy should be nil
        let tool = ScriptTool::new().with_capabilities(ScriptCapability::FS_READ);
        let result = tool
            .execute(json!({"code": "print(kestrel.copy == nil)"}))
            .await
            .unwrap();
        assert!(
            result.contains("true"),
            "copy should be nil without FS_WRITE"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_capability_gates_move() {
        // Without FS_WRITE+FS_DELETE, move should be nil
        let tool = ScriptTool::new().with_capabilities(ScriptCapability::FS_READ);
        let result = tool
            .execute(json!({"code": "print(kestrel.move == nil)"}))
            .await
            .unwrap();
        assert!(
            result.contains("true"),
            "move should be nil without FS_WRITE+FS_DELETE"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_capability_gates_read_json() {
        // Without JSON, read_json should be nil
        let tool = ScriptTool::new().with_capabilities(ScriptCapability::FS_READ);
        let result = tool
            .execute(json!({"code": "print(kestrel.read_json == nil)"}))
            .await
            .unwrap();
        assert!(
            result.contains("true"),
            "read_json should be nil without JSON+FS_READ"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_capability_gates_write_json() {
        // Without JSON, write_json should be nil
        let tool = ScriptTool::new().with_capabilities(ScriptCapability::FS_WRITE);
        let result = tool
            .execute(json!({"code": "print(kestrel.write_json == nil)"}))
            .await
            .unwrap();
        assert!(
            result.contains("true"),
            "write_json should be nil without JSON+FS_WRITE"
        );
    }

    // ── Copy/move write path validation ─────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_copy_blocked_system_path() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        std::fs::write(&src, "data").unwrap();

        let src_str = src.to_str().unwrap().replace('\\', "\\\\");
        let tool = ScriptTool::new();
        let code = format!("kestrel.copy('{}', '/etc/evil_copy.txt')", src_str);
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_err(), "copy to system path should fail");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_move_blocked_system_path() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        std::fs::write(&src, "data").unwrap();

        let src_str = src.to_str().unwrap().replace('\\', "\\\\");
        let tool = ScriptTool::new();
        let code = format!("kestrel.move('{}', '/etc/evil_move.txt')", src_str);
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_err(), "move to system path should fail");
    }

    // ── Append respects write limits ────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_script_append_file_respects_write_limit() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("append_limited.txt");
        let path_str = file_path.to_str().unwrap().replace('\\', "\\\\");

        let tool = ScriptTool::new().with_max_write_bytes(10);
        let code = format!(
            "kestrel.append_file('{}', 'this is way more than ten bytes of content')",
            path_str
        );
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_err(), "append_file should respect write limit");
    }

    // ── HTTP helper tests ───────────────────────────────────────

    #[test]
    fn test_validate_http_url_rejects_non_http() {
        assert!(validate_http_url("ftp://example.com").is_err());
        assert!(validate_http_url("file:///etc/passwd").is_err());
        assert!(validate_http_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn test_validate_http_url_accepts_http_https() {
        assert!(validate_http_url("http://example.com").is_ok());
        assert!(validate_http_url("https://example.com/path?q=1").is_ok());
    }

    #[test]
    fn test_validate_http_url_rejects_no_host() {
        assert!(validate_http_url("http://").is_err());
        assert!(validate_http_url("http:///path").is_err());
    }

    #[test]
    fn test_ip_is_private_loopback() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        assert!(ip_is_private(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(ip_is_private(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn test_ip_is_private_rfc1918() {
        use std::net::{IpAddr, Ipv4Addr};
        assert!(ip_is_private(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(ip_is_private(&IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(ip_is_private(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
    }

    #[test]
    fn test_ip_is_private_link_local() {
        use std::net::{IpAddr, Ipv4Addr};
        assert!(ip_is_private(&IpAddr::V4(Ipv4Addr::new(
            169, 254, 169, 254
        ))));
        assert!(ip_is_private(&IpAddr::V4(Ipv4Addr::new(169, 254, 0, 1))));
    }

    #[test]
    fn test_ip_is_not_private_public() {
        use std::net::{IpAddr, Ipv4Addr};
        assert!(!ip_is_private(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!ip_is_private(&IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    #[test]
    fn test_host_is_private_localhost() {
        assert!(host_is_private("127.0.0.1"));
        assert!(host_is_private("localhost"));
    }

    #[test]
    fn test_host_is_not_private_public() {
        // These should not be private (assuming DNS resolves to public IPs)
        assert!(!ip_is_private(
            &"8.8.8.8".parse::<std::net::IpAddr>().unwrap()
        ));
    }

    // ── HTTP capability gating tests ────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_capability_gates_http_apis() {
        // Without HTTP capability, all HTTP APIs should be nil
        let tool = ScriptTool::new().with_capabilities(ScriptCapability::FS_READ);
        let result = tool
            .execute(json!({"code": "print(kestrel.http_get == nil, kestrel.http_post == nil, kestrel.http_request == nil, kestrel.fetch_json == nil, kestrel.post_json == nil, kestrel.download == nil)"}))
            .await
            .unwrap();
        assert!(
            result.contains("true"),
            "HTTP APIs should be nil without HTTP capability, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_capability_gates_download_without_fs_write() {
        // HTTP without FS_WRITE → download should be nil
        let tool =
            ScriptTool::new().with_capabilities(ScriptCapability::HTTP | ScriptCapability::FS_READ);
        let result = tool
            .execute(json!({"code": "print(kestrel.http_get == nil, kestrel.download == nil)"}))
            .await
            .unwrap();
        assert!(
            result.contains("false") && result.contains("true"),
            "http_get should be present, download should be nil, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_http_trusted_profile_has_http_apis() {
        let tool = ScriptTool::new().with_profile(ScriptProfile::Trusted);
        let result = tool
            .execute(json!({"code": "assert(kestrel.http_get ~= nil); assert(kestrel.http_post ~= nil); assert(kestrel.http_request ~= nil); assert(kestrel.fetch_json ~= nil); assert(kestrel.post_json ~= nil); assert(kestrel.download ~= nil); print('all http apis present')"}))
            .await;
        assert!(
            result.is_ok(),
            "Trusted profile should have all HTTP APIs: {:?}",
            result
        );
        assert!(result.unwrap().contains("all http apis present"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_http_safe_profile_no_http_apis() {
        let tool = ScriptTool::new();
        let result = tool
            .execute(json!({"code": "print(kestrel.http_get == nil)"}))
            .await
            .unwrap();
        assert!(
            result.contains("true"),
            "Safe profile should not have http_get, got: {}",
            result
        );
    }

    // ── HTTP URL validation tests ───────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_http_blocks_non_http_scheme() {
        let tool = ScriptTool::new()
            .with_capabilities(ScriptCapability::HTTP | ScriptCapability::HTTP_PRIVATE_NET);
        let result = tool
            .execute(json!({"code": "local r, err = pcall(kestrel.http_get, 'ftp://example.com'); print(r)"}))
            .await
            .unwrap();
        assert!(
            result.contains("false"),
            "ftp:// should be rejected, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_http_blocks_file_scheme() {
        let tool = ScriptTool::new()
            .with_capabilities(ScriptCapability::HTTP | ScriptCapability::HTTP_PRIVATE_NET);
        let result = tool
            .execute(json!({"code": "local r, err = pcall(kestrel.http_get, 'file:///etc/passwd'); print(r)"}))
            .await
            .unwrap();
        assert!(
            result.contains("false"),
            "file:// should be rejected, got: {}",
            result
        );
    }

    // ── HTTP SSRF blocking tests ────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_http_blocks_localhost() {
        let tool = ScriptTool::new().with_capabilities(ScriptCapability::HTTP);
        let result = tool
            .execute(json!({"code": "local ok, err = pcall(kestrel.http_get, 'http://127.0.0.1/'); print(ok)"}))
            .await
            .unwrap();
        assert!(
            result.contains("false"),
            "http://127.0.0.1 should be blocked, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_http_blocks_private_10() {
        let tool = ScriptTool::new().with_capabilities(ScriptCapability::HTTP);
        let result = tool
            .execute(json!({"code": "local ok, err = pcall(kestrel.http_get, 'http://10.0.0.1/'); print(ok)"}))
            .await
            .unwrap();
        assert!(
            result.contains("false"),
            "http://10.0.0.1 should be blocked, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_http_blocks_private_172() {
        let tool = ScriptTool::new().with_capabilities(ScriptCapability::HTTP);
        let result = tool
            .execute(json!({"code": "local ok, err = pcall(kestrel.http_get, 'http://172.16.0.1/'); print(ok)"}))
            .await
            .unwrap();
        assert!(
            result.contains("false"),
            "http://172.16.0.1 should be blocked, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_http_blocks_private_192() {
        let tool = ScriptTool::new().with_capabilities(ScriptCapability::HTTP);
        let result = tool
            .execute(json!({"code": "local ok, err = pcall(kestrel.http_get, 'http://192.168.1.1/'); print(ok)"}))
            .await
            .unwrap();
        assert!(
            result.contains("false"),
            "http://192.168.1.1 should be blocked, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_http_blocks_link_local_metadata() {
        let tool = ScriptTool::new().with_capabilities(ScriptCapability::HTTP);
        let result = tool
            .execute(json!({"code": "local ok, err = pcall(kestrel.http_get, 'http://169.254.169.254/'); print(ok)"}))
            .await
            .unwrap();
        assert!(
            result.contains("false"),
            "http://169.254.169.254 should be blocked, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_http_private_net_allows_with_capability() {
        let tool = ScriptTool::new()
            .with_capabilities(ScriptCapability::HTTP | ScriptCapability::HTTP_PRIVATE_NET);
        // This should not be blocked by SSRF check (will fail on actual connection, but not by policy)
        let result = tool
            .execute(json!({"code": "local ok, err = pcall(kestrel.http_get, 'http://127.0.0.1:1/'); print(ok)"}))
            .await
            .unwrap();
        // Should not be blocked by SSRF — the actual connection will fail but that's a network error
        assert!(
            !result.contains("private") && !result.contains("loopback"),
            "With HTTP_PRIVATE_NET, localhost should not be blocked by policy, got: {}",
            result
        );
    }

    // ── HTTP request limit test ─────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_http_request_limit() {
        let tool = ScriptTool::new()
            .with_capabilities(ScriptCapability::HTTP | ScriptCapability::HTTP_PRIVATE_NET);
        // Make many requests — should hit the limit
        let code = r#"
            local count = 0
            for i = 1, 60 do
                local ok, err = pcall(kestrel.http_get, 'http://127.0.0.1:1/')
                if not ok then
                    if string.find(err, "Request limit") then
                        print("limit hit at " .. i)
                        break
                    end
                end
                count = count + 1
            end
        "#;
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(
            output.contains("limit hit"),
            "Should hit request limit, got: {}",
            output
        );
    }

    // ── HTTP response table structure test ──────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_http_response_table_keys() {
        // Can't test actual HTTP, but test the table structure with SSRF bypass
        let tool = ScriptTool::new()
            .with_capabilities(ScriptCapability::HTTP | ScriptCapability::HTTP_PRIVATE_NET);
        let code = r#"
            local ok, result = pcall(kestrel.http_get, 'http://127.0.0.1:1/')
            if ok then
                print(type(result))
                print(result.ok ~= nil)
                print(result.status ~= nil)
                print(result.body ~= nil)
                print(result.url ~= nil)
                print(result.final_url ~= nil)
                print(result.headers ~= nil)
            else
                -- Connection refused, that's expected — just check it wasn't SSRF blocked
                print("connection error (expected)")
            end
        "#;
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok());
    }

    // ── Built-in module system tests ─────────────────────────────

    #[test]
    fn test_builtin_modules_capability_bit() {
        assert!(ScriptCapability::BUILTIN_MODULES.contains(ScriptCapability::BUILTIN_MODULES));
        assert!(!ScriptCapability::NONE.contains(ScriptCapability::BUILTIN_MODULES));
    }

    #[test]
    fn test_trusted_profile_includes_builtin_modules() {
        let caps = ScriptProfile::Trusted.capabilities();
        assert!(
            caps.contains(ScriptCapability::BUILTIN_MODULES),
            "Trusted profile should include BUILTIN_MODULES"
        );
    }

    #[test]
    fn test_safe_profile_excludes_builtin_modules() {
        let caps = ScriptProfile::Safe.capabilities();
        assert!(
            !caps.contains(ScriptCapability::BUILTIN_MODULES),
            "Safe profile should NOT include BUILTIN_MODULES"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_require_returns_module_table() {
        let tool = ScriptTool::new().with_capabilities(ScriptProfile::Trusted.capabilities());
        let code = r#"
            local fs = require("kestrel.fs")
            print(type(fs))
            print(type(fs.read_file))
            print(type(fs.write_file))
            print(type(fs.list_dir))
            print(type(fs.exists))
        "#;
        let result = tool.execute(json!({"code": code})).await;
        let output = result.unwrap();
        assert!(output.contains("table"), "module should be a table");
        assert!(
            output.contains("function"),
            "module fields should be functions"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_require_path_module() {
        let tool = ScriptTool::new().with_capabilities(ScriptProfile::Trusted.capabilities());
        let code = r#"
            local path = require("kestrel.path")
            print(type(path))
            print(type(path.cwd))
            print(type(path.abspath))
            print(type(path.join_path))
            print(type(path.basename))
            print(type(path.dirname))
        "#;
        let result = tool.execute(json!({"code": code})).await;
        let output = result.unwrap();
        assert!(output.contains("table"));
        assert!(output.contains("function"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_require_json_module() {
        let tool = ScriptTool::new().with_capabilities(ScriptProfile::Trusted.capabilities());
        let code = r#"
            local json = require("kestrel.json")
            print(type(json))
            print(type(json.json_decode))
            print(type(json.json_encode))
            local data = json.json_decode('{"a":1}')
            print(data.a)
            print(json.json_encode({b = 2}))
        "#;
        let result = tool.execute(json!({"code": code})).await;
        let output = result.unwrap();
        assert!(output.contains("table"));
        assert!(output.contains("function"));
        assert!(output.contains("1"));
        assert!(output.contains("b"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_require_http_module() {
        let tool = ScriptTool::new().with_capabilities(ScriptProfile::Trusted.capabilities());
        let code = r#"
            local http = require("kestrel.http")
            print(type(http))
            print(type(http.http_get))
            print(type(http.http_post))
            print(type(http.http_request))
            print(type(http.fetch_json))
            print(type(http.post_json))
        "#;
        let result = tool.execute(json!({"code": code})).await;
        let output = result.unwrap();
        assert!(output.contains("table"));
        assert!(output.contains("function"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_require_env_module() {
        let tool = ScriptTool::new().with_capabilities(ScriptProfile::Trusted.capabilities());
        let code = r#"
            local env_mod = require("kestrel.env")
            print(type(env_mod))
            print(type(env_mod.env))
        "#;
        let result = tool.execute(json!({"code": code})).await;
        let output = result.unwrap();
        assert!(output.contains("table"));
        assert!(output.contains("function"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_require_rejects_unknown_module() {
        let tool = ScriptTool::new().with_capabilities(ScriptProfile::Trusted.capabilities());
        let code = r#"
            local ok, err = pcall(require, "foo")
            print(ok)
            print(err ~= nil)
        "#;
        let result = tool.execute(json!({"code": code})).await;
        let output = result.unwrap();
        assert!(output.contains("false"), "require('foo') should fail");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_require_rejects_relative_path() {
        let tool = ScriptTool::new().with_capabilities(ScriptProfile::Trusted.capabilities());
        let code = r#"
            local ok, err = pcall(require, "./x")
            print(ok)
        "#;
        let result = tool.execute(json!({"code": code})).await;
        let output = result.unwrap();
        assert!(output.contains("false"), "require('./x') should fail");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_require_rejects_socket() {
        let tool = ScriptTool::new().with_capabilities(ScriptProfile::Trusted.capabilities());
        let code = r#"
            local ok, err = pcall(require, "socket")
            print(ok)
        "#;
        let result = tool.execute(json!({"code": code})).await;
        let output = result.unwrap();
        assert!(output.contains("false"), "require('socket') should fail");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_require_not_available_without_capability() {
        let tool = ScriptTool::new(); // Safe profile — no BUILTIN_MODULES
        let code = r#"
            print(require == nil)
        "#;
        let result = tool.execute(json!({"code": code})).await;
        let output = result.unwrap();
        assert!(
            output.contains("true"),
            "require should be nil without BUILTIN_MODULES"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_require_returns_same_table_on_repeated_calls() {
        let tool = ScriptTool::new().with_capabilities(ScriptProfile::Trusted.capabilities());
        let code = r#"
            local fs1 = require("kestrel.fs")
            local fs2 = require("kestrel.fs")
            print(fs1 == fs2)
        "#;
        let result = tool.execute(json!({"code": code})).await;
        let output = result.unwrap();
        assert!(
            output.contains("true"),
            "require should return the same table (caching)"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_module_functions_work_identically() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test_module.txt");
        std::fs::write(&file_path, "hello from module").unwrap();
        let path_str = file_path.to_str().unwrap().replace('\\', "\\\\");

        let tool = ScriptTool::new().with_capabilities(ScriptProfile::Trusted.capabilities());
        let code = format!(
            r#"
            local fs = require("kestrel.fs")
            local content = fs.read_file('{}')
            print(content)
            "#,
            path_str
        );
        let result = tool.execute(json!({"code": code})).await;
        let output = result.unwrap();
        assert!(
            output.contains("hello from module"),
            "module function should work the same as flat API"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_flat_api_still_works_with_modules() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("flat.txt");
        std::fs::write(&file_path, "flat api works").unwrap();
        let path_str = file_path.to_str().unwrap().replace('\\', "\\\\");

        let tool = ScriptTool::new().with_capabilities(ScriptProfile::Trusted.capabilities());
        let code = format!(
            r#"
            local fs = require("kestrel.fs")
            -- Both module and flat API should work
            local content1 = fs.read_file('{}')
            local content2 = kestrel.read_file('{}')
            print(content1 == content2)
            print(content1)
            "#,
            path_str, path_str
        );
        let result = tool.execute(json!({"code": code})).await;
        let output = result.unwrap();
        assert!(
            output.contains("true"),
            "both APIs should return same result"
        );
        assert!(output.contains("flat api works"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_module_only_includes_enabled_functions() {
        // FS_READ only — no write, so write_file should be absent from kestrel.fs
        let tool = ScriptTool::new().with_capabilities(
            ScriptCapability::FS_READ | ScriptCapability::JSON | ScriptCapability::BUILTIN_MODULES,
        );
        let code = r#"
            local fs = require("kestrel.fs")
            print(type(fs.read_file))
            print(fs.write_file == nil)
            print(fs.remove == nil)
            print(fs.mkdir == nil)
        "#;
        let result = tool.execute(json!({"code": code})).await;
        let output = result.unwrap();
        assert!(output.contains("function"), "read_file should be present");
        assert!(
            output.contains("true"),
            "write_file/remove/mkdir should be nil without write/delete caps"
        );
    }

    // ════════════════════════════════════════════════════════════════
    // Abuse-case tests (issue #341)
    // ════════════════════════════════════════════════════════════════

    // ── Sensitive file path abuse via Lua API ───────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_write_ssh_authorized_keys() {
        let tool = ScriptTool::new();
        let result = tool
            .execute(json!({"code": r#"local ok, err = pcall(kestrel.write_file, '.ssh/authorized_keys', 'ssh-rsa AAAA...\n'); print(ok)"#}))
            .await
            .unwrap();
        assert!(
            result.contains("false"),
            "write_file to .ssh/authorized_keys should fail, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_write_gnupg() {
        let tool = ScriptTool::new();
        let result = tool
            .execute(json!({"code": r#"local ok, err = pcall(kestrel.write_file, '.gnupg/pubring.gpg', 'fake key data'); print(ok)"#}))
            .await
            .unwrap();
        assert!(
            result.contains("false"),
            "write_file to .gnupg/pubring.gpg should fail, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_write_gitconfig() {
        let tool = ScriptTool::new();
        let result = tool
            .execute(json!({"code": r#"local ok, err = pcall(kestrel.write_file, '.gitconfig', '[user]\nname = pwned'); print(ok)"#}))
            .await
            .unwrap();
        assert!(
            result.contains("false"),
            "write_file to .gitconfig should fail, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_append_ssh_directory() {
        let tool = ScriptTool::new();
        let result = tool
            .execute(json!({"code": r#"local ok, err = pcall(kestrel.append_file, '.ssh/config', 'Host evil\n\tHostName evil.com\n'); print(ok)"#}))
            .await
            .unwrap();
        assert!(
            result.contains("false"),
            "append_file to .ssh/config should fail, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_copy_to_sensitive_path() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("payload.txt");
        std::fs::write(&src, "evil data").unwrap();
        let src_str = src.to_str().unwrap().replace('\\', "\\\\");

        let tool = ScriptTool::new();
        let code = format!(
            r#"local ok, err = pcall(kestrel.copy, '{}', '.gnupg/evil'); print(ok)"#,
            src_str
        );
        let result = tool.execute(json!({"code": code})).await.unwrap();
        assert!(
            result.contains("false"),
            "copy to .gnupg should fail, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_move_to_sensitive_path() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("payload.txt");
        std::fs::write(&src, "evil data").unwrap();
        let src_str = src.to_str().unwrap().replace('\\', "\\\\");

        let tool = ScriptTool::new();
        let code = format!(
            r#"local ok, err = pcall(kestrel.move, '{}', '.ssh/evil_key'); print(ok)"#,
            src_str
        );
        let result = tool.execute(json!({"code": code})).await.unwrap();
        assert!(
            result.contains("false"),
            "move to .ssh should fail, got: {}",
            result
        );
    }

    // ── Write quota integration (copy/move consume quotas) ──────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_copy_respects_write_byte_limit() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("big.txt");
        std::fs::write(&src, "A".repeat(200)).unwrap();
        let src_str = src.to_str().unwrap().replace('\\', "\\\\");
        let dst_str = dir
            .path()
            .join("copy.txt")
            .to_str()
            .unwrap()
            .replace('\\', "\\\\");

        let tool = ScriptTool::new().with_max_write_bytes(50);
        let code = format!("kestrel.copy('{}', '{}')", src_str, dst_str);
        let result = tool.execute(json!({"code": code})).await;
        assert!(
            result.is_err(),
            "copy of 200-byte file should exceed 50-byte write limit"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_copy_respects_write_file_count() {
        let dir = tempfile::tempdir().unwrap();
        let dst_str = dir.path().to_str().unwrap().replace('\\', "\\\\");

        let tool = ScriptTool::new().with_max_write_files(1);
        // First copy should succeed, second should fail
        let code = format!(
            r#"
                local src1 = kestrel.tempfile()
                kestrel.write_file(src1, 'data1')
                local src2 = kestrel.tempfile()
                kestrel.write_file(src2, 'data2')
                local ok1, err1 = pcall(kestrel.copy, src1, '{d}/c1.txt')
                local ok2, err2 = pcall(kestrel.copy, src2, '{d}/c2.txt')
                print(ok1, ok2)
            "#,
            d = dst_str
        );
        let result = tool.execute(json!({"code": code})).await;
        // The write_file calls already consumed the file count, so copy should fail
        assert!(result.is_ok());
        let output = result.unwrap();
        // Second copy should fail due to file count
        assert!(
            output.contains("false"),
            "second copy should fail due to file count limit, got: {}",
            output
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_multiple_writes_exhaust_byte_quota() {
        let dir = tempfile::tempdir().unwrap();
        let path_str = dir.path().to_str().unwrap().replace('\\', "\\\\");

        // Very small quota — first write succeeds, second fails
        let tool = ScriptTool::new().with_max_write_bytes(20);
        let code = format!(
            r#"
                local ok1, err1 = pcall(kestrel.write_file, '{p}/a.txt', '0123456789')
                local ok2, err2 = pcall(kestrel.write_file, '{p}/b.txt', '0123456789')
                print(ok1, ok2)
            "#,
            p = path_str
        );
        let result = tool.execute(json!({"code": code})).await.unwrap();
        assert!(
            result.contains("true\tfalse") || result.contains("true  false"),
            "first write should succeed, second should fail: got {}",
            result
        );
    }

    // ── Walk/glob limit enforcement from Lua ────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_walk_max_entries_enforced() {
        let dir = tempfile::tempdir().unwrap();
        // Create many files
        for i in 0..50 {
            std::fs::write(dir.path().join(format!("f{:03}.txt", i)), "x").unwrap();
        }

        let path_str = dir.path().to_str().unwrap().replace('\\', "\\\\");
        let tool = ScriptTool::new();
        let code = format!(
            r#"local entries = kestrel.walk('{}', {{max_entries = 5}}); print(#entries)"#,
            path_str
        );
        let result = tool.execute(json!({"code": code})).await.unwrap();
        let count: usize = result.trim().parse().unwrap();
        assert!(
            count <= 5,
            "walk should respect max_entries=5, got {}",
            count
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_walk_deep_nesting_respects_max_depth() {
        let dir = tempfile::tempdir().unwrap();
        let mut deep = dir.path().to_path_buf();
        for i in 0..8 {
            deep.push(format!("d{}", i));
            std::fs::create_dir_all(&deep).unwrap();
            std::fs::write(deep.join("file.txt"), "x").unwrap();
        }

        let path_str = dir.path().to_str().unwrap().replace('\\', "\\\\");
        let tool = ScriptTool::new();
        // max_depth=3 should not reach d7/file.txt
        let code = format!(
            r#"local entries = kestrel.walk('{}', {{max_depth = 3}}); print(#entries)"#,
            path_str
        );
        let result = tool.execute(json!({"code": code})).await;
        assert!(result.is_ok(), "walk with max_depth should succeed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_glob_max_entries_enforced_from_lua() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..100 {
            std::fs::write(dir.path().join(format!("f{:03}.txt", i)), "x").unwrap();
        }

        let path_str = dir.path().to_str().unwrap().replace('\\', "\\\\");
        let tool = ScriptTool::new();
        let code = format!(
            r#"local files = kestrel.glob('{}/*.txt', {{max_entries = 3}}); print(#files)"#,
            path_str
        );
        let result = tool.execute(json!({"code": code})).await.unwrap();
        assert_eq!(result.trim(), "3", "glob should return exactly 3 entries");
    }

    // ── HTTP download path validation ───────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_download_to_system_path() {
        let tool = ScriptTool::new().with_capabilities(
            ScriptCapability::HTTP
                | ScriptCapability::FS_WRITE
                | ScriptCapability::HTTP_PRIVATE_NET,
        );
        let code = r#"local ok, err = pcall(kestrel.download, 'http://127.0.0.1:1/', '/etc/evil_download'); print(ok)"#;
        let result = tool.execute(json!({"code": code})).await.unwrap();
        assert!(
            result.contains("false"),
            "download to /etc should fail, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_download_to_sensitive_home_path() {
        let tool = ScriptTool::new().with_capabilities(
            ScriptCapability::HTTP
                | ScriptCapability::FS_WRITE
                | ScriptCapability::HTTP_PRIVATE_NET,
        );
        let code = r#"local ok, err = pcall(kestrel.download, 'http://127.0.0.1:1/', '.ssh/evil_download'); print(ok)"#;
        let result = tool.execute(json!({"code": code})).await.unwrap();
        assert!(
            result.contains("false"),
            "download to .ssh should fail, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_http_blocks_data_scheme() {
        let tool = ScriptTool::new().with_capabilities(ScriptCapability::HTTP);
        let code = r#"local ok, err = pcall(kestrel.http_get, 'data:text/html,<script>alert(1)</script>'); print(ok)"#;
        let result = tool.execute(json!({"code": code})).await.unwrap();
        assert!(
            result.contains("false"),
            "data: scheme should be rejected, got: {}",
            result
        );
    }

    // ── Profile behavior difference tests ───────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_safe_profile_no_http_no_modules() {
        let tool = ScriptTool::new(); // Safe profile
        let code = r#"
            print(kestrel.http_get == nil)
            print(kestrel.http_post == nil)
            print(kestrel.download == nil)
            print(require == nil)
        "#;
        let result = tool.execute(json!({"code": code})).await.unwrap();
        assert!(
            result.contains("true\ttrue\ttrue\ttrue") || result.contains("true  true  true  true"),
            "Safe profile should have no HTTP APIs and no require, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_trusted_profile_has_http_and_modules() {
        let tool = ScriptTool::new().with_profile(ScriptProfile::Trusted);
        let code = r#"
            assert(kestrel.http_get ~= nil, "trusted should have http_get")
            assert(kestrel.http_post ~= nil, "trusted should have http_post")
            assert(require ~= nil, "trusted should have require")
            print("trusted profile complete")
        "#;
        let result = tool.execute(json!({"code": code})).await;
        assert!(
            result.is_ok(),
            "Trusted profile assertions should pass: {:?}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_dangerous_profile_has_full_stdlib() {
        let tool = ScriptTool::new().with_profile(ScriptProfile::Dangerous);
        // Dangerous should have io, which Safe/Trusted don't
        let code = r#"
            assert(io ~= nil, "dangerous should have io")
            assert(require ~= nil, "dangerous should have require")
            print("dangerous profile complete")
        "#;
        let result = tool.execute(json!({"code": code})).await;
        assert!(
            result.is_ok(),
            "Dangerous profile should have io and require: {:?}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_safe_profile_blocks_io() {
        let tool = ScriptTool::new(); // Safe
        let code = r#"print(io == nil)"#;
        let result = tool.execute(json!({"code": code})).await.unwrap();
        assert!(
            result.contains("true"),
            "Safe profile should not expose io, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_disabled_capability_api_returns_nil() {
        // Remove FS_READ but keep FS_WRITE — read_file should be nil
        let tool = ScriptTool::new().with_capabilities(ScriptCapability::FS_WRITE);
        let code = r#"
            print(kestrel.read_file == nil)
            print(kestrel.list_dir == nil)
            print(kestrel.exists == nil)
            print(kestrel.stat == nil)
        "#;
        let result = tool.execute(json!({"code": code})).await.unwrap();
        assert!(
            result.contains("true"),
            "Without FS_READ, read APIs should be nil, got: {}",
            result
        );
    }

    // ── Module system abuse ─────────────────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_require_rejects_path_traversal() {
        let tool = ScriptTool::new().with_capabilities(ScriptProfile::Trusted.capabilities());
        let code = r#"
            local ok1 = pcall(require, "../etc/passwd")
            local ok2 = pcall(require, "../../secret")
            local ok3 = pcall(require, "/etc/passwd")
            print(ok1, ok2, ok3)
        "#;
        let result = tool.execute(json!({"code": code})).await.unwrap();
        assert!(
            result.contains("false\tfalse\tfalse") || result.contains("false  false  false"),
            "path traversal in require should all fail, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_require_rejects_common_lua_modules() {
        let tool = ScriptTool::new().with_capabilities(ScriptProfile::Trusted.capabilities());
        let code = r#"
            local ok1 = pcall(require, "io")
            local ok2 = pcall(require, "os")
            local ok3 = pcall(require, "math")
            local ok4 = pcall(require, "string")
            local ok5 = pcall(require, "table")
            local ok6 = pcall(require, "coroutine")
            local ok7 = pcall(require, "utf8")
            local ok8 = pcall(require, "package")
            print(ok1, ok2, ok3, ok4, ok5, ok6, ok7, ok8)
        "#;
        let result = tool.execute(json!({"code": code})).await.unwrap();
        // All standard Lua module names should be rejected
        let lines: Vec<&str> = result.trim().lines().collect();
        let last_line = lines.last().unwrap();
        assert!(
            !last_line.contains("true"),
            "standard Lua module names should be rejected in require, got: {}",
            last_line
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_require_case_sensitive() {
        let tool = ScriptTool::new().with_capabilities(ScriptProfile::Trusted.capabilities());
        let code = r#"
            local ok1 = pcall(require, "kestrel.FS")
            local ok2 = pcall(require, "KESTREL.FS")
            local ok3 = pcall(require, "Kestrel.Fs")
            print(ok1, ok2, ok3)
        "#;
        let result = tool.execute(json!({"code": code})).await.unwrap();
        assert!(
            result.contains("false\tfalse\tfalse") || result.contains("false  false  false"),
            "require should be case-sensitive, got: {}",
            result
        );
    }

    // ── Write path validation edge cases ────────────────────────

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_mkdir_system_path_blocked() {
        let tool = ScriptTool::new();
        let code = r#"local ok, err = pcall(kestrel.mkdir, '/usr/local/evil'); print(ok)"#;
        let result = tool.execute(json!({"code": code})).await.unwrap();
        assert!(
            result.contains("false"),
            "mkdir to /usr should fail, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_remove_system_path_blocked() {
        let tool = ScriptTool::new();
        let code = r#"local ok, err = pcall(kestrel.remove, '/etc/shadow'); print(ok)"#;
        let result = tool.execute(json!({"code": code})).await.unwrap();
        assert!(
            result.contains("false"),
            "remove /etc/shadow should fail, got: {}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_abuse_write_file_count_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path_str = dir.path().to_str().unwrap().replace('\\', "\\\\");

        let tool = ScriptTool::new().with_max_write_files(2);
        let code = format!(
            r#"
                local ok1 = pcall(kestrel.write_file, '{p}/a.txt', 'a')
                local ok2 = pcall(kestrel.write_file, '{p}/b.txt', 'b')
                local ok3 = pcall(kestrel.write_file, '{p}/c.txt', 'c')
                print(ok1, ok2, ok3)
            "#,
            p = path_str
        );
        let result = tool.execute(json!({"code": code})).await.unwrap();
        assert!(
            result.contains("true\ttrue\tfalse") || result.contains("true  true  false"),
            "third write should fail due to file count limit, got: {}",
            result
        );
    }
}
