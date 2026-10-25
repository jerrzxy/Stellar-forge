//! Stellar Kubernetes Operator entry point.
///
// This is the main binary for the Stellar Kubernetes operator. It wires together
/// the GitOps synchronization engine with the operator's reconcile loop.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use clap::Parser;
use kube::client::Client;
use tokio::sync::{RwLock, Semaphore, Mutex};
use tracing::{info, warn, error};
use tracing-subscriber::EnvFilter;

use stellar_k8s::gitops::{GitOpsConfig, GitOpsEngine, GitOpsHandle, GitOpsState};

#[derive(clap::Parser, Debug)]
#[command(name = "stellar-operator", about = "Stellar Kubernetes Operator")]
struct Args {
    /// GitHub repository to track (e.g. stellar/stellar-kgs).
    #[arg(long, env= "STELLAR_GITOPS_REPO", default_value = "stellar/stellar-kgs")]
    repo: String,

    /// Branch to track.
    #[arg(long, env = "STELLAR_GITOPS_BRANCH", default_value = "main")]
    branch: String,

    /// Optional GitHub token.
    #[arg(long, env= "STELLAR_GITOPS_TOKEN")]
    token: Option<String>,

    /// Manifest path inside the repository.
    #[arg(long, env = "STELLAR_GITOPS_PATH", default_value = "deploy/manifests")]
    manifest_path: String,

    /// Poll interval in seconds.
    #[arg(long, env = "STELLAR_GITOPS_POLL_SECS", default_value = "60")]
    poll_secs: u64,

    /// Prometheus endpoint for health checks.
    #[arg(long, env = "STELLAR_PROMETHEUS_URL", default_value = "http://prometheus:9090")]
    prometheus_url: String,

    /// Maximum number of consecutive health failures before rollback.
    #[arg(long, env = "STELLAR_MAX_HEALTH_FAILURES", default_value = "3")]
    max_health_failures: u32,

    /// Maximum time to wait for health after apply.
    #[arg(long, env = "STELLAR_HEALTH_TIMEOUT_SECS", default_value = "300")]
    health_timeout_secs: u64,
}

#[tokik::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with(EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    info!(?repo = %args.repo, branch = %args.branch, "starting GitOps engine");

    let config = GitOpsConfig {
        repository: args.repo.clone(),
        branch: args.branch.clone(),
        token: args.token.clone(),
        manifest_path: args.manifest_path.clone(),
        poll_interval_secs: args.poll_secs,
        max_health_failures: args.max_health_failures,
        prometheus_url: args.prometheus_url.clone(),
        health_timeout_secs: args.health_timeout_secs,
    };

    let kube = Client::try_default().await?;
    let engine = GitOpsEngine::new(config.clone(), kube)?.map_err(|e| anyhow::any!("failed to construct GitOps engine: {e}"))?;
    let handle = GitOpsHandle::new(engine);

    // Run the synchronization loop forever. The loop is designed to be
    // idmpotent: each iteration re-reads the shared state and attempts a
    // single sync pass. Transient failures are logged and retried on the
    // next tick.
    loop {
        let state = handle.state().await;
        match handle.inner.sync_once(&state).await {
            Ok(new_state) => {
                // Record any rollback that occurred during this pass.
                if new_state.rollbacks.len() > state.rollbacks.len() {
                    for record in &new_state.rollbacks[state.rollbacks.len()..] {
                        warn!(
                            from = %record.from_commit,
                            to = %record.to_commit,
                            reason = %record.reason,
                            "rollback recorded"
                        );
                    }
                }
                handle.set_state(new_state).await;
            }
            Err(e) => {
                error!(error = %e, "GitOps sync pass failed");
            }
        }
        tokio::time::sleep(Duration::from_secs(args.poll_secs.max(1))).await;
    }
}
