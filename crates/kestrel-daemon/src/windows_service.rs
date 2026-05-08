//! Windows Service support for kestrel.
//!
//! Provides an alternative to Unix daemonization by running kestrel as a
//! standard Windows Service. Uses the `windows-service` crate for service
//! registration, control handler, and lifecycle management.
//!
//! # Architecture
//!
//! On Unix, kestrel daemonizes via double-fork and manages its own PID file
//! with `flock`. On Windows, the equivalent is registering as a Windows
//! Service and letting the Service Control Manager (SCM) manage the process
//! lifecycle.
//!
//! # Modules provided (mirrors Unix API surface)
//!
//! - **PID file** — `WinPidFile` with `fs4` file locking (analogous to Unix `PidFile`)
//! - **Service lifecycle** — `run_as_service`, `install_service`, `uninstall_service`
//! - **Signal handling** — `wait_for_signal` via `tokio::signal::ctrl_c()`
//! - **Process management** — `send_stop_and_wait` via `taskkill`
//!
//! # Usage
//!
//! ```ignore
//! // Register the service with the SCM:
//! kestrel_daemon::windows_service::install_service("kestrel", "Kestrel Agent")?;
//!
//! // Unregister:
//! kestrel_daemon::windows_service::uninstall_service("kestrel")?;
//!
//! // Run as a service (called by the SCM entry point):
//! kestrel_daemon::windows_service::run_as_service(|ctx| {
//!     ctx.report_running()?;
//!     ctx.wait_for_shutdown()
//! })?;
//! ```

use anyhow::{bail, Context, Result};
use fs4::fs_std::FileExt;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

/// The name of the service as registered with the Windows Service Control Manager.
const SERVICE_NAME: &str = "kestrel";
/// Display name shown in the Windows Services snap-in.
#[allow(dead_code)]
const SERVICE_DISPLAY_NAME: &str = "Kestrel Agent";
/// Service type: own process (not shared).
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

// ---------------------------------------------------------------------------
// PID file (Windows: file-based locking via fs4)
// ---------------------------------------------------------------------------

/// A PID file for Windows using file-based locking via `fs4`.
///
/// On Unix, `flock(LOCK_EX|LOCK_NB)` is used. On Windows, we approximate
/// this with `fs4::FileExt::try_lock_exclusive()`. The lock is held for
/// the lifetime of this struct and released on drop.
///
/// The file is created (or opened) and locked exclusively. If another process
/// holds the lock, creation fails immediately.
///
/// # Drop behavior
///
/// When dropped, the `fs4` lock is released. The PID file is **not** deleted
/// automatically — call [`WinPidFile::clean()`] to remove it on graceful shutdown.
pub struct WinPidFile {
    /// Path to the PID file on disk.
    path: String,
    /// The locked file handle. Kept open to hold the exclusive lock.
    file: std::fs::File,
}

impl WinPidFile {
    /// Create and lock a PID file atomically on Windows.
    ///
    /// Writes the current process ID to the file and acquires an exclusive,
    /// non-blocking lock via `fs4`. If another process already holds the lock,
    /// this returns an error.
    ///
    /// # Arguments
    ///
    /// * `path` - Filesystem path for the PID file.
    ///
    /// # Errors
    ///
    /// - If the parent directory does not exist.
    /// - If another process holds the lock (double-start prevention).
    /// - If I/O operations fail.
    pub fn create(path: &str) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = Path::new(path).parent() {
            fs::create_dir_all(parent).context("create PID file parent directory")?;
        }

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path)
            .context("open PID file")?;

        // Acquire exclusive, non-blocking lock via fs4
        file.try_lock_exclusive().map_err(|e| {
            anyhow::anyhow!(
                "failed to lock PID file ({}) — another instance may be running",
                e
            )
        })?;

        // Write current PID
        let pid = std::process::id();
        write!(file, "{pid}").context("write PID to file")?;

        tracing::info!("PID file created and locked: {path} (pid={pid})");

        Ok(Self {
            path: path.to_string(),
            file,
        })
    }

    /// Read the PID from a file without acquiring a lock.
    ///
    /// Returns `Ok(None)` if the file does not exist.
    pub fn read_pid(path: &str) -> Result<Option<i32>> {
        if !Path::new(path).exists() {
            return Ok(None);
        }

        let mut file = fs::File::open(path).context("open PID file for reading")?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .context("read PID file")?;

        let pid: i32 = contents.trim().parse().context("parse PID from file")?;
        Ok(Some(pid))
    }

    /// Remove the PID file and release the lock.
    ///
    /// Consumes `self`. This is the clean-shutdown path: unlink the file
    /// and drop the file handle (which releases the lock).
    pub fn clean(self) -> Result<()> {
        let path = self.path.clone();
        drop(self); // Release the lock first

        if Path::new(&path).exists() {
            fs::remove_file(&path).context("remove PID file")?;
            tracing::info!("PID file removed: {path}");
        }

        Ok(())
    }

    /// Returns the path to the PID file.
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl Drop for WinPidFile {
    fn drop(&mut self) {
        // Release the exclusive lock explicitly
        // (also happens on file drop, but being explicit is clearer)
        let _ = self.file.unlock();
    }
}

