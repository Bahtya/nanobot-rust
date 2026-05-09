//! Built-in Lua script engine tool.
//!
//! Provides a cross-platform sandboxed Lua execution environment for script
//! orchestration (string processing, JSON, file I/O, batch operations) when
//! shell commands are unavailable or unreliable (especially on Windows).

use crate::trait_def::{Tool, ToolError};
use async_trait::async_trait;
use kestrel_core::MAX_TOOL_OUTPUT_LENGTH;
use mlua::{HookTriggers, Lua, LuaOptions, StdLib, VmState};
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    dangerous: bool,
}

impl ScriptTool {
    /// Create a new ScriptTool with safe defaults.
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_secs(DEFAULT_SCRIPT_TIMEOUT_SECS),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_write_bytes: DEFAULT_MAX_WRITE_BYTES,
            max_write_files: DEFAULT_MAX_WRITE_FILES,
            max_instructions: DEFAULT_MAX_INSTRUCTIONS,
            dangerous: false,
        }
    }

    /// Disable sandbox restrictions for trusted environments.
    pub fn dangerous(mut self, dangerous: bool) -> Self {
        self.dangerous = dangerous;
        self
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
        if self.dangerous {
            info!("Creating script VM in dangerous mode — all standard libraries enabled");
        }
        let safe_libs = if self.dangerous {
            // Dangerous mode: all standard libraries
            StdLib::ALL
        } else {
            // Safe mode: only non-dangerous libraries (no PACKAGE to prevent loader abuse)
            StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::UTF8
        };

        let lua = unsafe { Lua::unsafe_new_with(safe_libs, LuaOptions::default()) };

        if !self.dangerous {
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
                    // chrono::format() panics on unknown specifiers, so we use
                    // DelayedFormat's fallible cousin via format_items + write.
                    // For simplicity we catch the panic via a wrapper.
                    let formatted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        dt.format(&format).to_string()
                    }));
                    match formatted {
                        Ok(s) => lua.create_string(&s),
                        Err(_) => {
                            // Fallback: use a safe default format instead of crashing
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
    fn inject_kestrel_api(&self, lua: &Lua) -> Result<(), ToolError> {
        let kestrel_table = lua
            .create_table()
            .map_err(|e| ToolError::Execution(format!("Failed to create kestrel table: {}", e)))?;

        // --- kestrel.read_file(path [, offset, limit]) ---
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

        // --- kestrel.write_file(path, content [, append]) ---
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

        // --- kestrel.list_dir(path [, recursive]) ---
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

        // --- kestrel.exists(path) ---
        let exists_fn = lua
            .create_function(|_, path: String| Ok(Path::new(&path).exists()))
            .map_err(|e| ToolError::Execution(format!("Failed to create exists: {}", e)))?;
        kestrel_table
            .set("exists", exists_fn)
            .map_err(|e| ToolError::Execution(format!("Failed to set exists: {}", e)))?;

        // --- kestrel.stat(path) ---
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

        // --- kestrel.mkdir(path) ---
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

        // --- kestrel.remove(path) ---
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

        // --- kestrel.json_decode(string) ---
        let json_decode_fn = lua
            .create_function(|lua, json_str: String| {
                let val: Value = serde_json::from_str(&json_str)
                    .map_err(|e| mlua::Error::external(format!("JSON decode error: {}", e)))?;
                json_value_to_lua(lua, &val)
            })
            .map_err(|e| ToolError::Execution(format!("Failed to create json_decode: {}", e)))?;
        kestrel_table
            .set("json_decode", json_decode_fn)
            .map_err(|e| ToolError::Execution(format!("Failed to set json_decode: {}", e)))?;

        // --- kestrel.json_encode(table) ---
        let json_encode_fn = lua
            .create_function(|_, table: mlua::Value| {
                let json_val = lua_value_to_json(&table)?;
                serde_json::to_string_pretty(&json_val)
                    .map_err(|e| mlua::Error::external(format!("JSON encode error: {}", e)))
            })
            .map_err(|e| ToolError::Execution(format!("Failed to create json_encode: {}", e)))?;
        kestrel_table
            .set("json_encode", json_encode_fn)
            .map_err(|e| ToolError::Execution(format!("Failed to set json_encode: {}", e)))?;

        // --- kestrel.env(name) ---
        let env_fn = lua
            .create_function(|lua, name: String| match std::env::var(&name) {
                Ok(val) => Ok(Some(lua.create_string(&val)?)),
                Err(_) => Ok(None),
            })
            .map_err(|e| ToolError::Execution(format!("Failed to create env: {}", e)))?;
        kestrel_table
            .set("env", env_fn)
            .map_err(|e| ToolError::Execution(format!("Failed to set env: {}", e)))?;

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

        // Set kestrel global
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
         kestrel.json_decode, kestrel.json_encode, kestrel.env, kestrel.platform."
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
            dangerous = self.dangerous,
            max_instructions = self.max_instructions,
            "Script execution starting"
        );

        let lua = self.create_sandboxed_vm()?;

        // Set up instruction limit and wall-clock timeout via debug hook
        let start = std::time::Instant::now();
        if !self.dangerous {
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

        // Set up output capture buffer
        let stdout_buf: Arc<std::sync::Mutex<Vec<u8>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let buf_clone = stdout_buf.clone();

        // Override print to write to our buffer
        let globals = lua.globals();
        let print_capture = lua
            .create_function(move |_, args: mlua::MultiValue| {
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
        let max_output = self.max_output_bytes;
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
                let output = {
                    let buf = stdout_buf.lock().unwrap_or_else(|e| e.into_inner());
                    String::from_utf8_lossy(&buf).to_string()
                };

                let output_len = output.len();
                let truncated = output_len > MAX_TOOL_OUTPUT_LENGTH || output_len > max_output;

                info!(
                    duration_ms = elapsed_ms,
                    output_len,
                    truncated,
                    dangerous = self.dangerous,
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
                    dangerous = self.dangerous,
                    "Script execution failed"
                );
                exec_result?;
                unreachable!()
            }
        }
    }
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
        assert!(tool.dangerous);
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
}
