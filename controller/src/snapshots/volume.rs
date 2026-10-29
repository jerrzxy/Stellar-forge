// Copyright 2024 Stellar-K8s Contributors
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Kubernetes VolumeSnapshot controller for Stellar validator PVCs.
//!
//! This module implements the core snapshot lifecycle:
//!
//! 1. **Fsync/pause coordination** — Before triggering a snapshot the controller
//!    sends a `POST /management/stop-writes` request to the Stellar Core HTTP
//!    management endpoint, waits for acknowledgement, triggers the snapshot, then
//!    immediately re-enables writes via `POST /management/resume-writes`.
//!    Coordination is limited to a configurable [`StellarCoreConfig::pause_timeout`]
//!    so a stalled snapshot never locks out the validator indefinitely.
//!
//! 2. **Cloud-provider snapshot** — Delegates to the [`crate::snapshots::SnapshotClient`]
//!    trait whose implementations live in [`crate::snapshots::cloud_api`].
//!
//! 3. **PVC auto-generation** — Once the cloud snapshot is ready the controller
//!    generates a [`PvcCloneSpec`] that callers can use to instantiate a new
//!    `PersistentVolumeClaim` bound to the snapshot, enabling a fresh pod to
//!    start from the cloned state.
//!
//! 4. **Sync verification** — The controller exposes
//!    [`SnapshotController::verify_sync`] to check whether a node bootstrapped
//!    from a snapshot has caught up within a configurable deadline (default
//!    5 minutes per issue requirements).

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, error, info, instrument, warn};

use crate::snapshots::cloud_api::{CloudSnapshotError, SnapshotClient};

// ─── Error types ─────────────────────────────────────────────────────────────

/// Errors produced by the snapshot controller.
#[derive(Debug, Error)]
pub enum SnapshotError {
    /// The Stellar Core management API returned a non-2xx status while pausing
    /// or resuming writes for snapshot consistency.
    #[error("Stellar Core management API error (url={url}): {status} {body}")]
    StellarCoreApi {
        url: String,
        status: u16,
        body: String,
    },

    /// The Stellar Core pause-writes request timed out.
    #[error("timed out waiting for Stellar Core to pause writes after {elapsed:?}")]
    PauseTimeout { elapsed: Duration },

    /// An HTTP transport error occurred when contacting the Stellar Core
    /// management endpoint.
    #[error("HTTP transport error contacting Stellar Core at {url}: {source}")]
    Transport {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    /// The underlying cloud provider returned an error while managing the
    /// volume snapshot.
    #[error("cloud provider snapshot error: {0}")]
    Cloud(#[from] CloudSnapshotError),

    /// A required configuration field is missing or invalid.
    #[error("invalid snapshot configuration: {reason}")]
    InvalidConfig { reason: String },

    /// The snapshot did not reach the `Ready` phase within the deadline.
    #[error("snapshot {snapshot_id} did not become ready within {timeout:?}")]
    SnapshotTimeout {
        snapshot_id: String,
        timeout: Duration,
    },

    /// The node bootstrapped from the snapshot failed to sync within the
    /// 5-minute deadline specified in issue #243.
    #[error("node {node_name} failed to sync within {timeout:?}; last ledger seq: {last_ledger}")]
    SyncTimeout {
        node_name: String,
        timeout: Duration,
        last_ledger: u64,
    },

    /// A JSON serialisation/deserialisation error.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

// ─── Domain types ─────────────────────────────────────────────────────────────

/// The phase of a snapshot request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum SnapshotPhase {
    /// The request has been accepted and is queued for processing.
    Pending,
    /// Stellar Core writes are being paused for consistency.
    PausingWrites,
    /// The cloud provider snapshot is being created.
    Creating,
    /// The cloud provider snapshot is ready and the PVC spec has been generated.
    Ready,
    /// The snapshot failed; see the accompanying status message.
    Failed,
}

impl std::fmt::Display for SnapshotPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotPhase::Pending => write!(f, "Pending"),
            SnapshotPhase::PausingWrites => write!(f, "PausingWrites"),
            SnapshotPhase::Creating => write!(f, "Creating"),
            SnapshotPhase::Ready => write!(f, "Ready"),
            SnapshotPhase::Failed => write!(f, "Failed"),
        }
    }
}