/// Check whether a process with the given PID is currently running on Windows.
///
/// Uses `tasklist` command to check if a process with the given PID exists.
pub fn is_process_running(pid: i32) -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", pid), "/NH"])
        .output();

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            stdout.contains(&pid.to_string())
        }
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Service lifecycle
// ---------------------------------------------------------------------------

/// Run the current process as a Windows Service.
///
/// This function connects to the Windows Service Control Manager, registers
/// a service control handler, and then calls the provided `on_start` closure.
///
/// The `on_start` closure receives a [`ServiceContext`] that provides a
/// shutdown signal receiver. The service should run until a shutdown signal
/// is received.
///
/// # Arguments
///
/// * `on_start` - Closure called when the service starts. Receives a
///   `ServiceContext` with a shutdown signal.
///
/// # Errors
///
/// Returns an error if the SCM connection fails or the service cannot be
/// registered.
pub fn run_as_service<F>(on_start: F) -> Result<()>
where
    F: FnOnce(ServiceContext) -> Result<()> + Send + 'static,
{
    // Create a channel for shutdown signaling
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();

    // Connect to the service control manager and register the control handler.
    // The handler sends shutdown signals through the channel.
    let event_handler = move |control_event| -> ServiceControlHandlerResult {
        match control_event {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                tracing::info!("Received service stop/shutdown event");
                let _ = shutdown_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::ParamChange => ServiceControlHandlerResult::NoError,
            ServiceControl::NetBindAdd
            | ServiceControl::NetBindRemove
            | ServiceControl::NetBindEnable
            | ServiceControl::NetBindDisable => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };

    // Register the service control handler
    let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
        .context("register service control handler")?;

    // Tell SCM we're starting
    status_handle
        .set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: ServiceState::StartPending,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::NO_ERROR,
            checkpoint: 0,
            wait_hint: Duration::from_secs(5),
            process_id: None,
        })
        .context("set service status to StartPending")?;

    // Create the service context
    let context = ServiceContext {
        shutdown_rx,
        status_handle,
    };

    // Run the on_start closure
    let result = on_start(context);

    // Report final status to SCM
    let (final_state, exit_code) = match &result {
        Ok(()) => (ServiceState::Stopped, ServiceExitCode::NO_ERROR),
        Err(e) => {
            tracing::error!("Service failed: {}", e);
            (ServiceState::Stopped, ServiceExitCode::ServiceSpecific(1))
        }
    };

    // Best-effort status update
    let status_handle = service_control_handler::register(SERVICE_NAME, |_| {
        ServiceControlHandlerResult::NotImplemented
    })
    .ok();
    if let Some(handle) = status_handle {
        handle
            .set_service_status(ServiceStatus {
                service_type: SERVICE_TYPE,
                current_state: final_state,
                controls_accepted: ServiceControlAccept::empty(),
                exit_code,
                checkpoint: 0,
                wait_hint: Duration::default(),
                process_id: None,
            })
            .ok();
    }

    result
}

/// Context provided to the service's on_start closure.
///
/// Contains a shutdown signal receiver and a handle to report status
/// to the Service Control Manager.
pub struct ServiceContext {
    shutdown_rx: mpsc::Receiver<()>,
    status_handle: service_control_handler::ServiceStatusHandle,
}

impl ServiceContext {
    /// Report that the service is now running.
    ///
    /// Call this once initialization is complete and the service is ready
    /// to accept requests.
    pub fn report_running(&self) -> Result<()> {
        self.status_handle
            .set_service_status(ServiceStatus {
                service_type: SERVICE_TYPE,
                current_state: ServiceState::Running,
                controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
                exit_code: ServiceExitCode::NO_ERROR,
                checkpoint: 0,
                wait_hint: Duration::default(),
                process_id: None,
            })
            .context("set service status to Running")
    }

    /// Wait for the shutdown signal from the SCM.
    ///
    /// Blocks until the Service Control Manager sends a Stop or Shutdown
    /// command. Returns `Ok(())` when shutdown is signaled.
    pub fn wait_for_shutdown(self) -> Result<()> {
        self.shutdown_rx.recv().context("shutdown signal channel")?;
        Ok(())
    }

    /// Report that the service is stopping.
    pub fn report_stopping(&self) -> Result<()> {
        self.status_handle
            .set_service_status(ServiceStatus {
                service_type: SERVICE_TYPE,
                current_state: ServiceState::StopPending,
                controls_accepted: ServiceControlAccept::empty(),
                exit_code: ServiceExitCode::NO_ERROR,
                checkpoint: 0,
                wait_hint: Duration::from_secs(10),
                process_id: None,
            })
            .context("set service status to StopPending")
    }

    /// Returns a reference to the status handle for custom status reporting.
    pub fn status_handle(&self) -> &service_control_handler::ServiceStatusHandle {
        &self.status_handle
    }
}

