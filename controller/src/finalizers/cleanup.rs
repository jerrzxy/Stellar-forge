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

//! Background garbage-collection reconciler for orphaned finalizers.
//!
//! # Overview
//!
//! When the Stellar-K8s controller process is interrupted mid-deletion (e.g.
//! a pod crash, OOM kill, or node failure), a `StellarNode` resource can become
//! permanently stuck in the `Terminating` state.  The resource has a non-zero
//! `deletionTimestamp` but the finalizer `stellarnode.stellar.org/finalizer`
//! is never removed because the controller that would remove it is no longer
//! running.
//!
//! This module implements the recovery path:
//!
//! 1. **Detect** — List all `StellarNode` resources with a non-zero
//!    `deletionTimestamp`.
//! 2. **Verify** — For each such resource, locate its `PersistentVolumeClaim`
//!    and query the cloud provider API to confirm the underlying EBS/GCP PD
//!    volume has no active attachments.
//! 3. **Recover** — Only after the cloud API confirms full detachment, patch
//!    the resource to remove the stuck finalizer.
//!
//! # Safety Guarantees
//!
//! The reconciler **never** removes a finalizer unless
//! [`CloudVolumeVerifier::is_volume_detached`] returns `Ok(true)`.  A
//! temporary API error causes the reconciler to log a warning and retry on the
//! next cycle.  This ensures that no volume is released prematurely, preventing
//! data leaks or filesystem corruption.
//!
//! # Crash-Recovery Scenario
//!
//! Simulate: controller crashes while deleting a 500 GiB validator node.
//!
//! 1. Kubernetes sets `deletionTimestamp` on the `StellarNode`.
//! 2. Controller process is killed before it removes the finalizer.
//! 3. On restart, [`FinalizerCleanupReconciler::run_once`] finds the resource.
//! 4. It queries the cloud API — the EBS volume detaches after the pod exits.
//! 5. Once `is_volume_detached` returns `true`, the finalizer is patched away.
//! 6. Kubernetes garbage-collects the now-finalizer-free resource.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tracing::{debug, error, info, warn};

use super::cloud_verify::CloudVolumeVerifier;

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// The finalizer string registered on `StellarNode` resources.
pub const STELLAR_FINALIZER: &str = "stellarnode.stellar.org/finalizer";

/// Annotation key used to record the cloud-provider type on a `StellarNode`.
/// Expected values: `"aws"`, `"aws-ebs"`, `"gcp"`, `"gcp-pd"`.
pub const ANNOTATION_CLOUD_PROVIDER: &str = "stellar.org/cloud-provider";

/// Annotation key used to record the cloud region or zone on a `StellarNode`.
pub const ANNOTATION_CLOUD_REGION: &str = "stellar.org/cloud-region";

/// Annotation key on the associated `PersistentVolumeClaim` / `PersistentVolume`
/// that holds the cloud-provider volume identifier (e.g. `vol-0abc1234`).
pub const ANNOTATION_VOLUME_ID: &str = "stellar.org/volume-id";

/// Default polling interval for the background GC loop.
pub const DEFAULT_GC_INTERVAL: Duration = Duration::from_secs(30);

// ─────────────────────────────────────────────────────────────────────────────
// Orphaned resource descriptor
// ─────────────────────────────────────────────────────────────────────────────

/// A lightweight description of a `StellarNode` that is stuck in the
/// `Terminating` state with a non-zero `deletionTimestamp`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanedResource {
    /// Kubernetes namespace of the resource.
    pub namespace: String,
    /// Name of the `StellarNode` resource.
    pub name: String,
    /// Cloud-provider type annotation value (`"aws"`, `"gcp"`, …).
    pub cloud_provider: String,
    /// Cloud region or zone (`"us-east-1"`, `"us-central1-a"`, …).
    pub region_or_zone: String,
    /// Cloud volume ID extracted from the associated PVC annotation.
    pub volume_id: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Outcome type
// ─────────────────────────────────────────────────────────────────────────────

/// The outcome of a single reconciliation attempt for one orphaned resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileOutcome {
    /// The finalizer was successfully removed.
    FinalizerRemoved,
    /// The volume is not yet detached; the reconciler will retry.
    VolumeStillAttached,
    /// The cloud API returned an error; the reconciler will retry.
    VerificationError(String),
    /// The resource did not carry the expected finalizer (already clean).
    NoFinalizerPresent,
}