/// A snapshot request describing which PVC to snapshot and what to name the
/// resulting cloud-provider snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotRequest {
    /// Kubernetes namespace where the source PVC lives.
    pub namespace: String,
    /// Name of the source `PersistentVolumeClaim` to snapshot.
    pub pvc_name: String,
    /// Cloud-provider volume ID backing the PVC (e.g. `vol-0abc123` on AWS).
    pub volume_id: String,
    /// Human-readable description embedded in the cloud snapshot metadata.
    pub description: String,
    /// Key/value tags to attach to the snapshot for cost allocation and
    /// lifecycle management.
    pub tags: Vec<(String, String)>,
}

/// The live status of a snapshot operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotStatus {
    /// Current lifecycle phase.
    pub phase: SnapshotPhase,
    /// Cloud-provider snapshot identifier (populated once the snapshot is created).
    pub snapshot_id: Option<String>,
    /// ISO-8601 timestamp at which the snapshot was created.
    pub created_at: Option<String>,
    /// Human-readable status message suitable for logging / operator events.
    pub message: String,
    /// The auto-generated PVC spec to clone from this snapshot (populated once
    /// the snapshot is `Ready`).
    pub pvc_clone_spec: Option<PvcCloneSpec>,
}

impl SnapshotStatus {
    fn pending() -> Self {
        Self {
            phase: SnapshotPhase::Pending,
            snapshot_id: None,
            created_at: None,
            message: "Snapshot request queued".to_string(),
            pvc_clone_spec: None,
        }
    }
}

/// A Kubernetes `PersistentVolumeClaim` specification that can be applied to
/// the cluster to clone storage from a completed snapshot.
///
/// Consumers should populate the remaining PVC metadata (name, labels, etc.)
/// and submit the object to the Kubernetes API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PvcCloneSpec {
    /// Storage class to use for the cloned PVC.
    /// Defaults to the same storage class as the source PVC.
    pub storage_class_name: String,
    /// The cloud-provider snapshot ID to restore from.
    pub snapshot_id: String,
    /// Requested storage capacity (e.g. `"100Gi"`).
    pub storage_request: String,
    /// Access modes for the cloned PVC (e.g. `["ReadWriteOnce"]`).
    pub access_modes: Vec<String>,
    /// Additional annotations to apply to the PVC object.
    pub annotations: Vec<(String, String)>,
}

// ─── Configuration ────────────────────────────────────────────────────────────

/// Configuration for the Stellar Core management endpoint used to coordinate
/// crash-consistent snapshots.
#[derive(Debug, Clone)]
pub struct StellarCoreConfig {
    /// Base URL of the Stellar Core HTTP management interface
    /// (e.g. `http://stellar-core:11626`).
    pub management_url: String,
    /// Maximum time to wait for Stellar Core to acknowledge paused writes.
    pub pause_timeout: Duration,
    /// How long to wait between write-pause poll attempts.
    pub poll_interval: Duration,
}

impl Default for StellarCoreConfig {
    fn default() -> Self {
        Self {
            management_url: "http://localhost:11626".to_string(),
            pause_timeout: Duration::from_secs(30),
            poll_interval: Duration::from_millis(500),
        }
    }
}

/// Top-level configuration for the snapshot controller.
#[derive(Debug, Clone)]
pub struct SnapshotControllerConfig {
    /// Stellar Core management endpoint configuration.
    pub stellar_core: StellarCoreConfig,
    /// Maximum time to wait for the cloud snapshot to reach the `Ready` state.
    pub snapshot_ready_timeout: Duration,
    /// Deadline within which a node bootstrapped from a snapshot must catch up
    /// to the live network (see issue #243 requirement: ≤ 5 minutes).
    pub sync_verification_timeout: Duration,
    /// Storage class name applied to auto-generated PVC clone specs.
    pub default_storage_class: String,
}

impl Default for SnapshotControllerConfig {
    fn default() -> Self {
        Self {
            stellar_core: StellarCoreConfig::default(),
            snapshot_ready_timeout: Duration::from_secs(600),
            sync_verification_timeout: Duration::from_secs(300), // 5 minutes per #243
            default_storage_class: "ebs-sc".to_string(),
        }
    }
}