// ---------------------------------------------------------------------------
// Service installation / uninstallation
// ---------------------------------------------------------------------------

/// Install the kestrel service with the Windows Service Control Manager.
///
/// Registers the current executable as a Windows Service with the given name
/// and display name. The service is configured to start automatically on boot.
///
/// # Arguments
///
/// * `service_name` - Internal name for the service (e.g. `"kestrel"`).
/// * `display_name` - Human-readable name shown in the Services snap-in.
///
/// # Errors
///
/// Returns an error if:
/// - The Service Control Manager cannot be reached (requires Administrator privileges).
/// - A service with the same name already exists.
/// - The current executable path cannot be determined.
pub fn install_service(service_name: &str, display_name: &str) -> Result<()> {
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("connect to Service Control Manager (are you running as Administrator?)")?;

    let executable_path = std::env::current_exe().context("get current executable path")?;

    let service_info = ServiceInfo {
        name: OsString::from(service_name),
        display_name: OsString::from(display_name),
        service_type: SERVICE_TYPE,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path,
        launch_arguments: vec![OsString::from("service")],
        dependencies: vec![],
        account_name: None, // Run as LocalSystem by default
        account_password: None,
    };

    manager
        .create_service(&service_info, ServiceAccess::CHANGE_CONFIG)
        .context("create service")?;

    println!("Service '{service_name}' installed successfully.");
    println!("Start it with: sc start {service_name}");

    Ok(())
}

/// Uninstall the kestrel service from the Windows Service Control Manager.
///
/// Stops the service if it is running, then deletes it from the SCM database.
///
/// # Arguments
///
/// * `service_name` - Internal name of the service to remove.
///
/// # Errors
///
/// Returns an error if the SCM cannot be reached or the service doesn't exist.
pub fn uninstall_service(service_name: &str) -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("connect to Service Control Manager (are you running as Administrator?)")?;

    let service = manager
        .open_service(service_name, ServiceAccess::DELETE | ServiceAccess::STOP)
        .context("open service for deletion")?;

    // Try to stop the service first — ignore errors if it's not running
    let _ = service.stop();

    service.delete().context("delete service")?;

    println!("Service '{service_name}' uninstalled successfully.");

    Ok(())
}

// ---------------------------------------------------------------------------
// Ctrl+C / CtrlBreak signal handling (non-Unix)
// ---------------------------------------------------------------------------

/// The type of shutdown signal received on Windows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownSignal {
    /// Ctrl+C or service stop — graceful shutdown.
    Graceful,
    /// Service shutdown event.
    Fast,
}

/// Wait for a shutdown signal on Windows.
///
/// Uses `tokio::signal::ctrl_c()` for Ctrl+C handling. When running as a
/// Windows Service, the service control handler manages shutdown instead.
pub async fn wait_for_signal() -> ShutdownSignal {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl+C handler");
    tracing::info!("Received Ctrl+C on Windows");
    ShutdownSignal::Graceful
}

/// Send a stop signal to the service process on Windows.
///
/// On Unix, this sends SIGTERM. On Windows, we use `taskkill` for a graceful
/// shutdown, falling back to force kill after the timeout.
///
/// # Arguments
///
/// * `pid` - Process ID to signal.
/// * `timeout_secs` - Maximum seconds to wait for exit.
pub fn send_stop_and_wait(pid: i32, timeout_secs: u64) -> Result<()> {
    // Use taskkill for graceful termination on Windows
    let kill_result = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string()])
        .output();

    match kill_result {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("taskkill failed for pid {pid}: {stderr}");
        }
        Err(e) => bail!("failed to execute taskkill for pid {pid}: {e}"),
    }

    // Wait for the process to exit
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    while std::time::Instant::now() < deadline {
        if !is_process_running(pid) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Force kill if still running after timeout
    let _ = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .output();

    bail!("process {pid} did not exit within {timeout_secs}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shutdown_signal_variants() {
        let graceful = ShutdownSignal::Graceful;
        let fast = ShutdownSignal::Fast;
        assert_eq!(graceful, ShutdownSignal::Graceful);
        assert_ne!(graceful, fast);
    }

    #[test]
    fn test_is_process_running_current() {
        // Our own PID should be running
        assert!(is_process_running(std::process::id() as i32));
    }

    #[test]
    fn test_is_process_running_nonexistent() {
        // Very high PID is extremely unlikely
        assert!(!is_process_running(4_000_000));
    }

    #[test]
    fn test_pid_file_read_nonexistent() {
        let pid = WinPidFile::read_pid("C:\\tmp\\nonexistent_kestrel_test.pid").unwrap();
        assert!(pid.is_none());
    }

    #[test]
    fn test_service_display_name_not_empty() {
        assert!(!SERVICE_DISPLAY_NAME.is_empty());
    }

    #[test]
    fn test_service_name_not_empty() {
        assert!(!SERVICE_NAME.is_empty());
    }
}
