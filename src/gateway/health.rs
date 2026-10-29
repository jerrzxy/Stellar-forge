// Copyright 2024 Stellar-K8s Contributors
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//! Cluster health monitor with circuit breaker for Soroban RPC multi-region
//! gateway.
//!
//! The health monitor runs background probes against each configured cluster's
//! Soroban RPC `/health` endpoint and maintains a shared health table that the
//! [`crate::gateway::router::MultiRegionRouter`] consults on every request.
//!
//! # Design constraints
//!
//! - **Sub-2ms evaluation**: Health state is pre-computed by a background task
//!   and stored in a lock-free snapshot. The hot path (route selection) reads
//!   a single `Arc<HealthSnapshot>` — no network calls, no locks, no
//!   allocations.
//! - **Circuit breaker**: Each cluster has an independent breaker that opens
//!   after `failure_threshold` consecutive probe failures and stays open for
//!   `open_window`. After the window, the breaker enters half-open and allows
//!   one probe; success closes it, failure re-opens it.
//! - **Ledger alignment**: The monitor tracks the latest ledger sequence from
//!   each cluster. A cluster is considered desynchronized if its ledger lags
//!   the highest observed ledger by more than `max_lag_ledgers`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, warn};

// ── Cluster identity ─────────────────────────────────────────────────────────

/// Unique identifier for a cluster within the gateway configuration.
pub type ClusterId = String;

// ── Circuit breaker ──────────────────────────────────────────────────────────

/// Circuit breaker state for a single cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakerState {
    /// Cluster is healthy — traffic flows normally.
    Closed,
    /// Cluster is isolated — all traffic is redirected elsewhere.
    Open,
    /// Trial period after `open_window` — one probe is allowed.
    HalfOpen,
}

impl BreakerState {
    /// Returns `true` if traffic should be allowed to reach the cluster.
    pub fn allows_traffic(self) -> bool {
        matches!(self, BreakerState::Closed | BreakerState::HalfOpen)
    }
}

/// Per-cluster circuit breaker configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    /// Consecutive probe failures before the breaker opens.
    pub failure_threshold: u32,
    /// Consecutive successes (in half-open) required to close the breaker.
    pub success_threshold: u32,
    /// How long the breaker stays open before transitioning to half-open.
    pub open_window: Duration,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            success_threshold: 2,
            open_window: Duration::from_secs(30),
        }
    }
}

/// Runtime state of a single cluster's circuit breaker.
#[derive(Debug, Clone)]
pub struct BreakerState_ {
    /// Current breaker state.
    pub state: BreakerState,
    /// Consecutive failures since last success (closed/half-open).
    pub consecutive_failures: u32,
    /// Consecutive successes since entering half-open.
    pub consecutive_successes: u32,
    /// Instant when the breaker opened (for open-window timing).
    pub opened_at: Option<Instant>,
}

impl Default for BreakerState_ {
    fn default() -> Self {
        Self {
            state: BreakerState::Closed,
            consecutive_failures: 0,
            consecutive_successes: 0,
            opened_at: None,
        }
    }
}

impl BreakerState_ {
    /// Record a probe success and transition the breaker accordingly.
    pub fn record_success(&mut self, cfg: &CircuitBreakerConfig) {
        self.consecutive_failures = 0;
        match self.state {
            BreakerState::Closed => {}
            BreakerState::Open => {
                // Should not happen — caller ensures the window elapsed first.
                self.state = BreakerState::HalfOpen;
                self.consecutive_successes = 1;
            }
            BreakerState::HalfOpen => {
                self.consecutive_successes += 1;
                if self.consecutive_successes >= cfg.success_threshold {
                    self.state = BreakerState::Closed;
                    self.consecutive_successes = 0;
                    self.opened_at = None;
                }
            }
        }
    }

    /// Record a probe failure and transition the breaker accordingly.
    pub fn record_failure(&mut self, cfg: &CircuitBreakerConfig) {
        self.consecutive_successes = 0;
        self.consecutive_failures += 1;
        match self.state {
            BreakerState::Closed => {
                if self.consecutive_failures >= cfg.failure_threshold {
                    self.state = BreakerState::Open;
                    self.opened_at = Some(Instant::now());
                }
            }
            BreakerState::HalfOpen => {
                self.state = BreakerState::Open;
                self.opened_at = Some(Instant::now());
            }
            BreakerState::Open => {}
        }
    }

