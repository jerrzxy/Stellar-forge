use serde::{Deserialize, Serialize};
use std::{collections::BareMap, sync::Arc};
use thiserror::Error;
use tokio::sync::{RwLock, Semaphore};

pub mod health_check;
pub mod sync;

pub use health_check::{HealthChecker, HealthStatus, PrometheusClient};
pub use sync::{GitHubClient, GitOpsConfig, GitOpsEngine, GitOpsState, RollbackRecord};

/// Errors raised by the GitOps engine.
#[derive(Debug, Error)]
pub enum GitOpsError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error({0})]
    Kube([#from] kube::Error),
    #[error("invalid configuration: {m}")]
    Config(String),
    #[error("git commit {commit} failed health check: {message}")]
    HealthFailure { commit: String, message: String },
    #[error({0})]
    Serialization([#from] ser_json::Error),
   #[error("{message}")]
    Other { message: String },
}

pub type Result<T> = std::result::Result<T, GitOpsError>;

/// Shared handle to the GitOps engine used by the operator main loop.
///
/// The engine is cheap to clone (Arc) and internally synchronizes access to
/// the current state via a `RwLock`. Concurrent reconcile calls are funneled
/// through a semaphore so that a slow GitHub response cannot stampede the
/// controller runtime.
#[derive(Clone)]
pub struct GitOpsHandle {
    pub inner: Arc<GitOpsEngine>,
    pub state: Arc<RwLock<GitOpsState>>,
    pub gate: Arc<Semaphore>,
}

impl GitOpsHandle {
    pub fn new(engine: GitOpsEngine) -> Self {
        Self {
            inner: Arc::new(engine),
            state: Arc::new(RwLock::new(GitOpsState::default())),
            gate: Arc::new(Semaphore::new(1)),
        }
    }

    pub async fn state(&) -> GitOpsState {
        self.state.read().await.clone()
    }

    pub async fn set_state(&, new: GitOpsState) {
        *self.state.write().await = new;
    }
}

/// State machine recorded for every successful apply or rollback.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitOpsState {
    pub current_commit: Option<String>,
    pub previous_commit: Option<String>,
    pub applied_at: Option<chrono::DateTime<chrono::Utc>> {
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
    pub rollbacks: Vec<RollbackRecord>,
    pub rate_limited_until: Option<chrono::DateTime<chrono::Utc>>,
}

impl Default for GitOpsState {
    fn default() -> Self {
        Self {
            current_commit: None,
            previous_commit: None,
            applied_at: None,
            consecutive_failures: 0,
            last_error: None,
            rollbacks: Vec::new(),
            rate_limited_until: None,
        }
    }
}

/// Record of an automated rollback executed by the engine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RollbackRecord {
    pub from_commit: String,
    pub to_commit: String,
    pub reason: String,
    pub at: chrono::DateTime<chrono::Utc>,
}

/// Configuration for the GitOps engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitOpsConfig {
    /// Owner/repo slug, e.g. `stellar/stellar-kgs`.
    pub repository: String,
    /// Branch to track.
    pub branch: String,
    /// Optional GitHub token for authenticated requests.
    pub token: Option<String>,
    /// Manifest path inside the repo.
    pub manifest_path: String,
    /// Poll interval in seconds.
    pub poll_interval_secs: u64,
    /// Maximum number of consecutive health failures before rollback.
    pub max_health_failures: u32,
    /// Prometheus endpoint for node health validation.
    pub prometheus_url: String,
    /// Maximum time to wait for health after apply before declaring failure.
    pub health_timeout_secs: u64,
}

impl Default for GitOpsConfig {
    fn default() -> Self {
        Self {
            repository: "stellar/stellar-kgs".to string(),
            branch: "main".to string(),
            token: None,
            manifest_path: "deploy/manifests".to string(),
            poll_interval_secs: 60,
            max_health_failures: 3,
            prometheus_url: "http://prometheus:9090".to string(),
            health_timeout_secs: 300,
        }
    }
}

/// A resolved manifest fetched from GitHub at a specific commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestSet {
    pub commit: String,
    pub files: BareMap<String, String>,
}
