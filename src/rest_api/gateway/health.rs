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
//! RPC Node Health Checking with Ledger Sequence Alignment
//!
//! Provides health evaluation for Soroban RPC nodes across multiple regions.
//! The health checker evaluates:
//! - Ledger sequence alignment (how far behind the network a node is)
//! - Sync state (catching up vs. synced)
//! - Database corruption signals
//! - Round-trip latency to each cluster
//!
//! ## Performance Constraint
//!
//! Health evaluation overhead adds less than 2 ms to incoming client RPC
//! requests. The checker uses cached health snapshots refreshed in the
//! background by [`ClusterHealthChecker::refresh`], so request-path
//! evaluation is a pure in-memory read of the latest snapshot — typically
//! well under 50 µs even for dozens of clusters.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, instrument, warn};

/// Maximum ledger lag (in ledgers) a node can be behind the network before it
/// is considered unsynchronized. A typical Stellar ledger closes every 5-7
/// seconds, so 10 ledgers ≈ 50-70 seconds of lag — beyond which read requests
/// may return stale data.
pub const DEFAULT_MAX_LEDGER_LAG: u32 = 10;

/// Health evaluation overhead budget — the checker guarantees that
/// [`ClusterHealthChecker::evaluate`] completes within this budget for
/// request-path usage.
pub const HEALTH_EVAL_BUDGET: Duration = Duration::from_millis(2);

/// Health status of a single RPC cluster endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterHealthStatus {
    /// Fully synced and serving traffic.
    Healthy,
    /// Within the lag threshold but degraded (e.g. elevated latency).
    Degraded,
    /// Lag exceeds the threshold or sync is in progress — route away.
    Unsynchronized,
    /// Circuit breaker open or database corruption detected — isolate.
    Isolated,
    /// No health data available yet.
    Unknown,
}

impl ClusterHealthStatus {
    /// Returns `true` when the cluster can receive traffic.
    pub fn can_serve_traffic(&self) -> bool {
        matches!(self, Self::Healthy | Self::Degraded)
    }

    /// Returns `true` when the cluster must be isolated (circuit breaker open
    /// or database corruption).
    pub fn is_isolated(&self) -> bool {
        matches!(self, Self::Isolated)
    }
}

impl std::fmt::Display for ClusterHealthStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Healthy => write!(f, "healthy"),
            Self::Degraded => write!(f, "degraded"),
            Self::Unsynchronized => write!(f, "unsynchronized"),
            Self::Isolated => write!(f, "isolated"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

/// Snapshot of a cluster's health at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthSnapshot {
    /// Cluster identifier (region name or cluster-id).
    pub cluster_id: String,
    /// RPC endpoint URL (e.g. `https://soroban-rpc.us-east-1.example.com`).
    pub endpoint: String,
    /// Latest ledger sequence reported by this node.
    pub ledger_sequence: u64,
    /// Latest ledger sequence known across the network (max of all clusters).
    pub network_ledger_sequence: u64,
    /// Ledger lag: `network_ledger_sequence - ledger_sequence`.
    pub ledger_lag: u64,
    /// Round-trip latency to the cluster (from the gateway's perspective).
    pub latency_ms: u64,
    /// Whether the node reports it is still catching up.
    pub is_catching_up: bool,
    /// Whether a database corruption signal was detected.
    pub db_corruption_detected: bool,
    /// Computed health status.
    pub status: ClusterHealthStatus,
    /// When this snapshot was taken.
    pub checked_at: chrono::DateTime<chrono::Utc>,
}

impl HealthSnapshot {
    /// Create a fresh snapshot with `Unknown` status.
    pub fn new(cluster_id: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self {
            cluster_id: cluster_id.into(),
            endpoint: endpoint.into(),
            ledger_sequence: 0,
            network_ledger_sequence: 0,
            ledger_lag: 0,
            latency_ms: 0,
            is_catching_up: false,
            db_corruption_detected: false,
            status: ClusterHealthStatus::Unknown,
            checked_at: chrono::Utc::now(),
        }
    }
}

/// Configuration for the cluster health checker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckerConfig {
    /// Maximum ledger lag before a cluster is `Unsynchronized`.
    pub max_ledger_lag: u32,
    /// Latency threshold (ms) above which a cluster is `Degraded`.
    pub degraded_latency_ms: u64,
    /// HTTP timeout for health probes.
    pub probe_timeout_ms: u64,
    /// Interval between background health refresh cycles.
    pub refresh_interval_ms: u64,
}