// ─── Controller ───────────────────────────────────────────────────────────────

/// The snapshot controller orchestrates the full snapshot lifecycle:
/// fsync coordination, cloud-provider snapshot creation, and PVC clone spec
/// generation.
pub struct SnapshotController {
    config: SnapshotControllerConfig,
    http: reqwest::Client,
    cloud: Arc<dyn SnapshotClient>,
}

impl SnapshotController {
    /// Creates a new [`SnapshotController`] with the provided configuration and
    /// cloud provider client.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::InvalidConfig`] when `config.default_storage_class`
    /// is empty.
    pub fn new(
        config: SnapshotControllerConfig,
        cloud: Arc<dyn SnapshotClient>,
    ) -> Result<Self, SnapshotError> {
        if config.default_storage_class.is_empty() {
            return Err(SnapshotError::InvalidConfig {
                reason: "default_storage_class must not be empty".to_string(),
            });
        }

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| SnapshotError::Transport {
                url: config.stellar_core.management_url.clone(),
                source: e,
            })?;

        Ok(Self { config, http, cloud })
    }

    /// Executes the complete snapshot lifecycle for the given [`SnapshotRequest`].
    ///
    /// The sequence is:
    /// 1. Pause writes on Stellar Core.
    /// 2. Create the cloud-provider snapshot.
    /// 3. Resume writes on Stellar Core.
    /// 4. Poll until the snapshot is ready.
    /// 5. Build and return a [`PvcCloneSpec`].
    ///
    /// # Errors
    ///
    /// Returns the first error encountered. If Stellar Core write-pausing fails
    /// the snapshot is still attempted (best-effort crash-consistent behaviour)
    /// but the error is logged as a warning.
    #[instrument(skip(self), fields(
        pvc = %req.pvc_name,
        volume_id = %req.volume_id,
        namespace = %req.namespace,
    ))]
    pub async fn run_snapshot(
        &self,
        req: &SnapshotRequest,
    ) -> Result<SnapshotStatus, SnapshotError> {
        let mut status = SnapshotStatus::pending();

        // ── Step 1: coordinate writes with Stellar Core ──────────────────────
        status.phase = SnapshotPhase::PausingWrites;
        status.message = "Pausing Stellar Core writes for snapshot consistency".to_string();
        info!(
            pvc = %req.pvc_name,
            url = %self.config.stellar_core.management_url,
            "pausing Stellar Core writes"
        );

        let pause_result = self.pause_stellar_core_writes().await;
        match &pause_result {
            Ok(()) => {
                info!("Stellar Core writes successfully paused");
            }
            Err(e) => {
                // Non-fatal: many CSI drivers provide crash-consistent snapshots
                // without application cooperation. Log and proceed.
                warn!(
                    error = %e,
                    "failed to pause Stellar Core writes; proceeding with crash-consistent snapshot"
                );
            }
        }

        // ── Step 2: create cloud-provider snapshot ───────────────────────────
        status.phase = SnapshotPhase::Creating;
        status.message = format!(
            "Creating cloud snapshot of volume {} (PVC: {})",
            req.volume_id, req.pvc_name
        );
        info!(
            volume_id = %req.volume_id,
            description = %req.description,
            "requesting cloud-provider volume snapshot"
        );

        let snap_result = self
            .cloud
            .create_snapshot(
                &req.volume_id,
                &req.description,
                req.tags
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect::<Vec<_>>()
                    .as_slice(),
            )
            .await;

        // ── Step 3: resume Stellar Core writes regardless of snapshot outcome ─
        if pause_result.is_ok() {
            info!("resuming Stellar Core writes");
            if let Err(e) = self.resume_stellar_core_writes().await {
                error!(error = %e, "failed to resume Stellar Core writes after snapshot");
            }
        }

        // Now check whether the snapshot creation itself succeeded.
        let snap = snap_result?;

        status.snapshot_id = Some(snap.snapshot_id.clone());
        status.created_at = Some(snap.created_at.clone());
        info!(
            snapshot_id = %snap.snapshot_id,
            created_at = %snap.created_at,
            "cloud snapshot created; polling for ready state"
        );

        // ── Step 4: poll until snapshot ready ────────────────────────────────
        self.wait_for_snapshot_ready(&snap.snapshot_id).await?;

        // ── Step 5: build PVC clone spec ─────────────────────────────────────
        let pvc_clone_spec = PvcCloneSpec {
            storage_class_name: self.config.default_storage_class.clone(),
            snapshot_id: snap.snapshot_id.clone(),
            storage_request: snap.size_gib.map(|g| format!("{g}Gi")).unwrap_or_else(|| "100Gi".to_string()),
            access_modes: vec!["ReadWriteOnce".to_string()],
            annotations: vec![
                (
                    "stellar.org/snapshot-source-pvc".to_string(),
                    req.pvc_name.clone(),
                ),
                (
                    "stellar.org/snapshot-id".to_string(),
                    snap.snapshot_id.clone(),
                ),
                (
                    "stellar.org/snapshot-namespace".to_string(),
                    req.namespace.clone(),
                ),
            ],
        };

        status.phase = SnapshotPhase::Ready;
        status.message = format!(
            "Snapshot {} is ready; PVC clone spec generated",
            snap.snapshot_id
        );
        status.pvc_clone_spec = Some(pvc_clone_spec);

        info!(
            snapshot_id = %snap.snapshot_id,
            pvc = %req.pvc_name,
            "snapshot lifecycle complete"
        );

        Ok(status)
    }

    /// Verifies that a Stellar node bootstrapped from a snapshot successfully
    /// acquires network sync within [`SnapshotControllerConfig::sync_verification_timeout`]
    /// (default 5 minutes, as required by issue #243).
    ///
    /// The check polls the node's Stellar Core `/info` endpoint and looks for
    /// `state == "Synced!"` in the response JSON.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotError::SyncTimeout`] if the node does not sync in time.
    #[instrument(skip(self), fields(node_name = %node_name, horizon_url = %horizon_url))]
    pub async fn verify_sync(
        &self,
        node_name: &str,
        horizon_url: &str,
    ) -> Result<(), SnapshotError> {
        let deadline = Instant::now() + self.config.sync_verification_timeout;
        let mut last_ledger: u64 = 0;

        info!(
            node_name = %node_name,
            timeout = ?self.config.sync_verification_timeout,
            "waiting for node to sync after snapshot restore"
        );

        while Instant::now() < deadline {
            match self.query_stellar_core_info(horizon_url).await {
                Ok(info) => {
                    last_ledger = info.ledger_num;
                    debug!(
                        node_name = %node_name,
                        state = %info.state,
                        ledger = info.ledger_num,
                        "node sync status"
                    );
                    if info.state.contains("Synced") {
                        info!(
                            node_name = %node_name,
                            ledger = info.ledger_num,
                            elapsed = ?deadline - Instant::now(),
                            "node successfully synced after snapshot restore"
                        );
                        return Ok(());
                    }
                }
                Err(e) => {
                    debug!(error = %e, "error querying Stellar Core info; will retry");
                }
            }
            tokio::time::sleep(self.config.stellar_core.poll_interval).await;
        }

        Err(SnapshotError::SyncTimeout {
            node_name: node_name.to_string(),
            timeout: self.config.sync_verification_timeout,
            last_ledger,
        })
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Sends `POST /management/stop-writes` to the local Stellar Core instance.
    async fn pause_stellar_core_writes(&self) -> Result<(), SnapshotError> {
        let url = format!(
            "{}/management/stop-writes",
            self.config.stellar_core.management_url.trim_end_matches('/')
        );
        let start = Instant::now();
        let pause_timeout = self.config.stellar_core.pause_timeout;

        loop {
            let resp = self
                .http
                .post(&url)
                .send()
                .await
                .map_err(|e| SnapshotError::Transport {
                    url: url.clone(),
                    source: e,
                })?;

            let status_code = resp.status().as_u16();
            if resp.status().is_success() {
                return Ok(());
            }

            let body = resp.text().await.unwrap_or_default();

            // Stellar Core returns 503 when a previous pause is still in effect
            // — keep retrying until the deadline.
            if status_code == 503 && start.elapsed() < pause_timeout {
                debug!(
                    elapsed = ?start.elapsed(),
                    "Stellar Core not ready to pause writes; retrying"
                );
                tokio::time::sleep(self.config.stellar_core.poll_interval).await;
                continue;
            }

            if start.elapsed() >= pause_timeout {
                return Err(SnapshotError::PauseTimeout {
                    elapsed: start.elapsed(),
                });
            }

            return Err(SnapshotError::StellarCoreApi {
                url,
                status: status_code,
                body,
            });
        }
    }

    /// Sends `POST /management/resume-writes` to re-enable normal Stellar Core
    /// operation after the snapshot has been initiated.
    async fn resume_stellar_core_writes(&self) -> Result<(), SnapshotError> {
        let url = format!(
            "{}/management/resume-writes",
            self.config.stellar_core.management_url.trim_end_matches('/')
        );

        let resp = self
            .http
            .post(&url)
            .send()
            .await
            .map_err(|e| SnapshotError::Transport {
                url: url.clone(),
                source: e,
            })?;

        if resp.status().is_success() {
            return Ok(());
        }

        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Err(SnapshotError::StellarCoreApi { url, status, body })
    }

    /// Polls the cloud provider until the snapshot reaches the `Ready` state.
    async fn wait_for_snapshot_ready(&self, snapshot_id: &str) -> Result<(), SnapshotError> {
        let deadline = Instant::now() + self.config.snapshot_ready_timeout;

        while Instant::now() < deadline {
            match self.cloud.describe_snapshot(snapshot_id).await {
                Ok(snap) if snap.ready => {
                    info!(snapshot_id = %snapshot_id, "cloud snapshot is ready");
                    return Ok(());
                }
                Ok(snap) => {
                    debug!(
                        snapshot_id = %snapshot_id,
                        progress = %snap.progress_pct.unwrap_or(0),
                        "snapshot not yet ready; polling"
                    );
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        snapshot_id = %snapshot_id,
                        "error describing snapshot; will retry"
                    );
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }

        Err(SnapshotError::SnapshotTimeout {
            snapshot_id: snapshot_id.to_string(),
            timeout: self.config.snapshot_ready_timeout,
        })
    }

    /// Queries the Stellar Core `/info` endpoint for sync state and ledger
    /// sequence number.
    async fn query_stellar_core_info(
        &self,
        core_url: &str,
    ) -> Result<StellarCoreInfo, SnapshotError> {
        let url = format!("{}/info", core_url.trim_end_matches('/'));
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| SnapshotError::Transport {
                url: url.clone(),
                source: e,
            })?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(SnapshotError::StellarCoreApi { url, status, body });
        }

        let raw: serde_json::Value = resp.json().await.map_err(|e| SnapshotError::Transport {
            url: url.clone(),
            source: e,
        })?;

        let state = raw["info"]["state"]
            .as_str()
            .unwrap_or("Unknown")
            .to_string();
        let ledger_num = raw["info"]["ledger"]["num"].as_u64().unwrap_or(0);

        Ok(StellarCoreInfo { state, ledger_num })
    }
}

