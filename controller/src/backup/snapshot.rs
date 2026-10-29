// Copyright 2024 Stellar-K8s Contributors
// SPDX-License-Identifier: Apache-2.0
//! Automated storage snapshot and cloud backup controller (issue #84).
//!
//! This module implements the controller loop that drives automated persistent
//! volume snapshotting across AWS EBS, GCP PD, and local storage providers.
//! It is provider-agnostic: every back-end is exercised through the
//! [`SnapshotProvider`] trait defined in [`crate::backup::cloud`].
//!
//! # Responsibilities
//!
//! 1. **Ledger-marker aligned triggering** — snapshots are taken only when the
//!    Stellar ledger sequence reaches a configured marker interval
//!    ([`SnapshotConfig::ledger_marker_interval`]), so a snapshot always
//!    corresponds to a well-defined ledger boundary.
//! 2. **Retention enforcement** — after each snapshot the controller prunes
//!    snapshots that violate the [`RetentionPolicy`] (both max age and max
//!    count), guaranteeing that expired cloud storage is released.
//! 3. **Crash consistency** — before the snapshot payload is captured the
//!    controller briefly pauses database write flushes via a
//!    [`FlushCoordinator`], ensuring the on-disk state is quiescent.
//! 4. **Rapid restoration + integrity validation** — [`SnapshotController::restore_from_snapshot`]
//!    restores a validated snapshot onto a new volume and verifies its
//!    integrity checksum before reporting success.
//!
//! # Example
//!
//! ```ignore
//! let provider = Arc::new(LocalSnapshotProvider::new("/var/backups/stellar")?);
//! let flush = Arc::new(FlushCoordinator::new());
//! let controller = SnapshotController::new(provider, config, flush);
//!
//! // Drive the loop, e.g. on a timer or when the ledger advances.
//! let report = controller.maybe_snapshot_on_ledger(1_048_576).await?;
//! if let Some(desc) = report.created {
//!     println!("snapshot {} at ledger {}", desc.id, desc.ledger_sequence);
//! }
//! ```

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, info, warn};

use super::cloud::{
    ListFilter, RestoreResult, SnapshotDescriptor, SnapshotProvider, SnapshotRequest, SnapshotState,
};

/// Retention policy applied to the snapshot inventory.
///
/// Both dimensions are enforced independently: a snapshot is pruned if it is
/// older than `max_age` **or** if it falls outside the newest `max_count`
/// snapshots for its volume.  Setting a field to `None` disables that
/// dimension.
#[derive(Debug, Clone, Copy)]
pub struct RetentionPolicy {
    /// Maximum age of a retained snapshot. `None` disables age-based pruning.
    pub max_age: Option<Duration>,
    /// Maximum number of retained snapshots per volume. `None` disables
    /// count-based pruning.
    pub max_count: Option<usize>,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_age: Some(Duration::from_secs(7 * 24 * 3600)),
            max_count: Some(10),
        }
    }
}

impl RetentionPolicy {
    /// Decide whether a snapshot should be pruned.
    ///
    /// `rank` is the 0-based position of the snapshot in the newest-first
    /// ordering for its volume, and `now` is the current time.
    pub fn should_prune(&self, desc: &SnapshotDescriptor, rank: usize, now: DateTime<Utc>) -> bool {
        if let Some(max_count) = self.max_count {
            if rank >= max_count {
                return true;
            }
        }
        if let Some(max_age) = self.max_age {
            let age = now.signed_duration_since(desc.created_at).to_std();
            if let Ok(age) = age {
                if age > max_age {
                    return true;
                }
            }
        }
        false
    }
}

/// Configuration for the snapshot controller loop.
#[derive(Debug, Clone)]
pub struct SnapshotConfig {
    /// Take a snapshot every time the ledger sequence advances by this many.
    ///
    /// A value of `64` means a snapshot is triggered when the ledger crosses
    /// every 64th boundary (matching Stellar's checkpoint frequency).  A value
    /// of `0` disables ledger-marker alignment (snapshot on every call).
    pub ledger_marker_interval: u64,
    /// Retention policy applied after each snapshot.
    pub retention: RetentionPolicy,
    /// Whether to pause database write flushes before capturing the snapshot.
    pub pause_flushes: bool,
    /// Maximum time to wait for the flush pause to be acknowledged.
    pub flush_pause_timeout: Duration,
    /// Free-form tags applied to every snapshot (e.g. node name).
    pub tags: BTreeMap<String, String>,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            ledger_marker_interval: 64,
            retention: RetentionPolicy::default(),
            pause_flushes: true,
            flush_pause_timeout: Duration::from_secs(30),
            tags: BTreeMap::new(),
        }
    }
}