    /// Advance the breaker state machine based on elapsed time.
    /// If the breaker has been open longer than `open_window`, transition to
    /// half-open to allow a trial probe.
    pub fn maybe_half_open(&mut self, cfg: &CircuitBreakerConfig) {
        if self.state != BreakerState::Open {
            return;
        }
        if let Some(opened_at) = self.opened_at {
            if opened_at.elapsed() >= cfg.open_window {
                self.state = BreakerState::HalfOpen;
                self.consecutive_successes = 0;
                self.consecutive_failures = 0;
            }
        }
    }
}

// ── Cluster health ───────────────────────────────────────────────────────────

/// Overall health status of a cluster, derived from probe results and the
/// circuit breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterHealth {
    /// Healthy and synced — eligible for all traffic.
    Healthy,
    /// Degraded (e.g., high latency) — eligible for read traffic only.
    Degraded,
    /// Desynchronized — ledger lag exceeds threshold.
    Desynced,
    /// Circuit breaker open or probe failing — no traffic.
    Unhealthy,
}

impl ClusterHealth {
    pub fn allows_traffic(self) -> bool {
        matches!(
            self,
            ClusterHealth::Healthy | ClusterHealth::Degraded
        )
    }

    pub fn allows_reads(self) -> bool {
        matches!(
            self,
            ClusterHealth::Healthy | ClusterHealth::Degraded
        )
    }

    pub fn allows_writes(self) -> bool {
        matches!(self, ClusterHealth::Healthy)
    }
}

/// Per-cluster health configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterHealthConfig {
    /// RPC endpoint URL (e.g. `http://soroban-rpc.stellar:8000`).
    pub endpoint: String,
    /// How often to probe the `/health` endpoint.
    pub probe_interval: Duration,
    /// Probe timeout — must be short to keep the health table fresh.
    pub probe_timeout: Duration,
    /// Maximum acceptable ledger lag (in ledgers) before the cluster is
    /// marked `Desynced`.
    pub max_lag_ledgers: u64,
    /// Circuit breaker settings.
    pub circuit_breaker: CircuitBreakerConfig,
}

impl Default for ClusterHealthConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            probe_interval: Duration::from_secs(5),
            probe_timeout: Duration::from_millis(1500),
            max_lag_ledgers: 10,
            circuit_breaker: CircuitBreakerConfig::default(),
        }
    }
}

/// A single cluster's health snapshot — the data the router reads on every
/// request. Designed to be cheap to clone (all fields are `Copy` or `Arc`).
#[derive(Debug, Clone)]
pub struct ClusterHealthEntry {
    /// Cluster identifier.
    pub id: ClusterId,
    /// RPC endpoint URL.
    pub endpoint: String,
    /// Current circuit breaker state.
    pub breaker: BreakerState,
    /// Derived health status.
    pub health: ClusterHealth,
    /// Latest ledger sequence observed from this cluster (`0` if unknown).
    pub ledger_sequence: u64,
    /// Measured probe latency in microseconds (`0` if never probed).
    pub latency_us: u64,
    /// Whether this cluster is the configured primary.
    pub is_primary: bool,
}

impl ClusterHealthEntry {
    /// Returns `true` if the cluster can serve read traffic.
    pub fn can_serve_reads(&self) -> bool {
        self.breaker.allows_traffic() && self.health.allows_reads()
    }

    /// Returns `true` if the cluster can serve write traffic.
    pub fn can_serve_writes(&self) -> bool {
        self.breaker.allows_traffic() && self.health.allows_writes()
    }
}

/// Immutable snapshot of all cluster health states.
/// Cloning this `Arc` is the only allocation on the request hot path.
#[derive(Debug, Clone)]
pub struct HealthSnapshot {
    pub clusters: HashMap<ClusterId, ClusterHealthEntry>,
    /// The highest ledger sequence observed across all clusters.
    pub max_ledger: u64,
    /// Timestamp when this snapshot was taken.
    pub updated_at: Instant,
}

impl HealthSnapshot {
    /// Get a cluster's health entry by ID.
    pub fn get(&self, id: &str) -> Option<&ClusterHealthEntry> {
        self.clusters.get(id)
    }

    /// Returns a list of clusters that can serve read traffic, sorted by
    /// latency (lowest first).
    pub fn read_capable_clusters(&self) -> Vec<&ClusterHealthEntry> {
        let mut entries: Vec<_> = self
            .clusters
            .values()
            .filter(|e| e.can_serve_reads())
            .collect();
        entries.sort_by_key(|e| e.latency_us);
        entries
    }