/// Minimal Stellar Core `/info` response fields consumed by the sync verifier.
#[derive(Debug)]
struct StellarCoreInfo {
    state: String,
    ledger_num: u64,
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshots::cloud_api::{
        CloudSnapshotError, CloudSnapshotResult, MockSnapshotClient, SnapshotDescribeResult,
    };

    fn make_controller(cloud: Arc<dyn SnapshotClient>) -> SnapshotController {
        SnapshotController::new(SnapshotControllerConfig::default(), cloud).unwrap()
    }

    // ── Unit tests for SnapshotPhase Display ─────────────────────────────────

    #[test]
    fn snapshot_phase_display() {
        assert_eq!(SnapshotPhase::Pending.to_string(), "Pending");
        assert_eq!(SnapshotPhase::PausingWrites.to_string(), "PausingWrites");
        assert_eq!(SnapshotPhase::Creating.to_string(), "Creating");
        assert_eq!(SnapshotPhase::Ready.to_string(), "Ready");
        assert_eq!(SnapshotPhase::Failed.to_string(), "Failed");
    }

    // ── Unit tests for config validation ─────────────────────────────────────

    #[test]
    fn rejects_empty_storage_class() {
        let mut cfg = SnapshotControllerConfig::default();
        cfg.default_storage_class = String::new();
        let cloud = Arc::new(MockSnapshotClient::new_always_success());
        let result = SnapshotController::new(cfg, cloud);
        assert!(matches!(
            result,
            Err(SnapshotError::InvalidConfig { .. })
        ));
    }