// ─────────────────────────────────────────────────────────────────────────────
// Kubernetes client abstraction
// ─────────────────────────────────────────────────────────────────────────────

/// Abstraction over the Kubernetes API operations required by the GC reconciler.
///
/// A concrete implementation wraps `kube::Api`; tests inject a
/// [`MockKubeClient`] to avoid requiring a live cluster.
#[async_trait::async_trait]
pub trait KubeClient: Send + Sync {
    /// Lists all `StellarNode` resources across all namespaces that have a
    /// non-zero `deletionTimestamp`.
    async fn list_terminating_nodes(&self) -> anyhow::Result<Vec<OrphanedResource>>;

    /// Patches the named `StellarNode` in `namespace` to remove the
    /// `stellarnode.stellar.org/finalizer` entry from its `finalizers` list.
    async fn remove_finalizer(
        &self,
        namespace: &str,
        name: &str,
    ) -> anyhow::Result<()>;
}

// ─────────────────────────────────────────────────────────────────────────────
// Reconciler
// ─────────────────────────────────────────────────────────────────────────────

/// Background garbage-collection reconciler that detects and resolves stuck
/// `StellarNode` finalizers.
///
/// # Usage
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use controller::finalizers::cleanup::FinalizerCleanupReconciler;
/// use controller::finalizers::cloud_verify::MockCloudVerifier;
///
/// // In production, replace with a real kube client and cloud verifier.
/// // let kube = Arc::new(ProductionKubeClient::new(client));
/// // let verifier = Arc::new(AwsEbsVerifier::new());
/// // let reconciler = FinalizerCleanupReconciler::new(kube, verifier);
/// // reconciler.run(DEFAULT_GC_INTERVAL).await;
/// ```
pub struct FinalizerCleanupReconciler<K, V>
where
    K: KubeClient,
    V: CloudVolumeVerifier,
{
    kube: Arc<K>,
    verifier: Arc<V>,
}