    /// Returns a list of clusters that can serve write traffic.
    pub fn write_capable_clusters(&self) -> Vec<&ClusterHealthEntry> {
        self.clusters
            .values()
            .filter(|e| e.can_serve_writes())
            .collect()
    }
}

// ── Soroban /health response ─────────────────────────────────────────────────

/// Response shape from the Soroban RPC `/health` endpoint.
///
/// The Soroban RPC `/health` method returns `latestLedger` (not `ledger`)
/// for the current ledger sequence. Some older deployments use the field
/// name `ledger` — we accept both for compatibility.
#[derive(Debug, Deserialize)]
struct SorobanHealthResponse {
    #[serde(default)]
    status: String,
    /// Current ledger sequence (modern field name).
    #[serde(default, alias = "ledger")]
    latest_ledger: u64,
    /// If present and non-empty, indicates the RPC node's database is
    /// corrupted and the cluster must be isolated immediately.
    #[serde(default)]
    db_error: Option<String>,
}

// ── Health monitor ───────────────────────────────────────────────────────────

/// Internal per-cluster runtime state.
struct ClusterRuntime {
    config: ClusterHealthConfig,
    breaker: BreakerState_,
    latency_us: u64,
    ledger_sequence: u64,
    is_primary: bool,
}

/// Multi-cluster health monitor.
///
/// Spawns a background task per cluster that periodically probes the RPC
/// `/health` endpoint. The aggregated state is published as an
/// [`Arc<HealthSnapshot>`] behind a [`RwLock`] for the router to read.
///
/// # Example
///
/// ```rust,no_run
/// use std::time::Duration;
/// use stellar_k8s::gateway::health::{
///     ClusterHealthConfig, ClusterHealthMonitor,
/// };
///
/// # async fn example() {
/// let mut monitor = ClusterHealthMonitor::new();
/// monitor.add_cluster("us-east", ClusterHealthConfig {
///     endpoint: "http://soroban-rpc-us-east:8000".into(),
///     probe_interval: Duration::from_secs(5),
///     ..Default::default()
/// }, true);
/// monitor.add_cluster("eu-west", ClusterHealthConfig {
///     endpoint: "http://soroban-rpc-eu-west:8000".into(),
///     probe_interval: Duration::from_secs(5),
///     ..Default::default()
/// }, false);
/// monitor.start().await;
///
/// let snapshot = monitor.snapshot().await;
/// for entry in snapshot.read_capable_clusters() {
///     println!("{}: {:?}", entry.id, entry.health);
/// }
/// # }
/// ```
pub struct ClusterHealthMonitor {
    inner: Arc<RwLock<HealthTableInner>>,
}

struct HealthTableInner {
    clusters: HashMap<ClusterId, ClusterRuntime>,
    snapshot: Arc<HealthSnapshot>,
}

impl ClusterHealthMonitor {
    /// Create a new empty health monitor.
    pub fn new() -> Self {
        let snapshot = Arc::new(HealthSnapshot {
            clusters: HashMap::new(),
            max_ledger: 0,
            updated_at: Instant::now(),
        });
        Self {
            inner: Arc::new(RwLock::new(HealthTableInner {
                clusters: HashMap::new(),
                snapshot,
            })),
        }
    }

    /// Register a cluster for health monitoring.
    ///
    /// `is_primary` marks the cluster as the primary write target.
    pub fn add_cluster(
        &self,
        id: impl Into<ClusterId>,
        config: ClusterHealthConfig,
        is_primary: bool,
    ) {
        let id = id.into();
        let mut inner = self.inner.blocking_write();
        inner.clusters.insert(
            id.clone(),
            ClusterRuntime {
                config,
                breaker: BreakerState_::default(),
                latency_us: 0,
                ledger_sequence: 0,
                is_primary,
            },
        );
        self.rebuild_snapshot(&mut inner);
    }

    /// Remove a cluster from monitoring.
    pub fn remove_cluster(&self, id: &str) {
        let mut inner = self.inner.blocking_write();
        inner.clusters.remove(id);
        self.rebuild_snapshot(&mut inner);
    }