    // ── Integration-style tests using MockSnapshotClient ─────────────────────

    #[tokio::test]
    async fn run_snapshot_succeeds_with_mock_cloud() {
        let cloud = Arc::new(MockSnapshotClient::new_always_success());
        let controller = make_controller(cloud);

        let req = SnapshotRequest {
            namespace: "stellar".to_string(),
            pvc_name: "validator-data".to_string(),
            volume_id: "vol-0abc1234567890def".to_string(),
            description: "pre-scale snapshot".to_string(),
            tags: vec![
                ("app".to_string(), "stellar-validator".to_string()),
                ("issue".to_string(), "243".to_string()),
            ],
        };

        // The mock never contacts real Stellar Core or AWS/GCP. The controller
        // will fail at the Stellar Core pause step (no server running) but
        // should proceed with best-effort crash-consistent snapshot.
        let result = controller.run_snapshot(&req).await;
        assert!(
            result.is_ok(),
            "expected snapshot to succeed with mock cloud; got {result:?}"
        );

        let status = result.unwrap();
        assert_eq!(status.phase, SnapshotPhase::Ready);
        assert!(status.snapshot_id.is_some());
        assert!(status.pvc_clone_spec.is_some());

        let spec = status.pvc_clone_spec.unwrap();
        assert_eq!(spec.access_modes, vec!["ReadWriteOnce"]);
        assert!(spec
            .annotations
            .iter()
            .any(|(k, _)| k == "stellar.org/snapshot-source-pvc"));
    }