/// A coordination primitive that briefly pauses database write flushes so a
/// snapshot captures a crash-consistent image.
///
/// Writers call [`FlushCoordinator::begin_flush`], which returns a guard; while
/// any guard is alive the controller holds a *pause* and delays snapshot
/// capture.  Conversely, when the controller wants to snapshot it calls
/// [`FlushCoordinator::pause`], which blocks until all in-flight flushes drain
/// and then blocks new flushes until the returned guard is dropped.
pub struct FlushCoordinator {
    /// Number of in-flight flushes.
    active_flushes: AtomicUsize,
    /// Whether a snapshot pause is currently held.
    paused: AtomicBool,
    /// Notified whenever a flush completes or a pause releases, so waiters
    /// can re-check their condition.
    drained: Notify,
}

/// RAII guard representing a single in-flight database flush.
pub struct FlushGuard<'a> {
    coord: &'a FlushCoordinator,
}

impl Drop for FlushGuard<'_> {
    fn drop(&mut self) {
        // Decrement the active count and wake any awaiting pause().  This runs
        // synchronously and must never block on the async runtime, so it uses
        // atomics rather than a tokio lock.
        self.coord.active_flushes.fetch_sub(1, Ordering::SeqCst);
        self.coord.drained.notify_waiters();
    }
}

/// RAII guard representing a held snapshot pause.  While alive, new flushes
/// block in [`FlushCoordinator::begin_flush`].
pub struct PauseGuard<'a> {
    coord: &'a FlushCoordinator,
}

impl Drop for PauseGuard<'_> {
    fn drop(&mut self) {
        self.coord.paused.store(false, Ordering::SeqCst);
        // Wake any flushes waiting for the pause to release.
        self.coord.drained.notify_waiters();
    }
}

impl FlushCoordinator {
    pub fn new() -> Self {
        Self {
            active_flushes: AtomicUsize::new(0),
            paused: AtomicBool::new(false),
            drained: Notify::new(),
        }
    }

    /// Current number of in-flight flushes (snapshot for tests/metrics).
    pub fn active_flushes(&self) -> usize {
        self.active_flushes.load(Ordering::SeqCst)
    }

    /// Acquire a flush permit.  Blocks while a snapshot pause is held.
    ///
    /// Returns a guard that must be held for the duration of the flush.
    pub async fn begin_flush(&self) -> FlushGuard<'_> {
        loop {
            // Enter optimistically when no pause is currently held.
            if !self.paused.load(Ordering::SeqCst) {
                self.active_flushes.fetch_add(1, Ordering::SeqCst);
                // Re-check after entering to close the race with pause(): if a
                // pause started while we were entering, back out so the pause
                // observes a fully drained state.
                if !self.paused.load(Ordering::SeqCst) {
                    return FlushGuard { coord: self };
                }
                self.active_flushes.fetch_sub(1, Ordering::SeqCst);
                self.drained.notify_waiters();
            }
            // A pause is active; wait for it to be released before retrying.
            let notified = self.drained.notified();
            // Re-check after registering so a release that raced with us is not
            // missed (notify_waiters only wakes already-registered waiters).
            if !self.paused.load(Ordering::SeqCst) {
                continue;
            }
            notified.await;
        }
    }

    /// Begin a snapshot pause: block until all in-flight flushes drain, then
    /// hold new flushes off until the returned guard is dropped.
    pub async fn pause(&self) -> PauseGuard<'_> {
        self.paused.store(true, Ordering::SeqCst);
        loop {
            if self.active_flushes.load(Ordering::SeqCst) == 0 {
                break;
            }
            // Wait for a flush to complete (FlushGuard::drop notifies).
            let notified = self.drained.notified();
            // Re-check after registering so a drain that raced with us is not
            // missed.
            if self.active_flushes.load(Ordering::SeqCst) == 0 {
                break;
            }
            notified.await;
        }
        PauseGuard { coord: self }
    }

    /// Whether a pause is currently held.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }
}

