//! Restarting supervisor for the Captive Core child process.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::{
    process::{Child, Command},
    time::{sleep, timeout},
};

use super::ipc::probe_http;

use libc;

/// Supervisor lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorState {
    Idle,
    Starting,
    Running,
    Restarting,
    Stopped,
    Failed,
}

/// Runtime settings for a Captive Core supervisor.
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub command: String,
    pub args: Vec<String>,
    pub ipc_endpoint: String,
    pub lock_path: PathBuf,
    pub probe_interval: Duration,
    pub hung_after: Duration,
    pub term_grace: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            command: "stellar-core".into(),
            args: Vec::new(),
            ipc_endpoint: "http://127.0.0.1:11626/info".into(),
            lock_path: PathBuf::from("/var/lib/stellar/core.lock"),
            probe_interval: Duration::from_secs(5),
            hung_after: Duration::from_secs(15),
            term_grace: Duration::from_secs(5),
        }
    }
}

/// Owns one Captive Core process and restarts it after crash or IPC hangs.
pub struct CaptiveCoreSupervisor {
    config: SupervisorConfig,
    child: Option<Child>,
    client: reqwest::Client,
    state: SupervisorState,
    started_at: Option<Instant>,
    restarts: u64,
}

impl CaptiveCoreSupervisor {
    pub fn new(config: SupervisorConfig) -> Self {
        Self {
            config,
            child: None,
            client: reqwest::Client::new(),
            state: SupervisorState::Idle,
            started_at: None,
            restarts: 0,
        }
    }

    pub fn state(&self) -> SupervisorState {
        self.state
    }

    pub fn restarts(&self) -> u64 {
        self.restarts
    }

    pub fn uptime(&self) -> Duration {
        self.started_at.map(|t| t.elapsed()).unwrap_or(Duration::ZERO)
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(|c| c.id())
    }

    pub fn clean_lock(&self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.config.lock_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub fn remove_stale_lock(&self) -> std::io::Result<()> {
        self.clean_lock()
    }

    pub async fn start(&mut self) -> std::io::Result<()> {
        self.state = SupervisorState::Starting;
        self.clean_lock()?;
        let child = Command::new(&self.config.command)
            .args(&self.config.args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        self.child = Some(child);
        self.state = SupervisorState::Running;
        self.started_at = Some(Instant::now());
        Ok(())
    }

    pub async fn supervise_once(&mut self) -> Result<bool, String> {
        if self.child.is_none() {
            self.start().await.map_err(|error| error.to_string())?;
            self.restarts += 1;
            return Ok(true);
        }

        let exited = {
            let child = self.child.as_mut().expect("child checked");
            match child.try_wait() {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(e) => return Err(e.to_string()),
            }
        };
        if exited {
            self.child = None;
            let _ = self.clean_lock();
            self.state = SupervisorState::Restarting;
            self.start().await.map_err(|error| error.to_string())?;
            self.restarts += 1;
            self.state = SupervisorState::Running;
            self.started_at = Some(Instant::now());
            return Ok(true);
        }

        sleep(self.config.probe_interval).await;
        if probe_http(
            &self.client,
            &self.config.ipc_endpoint,
            self.config.hung_after,
        )
        .await
        .is_ok()
        {
            return Ok(false);
        }

        self.state = SupervisorState::Restarting;
        self.terminate().await?;
        let _ = self.clean_lock();
        self.start().await.map_err(|error| error.to_string())?;
        self.restarts += 1;
        self.state = SupervisorState::Running;
        self.started_at = Some(Instant::now());
        Ok(true)
    }

    pub async fn terminate(&mut self) -> Result<(), String> {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        if let Some(pid) = child.id() {
            unsafe { libc::kill(pid as i32, libc::SIGTERM); }
        }
        if timeout(self.config.term_grace, child.wait()).await.is_err() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        if let Some(pid) = self.child.as_ref().and_then(|c| c.id()) {
            unsafe { libc::kill(pid as i32, libc::SIGKILL); }
        }
        self.child = None;
        Ok(())
    }

    pub async fn stop(&mut self) -> Result<(), String> {
        self.state = SupervisorState::Stopped;
        self.terminate().await
    }

    pub async fn run(&mut self) -> Result<(), String> {
        self.state = SupervisorState::Running;
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|e| e.to_string())?;
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    let _ = self.stop().await;
                    return Ok(());
                }
                _ = sigterm.recv() => {
                    let _ = self.stop().await;
                    return Ok(());
                }
                res = self.supervise_once() => {
                    if let Err(e) = res {
                        tracing::warn!("supervise_once error: {}", e);
                    }
                }
            }
        }
    }
}