    #[tokio::test]
    async fn run_snapshot_propagates_cloud_error() {
        let cloud = Arc::new(MockSnapshotClient::new_always_error());
        let controller = make_controller(cloud);

        let req = SnapshotRequest {
            namespace: "stellar".to_string(),
            pvc_name: "validator-data".to_string(),
            volume_id: "vol-bad".to_string(),
            description: "failing snapshot".to_string(),
            tags: vec![],
        };

        let result = controller.run_snapshot(&req).await;
        assert!(
            matches!(result, Err(SnapshotError::Cloud(_))),
            "expected cloud error; got {result:?}"
        );
    }

    #[tokio::test]
    async fn pvc_clone_spec_has_correct_snapshot_id() {
        let cloud = Arc::new(MockSnapshotClient::new_always_success());
        let controller = make_controller(cloud);

        let req = SnapshotRequest {
            namespace: "stellar".to_string(),
            pvc_name: "validator-data".to_string(),
            volume_id: "vol-0abc1234567890def".to_string(),
            description: "check spec snapshot id".to_string(),
            tags: vec![],
        };

        let status = controller.run_snapshot(&req).await.unwrap();
        let spec = status.pvc_clone_spec.unwrap();
        let snap_id = status.snapshot_id.unwrap();
        assert_eq!(spec.snapshot_id, snap_id);
    }

    /// Verifies that the PVC clone annotation records the source PVC name so
    /// operators can trace a cloned PVC back to its origin.
    #[tokio::test]
    async fn pvc_clone_spec_annotation_source_pvc() {
        let cloud = Arc::new(MockSnapshotClient::new_always_success());
        let controller = make_controller(cloud);

        let req = SnapshotRequest {
            namespace: "stellar".to_string(),
            pvc_name: "my-special-pvc".to_string(),
            volume_id: "vol-0abc1234567890def".to_string(),
            description: "source pvc annotation test".to_string(),
            tags: vec![],
        };

        let status = controller.run_snapshot(&req).await.unwrap();
        let spec = status.pvc_clone_spec.unwrap();
        let source_ann = spec
            .annotations
            .iter()
            .find(|(k, _)| k == "stellar.org/snapshot-source-pvc")
            .map(|(_, v)| v.clone());
        assert_eq!(source_ann.as_deref(), Some("my-special-pvc"));
    }
}
