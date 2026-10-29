//! Captive Core process lifecycle management.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use tokio::process::Command;
use tokio::time::sleep;
use tracing::{debug, info, warn};


/// Path to Captive Core lock file
pub const CORE_LOCK_PATH: &str = "/var/lib/stellar/core.lock";

/// Maximum time to wait for graceful process termination (SIGTERM)
pub const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum time to wait for process to respond after forced termination (SIGKILL)
pub const FORCED_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Captive Core process state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Running,
    Stopped,
    Terminating,
    Unknown,
}

/// Lock file information
#[derive(Debug, Clone)]
pub struct LockFileInfo {
    pub exists: bool,
    pub age: Option<Duration>,
    pub pid: Option<u32>,
}

/// Captive Core process handle
pub struct CaptiveCoreProcess {
    pid: Option<u32>,
    state: ProcessState,
    lock_path: PathBuf,
    core_binary: PathBuf,
    state_changed_at: Option<SystemTime>,
}

impl CaptiveCoreProcess {
    pub fn new(core_binary: PathBuf, lock_path: Option<PathBuf>) -> Self {
        Self {
            pid: None,
            state: ProcessState::Unknown,
            lock_path: lock_path.unwrap_or_else(|| PathBuf::from(CORE_LOCK_PATH)),
            core_binary,
            state_changed_at: None,
        }
    }

    pub fn state(&self) -> ProcessState {
        self.state
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn lock_path(&self) -> &PathBuf {
        &self.lock_path
    }

    pub async fn check_lock_file(&self) -> Result<LockFileInfo, String> {
        match fs::metadata(&self.lock_path) {
            Ok(metadata) => {
                let modified = metadata.modified().map_err(|e| format!("Failed to read lock file metadata: {}", e))?;
                let age = SystemTime::now().duration_since(modified).unwrap_or(Duration::from_secs(0));
                let pid = self.read_pid_from_lock().await;
                Ok(LockFileInfo { exists: true, age: Some(age), pid })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(LockFileInfo { exists: false, age: None, pid: None })
            }
            Err(e) => Err(format!("Failed to check lock file: {}", e)),
        }
    }

    async fn read_pid_from_lock(&self) -> Option<u32> {
        match fs::read_to_string(&self.lock_path) {
            Ok(content) => content.trim().parse::<u32>().ok(),
            Err(_) => None,
        }
    }

    pub async fn is_lock_stale(&self) -> Result<bool, String> {
        let lock_info = self.check_lock_file().await?;
        if !lock_info.exists {
            return Ok(false);
        }
        if let Some(lock_pid) = lock_info.pid {
            if !self.process_exists(lock_pid).await {
                debug!("Lock file exists but process {} is not running", lock_pid);
                return Ok(true);
            }
        } else {
            // Unparseable/filled lockfile is treated as stale.
            return Ok(true);
        }
        if let Some(age) = lock_info.age {
            if age > Duration::from_secs(3600) {
                warn!("Lock file is very old ({:?}), considering it stale", age);
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub async fn remove_stale_lock(&mut self) -> Result<(), String> {
        if !self.is_lock_stale().await? {
            debug!("Lock is not stale, not removing");
            return Ok(());
        }
        let lock_info = self.check_lock_file().await?;
        if let Some(lock_pid) = lock_info.pid {
            if self.process_exists(lock_pid).await {
                return Err("Cannot remove lock file: process is still running".to_string());
            }
        }
        match fs::remove_file(&self.lock_path) {
            Ok(_) => { info!("Successfully removed stale lock file"); Ok(()) }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => { debug!("Lock file already removed"); Ok(()) }
            Err(e) => Err(format!("Failed to remove lock file: {}", e)),
        }
    }

    pub async fn remove_lock(&mut self) -> Result<(), String> {
        match fs::remove_file(&self.lock_path) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("Failed to remove lock file: {}", e)),
        }
    }

    async fn process_exists(&self, pid: u32) -> bool {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    pub async fn terminate_graceful(&mut self, timeout: Duration) -> Result<bool, String> {
        if self.state == ProcessState::Stopped {
            debug!("Process already stopped");
            return Ok(true);
        }
        if let Some(pid) = self.pid {
            info!("Sending SIGTERM to process {}", pid);
            unsafe {
                if libc::kill(pid as i32, libc::SIGTERM) != 0 {
                    warn!("Failed to send SIGTERM to {}", pid);
                    return Ok(false);
                }
            }
            self.state = ProcessState::Terminating;
            self.state_changed_at = Some(SystemTime::now());
            let start = SystemTime::now();
            while start.elapsed().unwrap_or(Duration::MAX) < timeout {
                sleep(Duration::from_millis(100)).await;
                if !self.process_exists(pid).await {
                    self.state = ProcessState::Stopped;
                    self.pid = None;
                    info!("Process {} terminated gracefully", pid);
                    return Ok(true);
                }
            }
            warn!("Process {} did not terminate after {:?}", pid, timeout);
            return Ok(false);
        }
        Ok(true)
    }

    pub async fn terminate_forced(&mut self) -> Result<(), String> {
        if self.state == ProcessState::Stopped {
            return Ok(());
        }
        if let Some(pid) = self.pid {
            info!("Sending SIGKILL to process {}", pid);
            unsafe { libc::kill(pid as i32, libc::SIGKILL); }
            sleep(Duration::from_millis(500)).await;
            if !self.process_exists(pid).await {
                self.state = ProcessState::Stopped;
                self.pid = None;
                info!("Process {} killed", pid);
            }
        }
        Ok(())
    }

    pub async fn spawn(&mut self, args: Vec<String>) -> Result<u32, String> {
        if self.state == ProcessState::Running {
            debug!("Process already running");
            return self.pid.ok_or_else(|| "Process state is Running but no PID".to_string());
        }
        info!("Spawning Captive Core process: {} {:?}", self.core_binary.display(), args);
        let mut cmd = Command::new(&self.core_binary);
        cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(false);
        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id().ok_or_else(|| "Failed to get child process ID".to_string())?;
                // We detach; store pid only. Child dropped but kill_on_drop false.
                self.pid = Some(pid);
                self.state = ProcessState::Running;
                self.state_changed_at = Some(SystemTime::now());
                info!("Spawned Captive Core with PID {}", pid);
                Ok(pid)
            }
            Err(e) => Err(format!("Failed to spawn Captive Core: {}", e)),
        }
    }

    pub async fn shutdown(&mut self) -> Result<(), String> {
        debug!("Initiating Captive Core shutdown");
        let graceful_success = self.terminate_graceful(GRACEFUL_SHUTDOWN_TIMEOUT).await?;
        if !graceful_success {
            warn!("Graceful shutdown failed, forcing termination");
            self.terminate_forced().await?;
        }
        if self.is_lock_stale().await.unwrap_or(false) {
            let _ = self.remove_stale_lock().await;
        }
        Ok(())
    }

    pub async fn restart(&mut self, args: Vec<String>) -> Result<u32, String> {
        info!("Restarting Captive Core process");
        self.shutdown().await?;
        if self.is_lock_stale().await.unwrap_or(false) {
            let _ = self.remove_stale_lock().await;
        }
        sleep(Duration::from_secs(1)).await;
        self.spawn(args).await
    }
}

// Controller crate has no crate::error module; define none.