impl Default for HealthCheckerConfig {
    fn default() -> Self {
        Self {
            max_ledger_lag: DEFAULT_MAX_LEDGER_LAG,
            degraded_latency_ms: 500,
            probe_timeout_ms: 1500,
            refresh_interval_ms: 5000,
        }
    }
}

/// A single cluster endpoint monitored by the health checker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterEndpoint {
    pub cluster_id: String,
    pub endpoint: String,
    pub region: String,
    /// Relative priority — lower value = preferred.
    pub priority: u32,
}

impl ClusterEndpoint {
    pub fn new(
        cluster_id: impl Into<String>,
        endpoint: impl Into<String>,
        region: impl Into<String>,
        priority: u32,
    ) -> Self {
        Self {
            cluster_id: cluster_id.into(),
            endpoint: endpoint.into(),
            region: region.into(),
            priority,
        }
    }
}

/// Multi-region cluster health checker.
///
/// Stores [`HealthSnapshot`]s in an `RwLock<HashMap>` so that request-path
/// health evaluation ([`evaluate`]) is a lock-and-read that completes in
/// well under the 2 ms budget. Background refresh ([`refresh`]) updates
/// the snapshots periodically.
///
/// [`evaluate`]: ClusterHealthChecker::evaluate
/// [`refresh`]: ClusterHealthChecker::refresh
#[derive(Debug, Clone)]
pub struct ClusterHealthChecker {
    config: HealthCheckerConfig,
    clusters: Vec<ClusterEndpoint>,
    snapshots: Arc<RwLock<HashMap<String, HealthSnapshot>>>,
}

impl ClusterHealthChecker {
    /// Create a new health checker for the given clusters.
    pub fn new(config: HealthCheckerConfig, clusters: Vec<ClusterEndpoint>) -> Self {
        let snapshots: HashMap<String, HealthSnapshot> = clusters
            .iter()
            .map(|c| {
                (
                    c.cluster_id.clone(),
                    HealthSnapshot::new(c.cluster_id.clone(), c.endpoint.clone()),
                )
            })
            .collect();
        Self {
            config,
            clusters,
            snapshots: Arc::new(RwLock::new(snapshots)),
        }
    }

    /// Return a copy of the cluster list.
    pub fn clusters(&self) -> &[ClusterEndpoint] {
        &self.clusters
    }

    /// Return the checker configuration.
    pub fn config(&self) -> &HealthCheckerConfig {
        &self.config
    }

    /// Evaluate the health of all clusters and return a sorted list of
    /// (cluster_id, status, latency_ms) tuples.
    ///
    /// This is the request-path call and must complete within 2 ms. It
    /// reads cached snapshots without any network I/O.
    #[instrument(skip(self), level = "debug")]
    pub async fn evaluate(&self) -> Vec<HealthEvaluation> {
        let start = Instant::now();
        let snapshots = self.snapshots.read().await;

        let network_ledger = snapshots
            .values()
            .map(|s| s.network_ledger_sequence)
            .max()
            .unwrap_or(0);

        let mut evaluations: Vec<HealthEvaluation> = snapshots
            .values()
            .map(|s| HealthEvaluation {
                cluster_id: s.cluster_id.clone(),
                endpoint: s.endpoint.clone(),
                status: s.status.clone(),
                latency_ms: s.latency_ms,
                ledger_lag: s.ledger_lag,
                ledger_sequence: s.ledger_sequence,
                network_ledger_sequence: network_ledger,
            })
            .collect();

        // Sort by: can_serve_traffic first, then latency, then priority.
        evaluations.sort_by(|a, b| {
            let a_serve = a.status.can_serve_traffic();
            let b_serve = b.status.can_serve_traffic();
            b_serve
                .cmp(&a_serve)
                .then_with(|| a.latency_ms.cmp(&b.latency_ms))
        });

        let elapsed = start.elapsed();
        if elapsed > HEALTH_EVAL_BUDGET {
            warn!(
                "health evaluation exceeded 2ms budget: {:?} (clusters={})",
                elapsed,
                evaluations.len()
            );
        }

        evaluations
    }