impl<K, V> FinalizerCleanupReconciler<K, V>
where
    K: KubeClient,
    V: CloudVolumeVerifier,
{
    /// Creates a new reconciler with the given Kubernetes and cloud verifier
    /// implementations.
    pub fn new(kube: Arc<K>, verifier: Arc<V>) -> Self {
        Self { kube, verifier }
    }

    /// Runs a single garbage-collection pass over all `Terminating`
    /// `StellarNode` resources.
    ///
    /// For each orphaned resource the reconciler:
    ///
    /// 1. Calls [`CloudVolumeVerifier::is_volume_detached`].
    /// 2. If detached → patches the resource to remove the finalizer.
    /// 3. Otherwise → logs the reason and leaves the resource for the next cycle.
    ///
    /// Returns the list of outcomes so callers (and tests) can inspect what
    /// happened without relying on log output.
    pub async fn run_once(&self) -> Vec<(OrphanedResource, ReconcileOutcome)> {
        let orphans = match self.kube.list_terminating_nodes().await {
            Ok(list) => list,
            Err(e) => {
                error!(error = %e, "failed to list terminating StellarNode resources");
                return vec![];
            }
        };

        if orphans.is_empty() {
            debug!("no terminating StellarNode resources found");
            return vec![];
        }

        info!(count = orphans.len(), "found terminating StellarNode resources");

        let mut results = Vec::with_capacity(orphans.len());

        for orphan in orphans {
            let outcome = self.reconcile_one(&orphan).await;

            match &outcome {
                ReconcileOutcome::FinalizerRemoved => info!(
                    namespace = %orphan.namespace,
                    name = %orphan.name,
                    "finalizer removed — resource can now be garbage-collected"
                ),
                ReconcileOutcome::VolumeStillAttached => info!(
                    namespace = %orphan.namespace,
                    name = %orphan.name,
                    volume_id = %orphan.volume_id,
                    "volume still attached; will retry next cycle"
                ),
                ReconcileOutcome::VerificationError(msg) => warn!(
                    namespace = %orphan.namespace,
                    name = %orphan.name,
                    error = %msg,
                    "cloud verification error; will retry next cycle"
                ),
                ReconcileOutcome::NoFinalizerPresent => debug!(
                    namespace = %orphan.namespace,
                    name = %orphan.name,
                    "no finalizer present; resource is already clean"
                ),
            }

            results.push((orphan, outcome));
        }

        results
    }

    /// Reconciles a single orphaned resource.
    async fn reconcile_one(&self, orphan: &OrphanedResource) -> ReconcileOutcome {
        // Step 1: Verify the volume is fully detached.
        match self
            .verifier
            .is_volume_detached(&orphan.volume_id, &orphan.region_or_zone)
            .await
        {
            Err(e) => {
                return ReconcileOutcome::VerificationError(e.to_string());
            }
            Ok(false) => {
                return ReconcileOutcome::VolumeStillAttached;
            }
            Ok(true) => {
                // Volume is confirmed detached; proceed to remove the finalizer.
                info!(
                    namespace = %orphan.namespace,
                    name = %orphan.name,
                    volume_id = %orphan.volume_id,
                    provider = %self.verifier.provider_name(),
                    "cloud API confirmed volume detached; removing finalizer"
                );
            }
        }

        // Step 2: Remove the finalizer via a Kubernetes patch.
        match self
            .kube
            .remove_finalizer(&orphan.namespace, &orphan.name)
            .await
        {
            Ok(()) => ReconcileOutcome::FinalizerRemoved,
            Err(e) if e.to_string().contains("not found") => {
                ReconcileOutcome::NoFinalizerPresent
            }
            Err(e) => {
                error!(
                    namespace = %orphan.namespace,
                    name = %orphan.name,
                    error = %e,
                    "failed to patch StellarNode to remove finalizer"
                );
                ReconcileOutcome::VerificationError(e.to_string())
            }
        }
    }

    /// Runs the GC reconciler in an infinite loop with the given poll interval.
    ///
    /// This is the entry-point for the background task spawned by the operator.
    /// It runs until the tokio runtime is shut down.
    pub async fn run(self: Arc<Self>, interval: Duration) {
        info!(
            interval_secs = interval.as_secs(),
            "finalizer cleanup reconciler started"
        );

        loop {
            self.run_once().await;
            tokio::time::sleep(interval).await;
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Mock Kubernetes client for tests
// ─────────────────────────────────────────────────────────────────────────────

/// A mock Kubernetes client for unit-testing the reconciler without a live
/// cluster.
///
/// Configure it with a list of orphaned resources to return from
/// `list_terminating_nodes`.  Call [`MockKubeClient::removed_finalizers`] after
/// a reconcile pass to inspect which resources were patched.
pub struct MockKubeClient {
    orphans: Vec<OrphanedResource>,
    /// Tracks the `(namespace, name)` pairs whose finalizers were removed.
    removed: std::sync::Mutex<Vec<(String, String)>>,
    /// If set, `remove_finalizer` returns this error.
    remove_error: Option<String>,
}

impl MockKubeClient {
    /// Creates a new mock with the given list of orphaned resources.
    pub fn new(orphans: Vec<OrphanedResource>) -> Self {
        Self {
            orphans,
            removed: std::sync::Mutex::new(vec![]),
            remove_error: None,
        }
    }

    /// Creates a mock whose `remove_finalizer` always returns an error.
    pub fn with_remove_error(orphans: Vec<OrphanedResource>, msg: impl Into<String>) -> Self {
        Self {
            orphans,
            removed: std::sync::Mutex::new(vec![]),
            remove_error: Some(msg.into()),
        }
    }

    /// Returns a snapshot of all `(namespace, name)` pairs whose finalizers
    /// have been successfully removed by the reconciler.
    pub fn removed_finalizers(&self) -> Vec<(String, String)> {
        self.removed.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl KubeClient for MockKubeClient {
    async fn list_terminating_nodes(&self) -> anyhow::Result<Vec<OrphanedResource>> {
        Ok(self.orphans.clone())
    }

    async fn remove_finalizer(
        &self,
        namespace: &str,
        name: &str,
    ) -> anyhow::Result<()> {
        if let Some(ref msg) = self.remove_error {
            return Err(anyhow::anyhow!("{}", msg));
        }
        self.removed
            .lock()
            .unwrap()
            .push((namespace.to_owned(), name.to_owned()));
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Patch builder helper (used by production KubeClient implementations)
// ─────────────────────────────────────────────────────────────────────────────

/// Builds the JSON merge-patch payload that removes the Stellar finalizer from
/// a `StellarNode`'s `metadata.finalizers` list.
///
/// The patch uses `null` to indicate removal; Kubernetes interprets a `null`
/// item in the strategic-merge-patch `$deleteFromPrimitiveList` as a removal
/// instruction.  For a simpler approach with full kube-rs support, use
/// [`build_finalizer_remove_patch`] with the array-replacement strategy.
pub fn build_finalizer_remove_patch(remaining_finalizers: &[String]) -> serde_json::Value {
    json!({
        "metadata": {
            "finalizers": remaining_finalizers
        }
    })
}

/// Returns a copy of `finalizers` with `STELLAR_FINALIZER` removed.
pub fn strip_stellar_finalizer(finalizers: &[String]) -> Vec<String> {
    finalizers
        .iter()
        .filter(|f| f.as_str() != STELLAR_FINALIZER)
        .cloned()
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finalizers::cloud_verify::MockCloudVerifier;

    fn sample_orphan(name: &str) -> OrphanedResource {
        OrphanedResource {
            namespace: "stellar".to_owned(),
            name: name.to_owned(),
            cloud_provider: "aws".to_owned(),
            region_or_zone: "us-east-1".to_owned(),
            volume_id: "vol-0abc1234567890ab".to_owned(),
        }
    }

    // ── Happy path: volume detached ────────────────────────────────────────

    #[tokio::test]
    async fn finalizer_removed_when_volume_is_detached() {
        let orphan = sample_orphan("my-validator");
        let kube = Arc::new(MockKubeClient::new(vec![orphan.clone()]));
        let verifier = Arc::new(MockCloudVerifier::detached());
        let reconciler = FinalizerCleanupReconciler::new(kube.clone(), verifier);

        let results = reconciler.run_once().await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, ReconcileOutcome::FinalizerRemoved);

        let removed = kube.removed_finalizers();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0], ("stellar".to_owned(), "my-validator".to_owned()));
    }

    // ── Volume still attached ──────────────────────────────────────────────

    #[tokio::test]
    async fn finalizer_not_removed_when_volume_still_attached() {
        let orphan = sample_orphan("still-running-validator");
        let kube = Arc::new(MockKubeClient::new(vec![orphan]));
        let verifier = Arc::new(MockCloudVerifier::attached());
        let reconciler = FinalizerCleanupReconciler::new(kube.clone(), verifier);

        let results = reconciler.run_once().await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, ReconcileOutcome::VolumeStillAttached);

        // Nothing should have been patched.
        assert!(kube.removed_finalizers().is_empty());
    }

    // ── Cloud API error ────────────────────────────────────────────────────

    #[tokio::test]
    async fn finalizer_not_removed_on_cloud_api_error() {
        let orphan = sample_orphan("flapping-validator");
        let kube = Arc::new(MockKubeClient::new(vec![orphan]));
        let verifier = Arc::new(MockCloudVerifier::error("EC2 rate limit exceeded"));
        let reconciler = FinalizerCleanupReconciler::new(kube.clone(), verifier);

        let results = reconciler.run_once().await;

        assert_eq!(results.len(), 1);
        assert!(
            matches!(&results[0].1, ReconcileOutcome::VerificationError(msg) if msg.contains("EC2 rate limit")),
            "expected VerificationError, got {:?}",
            results[0].1
        );

        assert!(kube.removed_finalizers().is_empty());
    }

    // ── Kube patch error ───────────────────────────────────────────────────

    #[tokio::test]
    async fn verification_error_returned_on_kube_patch_failure() {
        let orphan = sample_orphan("patch-fail-validator");
        let kube = Arc::new(MockKubeClient::with_remove_error(
            vec![orphan],
            "connection refused",
        ));
        let verifier = Arc::new(MockCloudVerifier::detached());
        let reconciler = FinalizerCleanupReconciler::new(kube.clone(), verifier);

        let results = reconciler.run_once().await;

        assert_eq!(results.len(), 1);
        assert!(
            matches!(&results[0].1, ReconcileOutcome::VerificationError(_)),
            "Kubernetes patch failure must yield VerificationError"
        );
    }

    // ── Empty list ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn empty_list_returns_no_results() {
        let kube = Arc::new(MockKubeClient::new(vec![]));
        let verifier = Arc::new(MockCloudVerifier::detached());
        let reconciler = FinalizerCleanupReconciler::new(kube, verifier);

        let results = reconciler.run_once().await;
        assert!(results.is_empty());
    }

    // ── Multiple resources ─────────────────────────────────────────────────

    #[tokio::test]
    async fn handles_multiple_orphaned_resources() {
        let orphans = vec![
            sample_orphan("validator-1"),
            sample_orphan("validator-2"),
            sample_orphan("validator-3"),
        ];
        let kube = Arc::new(MockKubeClient::new(orphans));
        let verifier = Arc::new(MockCloudVerifier::detached());
        let reconciler = FinalizerCleanupReconciler::new(kube.clone(), verifier);

        let results = reconciler.run_once().await;

        assert_eq!(results.len(), 3);
        for (_, outcome) in &results {
            assert_eq!(*outcome, ReconcileOutcome::FinalizerRemoved);
        }
        assert_eq!(kube.removed_finalizers().len(), 3);
    }

    // ── crash-recovery simulation ──────────────────────────────────────────

    /// Simulates the crash-recovery scenario described in the issue:
    /// controller crashes mid-deletion of a 500 GiB validator node, then
    /// restarts and the GC reconciler resolves the stuck finalizer.
    #[tokio::test]
    async fn crash_recovery_simulation() {
        // On first run the volume is still attached (controller was killed
        // before the pod finished detaching the EBS volume).
        let orphan = OrphanedResource {
            namespace: "stellar".to_owned(),
            name: "big-validator-500gi".to_owned(),
            cloud_provider: "aws".to_owned(),
            region_or_zone: "us-east-1".to_owned(),
            volume_id: "vol-0deadbeef500gb00".to_owned(),
        };

        let kube = Arc::new(MockKubeClient::new(vec![orphan.clone()]));

        // First reconcile pass: volume still attached (pod shutting down)
        let verifier_attached = Arc::new(MockCloudVerifier::attached());
        let reconciler = FinalizerCleanupReconciler::new(kube.clone(), verifier_attached);
        let results = reconciler.run_once().await;
        assert_eq!(results[0].1, ReconcileOutcome::VolumeStillAttached);
        assert!(kube.removed_finalizers().is_empty(), "must not remove finalizer while volume is still attached");

        // Second reconcile pass: volume has detached
        let verifier_detached = Arc::new(MockCloudVerifier::detached());
        let reconciler2 = FinalizerCleanupReconciler::new(kube.clone(), verifier_detached);
        let results2 = reconciler2.run_once().await;
        assert_eq!(results2[0].1, ReconcileOutcome::FinalizerRemoved);
        assert_eq!(kube.removed_finalizers().len(), 1);
        assert_eq!(
            kube.removed_finalizers()[0],
            ("stellar".to_owned(), "big-validator-500gi".to_owned())
        );
    }

    // ── Patch builder ──────────────────────────────────────────────────────

    #[test]
    fn strip_stellar_finalizer_removes_only_stellar_entry() {
        let finalizers = vec![
            "stellarnode.stellar.org/finalizer".to_owned(),
            "other-operator.io/cleanup".to_owned(),
        ];
        let result = strip_stellar_finalizer(&finalizers);
        assert_eq!(result, vec!["other-operator.io/cleanup".to_owned()]);
    }

    #[test]
    fn strip_stellar_finalizer_noop_when_not_present() {
        let finalizers = vec!["other-operator.io/cleanup".to_owned()];
        let result = strip_stellar_finalizer(&finalizers);
        assert_eq!(result, finalizers);
    }

    #[test]
    fn build_finalizer_remove_patch_produces_expected_json() {
        let patch = build_finalizer_remove_patch(&["other.io/cleanup".to_owned()]);
        let expected = json!({
            "metadata": {
                "finalizers": ["other.io/cleanup"]
            }
        });
        assert_eq!(patch, expected);
    }
}