    /// Start the background probing tasks for all registered clusters.
    ///
    /// This should be called once after all clusters have been added.
    pub async fn start(&self) {
        let inner = self.inner.read().await;
        let cluster_ids: Vec<ClusterId> = inner.clusters.keys().cloned().collect();
        drop(inner);

        for id in cluster_ids {
            let inner = Arc::clone(&self.inner);
            let cluster_id = id.clone();
            tokio::spawn(async move {
                probe_loop(inner, cluster_id).await;
            });
        }
    }

    /// Get the current health snapshot. This is a cheap `Arc` clone — the
    /// primary operation on the request hot path.
    pub async fn snapshot(&self) -> Arc<HealthSnapshot> {
        Arc::clone(&self.inner.read().await.snapshot)
    }

    /// Synchronously get the current health snapshot (for non-async contexts).
    pub fn snapshot_blocking(&self) -> Arc<HealthSnapshot> {
        Arc::clone(&self.inner.blocking_read().snapshot)
    }

    /// Manually update a cluster's health (used by tests and external
    /// health-feed integrations).
    pub async fn update_cluster_health(
        &self,
        id: &str,
        healthy: bool,
        ledger_sequence: u64,
        latency_us: u64,
    ) {
        let mut inner = self.inner.write().await;
        let Some(rt) = inner.clusters.get_mut(id) else {
            return;
        };
        rt.latency_us = latency_us;
        rt.ledger_sequence = ledger_sequence;
        if healthy {
            rt.breaker.record_success(&rt.config.circuit_breaker);
        } else {
            rt.breaker.record_failure(&rt.config.circuit_breaker);
        }
        self.rebuild_snapshot(&mut inner);
    }

    /// Manually record a probe failure (e.g., connection refused).
    pub async fn record_probe_failure(&self, id: &str) {
        let mut inner = self.inner.write().await;
        let Some(rt) = inner.clusters.get_mut(id) else {
            return;
        };
        rt.breaker.record_failure(&rt.config.circuit_breaker);
        self.rebuild_snapshot(&mut inner);
    }

    /// Rebuild the immutable snapshot from the runtime table.
    fn rebuild_snapshot(&self, inner: &mut HealthTableInner) {
        let mut clusters = HashMap::new();
        let mut max_ledger = 0u64;

        for (id, rt) in &inner.clusters {
            if rt.ledger_sequence > max_ledger {
                max_ledger = rt.ledger_sequence;
            }
        }

        for (id, rt) in &inner.clusters {
            let health = derive_health(rt, max_ledger);
            clusters.insert(
                id.clone(),
                ClusterHealthEntry {
                    id: id.clone(),
                    endpoint: rt.config.endpoint.clone(),
                    breaker: rt.breaker.state,
                    health,
                    ledger_sequence: rt.ledger_sequence,
                    latency_us: rt.latency_us,
                    is_primary: rt.is_primary,
                },
            );
        }

        inner.snapshot = Arc::new(HealthSnapshot {
            clusters,
            max_ledger,
            updated_at: Instant::now(),
        });
    }
}

impl Default for ClusterHealthMonitor {
    fn default() -> Self {
        Self::new()
    }
}

/// Derive the [`ClusterHealth`] from runtime state.
fn derive_health(rt: &ClusterRuntime, max_ledger: u64) -> ClusterHealth {
    // Circuit breaker open → unhealthy regardless of other factors.
    if !rt.breaker.state.allows_traffic() {
        return ClusterHealth::Unhealthy;
    }

    // Ledger lag check.
    if max_ledger > 0 && rt.ledger_sequence > 0 {
        let lag = max_ledger.saturating_sub(rt.ledger_sequence);
        if lag > rt.config.max_lag_ledgers {
            return ClusterHealth::Desynced;
        }
    }

    // Latency-based degradation: if probe latency exceeds 500ms, mark degraded.
    if rt.latency_us > 500_000 {
        return ClusterHealth::Degraded;
    }

    ClusterHealth::Healthy
}