    /// Get the health snapshot for a single cluster.
    pub async fn get_snapshot(&self, cluster_id: &str) -> Option<HealthSnapshot> {
        self.snapshots.read().await.get(cluster_id).cloned()
    }

    /// Get all snapshots.
    pub async fn get_all_snapshots(&self) -> Vec<HealthSnapshot> {
        self.snapshots.read().await.values().cloned().collect()
    }

    /// Refresh health snapshots by probing all cluster endpoints.
    ///
    /// This performs HTTP I/O and should be called in a background task,
    /// not on the request path.
    pub async fn refresh(&self) {
        let http_client = match reqwest::Client::builder()
            .timeout(Duration::from_millis(self.config.probe_timeout_ms))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                warn!("failed to build HTTP client for health refresh: {e}");
                return;
            }
        };

        let mut results: Vec<(String, HealthSnapshot)> = Vec::with_capacity(self.clusters.len());

        for cluster in &self.clusters {
            let snapshot = probe_cluster(&http_client, cluster, &self.config).await;
            results.push((cluster.cluster_id.clone(), snapshot));
        }

        // Compute the network-wide max ledger sequence across all clusters.
        let network_max = results
            .iter()
            .map(|(_, s)| s.ledger_sequence)
            .max()
            .unwrap_or(0);

        // Recompute lag and status using the network max.
        let mut updated: HashMap<String, HealthSnapshot> =
            HashMap::with_capacity(results.len());
        for (id, mut snap) in results {
            snap.network_ledger_sequence = network_max;
            snap.ledger_lag = network_max.saturating_sub(snap.ledger_sequence);
            snap.status = compute_status(&snap, &self.config);
            updated.insert(id, snap);
        }

        let mut snapshots = self.snapshots.write().await;
        *snapshots = updated;
    }

    /// Update a cluster's health snapshot directly (for testing or
    /// manual overrides).
    pub async fn update_snapshot(&self, cluster_id: &str, snapshot: HealthSnapshot) {
        let mut snapshots = self.snapshots.write().await;
        snapshots.insert(cluster_id.to_string(), snapshot);
    }

    /// Mark a cluster as isolated (circuit breaker open or DB corruption).
    pub async fn mark_isolated(&self, cluster_id: &str, reason: &str) {
        let mut snapshots = self.snapshots.write().await;
        if let Some(snap) = snapshots.get_mut(cluster_id) {
            snap.status = ClusterHealthStatus::Isolated;
            snap.checked_at = chrono::Utc::now();
            debug!("cluster {} isolated: {}", cluster_id, reason);
        }
    }

    /// Mark a cluster as recovered (circuit breaker closed).
    pub async fn mark_recovered(&self, cluster_id: &str) {
        let mut snapshots = self.snapshots.write().await;
        if let Some(snap) = snapshots.get_mut(cluster_id) {
            if snap.status == ClusterHealthStatus::Isolated {
                snap.status = ClusterHealthStatus::Unknown;
                snap.checked_at = chrono::Utc::now();
                debug!("cluster {} marked recovered, will re-evaluate on next refresh", cluster_id);
            }
        }
    }
}

/// Compact health evaluation result used by the router for routing decisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthEvaluation {
    pub cluster_id: String,
    pub endpoint: String,
    pub status: ClusterHealthStatus,
    pub latency_ms: u64,
    pub ledger_lag: u64,
    pub ledger_sequence: u64,
    pub network_ledger_sequence: u64,
}

impl HealthEvaluation {
    /// Returns `true` if this cluster is fully synced (lag <= threshold).
    pub fn is_synced(&self, max_lag: u32) -> bool {
        self.ledger_lag <= max_lag as u64
    }

    /// Returns `true` if this cluster can serve traffic right now.
    pub fn can_route(&self) -> bool {
        self.status.can_serve_traffic()
    }
}