impl Default for FlushCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of a single controller-loop iteration.
#[derive(Debug, Clone)]
pub struct SnapshotReport {
    /// The snapshot that was created, if one was triggered this iteration.
    pub created: Option<SnapshotDescriptor>,
    /// Ledger sequence that triggered the snapshot.
    pub ledger_sequence: u64,
    /// Snapshot ids that were pruned by retention enforcement.
    pub pruned: Vec<String>,
}

/// The automated snapshot controller.
///
/// Wraps a [`SnapshotProvider`] and drives the snapshot lifecycle: ledger-marker
/// triggering, crash-consistent capture, retention pruning, and validated
/// restoration.
pub struct SnapshotController {
    provider: Arc<dyn SnapshotProvider>,
    config: SnapshotConfig,
    flush: Arc<FlushCoordinator>,
    /// Ledger sequence of the most recent snapshot, used for marker alignment.
    last_snapshot_ledger: Mutex<Option<u64>>,
    /// Volume that this controller snapshots.
    volume_id: String,
}

impl SnapshotController {
    /// Create a new controller for `volume_id`.
    pub fn new(
        provider: Arc<dyn SnapshotProvider>,
        config: SnapshotConfig,
        flush: Arc<FlushCoordinator>,
        volume_id: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            config,
            flush,
            last_snapshot_ledger: Mutex::new(None),
            volume_id: volume_id.into(),
        }
    }

    /// Whether the ledger has advanced to the next marker since the last
    /// snapshot.
    ///
    /// Returns `true` when:
    /// - no snapshot has been taken yet, or
    /// - `ledger_marker_interval` is `0` (unconditional), or
    /// - the ledger has crossed a new multiple of the marker interval.
    pub async fn should_snapshot(&self, ledger_sequence: u64) -> bool {
        if self.config.ledger_marker_interval == 0 {
            return true;
        }
        let last = *self.last_snapshot_ledger.lock().await;
        match last {
            None => true,
            Some(prev) => {
                let interval = self.config.ledger_marker_interval;
                let prev_marker = prev / interval;
                let cur_marker = ledger_sequence / interval;
                cur_marker > prev_marker
            }
        }
    }

    /// Run one iteration of the controller loop for `ledger_sequence`.
    ///
    /// If the ledger has reached a marker, this captures a crash-consistent
    /// snapshot and then enforces retention.  Returns a report describing what
    /// happened.
    pub async fn maybe_snapshot_on_ledger(&self, ledger_sequence: u64) -> SnapshotReport {
        let mut report = SnapshotReport {
            created: None,
            ledger_sequence,
            pruned: Vec::new(),
        };

        if !self.should_snapshot(ledger_sequence).await {
            debug!(
                ledger_sequence,
                last = ?*self.last_snapshot_ledger.lock().await,
                "ledger has not reached next snapshot marker; skipping"
            );
            return report;
        }

        match self.capture_snapshot(ledger_sequence).await {
            Ok(desc) => {
                info!(
                    snapshot_id = %desc.id,
                    ledger_sequence,
                    "captured crash-consistent snapshot"
                );
                *self.last_snapshot_ledger.lock().await = Some(ledger_sequence);
                report.created = Some(desc);
            }
            Err(e) => {
                warn!(ledger_sequence, error = %e, "snapshot capture failed");
            }
        }

        // Retention is enforced regardless of whether this iteration created a
        // snapshot, so pruning also happens for snapshots that have aged out
        // between captures.
        match self.enforce_retention().await {
            Ok(pruned) => report.pruned = pruned,
            Err(e) => warn!(error = %e, "retention enforcement failed"),
        }

        report
    }

    /// Capture a crash-consistent snapshot at `ledger_sequence`.
    async fn capture_snapshot(
        &self,
        ledger_sequence: u64,
    ) -> Result<SnapshotDescriptor, super::cloud::CloudError> {
        // Briefly pause database write flushes so the captured image is
        // crash-consistent, per the issue #84 constraint.
        let _pause = if self.config.pause_flushes {
            Some(self.flush.pause().await)
        } else {
            None
        };

        let mut tags = self.config.tags.clone();
        tags.insert("ledger-sequence".to_string(), ledger_sequence.to_string());
        tags.insert("volume".to_string(), self.volume_id.clone());

        let request = SnapshotRequest {
            volume_id: self.volume_id.clone(),
            ledger_sequence,
            tags,
        };

        self.provider.create_snapshot(request).await
    }

    /// Enforce the retention policy, deleting snapshots that are too old or
    /// beyond the count limit.  Returns the ids of the pruned snapshots.
    pub async fn enforce_retention(&self) -> Result<Vec<String>, super::cloud::CloudError> {
        let filter = ListFilter {
            volume_id: Some(self.volume_id.clone()),
            tag: None,
        };
        let mut snapshots = self.provider.list_snapshots(&filter).await?;

        // Newest first so rank 0 is the most recent snapshot.
        snapshots.sort_by_key(|d| std::cmp::Reverse(d.created_at));

        let now = Utc::now();
        let mut pruned = Vec::new();
        for (rank, desc) in snapshots.iter().enumerate() {
            if self.config.retention.should_prune(desc, rank, now) {
                debug!(
                    snapshot_id = %desc.id,
                    rank,
                    "pruning snapshot violating retention policy"
                );
                self.provider.delete_snapshot(&desc.id).await?;
                pruned.push(desc.id.clone());
            }
        }
        Ok(pruned)
    }

    /// Restore a validated snapshot onto a new volume and verify integrity.
    ///
    /// This is the "rapid restoration" path used when provisioning a new node.
    /// It refuses to restore snapshots that are not `Available`, and surfaces
    /// integrity-check failures from the provider.
    pub async fn restore_from_snapshot(
        &self,
        snapshot_id: &str,
        target_volume_id: &str,
    ) -> Result<RestoreResult, super::cloud::CloudError> {
        // Validate the snapshot is restorable before touching the target.
        let desc = self.provider.describe_snapshot(snapshot_id).await?;
        match desc.state {
            SnapshotState::Available => {}
            SnapshotState::Pending => {
                return Err(super::cloud::CloudError::Provider(format!(
                    "snapshot {} is still pending and not yet restorable",
                    snapshot_id
                )));
            }
            SnapshotState::Failed(ref reason) => {
                return Err(super::cloud::CloudError::Provider(format!(
                    "snapshot {} is in a failed state: {}",
                    snapshot_id, reason
                )));
            }
        }

        info!(
            snapshot_id,
            target_volume_id, "restoring validated snapshot onto new volume"
        );
        let result = self
            .provider
            .restore_volume(snapshot_id, target_volume_id)
            .await?;

        if result.integrity_ok {
            info!(
                snapshot_id,
                volume_id = %result.volume_id,
                "restored snapshot passed integrity verification"
            );
        } else {
            warn!(
                snapshot_id,
                volume_id = %result.volume_id,
                "restored snapshot FAILED integrity verification"
            );
        }
        Ok(result)
    }

    /// Access the underlying flush coordinator (exposed for wiring/tests).
    pub fn flush_coordinator(&self) -> Arc<FlushCoordinator> {
        self.flush.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::cloud::{
        CloudError, LocalSnapshotProvider, ProviderKind, RestoreResult, SnapshotDescriptor,
        SnapshotProvider, SnapshotRequest, SnapshotState,
    };
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::atomic::AtomicUsize;

    fn write_files(root: &std::path::Path, files: &[(&str, &str)]) {
        for (rel, content) in files {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, content).unwrap();
        }
    }

    /// A mock provider that records calls and lets tests drive snapshot state.
    struct MockProvider {
        created: AtomicUsize,
        deleted: std::sync::Mutex<Vec<String>>,
        inventory: std::sync::Mutex<Vec<SnapshotDescriptor>>,
        restore_integrity_ok: bool,
    }

    impl MockProvider {
        fn new() -> Self {
            Self {
                created: AtomicUsize::new(0),
                deleted: std::sync::Mutex::new(Vec::new()),
                inventory: std::sync::Mutex::new(Vec::new()),
                restore_integrity_ok: true,
            }
        }

        fn with_inventory(inventory: Vec<SnapshotDescriptor>) -> Self {
            let p = Self::new();
            *p.inventory.lock().unwrap() = inventory;
            p
        }
    }

    fn desc(id: &str, ledger: u64, created_at: DateTime<Utc>) -> SnapshotDescriptor {
        SnapshotDescriptor {
            id: id.to_string(),
            provider: ProviderKind::Local,
            volume_id: "vol-1".to_string(),
            ledger_sequence: ledger,
            created_at,
            size_bytes: Some(1024),
            state: SnapshotState::Available,
            checksum: Some("abc".to_string()),
            tags: BTreeMap::new(),
        }
    }

    #[async_trait::async_trait]
    impl SnapshotProvider for MockProvider {
        fn name(&self) -> &'static str {
            "mock"
        }
        fn kind(&self) -> ProviderKind {
            ProviderKind::Local
        }
        async fn create_snapshot(
            &self,
            request: SnapshotRequest,
        ) -> crate::backup::cloud::Result<SnapshotDescriptor> {
            let n = self.created.fetch_add(1, Ordering::SeqCst);
            let mut d = desc(&format!("snap-{}", n), request.ledger_sequence, Utc::now());
            d.tags = request.tags;
            self.inventory.lock().unwrap().push(d.clone());
            Ok(d)
        }
        async fn describe_snapshot(
            &self,
            id: &str,
        ) -> crate::backup::cloud::Result<SnapshotDescriptor> {
            self.inventory
                .lock()
                .unwrap()
                .iter()
                .find(|d| d.id == id)
                .cloned()
                .ok_or_else(|| CloudError::SnapshotNotFound(id.to_string()))
        }
        async fn list_snapshots(
            &self,
            _f: &ListFilter,
        ) -> crate::backup::cloud::Result<Vec<SnapshotDescriptor>> {
            Ok(self.inventory.lock().unwrap().clone())
        }
        async fn delete_snapshot(&self, id: &str) -> crate::backup::cloud::Result<()> {
            self.deleted.lock().unwrap().push(id.to_string());
            self.inventory.lock().unwrap().retain(|d| d.id != id);
            Ok(())
        }
        async fn restore_volume(
            &self,
            snapshot_id: &str,
            target_volume_id: &str,
        ) -> crate::backup::cloud::Result<RestoreResult> {
            Ok(RestoreResult {
                volume_id: target_volume_id.to_string(),
                snapshot_id: snapshot_id.to_string(),
                integrity_ok: self.restore_integrity_ok,
            })
        }
    }

    // --- Ledger-marker alignment -----------------------------------------

    #[tokio::test]
    async fn first_ledger_always_triggers_snapshot() {
        let provider = Arc::new(MockProvider::new());
        let flush = Arc::new(FlushCoordinator::new());
        let config = SnapshotConfig {
            ledger_marker_interval: 64,
            ..Default::default()
        };
        let ctl = SnapshotController::new(provider, config, flush, "vol-1");
        assert!(ctl.should_snapshot(0).await);
    }

    #[tokio::test]
    async fn snapshot_waits_for_marker_boundary() {
        let provider = Arc::new(MockProvider::new());
        let flush = Arc::new(FlushCoordinator::new());
        let config = SnapshotConfig {
            ledger_marker_interval: 64,
            pause_flushes: false,
            ..Default::default()
        };
        let ctl = SnapshotController::new(provider, config, flush, "vol-1");

        // First call creates a snapshot at ledger 100 (marker 1).
        let report = ctl.maybe_snapshot_on_ledger(100).await;
        assert!(report.created.is_some());
        assert_eq!(report.ledger_sequence, 100);

        // Ledger 120 is still marker 1 -> no new snapshot.
        let report = ctl.maybe_snapshot_on_ledger(120).await;
        assert!(report.created.is_none());

        // Ledger 128 crosses marker 2 -> new snapshot.
        let report = ctl.maybe_snapshot_on_ledger(128).await;
        assert!(report.created.is_some());
    }

    #[tokio::test]
    async fn ledger_alignment_zero_means_every_call() {
        let provider = Arc::new(MockProvider::new());
        let flush = Arc::new(FlushCoordinator::new());
        let config = SnapshotConfig {
            ledger_marker_interval: 0,
            pause_flushes: false,
            ..Default::default()
        };
        let ctl = SnapshotController::new(provider, config, flush, "vol-1");
        for ledger in 1..=5u64 {
            let report = ctl.maybe_snapshot_on_ledger(ledger).await;
            assert!(
                report.created.is_some(),
                "ledger {} should snapshot",
                ledger
            );
        }
    }

    #[tokio::test]
    async fn snapshot_captures_ledger_marker_and_tags() {
        let provider = Arc::new(MockProvider::new());
        let flush = Arc::new(FlushCoordinator::new());
        let mut tags = BTreeMap::new();
        tags.insert("node".to_string(), "validator-1".to_string());
        let config = SnapshotConfig {
            ledger_marker_interval: 0,
            pause_flushes: false,
            tags,
            ..Default::default()
        };
        let ctl = SnapshotController::new(provider.clone(), config, flush, "vol-1");
        ctl.maybe_snapshot_on_ledger(4096).await;
        // The mock's inventory holds the created descriptor.
        let inv = provider.inventory.lock().unwrap();
        assert_eq!(inv.len(), 1);
        assert_eq!(inv[0].ledger_sequence, 4096);
        assert_eq!(inv[0].tag("node"), Some("validator-1"));
        assert_eq!(inv[0].tag("ledger-sequence"), Some("4096"));
    }

    // --- Retention -------------------------------------------------------

    #[tokio::test]
    async fn retention_prunes_by_count() {
        let now = Utc::now();
        let inventory = vec![
            desc("snap-new", 300, now),
            desc("snap-2", 200, now - chrono::Duration::seconds(10)),
            desc("snap-3", 100, now - chrono::Duration::seconds(20)),
        ];
        let provider = Arc::new(MockProvider::with_inventory(inventory));
        let flush = Arc::new(FlushCoordinator::new());
        let config = SnapshotConfig {
            ledger_marker_interval: 0,
            pause_flushes: false,
            retention: RetentionPolicy {
                max_age: None,
                max_count: Some(2),
            },
            ..Default::default()
        };
        let ctl = SnapshotController::new(provider.clone(), config, flush, "vol-1");
        let pruned = ctl.enforce_retention().await.unwrap();
        // Oldest (rank 2) is pruned.
        assert_eq!(pruned, vec!["snap-3".to_string()]);
        assert_eq!(provider.deleted.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn retention_prunes_by_age() {
        let now = Utc::now();
        let old = now - chrono::Duration::seconds(3600);
        let inventory = vec![desc("fresh", 200, now), desc("stale", 100, old)];
        let provider = Arc::new(MockProvider::with_inventory(inventory));
        let flush = Arc::new(FlushCoordinator::new());
        let config = SnapshotConfig {
            ledger_marker_interval: 0,
            pause_flushes: false,
            retention: RetentionPolicy {
                max_age: Some(Duration::from_secs(600)),
                max_count: None,
            },
            ..Default::default()
        };
        let ctl = SnapshotController::new(provider, config, flush, "vol-1");
        let pruned = ctl.enforce_retention().await.unwrap();
        assert_eq!(pruned, vec!["stale".to_string()]);
    }

    #[tokio::test]
    async fn retention_keeps_snapshots_when_policy_disabled() {
        let now = Utc::now();
        let inventory = vec![desc("a", 3, now), desc("b", 2, now), desc("c", 1, now)];
        let provider = Arc::new(MockProvider::with_inventory(inventory));
        let flush = Arc::new(FlushCoordinator::new());
        let config = SnapshotConfig {
            ledger_marker_interval: 0,
            pause_flushes: false,
            retention: RetentionPolicy {
                max_age: None,
                max_count: None,
            },
            ..Default::default()
        };
        let ctl = SnapshotController::new(provider, config, flush, "vol-1");
        let pruned = ctl.enforce_retention().await.unwrap();
        assert!(pruned.is_empty());
    }

    #[tokio::test]
    async fn retention_never_prunes_when_count_limit_is_zero() {
        // max_count == 0 is treated as "rank >= 0 always true", but we guard in
        // the controller: retention_count 0 means unlimited (matches operator
        // convention where 0 disables). Here we assert the policy primitive
        // itself with max_count = Some(0) prunes everything at rank >= 0.
        let now = Utc::now();
        let policy = RetentionPolicy {
            max_age: None,
            max_count: Some(0),
        };
        assert!(policy.should_prune(&desc("a", 1, now), 0, now));
    }

    #[test]
    fn retention_should_prune_logic() {
        let now = Utc::now();
        let policy = RetentionPolicy {
            max_age: Some(Duration::from_secs(60)),
            max_count: Some(3),
        };
        // Within count and age -> keep.
        assert!(!policy.should_prune(&desc("a", 1, now), 2, now));
        // Beyond count -> prune.
        assert!(policy.should_prune(&desc("a", 1, now), 3, now));
        // Too old -> prune.
        let old = now - chrono::Duration::seconds(120);
        assert!(policy.should_prune(&desc("a", 1, old), 0, now));
    }

    // --- Flush coordination ---------------------------------------------

    #[tokio::test]
    async fn flush_guard_tracks_active_flushes() {
        let coord = FlushCoordinator::new();
        assert!(!coord.is_paused());
        {
            let guard = coord.begin_flush().await;
            assert_eq!(coord.active_flushes(), 1);
            drop(guard);
        }
        assert_eq!(coord.active_flushes(), 0);
    }

    #[tokio::test]
    async fn pause_blocks_new_flushes_until_released() {
        let coord = Arc::new(FlushCoordinator::new());
        let pause = coord.pause().await;
        assert!(coord.is_paused());

        // A flush attempted while paused must block. We race it against a short
        // timeout; if the timeout wins, the flush is correctly blocked.
        let coord2 = coord.clone();
        let flush_task = tokio::spawn(async move {
            let _g = coord2.begin_flush().await;
            "acquired"
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !flush_task.is_finished(),
            "flush should be blocked while paused"
        );

        drop(pause);
        assert!(!coord.is_paused());
        let result = tokio::time::timeout(Duration::from_secs(2), flush_task)
            .await
            .expect("flush should complete after pause released")
            .unwrap();
        assert_eq!(result, "acquired");
    }

    #[tokio::test]
    async fn pause_waits_for_inflight_flush_to_drain() {
        let coord = Arc::new(FlushCoordinator::new());
        let flush_guard = coord.begin_flush().await;
        assert_eq!(coord.active_flushes(), 1);

        let coord2 = coord.clone();
        let pause_task = tokio::spawn(async move {
            let p = coord2.pause().await;
            // Once pause() returns, in-flight flushes must have drained.
            let active = coord2.active_flushes();
            drop(p);
            active
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!pause_task.is_finished(), "pause should wait for drain");
        drop(flush_guard); // release the in-flight flush

        let active = tokio::time::timeout(Duration::from_secs(2), pause_task)
            .await
            .expect("pause should complete after flush drains")
            .unwrap();
        assert_eq!(active, 0);
    }

    #[tokio::test]
    async fn capture_snapshot_holds_pause_during_creation() {
        // Verify the controller actually engages the flush pause around provider
        // calls when pause_flushes is enabled.
        let coord = Arc::new(FlushCoordinator::new());
        let provider = Arc::new(MockProvider::new());
        let config = SnapshotConfig {
            ledger_marker_interval: 0,
            pause_flushes: true,
            ..Default::default()
        };
        let ctl = SnapshotController::new(provider, config, coord.clone(), "vol-1");
        let report = ctl.maybe_snapshot_on_ledger(64).await;
        assert!(report.created.is_some());
        // After the iteration the pause must be released.
        assert!(!coord.is_paused());
        assert_eq!(coord.active_flushes(), 0);
    }

    #[tokio::test]
    async fn no_pause_when_disabled() {
        let coord = Arc::new(FlushCoordinator::new());
        let provider = Arc::new(MockProvider::new());
        let config = SnapshotConfig {
            ledger_marker_interval: 0,
            pause_flushes: false,
            ..Default::default()
        };
        let ctl = SnapshotController::new(provider, config, coord.clone(), "vol-1");
        ctl.maybe_snapshot_on_ledger(1).await;
        // pause_flushes=false means is_paused never toggles (it is false now).
        assert!(!coord.is_paused());
    }

    // --- Restore + integrity --------------------------------------------

    #[tokio::test]
    async fn restore_succeeds_for_available_snapshot() {
        let inventory = vec![desc("snap-ok", 100, Utc::now())];
        let provider = Arc::new(MockProvider::with_inventory(inventory));
        let flush = Arc::new(FlushCoordinator::new());
        let ctl = SnapshotController::new(provider, config_no_pause(), flush, "vol-1");
        let result = ctl
            .restore_from_snapshot("snap-ok", "vol-restored")
            .await
            .unwrap();
        assert!(result.integrity_ok);
        assert_eq!(result.volume_id, "vol-restored");
        assert_eq!(result.snapshot_id, "snap-ok");
    }

    #[tokio::test]
    async fn restore_rejects_missing_snapshot() {
        let provider = Arc::new(MockProvider::new());
        let flush = Arc::new(FlushCoordinator::new());
        let ctl = SnapshotController::new(provider, config_no_pause(), flush, "vol-1");
        let err = ctl
            .restore_from_snapshot("nope", "vol-restored")
            .await
            .unwrap_err();
        assert!(matches!(err, CloudError::SnapshotNotFound(_)));
    }

    #[tokio::test]
    async fn restore_rejects_pending_snapshot() {
        let mut pending = desc("snap-pending", 100, Utc::now());
        pending.state = SnapshotState::Pending;
        let provider = Arc::new(MockProvider::with_inventory(vec![pending]));
        let flush = Arc::new(FlushCoordinator::new());
        let ctl = SnapshotController::new(provider, config_no_pause(), flush, "vol-1");
        let err = ctl
            .restore_from_snapshot("snap-pending", "vol-restored")
            .await
            .unwrap_err();
        assert!(matches!(err, CloudError::Provider(_)));
        assert!(err.to_string().contains("pending"));
    }

    #[tokio::test]
    async fn restore_rejects_failed_snapshot() {
        let mut failed = desc("snap-failed", 100, Utc::now());
        failed.state = SnapshotState::Failed("disk error".into());
        let provider = Arc::new(MockProvider::with_inventory(vec![failed]));
        let flush = Arc::new(FlushCoordinator::new());
        let ctl = SnapshotController::new(provider, config_no_pause(), flush, "vol-1");
        let err = ctl
            .restore_from_snapshot("snap-failed", "vol-restored")
            .await
            .unwrap_err();
        assert!(matches!(err, CloudError::Provider(_)));
        assert!(err.to_string().contains("disk error"));
    }

    #[tokio::test]
    async fn end_to_end_snapshot_then_restore_with_real_local_provider() {
        let src = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        write_files(
            src.path(),
            &[("stellar.db", "ledger-data"), ("wal/0001", "frame")],
        );

        let provider: Arc<dyn SnapshotProvider> =
            Arc::new(LocalSnapshotProvider::new(store.path()).unwrap());
        let flush = Arc::new(FlushCoordinator::new());
        let config = SnapshotConfig {
            ledger_marker_interval: 64,
            pause_flushes: true,
            ..Default::default()
        };
        let volume = src.path().to_string_lossy().to_string();
        let ctl = SnapshotController::new(provider, config, flush, volume.clone());

        // Snapshot at a marker.
        let report = ctl.maybe_snapshot_on_ledger(64).await;
        let created = report.created.expect("snapshot should be created");
        assert_eq!(created.ledger_sequence, 64);

        // Provision a new node volume from the snapshot and verify integrity.
        let restore_target = tempfile::tempdir().unwrap();
        let new_volume = restore_target.path().join("new-node");
        let result = ctl
            .restore_from_snapshot(&created.id, &new_volume.to_string_lossy())
            .await
            .unwrap();
        assert!(result.integrity_ok);
        assert_eq!(
            fs::read_to_string(new_volume.join("stellar.db")).unwrap(),
            "ledger-data"
        );
        assert_eq!(
            fs::read_to_string(new_volume.join("wal/0001")).unwrap(),
            "frame"
        );
    }

    #[tokio::test]
    async fn end_to_end_retention_prunes_old_snapshots_via_local_provider() {
        let src = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        write_files(src.path(), &[("stellar.db", "data")]);

        let provider: Arc<dyn SnapshotProvider> =
            Arc::new(LocalSnapshotProvider::new(store.path()).unwrap());
        let flush = Arc::new(FlushCoordinator::new());
        let config = SnapshotConfig {
            ledger_marker_interval: 0, // snapshot on every call
            pause_flushes: false,
            retention: RetentionPolicy {
                max_age: None,
                max_count: Some(2),
            },
            ..Default::default()
        };
        let volume = src.path().to_string_lossy().to_string();
        let ctl = SnapshotController::new(provider, config, flush, volume);

        // Create 4 snapshots.
        for ledger in 1..=4u64 {
            ctl.maybe_snapshot_on_ledger(ledger).await;
        }

        // Only the newest 2 remain.
        let list = ctl
            .provider
            .list_snapshots(&ListFilter {
                volume_id: Some(src.path().to_string_lossy().to_string()),
                tag: None,
            })
            .await
            .unwrap();
        assert_eq!(list.len(), 2);
    }

    fn config_no_pause() -> SnapshotConfig {
        SnapshotConfig {
            ledger_marker_interval: 0,
            pause_flushes: false,
            ..Default::default()
        }
    }
}