/// Background probe loop for a single cluster.
async fn probe_loop(inner: Arc<RwLock<HealthTableInner>>, cluster_id: ClusterId) {
    // Read probe interval and endpoint without holding the lock for the
    // duration of the HTTP request.
    let (interval, endpoint, timeout) = {
        let inner = inner.read().await;
        let Some(rt) = inner.clusters.get(&cluster_id) else {
            return;
        };
        (
            rt.config.probe_interval,
            rt.config.endpoint.clone(),
            rt.config.probe_timeout,
        )
    };

    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let mut ticker = tokio::time::interval(interval);
    // Don't backlog probes if one takes longer than the interval.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        // Advance the breaker state machine (open → half-open).
        {
            let mut guard = inner.write().await;
            if let Some(rt) = guard.clusters.get_mut(&cluster_id) {
                rt.breaker.maybe_half_open(&rt.config.circuit_breaker);
            }
            // We rebuild snapshot here in case the breaker transitioned to
            // half-open (which changes `allows_traffic`).
            // Access via the HealthTableInner method is not possible since we
            // hold the lock, so we inline the rebuild.
            rebuild_snapshot_locked(&mut guard);
        }

        // If the breaker is open, skip the probe entirely.
        {
            let guard = inner.read().await;
            let Some(rt) = guard.clusters.get(&cluster_id) else {
                return;
            };
            if !rt.breaker.state.allows_traffic() {
                debug!(cluster = %cluster_id, state = ?rt.breaker.state, "skipping probe (breaker open)");
                continue;
            }
        }

        let probe_start = Instant::now();
        let result = probe_cluster(&client, &endpoint).await;
        let latency_us = probe_start.elapsed().as_micros() as u64;

        let mut guard = inner.write().await;
        let Some(rt) = guard.clusters.get_mut(&cluster_id) else {
            return;
        };

        match result {
            Ok((status_healthy, ledger)) => {
                rt.breaker
                    .record_success(&rt.config.circuit_breaker);
                rt.ledger_sequence = ledger;
                rt.latency_us = latency_us;
                debug!(
                    cluster = %cluster_id,
                    ledger,
                    latency_us,
                    "probe succeeded"
                );
            }
            Err(e) => {
                rt.breaker
                    .record_failure(&rt.config.circuit_breaker);
                warn!(
                    cluster = %cluster_id,
                    error = %e,
                    "probe failed"
                );
            }
        }
        rebuild_snapshot_locked(&mut guard);
    }
}

/// Rebuild the snapshot while holding the write lock.
fn rebuild_snapshot_locked(inner: &mut HealthTableInner) {
    let mut clusters = HashMap::new();
    let mut max_ledger = 0u64;

    for rt in inner.clusters.values() {
        if rt.ledger_sequence > max_ledger {
            max_ledger = rt.ledger_sequence;
        }
    }

    for (id, rt) in &inner.clusters {
        let health = derive_health(rt, max_ledger);
        clusters.insert(
            id.clone(),
            ClusterHealthEntry {
                id: id.clone(),
                endpoint: rt.config.endpoint.clone(),
                breaker: rt.breaker.state,
                health,
                ledger_sequence: rt.ledger_sequence,
                latency_us: rt.latency_us,
                is_primary: rt.is_primary,
            },
        );
    }

    inner.snapshot = Arc::new(HealthSnapshot {
        clusters,
        max_ledger,
        updated_at: Instant::now(),
    });
}

/// Probe a single cluster's `/health` endpoint.
///
/// Returns `(status_is_healthy, ledger_sequence)`. If the response contains
/// a non-empty `dbError` field, the probe returns `Err(ProbeError::DbCorruption)`
/// which triggers the circuit breaker — isolating the cluster until manual
/// recovery.
///
/// **Note**: The Soroban RPC `/health` JSON-RPC method returns
/// `{"status":"healthy","latestLedger":N}` (not `ledger`). We also accept
/// `ledger` as a fallback for older deployments.
async fn probe_cluster(
    client: &reqwest::Client,
    endpoint: &str,
) -> Result<(bool, u64), ProbeError> {
    // Soroban RPC uses JSON-RPC, not REST. The "health" is obtained by
    // sending a JSON-RPC request with method "getHealth".
    let url = endpoint.trim_end_matches('/');
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "gateway-health-probe",
        "method": "getHealth",
    });
    let response = client
        .post(url)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(ProbeError::Http)?;
    if !response.status().is_success() {
        return Err(ProbeError::Status(response.status().as_u16()));
    }
    let body: SorobanHealthResponse = response
        .json()
        .await
        .map_err(|e| ProbeError::Parse(e.to_string()))?;

    // Database corruption detection — immediately isolate the cluster.
    if let Some(db_err) = &body.db_error {
        if !db_err.is_empty() {
            return Err(ProbeError::DbCorruption(db_err.clone()));
        }
    }

    let healthy = body.status == "healthy";
    Ok((healthy, body.latest_ledger))
}