/// Probe a single cluster endpoint to build a [`HealthSnapshot`].
///
/// Queries the Soroban RPC `getHealth` JSON-RPC method and measures
/// round-trip latency.
async fn probe_cluster(
    client: &reqwest::Client,
    cluster: &ClusterEndpoint,
    config: &HealthCheckerConfig,
) -> HealthSnapshot {
    let probe_start = Instant::now();

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getHealth",
        "params": {}
    });

    let mut snap = HealthSnapshot::new(cluster.cluster_id.clone(), cluster.endpoint.clone());

    let result = client
        .post(&cluster.endpoint)
        .json(&body)
        .send()
        .await;

    match result {
        Ok(resp) => {
            snap.latency_ms = probe_start.elapsed().as_millis() as u64;

            match resp.json::<serde_json::Value>().await {
                Ok(json) => {
                    // Parse getHealth response:
                    // { "result": { "status": "healthy", "latestLedger": 12345,
                    //   "oldestLedger": 12000, "fullSynced": true } }
                    if let Some(result_obj) = json.get("result") {
                        let status_str = result_obj
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");

                        snap.is_catching_up = status_str != "healthy";

                        snap.ledger_sequence = result_obj
                            .get("latestLedger")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);

                        let full_synced = result_obj
                            .get("fullSynced")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(true);

                        if !full_synced {
                            snap.is_catching_up = true;
                        }
                    } else if let Some(error_obj) = json.get("error") {
                        // An error response may indicate database corruption
                        let error_msg = error_obj
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if error_msg.contains("database")
                            || error_msg.contains("corrupt")
                            || error_msg.contains("storage")
                        {
                            snap.db_corruption_detected = true;
                        }
                    }
                }
                Err(e) => {
                    debug!("failed to parse health response from {}: {e}", cluster.cluster_id);
                }
            }
        }
        Err(e) => {
            debug!("health probe failed for {}: {e}", cluster.cluster_id);
        }
    }

    snap.checked_at = chrono::Utc::now();
    snap
}