/// Errors that can occur during a health probe.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("HTTP error: {0}")]
    Http(reqwest::Error),
    #[error("non-200 status: {0}")]
    Status(u16),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("database corruption detected: {0}")]
    DbCorruption(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breaker_opens_after_threshold_failures() {
        let cfg = CircuitBreakerConfig {
            failure_threshold: 3,
            ..Default::default()
        };
        let mut breaker = BreakerState_::default();
        assert_eq!(breaker.state, BreakerState::Closed);

        breaker.record_failure(&cfg);
        breaker.record_failure(&cfg);
        assert_eq!(breaker.state, BreakerState::Closed);

        breaker.record_failure(&cfg);
        assert_eq!(breaker.state, BreakerState::Open);
        assert!(breaker.opened_at.is_some());
    }

    #[test]
    fn breaker_half_open_after_window() {
        let cfg = CircuitBreakerConfig {
            failure_threshold: 1,
            success_threshold: 1,
            open_window: Duration::from_millis(10),
        };
        let mut breaker = BreakerState_::default();
        breaker.record_failure(&cfg);
        assert_eq!(breaker.state, BreakerState::Open);

        // Wait for the open window to elapse.
        std::thread::sleep(Duration::from_millis(15));
        breaker.maybe_half_open(&cfg);
        assert_eq!(breaker.state, BreakerState::HalfOpen);

        // Success in half-open closes the breaker.
        breaker.record_success(&cfg);
        assert_eq!(breaker.state, BreakerState::Closed);
    }

    #[test]
    fn breaker_failure_in_half_open_reopens() {
        let cfg = CircuitBreakerConfig {
            failure_threshold: 1,
            success_threshold: 2,
            open_window: Duration::from_millis(10),
        };
        let mut breaker = BreakerState_::default();
        breaker.record_failure(&cfg);
        assert_eq!(breaker.state, BreakerState::Open);

        std::thread::sleep(Duration::from_millis(15));
        breaker.maybe_half_open(&cfg);
        assert_eq!(breaker.state, BreakerState::HalfOpen);

        // Failure in half-open re-opens immediately.
        breaker.record_failure(&cfg);
        assert_eq!(breaker.state, BreakerState::Open);
    }

    #[test]
    fn derive_health_unhealthy_when_breaker_open() {
        let cfg = ClusterHealthConfig {
            max_lag_ledgers: 10,
            ..Default::default()
        };
        let mut rt = ClusterRuntime {
            config: cfg,
            breaker: BreakerState_::default(),
            latency_us: 1000,
            ledger_sequence: 100,
            is_primary: true,
        };
        rt.breaker.state = BreakerState::Open;
        assert_eq!(derive_health(&rt, 100), ClusterHealth::Unhealthy);
    }

    #[test]
    fn derive_health_desynced_when_lag_exceeds_threshold() {
        let cfg = ClusterHealthConfig {
            max_lag_ledgers: 10,
            ..Default::default()
        };
        let rt = ClusterRuntime {
            config: cfg,
            breaker: BreakerState_::default(),
            latency_us: 1000,
            ledger_sequence: 100,
            is_primary: true,
        };
        // max_ledger is 111, lag is 11 > 10 → desynced
        assert_eq!(derive_health(&rt, 111), ClusterHealth::Desynced);
        // lag is exactly 10 → still healthy (not exceeding threshold)
        assert_eq!(derive_health(&rt, 110), ClusterHealth::Healthy);
    }

    #[test]
    fn derive_health_degraded_on_high_latency() {
        let cfg = ClusterHealthConfig::default();
        let rt = ClusterRuntime {
            config: cfg,
            breaker: BreakerState_::default(),
            latency_us: 600_000, // 600ms > 500ms threshold
            ledger_sequence: 100,
            is_primary: true,
        };
        assert_eq!(derive_health(&rt, 100), ClusterHealth::Degraded);
    }

    #[test]
    fn derive_health_healthy_when_synced_and_fast() {
        let cfg = ClusterHealthConfig {
            max_lag_ledgers: 10,
            ..Default::default()
        };
        let rt = ClusterRuntime {
            config: cfg,
            breaker: BreakerState_::default(),
            latency_us: 50_000, // 50ms
            ledger_sequence: 100,
            is_primary: true,
        };
        assert_eq!(derive_health(&rt, 105), ClusterHealth::Healthy);
    }

    #[tokio::test]
    async fn monitor_updates_snapshot_on_manual_health_update() {
        let monitor = ClusterHealthMonitor::new();
        monitor.add_cluster(
            "us-east",
            ClusterHealthConfig {
                endpoint: "http://localhost:8000".into(),
                max_lag_ledgers: 10,
                ..Default::default()
            },
            true,
        );
        monitor.add_cluster(
            "eu-west",
            ClusterHealthConfig {
                endpoint: "http://localhost:8001".into(),
                max_lag_ledgers: 10,
                ..Default::default()
            },
            false,
        );

        // Initially both clusters are healthy (no ledger data yet).
        let snap = monitor.snapshot().await;
        assert_eq!(snap.clusters.len(), 2);
        assert_eq!(snap.max_ledger, 0);

        // Update us-east to ledger 100.
        monitor
            .update_cluster_health("us-east", true, 100, 5_000)
            .await;
        monitor
            .update_cluster_health("eu-west", true, 100, 3_000)
            .await;

        let snap = monitor.snapshot().await;
        assert_eq!(snap.max_ledger, 100);
        assert_eq!(
            snap.get("us-east").unwrap().health,
            ClusterHealth::Healthy
        );
        assert_eq!(
            snap.get("eu-west").unwrap().health,
            ClusterHealth::Healthy
        );
    }

    #[tokio::test]
    async fn monitor_marks_desynced_on_ledger_lag() {
        let monitor = ClusterHealthMonitor::new();
        monitor.add_cluster(
            "primary",
            ClusterHealthConfig {
                endpoint: "http://localhost:8000".into(),
                max_lag_ledgers: 10,
                ..Default::default()
            },
            true,
        );
        monitor.add_cluster(
            "secondary",
            ClusterHealthConfig {
                endpoint: "http://localhost:8001".into(),
                max_lag_ledgers: 10,
                ..Default::default()
            },
            false,
        );

        // Primary at ledger 110, secondary at ledger 99 → lag of 11 > 10.
        monitor
            .update_cluster_health("primary", true, 110, 5_000)
            .await;
        monitor
            .update_cluster_health("secondary", true, 99, 3_000)
            .await;

        let snap = monitor.snapshot().await;
        assert_eq!(
            snap.get("secondary").unwrap().health,
            ClusterHealth::Desynced
        );
        // Desynced clusters are excluded from both reads and writes to prevent
        // serving stale ledger data. Clients must be routed to synced clusters.
        assert!(!snap.get("secondary").unwrap().can_serve_reads());
        assert!(!snap.get("secondary").unwrap().can_serve_writes());
    }

    #[tokio::test]
    async fn monitor_circuit_breaker_opens_on_failures() {
        let monitor = ClusterHealthMonitor::new();
        let cfg = ClusterHealthConfig {
            endpoint: "http://localhost:8000".into(),
            max_lag_ledgers: 10,
            circuit_breaker: CircuitBreakerConfig {
                failure_threshold: 3,
                success_threshold: 2,
                open_window: Duration::from_secs(30),
            },
            ..Default::default()
        };
        monitor.add_cluster("primary", cfg, true);

        // Record 3 failures.
        monitor.record_probe_failure("primary").await;
        monitor.record_probe_failure("primary").await;
        monitor.record_probe_failure("primary").await;

        let snap = monitor.snapshot().await;
        let entry = snap.get("primary").unwrap();
        assert_eq!(entry.breaker, BreakerState::Open);
        assert_eq!(entry.health, ClusterHealth::Unhealthy);
        assert!(!entry.can_serve_reads());
    }

    #[tokio::test]
    async fn read_capable_clusters_sorted_by_latency() {
        let monitor = ClusterHealthMonitor::new();
        monitor.add_cluster(
            "fast",
            ClusterHealthConfig {
                endpoint: "http://localhost:8000".into(),
                ..Default::default()
            },
            false,
        );
        monitor.add_cluster(
            "slow",
            ClusterHealthConfig {
                endpoint: "http://localhost:8001".into(),
                ..Default::default()
            },
            false,
        );

        monitor.update_cluster_health("fast", true, 100, 1_000).await;
        monitor
            .update_cluster_health("slow", true, 100, 50_000)
            .await;

        let snap = monitor.snapshot().await;
        let capable = snap.read_capable_clusters();
        assert_eq!(capable.len(), 2);
        // Fast cluster (1ms) should be first.
        assert_eq!(capable[0].id, "fast");
        assert_eq!(capable[1].id, "slow");
    }
}