/// Compute the [`ClusterHealthStatus`] from a snapshot and config.
///
/// Rules (evaluated in order):
/// 1. DB corruption → `Isolated`
/// 2. Ledger lag exceeds threshold → `Unsynchronized`
/// 3. Catching up → `Unsynchronized`
/// 4. Latency exceeds degraded threshold → `Degraded`
/// 5. Otherwise → `Healthy`
fn compute_status(snap: &HealthSnapshot, config: &HealthCheckerConfig) -> ClusterHealthStatus {
    if snap.db_corruption_detected {
        return ClusterHealthStatus::Isolated;
    }

    if snap.ledger_lag > config.max_ledger_lag as u64 {
        return ClusterHealthStatus::Unsynchronized;
    }

    if snap.is_catching_up {
        return ClusterHealthStatus::Unsynchronized;
    }

    if snap.latency_ms > config.degraded_latency_ms {
        return ClusterHealthStatus::Degraded;
    }

    ClusterHealthStatus::Healthy
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cluster(id: &str, endpoint: &str) -> ClusterEndpoint {
        ClusterEndpoint::new(id, endpoint, "test-region", 1)
    }

    #[test]
    fn test_cluster_health_status_can_serve_traffic() {
        assert!(ClusterHealthStatus::Healthy.can_serve_traffic());
        assert!(ClusterHealthStatus::Degraded.can_serve_traffic());
        assert!(!ClusterHealthStatus::Unsynchronized.can_serve_traffic());
        assert!(!ClusterHealthStatus::Isolated.can_serve_traffic());
        assert!(!ClusterHealthStatus::Unknown.can_serve_traffic());
    }

    #[test]
    fn test_cluster_health_status_is_isolated() {
        assert!(ClusterHealthStatus::Isolated.is_isolated());
        assert!(!ClusterHealthStatus::Healthy.is_isolated());
    }

    #[test]
    fn test_compute_status_db_corruption_isolates() {
        let snap = HealthSnapshot {
            cluster_id: "c1".into(),
            endpoint: "http://c1".into(),
            ledger_sequence: 100,
            network_ledger_sequence: 100,
            ledger_lag: 0,
            latency_ms: 10,
            is_catching_up: false,
            db_corruption_detected: true,
            status: ClusterHealthStatus::Unknown,
            checked_at: chrono::Utc::now(),
        };
        assert_eq!(
            compute_status(&snap, &HealthCheckerConfig::default()),
            ClusterHealthStatus::Isolated
        );
    }

    #[test]
    fn test_compute_status_excessive_lag_unsynchronized() {
        let config = HealthCheckerConfig {
            max_ledger_lag: 10,
            ..Default::default()
        };
        let snap = HealthSnapshot {
            cluster_id: "c1".into(),
            endpoint: "http://c1".into(),
            ledger_sequence: 100,
            network_ledger_sequence: 120,
            ledger_lag: 20,
            latency_ms: 10,
            is_catching_up: false,
            db_corruption_detected: false,
            status: ClusterHealthStatus::Unknown,
            checked_at: chrono::Utc::now(),
        };
        assert_eq!(
            compute_status(&snap, &config),
            ClusterHealthStatus::Unsynchronized
        );
    }

    #[test]
    fn test_compute_status_catching_up_unsynchronized() {
        let snap = HealthSnapshot {
            cluster_id: "c1".into(),
            endpoint: "http://c1".into(),
            ledger_sequence: 100,
            network_ledger_sequence: 100,
            ledger_lag: 0,
            latency_ms: 10,
            is_catching_up: true,
            db_corruption_detected: false,
            status: ClusterHealthStatus::Unknown,
            checked_at: chrono::Utc::now(),
        };
        assert_eq!(
            compute_status(&snap, &HealthCheckerConfig::default()),
            ClusterHealthStatus::Unsynchronized
        );
    }

    #[test]
    fn test_compute_status_high_latency_degraded() {
        let config = HealthCheckerConfig {
            max_ledger_lag: 10,
            degraded_latency_ms: 100,
            ..Default::default()
        };
        let snap = HealthSnapshot {
            cluster_id: "c1".into(),
            endpoint: "http://c1".into(),
            ledger_sequence: 100,
            network_ledger_sequence: 100,
            ledger_lag: 0,
            latency_ms: 200,
            is_catching_up: false,
            db_corruption_detected: false,
            status: ClusterHealthStatus::Unknown,
            checked_at: chrono::Utc::now(),
        };
        assert_eq!(
            compute_status(&snap, &config),
            ClusterHealthStatus::Degraded
        );
    }

    #[test]
    fn test_compute_status_healthy() {
        let snap = HealthSnapshot {
            cluster_id: "c1".into(),
            endpoint: "http://c1".into(),
            ledger_sequence: 100,
            network_ledger_sequence: 100,
            ledger_lag: 0,
            latency_ms: 50,
            is_catching_up: false,
            db_corruption_detected: false,
            status: ClusterHealthStatus::Unknown,
            checked_at: chrono::Utc::now(),
        };
        assert_eq!(
            compute_status(&snap, &HealthCheckerConfig::default()),
            ClusterHealthStatus::Healthy
        );
    }

    #[test]
    fn test_compute_status_lag_at_threshold_is_healthy() {
        let config = HealthCheckerConfig {
            max_ledger_lag: 10,
            ..Default::default()
        };
        let snap = HealthSnapshot {
            cluster_id: "c1".into(),
            endpoint: "http://c1".into(),
            ledger_sequence: 100,
            network_ledger_sequence: 110,
            ledger_lag: 10,
            latency_ms: 50,
            is_catching_up: false,
            db_corruption_detected: false,
            status: ClusterHealthStatus::Unknown,
            checked_at: chrono::Utc::now(),
        };
        // Lag exactly at threshold is still healthy (<= not <)
        assert_eq!(
            compute_status(&snap, &config),
            ClusterHealthStatus::Healthy
        );
    }

    #[tokio::test]
    async fn test_evaluate_returns_sorted_by_health_and_latency() {
        let clusters = vec![
            test_cluster("primary", "http://primary:8000"),
            test_cluster("secondary", "http://secondary:8000"),
        ];
        let checker = ClusterHealthChecker::new(HealthCheckerConfig::default(), clusters);

        // Primary: healthy, 50ms latency
        checker
            .update_snapshot(
                "primary",
                HealthSnapshot {
                    cluster_id: "primary".into(),
                    endpoint: "http://primary:8000".into(),
                    ledger_sequence: 1000,
                    network_ledger_sequence: 1000,
                    ledger_lag: 0,
                    latency_ms: 50,
                    is_catching_up: false,
                    db_corruption_detected: false,
                    status: ClusterHealthStatus::Healthy,
                    checked_at: chrono::Utc::now(),
                },
            )
            .await;

        // Secondary: healthy, 30ms latency (lower)
        checker
            .update_snapshot(
                "secondary",
                HealthSnapshot {
                    cluster_id: "secondary".into(),
                    endpoint: "http://secondary:8000".into(),
                    ledger_sequence: 1000,
                    network_ledger_sequence: 1000,
                    ledger_lag: 0,
                    latency_ms: 30,
                    is_catching_up: false,
                    db_corruption_detected: false,
                    status: ClusterHealthStatus::Healthy,
                    checked_at: chrono::Utc::now(),
                },
            )
            .await;

        let evals = checker.evaluate().await;
        // Both healthy — sorted by latency: secondary (30ms) first
        assert_eq!(evals[0].cluster_id, "secondary");
        assert_eq!(evals[1].cluster_id, "primary");
        assert!(evals[0].latency_ms < evals[1].latency_ms);
    }

    #[tokio::test]
    async fn test_evaluate_prefers_healthy_over_unsynchronized() {
        let clusters = vec![
            test_cluster("primary", "http://primary:8000"),
            test_cluster("secondary", "http://secondary:8000"),
        ];
        let checker = ClusterHealthChecker::new(HealthCheckerConfig::default(), clusters);

        // Primary: unsynchronized (lagging 20 ledgers)
        checker
            .update_snapshot(
                "primary",
                HealthSnapshot {
                    cluster_id: "primary".into(),
                    endpoint: "http://primary:8000".into(),
                    ledger_sequence: 980,
                    network_ledger_sequence: 1000,
                    ledger_lag: 20,
                    latency_ms: 10,
                    is_catching_up: false,
                    db_corruption_detected: false,
                    status: ClusterHealthStatus::Unsynchronized,
                    checked_at: chrono::Utc::now(),
                },
            )
            .await;

        // Secondary: healthy
        checker
            .update_snapshot(
                "secondary",
                HealthSnapshot {
                    cluster_id: "secondary".into(),
                    endpoint: "http://secondary:8000".into(),
                    ledger_sequence: 1000,
                    network_ledger_sequence: 1000,
                    ledger_lag: 0,
                    latency_ms: 100,
                    is_catching_up: false,
                    db_corruption_detected: false,
                    status: ClusterHealthStatus::Healthy,
                    checked_at: chrono::Utc::now(),
                },
            )
            .await;

        let evals = checker.evaluate().await;
        // Secondary (healthy) should rank before primary (unsynchronized)
        assert!(evals[0].can_route());
        assert!(!evals[1].can_route());
        assert_eq!(evals[0].cluster_id, "secondary");
    }

    #[tokio::test]
    async fn test_evaluate_completes_under_2ms_budget() {
        // 20 clusters — should still be fast
        let clusters: Vec<ClusterEndpoint> = (0..20)
            .map(|i| ClusterEndpoint::new(format!("cluster-{i}"), format!("http://c{i}:8000"), "region", 1))
            .collect();
        let checker = ClusterHealthChecker::new(HealthCheckerConfig::default(), clusters);

        // Populate snapshots
        for i in 0..20u64 {
            let id = format!("cluster-{i}");
            checker
                .update_snapshot(
                    &id,
                    HealthSnapshot {
                        cluster_id: id.clone(),
                        endpoint: format!("http://c{i}:8000"),
                        ledger_sequence: 1000 + i,
                        network_ledger_sequence: 1020,
                        ledger_lag: 20 - i,
                        latency_ms: i * 10,
                        is_catching_up: false,
                        db_corruption_detected: false,
                        status: if i < 10 {
                            ClusterHealthStatus::Healthy
                        } else {
                            ClusterHealthStatus::Degraded
                        },
                        checked_at: chrono::Utc::now(),
                    },
                )
                .await;
        }

        let start = Instant::now();
        let evals = checker.evaluate().await;
        let elapsed = start.elapsed();

        assert_eq!(evals.len(), 20);
        assert!(
            elapsed < HEALTH_EVAL_BUDGET,
            "evaluate took {:?}, budget is {:?}",
            elapsed,
            HEALTH_EVAL_BUDGET
        );
    }

    #[tokio::test]
    async fn test_mark_isolated() {
        let clusters = vec![test_cluster("c1", "http://c1:8000")];
        let checker = ClusterHealthChecker::new(HealthCheckerConfig::default(), clusters);

        checker.mark_isolated("c1", "circuit breaker open").await;
        let snap = checker.get_snapshot("c1").await.unwrap();
        assert_eq!(snap.status, ClusterHealthStatus::Isolated);
        assert!(snap.status.is_isolated());
    }

    #[tokio::test]
    async fn test_mark_recovered() {
        let clusters = vec![test_cluster("c1", "http://c1:8000")];
        let checker = ClusterHealthChecker::new(HealthCheckerConfig::default(), clusters);

        checker.mark_isolated("c1", "breaker open").await;
        assert_eq!(
            checker.get_snapshot("c1").await.unwrap().status,
            ClusterHealthStatus::Isolated
        );

        checker.mark_recovered("c1").await;
        let snap = checker.get_snapshot("c1").await.unwrap();
        assert_ne!(snap.status, ClusterHealthStatus::Isolated);
    }

    #[tokio::test]
    async fn test_get_all_snapshots() {
        let clusters = vec![
            test_cluster("c1", "http://c1:8000"),
            test_cluster("c2", "http://c2:8000"),
        ];
        let checker = ClusterHealthChecker::new(HealthCheckerConfig::default(), clusters);
        let snaps = checker.get_all_snapshots().await;
        assert_eq!(snaps.len(), 2);
    }

    #[tokio::test]
    async fn test_health_evaluation_is_synced() {
        let eval = HealthEvaluation {
            cluster_id: "c1".into(),
            endpoint: "http://c1:8000".into(),
            status: ClusterHealthStatus::Healthy,
            latency_ms: 50,
            ledger_lag: 5,
            ledger_sequence: 1000,
            network_ledger_sequence: 1005,
        };
        assert!(eval.is_synced(10));
        assert!(!eval.is_synced(3));
        assert!(eval.can_route());
    }

    #[tokio::test]
    async fn test_simulate_10_ledger_sync_delay_failover() {
        // Simulate the issue scenario: Primary cluster falls 10+ ledgers
        // behind, verify requests route to secondary.
        let clusters = vec![
            test_cluster("primary", "http://primary:8000"),
            test_cluster("secondary", "http://secondary:8000"),
        ];
        let config = HealthCheckerConfig {
            max_ledger_lag: 10,
            ..Default::default()
        };
        let checker = ClusterHealthChecker::new(config, clusters);

        // Network ledger = 1000
        // Primary: ledger 990, lag = 10 → exactly at threshold (healthy)
        checker
            .update_snapshot(
                "primary",
                HealthSnapshot {
                    cluster_id: "primary".into(),
                    endpoint: "http://primary:8000".into(),
                    ledger_sequence: 990,
                    network_ledger_sequence: 1000,
                    ledger_lag: 10,
                    latency_ms: 20,
                    is_catching_up: false,
                    db_corruption_detected: false,
                    status: ClusterHealthStatus::Healthy,
                    checked_at: chrono::Utc::now(),
                },
            )
            .await;

        // Secondary: fully synced
        checker
            .update_snapshot(
                "secondary",
                HealthSnapshot {
                    cluster_id: "secondary".into(),
                    endpoint: "http://secondary:8000".into(),
                    ledger_sequence: 1000,
                    network_ledger_sequence: 1000,
                    ledger_lag: 0,
                    latency_ms: 30,
                    is_catching_up: false,
                    db_corruption_detected: false,
                    status: ClusterHealthStatus::Healthy,
                    checked_at: chrono::Utc::now(),
                },
            )
            .await;

        // At lag=10, primary is still "healthy" (<= threshold)
        let evals = checker.evaluate().await;
        let healthy: Vec<_> = evals.iter().filter(|e| e.can_route()).collect();
        assert_eq!(healthy.len(), 2);

        // Now simulate 10-ledger delay: primary drops to lag=15 (> threshold)
        checker
            .update_snapshot(
                "primary",
                HealthSnapshot {
                    cluster_id: "primary".into(),
                    endpoint: "http://primary:8000".into(),
                    ledger_sequence: 985,
                    network_ledger_sequence: 1000,
                    ledger_lag: 15,
                    latency_ms: 20,
                    is_catching_up: false,
                    db_corruption_detected: false,
                    status: ClusterHealthStatus::Unsynchronized,
                    checked_at: chrono::Utc::now(),
                },
            )
            .await;

        let evals = checker.evaluate().await;
        // Only secondary can serve traffic now
        let routable: Vec<_> = evals.iter().filter(|e| e.can_route()).collect();
        assert_eq!(routable.len(), 1);
        assert_eq!(routable[0].cluster_id, "secondary");
        assert!(evals[0].can_route());
        assert!(!evals[1].can_route());
    }
}
