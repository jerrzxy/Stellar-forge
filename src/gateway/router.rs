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
//! Multi-region traffic router for Soroban RPC with automatic failover.
//!
//! The router sits in front of multiple geographically distributed Soroban
//! RPC clusters and selects the best upstream for each incoming JSON-RPC
//! request. Selection is driven by the [`ClusterHealthMonitor`]'s lock-free
//! health snapshot, keeping per-request evaluation overhead well under 2ms.
//!
//! # Routing strategy
//!
//! - **Write methods** (`sendTransaction`, `simulateTransaction`): routed to
//!   the **primary** cluster when healthy. If the primary is unavailable, writes
//!   are routed to the next healthiest cluster that can serve writes. This
//!   preserves strong consistency — a transaction submitted to a lagging
//!   cluster would be rejected or delayed.
//! - **Read methods** (`getLedgerEntries`, `getLatestLedger`, `getNetwork`,
//!   `getHealth`, `getFeeStats`, `getEvents`, `getTransaction`,
//!   `getTransactions`, `getLedgers`): routed to the lowest-latency cluster
//!   that is synced (not desynchronized). This gives dApps the fastest
//!   possible read latency while ensuring data freshness.
//! - **Failover**: When the primary cluster falls behind by more than
//!   `max_lag_ledgers` (default 10), the router instantly redirects all
//!   traffic — reads and writes — to the secondary. No manual intervention
//!   is required; the health monitor's background probes detect the lag and
//!   update the snapshot within one probe interval.
//!
//! # Example
//!
//! ```rust,no_run
//! use stellar_k8s::gateway::health::{ClusterHealthConfig, ClusterHealthMonitor};
//! use stellar_k8s::gateway::router::{MultiRegionRouter, RouterConfig};
//! use std::time::Duration;
//!
//! # async fn example() {
//! let monitor = ClusterHealthMonitor::new();
//! monitor.add_cluster("us-east", ClusterHealthConfig {
//!     endpoint: "http://soroban-rpc-us-east:8000".into(),
//!     probe_interval: Duration::from_secs(5),
//!     max_lag_ledgers: 10,
//!     ..Default::default()
//! }, true);
//! monitor.add_cluster("eu-west", ClusterHealthConfig {
//!     endpoint: "http://soroban-rpc-eu-west:8000".into(),
//!     probe_interval: Duration::from_secs(5),
//!     max_lag_ledgers: 10,
//!     ..Default::default()
//! }, false);
//! monitor.start().await;
//!
//! let router = MultiRegionRouter::new(monitor, RouterConfig::default());
//!
//! // Route a JSON-RPC request.
//! let body = br#"{"jsonrpc":"2.0","id":1,"method":"getLatestLedger","params":{}}"#;
//! let decision = router.route(body).await;
//! println!("routed to: {}", decision.target_cluster_id);
//! # }
//! ```

use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, warn};

use crate::gateway::health::{
    ClusterHealth, ClusterHealthEntry, ClusterHealthMonitor, ClusterId, HealthSnapshot,
};

// ── Read/Write classification ────────────────────────────────────────────────

/// Soroban JSON-RPC methods that modify on-chain state and must be routed to
/// the primary for strong consistency.
const WRITE_METHODS: &[&str] = &["sendTransaction"];

/// Soroban JSON-RPC methods that are read-only (including `simulateTransaction`
/// which executes in a sandbox without committing state).
const READ_METHODS: &[&str] = &[
    "getLedgerEntry",
    "getLedgerEntries",
    "getLatestLedger",
    "getNetwork",
    "getHealth",
    "getFeeStats",
    "getEvents",
    "getTransaction",
    "getTransactions",
    "getLedgers",
    "simulateTransaction",
    "getVersionInfo",
    "getContractData",
    "getContractCode",
];

/// Classifies a JSON-RPC method as read or write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RpcMethodKind {
    /// Modifies on-chain state — routed to primary.
    Write,
    /// Read-only or simulation — routed to lowest-latency synced cluster.
    Read,
    /// Unknown method — treated as write for safety.
    Unknown,
}

impl RpcMethodKind {
    /// Classify a JSON-RPC method string.
    pub fn from_method(method: &str) -> Self {
        if WRITE_METHODS.contains(&method) {
            Self::Write
        } else if READ_METHODS.contains(&method) {
            Self::Read
        } else {
            Self::Unknown
        }
    }

    /// Returns `true` for write methods (including unknown, which are treated
    /// as writes for safety).
    pub fn is_write(self) -> bool {
        matches!(self, Self::Write | Self::Unknown)
    }
}

// ── Router configuration ──────────────────────────────────────────────────────

/// Configuration for the multi-region router.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterConfig {
    /// Maximum ledger lag (in ledgers) before the primary is considered
    /// desynchronized and failover is triggered. Defaults to 10.
    pub max_lag_ledgers: u64,
    /// If `true`, the router falls back to degraded clusters when no healthy
    /// cluster is available. Otherwise, it returns an error.
    pub allow_degraded_fallback: bool,
    /// HTTP client timeout for upstream requests.
    pub upstream_timeout: std::time::Duration,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            max_lag_ledgers: 10,
            allow_degraded_fallback: true,
            upstream_timeout: std::time::Duration::from_secs(10),
        }
    }
}

// ── Routing decision ────────────────────────────────────────────────────────

/// The outcome of a routing decision.
#[derive(Debug, Clone)]
pub struct RoutingDecision {
    /// ID of the cluster selected to receive the request.
    pub target_cluster_id: ClusterId,
    /// Endpoint URL of the selected cluster.
    pub target_endpoint: String,
    /// Whether this is a read or write request.
    pub method_kind: RpcMethodKind,
    /// The JSON-RPC method name (if parseable).
    pub method: Option<String>,
    /// Why this cluster was chosen (for observability / metrics).
    pub reason: RouteReason,
    /// Time spent making the decision (for latency budget verification).
    pub eval_duration_us: u64,
}

/// Why a particular cluster was selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteReason {
    /// Primary cluster is healthy and this is a write request.
    PrimaryHealthy,
    /// Primary is unhealthy; failed over to the best available write-capable
    /// cluster.
    FailoverWrite,
    /// Lowest-latency synced cluster for a read request.
    LowestLatency,
    /// Only degraded clusters are available, and degraded fallback is enabled.
    DegradedFallback,
    /// No cluster is available to serve the request.
    NoClusterAvailable,
}

/// Error returned when no cluster can serve the request.
#[derive(Debug, thiserror::Error)]
#[error("no healthy cluster available for {method:?} request")]
pub struct NoClusterError {
    pub method: Option<String>,
    pub method_kind: RpcMethodKind,
}

// ── Multi-region router ─────────────────────────────────────────────────────

/// Multi-region Soroban RPC traffic router with automatic failover.
///
/// The router is designed for the request hot path: [`MultiRegionRouter::route`]
/// performs no network I/O and no locks. It reads a single `Arc<HealthSnapshot>`
/// from the health monitor and selects a target in a few hash lookups + a sort.
pub struct MultiRegionRouter {
    monitor: ClusterHealthMonitor,
    config: RouterConfig,
    http_client: reqwest::Client,
    metrics: AtomicRouterMetrics,
}

impl MultiRegionRouter {
    /// Create a new router backed by the given health monitor.
    pub fn new(monitor: ClusterHealthMonitor, config: RouterConfig) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(config.upstream_timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            monitor,
            config,
            http_client,
            metrics: AtomicRouterMetrics::default(),
        }
    }

    /// Get the current health snapshot (cheap `Arc` clone).
    pub async fn health_snapshot(&self) -> Arc<HealthSnapshot> {
        self.monitor.snapshot().await
    }

    /// Get a snapshot of the router's atomic metrics.
    pub fn metrics(&self) -> RouterMetrics {
        self.metrics.snapshot()
    }

    /// Decide which cluster should receive the given JSON-RPC request body.
    /// This is the core routing decision — no network I/O, sub-2ms.
    pub async fn route(&self, body: &[u8]) -> RoutingDecision {
        let eval_start = Instant::now();
        let snapshot = self.monitor.snapshot().await;
        let (method, method_kind) = parse_method(body);
        self.route_with_snapshot(&snapshot, method.as_deref(), method_kind, eval_start)
    }

    /// Synchronous routing decision using a blocking snapshot read.
    /// Useful in non-async contexts (e.g., Axum middleware without `.await`).
    pub fn route_blocking(&self, body: &[u8]) -> RoutingDecision {
        let eval_start = Instant::now();
        let snapshot = self.monitor.snapshot_blocking();
        let (method, method_kind) = parse_method(body);
        self.route_with_snapshot(&snapshot, method.as_deref(), method_kind, eval_start)
    }

    /// Core selection logic — operates on a pre-fetched snapshot.
    fn route_with_snapshot(
        &self,
        snapshot: &HealthSnapshot,
        method: Option<&str>,
        method_kind: RpcMethodKind,
        eval_start: Instant,
    ) -> RoutingDecision {
        let decision = if method_kind.is_write() {
            self.select_for_write(snapshot)
        } else {
            self.select_for_read(snapshot)
        };

        // Measure eval duration AFTER selection but BEFORE we overwrite the
        // field — this captures the true selection cost (hash lookups, sort).
        let eval_duration_us = eval_start.elapsed().as_micros() as u64;

        let mut decision = decision;
        decision.method = method.map(|s| s.to_string());
        decision.method_kind = method_kind;
        decision.eval_duration_us = eval_duration_us;

        // Record atomic metrics.
        self.metrics.record_request(&decision);

        decision
    }

    /// Select a cluster for write requests.
    ///
    /// Strategy: prefer the primary if it can serve writes. If the primary is
    /// unavailable, find the next write-capable cluster (sorted by ledger
    /// sequence — closest to the network head gets priority for writes).
    fn select_for_write(&self, snapshot: &HealthSnapshot) -> RoutingDecision {
        // 1. Try the primary cluster first.
        if let Some(primary) = snapshot.clusters.values().find(|e| e.is_primary) {
            if primary.can_serve_writes() {
                return RoutingDecision {
                    target_cluster_id: primary.id.clone(),
                    target_endpoint: primary.endpoint.clone(),
                    method_kind: RpcMethodKind::Write,
                    method: None,
                    reason: RouteReason::PrimaryHealthy,
                    eval_duration_us: 0,
                };
            }
        }

        // 2. Primary unavailable — find the best write-capable cluster.
        //    For writes, prefer the cluster closest to the network head
        //    (highest ledger sequence) to minimize transaction rejection.
        let mut candidates: Vec<&ClusterHealthEntry> = snapshot
            .clusters
            .values()
            .filter(|e| !e.is_primary && e.can_serve_writes())
            .collect();

        candidates.sort_by(|a, b| b.ledger_sequence.cmp(&a.ledger_sequence));

        if let Some(best) = candidates.first() {
            debug!(
                primary_unavailable = true,
                failover_to = %best.id,
                ledger = best.ledger_sequence,
                "write failover"
            );
            return RoutingDecision {
                target_cluster_id: best.id.clone(),
                target_endpoint: best.endpoint.clone(),
                method_kind: RpcMethodKind::Write,
                method: None,
                reason: RouteReason::FailoverWrite,
                eval_duration_us: 0,
            };
        }

        // 3. No write-capable clusters — try degraded fallback if allowed.
        if self.config.allow_degraded_fallback {
            if let Some(degraded) = self.select_degraded_fallback(snapshot) {
                return RoutingDecision {
                    target_cluster_id: degraded.id.clone(),
                    target_endpoint: degraded.endpoint.clone(),
                    method_kind: RpcMethodKind::Write,
                    method: None,
                    reason: RouteReason::DegradedFallback,
                    eval_duration_us: 0,
                };
            }
        }

        RoutingDecision {
            target_cluster_id: String::new(),
            target_endpoint: String::new(),
            method_kind: RpcMethodKind::Write,
            method: None,
            reason: RouteReason::NoClusterAvailable,
            eval_duration_us: 0,
        }
    }

    /// Select a cluster for read requests.
    ///
    /// Strategy: pick the lowest-latency synced (not desynced) cluster.
    /// Desynced clusters are excluded to prevent serving stale data.
    fn select_for_read(&self, snapshot: &HealthSnapshot) -> RoutingDecision {
        let candidates: Vec<&ClusterHealthEntry> =
            snapshot.read_capable_clusters();

        if let Some(best) = candidates.first() {
            return RoutingDecision {
                target_cluster_id: best.id.clone(),
                target_endpoint: best.endpoint.clone(),
                method_kind: RpcMethodKind::Read,
                method: None,
                reason: RouteReason::LowestLatency,
                eval_duration_us: 0,
            };
        }

        // No healthy/degraded clusters — try degraded fallback.
        if self.config.allow_degraded_fallback {
            if let Some(degraded) = self.select_degraded_fallback(snapshot) {
                return RoutingDecision {
                    target_cluster_id: degraded.id.clone(),
                    target_endpoint: degraded.endpoint.clone(),
                    method_kind: RpcMethodKind::Read,
                    method: None,
                    reason: RouteReason::DegradedFallback,
                    eval_duration_us: 0,
                };
            }
        }

        RoutingDecision {
            target_cluster_id: String::new(),
            target_endpoint: String::new(),
            method_kind: RpcMethodKind::Read,
            method: None,
            reason: RouteReason::NoClusterAvailable,
            eval_duration_us: 0,
        }
    }

    /// Find a degraded cluster to use as last resort.
    fn select_degraded_fallback<'a>(
        &self,
        snapshot: &'a HealthSnapshot,
    ) -> Option<&'a ClusterHealthEntry> {
        snapshot
            .clusters
            .values()
            .filter(|e| e.health == ClusterHealth::Degraded && e.breaker.allows_traffic())
            .min_by_key(|e| e.latency_us)
    }

    /// Forward a JSON-RPC request body to the selected upstream cluster.
    ///
    /// This is a convenience method that combines routing + forwarding.
    /// Returns the raw response bytes from the upstream.
    pub async fn forward(
        &self,
        body: &[u8],
    ) -> Result<reqwest::Response, ForwardError> {
        let decision = self.route(body).await;
        if decision.reason == RouteReason::NoClusterAvailable {
            return Err(ForwardError::NoCluster(NoClusterError {
                method: decision.method.clone(),
                method_kind: decision.method_kind,
            }));
        }

        let endpoint = decision.target_endpoint.trim_end_matches('/');
        debug!(
            cluster = %decision.target_cluster_id,
            endpoint = endpoint,
            method = ?decision.method,
            reason = ?decision.reason,
            "forwarding RPC request"
        );

        self.http_client
            .post(endpoint)
            .header("content-type", "application/json")
            .body(body.to_vec())
            .send()
            .await
            .map_err(ForwardError::Http)
    }

    /// Get a reference to the HTTP client (for custom forwarding logic).
    pub fn http_client(&self) -> &reqwest::Client {
        &self.http_client
    }

    /// Get a reference to the router config.
    pub fn config(&self) -> &RouterConfig {
        &self.config
    }
}

/// Error during request forwarding.
#[derive(Debug, thiserror::Error)]
pub enum ForwardError {
    #[error(transparent)]
    NoCluster(NoClusterError),
    #[error("upstream HTTP error: {0}")]
    Http(reqwest::Error),
}

// ── Method parsing ───────────────────────────────────────────────────────────

/// Extract the JSON-RPC method name from a request body and classify it.
fn parse_method(body: &[u8]) -> (Option<String>, RpcMethodKind) {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return (None, RpcMethodKind::Unknown);
    };
    let Some(obj) = value.as_object() else {
        return (None, RpcMethodKind::Unknown);
    };
    let Some(method_val) = obj.get("method") else {
        return (None, RpcMethodKind::Unknown);
    };
    let Some(method_str) = method_val.as_str() else {
        return (None, RpcMethodKind::Unknown);
    };
    let kind = RpcMethodKind::from_method(method_str);
    (Some(method_str.to_string()), kind)
}

// ── Router metrics ───────────────────────────────────────────────────────────

/// Lightweight metrics for the router, queryable for observability.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RouterMetrics {
    pub total_requests: u64,
    pub read_requests: u64,
    pub write_requests: u64,
    pub failovers: u64,
    pub degraded_fallbacks: u64,
    pub no_cluster_available: u64,
}

/// Atomic, lock-free metrics counters for the router hot path.
///
/// These are updated on every request via `record_request` without taking
/// any locks, keeping the overhead under the 2ms budget.
#[derive(Debug, Default)]
pub struct AtomicRouterMetrics {
    total_requests: std::sync::atomic::AtomicU64,
    read_requests: std::sync::atomic::AtomicU64,
    write_requests: std::sync::atomic::AtomicU64,
    failovers: std::sync::atomic::AtomicU64,
    degraded_fallbacks: std::sync::atomic::AtomicU64,
    no_cluster_available: std::sync::atomic::AtomicU64,
}

impl AtomicRouterMetrics {
    /// Record a routing decision in the atomic counters.
    fn record_request(&self, decision: &RoutingDecision) {
        use std::sync::atomic::Ordering::Relaxed;
        self.total_requests.fetch_add(1, Relaxed);
        match decision.method_kind {
            RpcMethodKind::Read => {
                self.read_requests.fetch_add(1, Relaxed);
            }
            RpcMethodKind::Write | RpcMethodKind::Unknown => {
                self.write_requests.fetch_add(1, Relaxed);
            }
        }
        match decision.reason {
            RouteReason::FailoverWrite => {
                self.failovers.fetch_add(1, Relaxed);
            }
            RouteReason::DegradedFallback => {
                self.degraded_fallbacks.fetch_add(1, Relaxed);
            }
            RouteReason::NoClusterAvailable => {
                self.no_cluster_available.fetch_add(1, Relaxed);
            }
            _ => {}
        }
    }

    /// Snapshot the current metrics values as a [`RouterMetrics`] struct.
    pub fn snapshot(&self) -> RouterMetrics {
        use std::sync::atomic::Ordering::Relaxed;
        RouterMetrics {
            total_requests: self.total_requests.load(Relaxed),
            read_requests: self.read_requests.load(Relaxed),
            write_requests: self.write_requests.load(Relaxed),
            failovers: self.failovers.load(Relaxed),
            degraded_fallbacks: self.degraded_fallbacks.load(Relaxed),
            no_cluster_available: self.no_cluster_available.load(Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::health::{
        CircuitBreakerConfig, ClusterHealthConfig, ClusterHealthMonitor,
    };
    use std::time::Duration;

    fn build_monitor_with_clusters(
        primary_ledger: u64,
        secondary_ledger: u64,
        primary_latency_us: u64,
        secondary_latency_us: u64,
        max_lag: u64,
    ) -> ClusterHealthMonitor {
        let monitor = ClusterHealthMonitor::new();
        monitor.add_cluster(
            "primary",
            ClusterHealthConfig {
                endpoint: "http://primary:8000".into(),
                max_lag_ledgers: max_lag,
                ..Default::default()
            },
            true,
        );
        monitor.add_cluster(
            "secondary",
            ClusterHealthConfig {
                endpoint: "http://secondary:8000".into(),
                max_lag_ledgers: max_lag,
                ..Default::default()
            },
            false,
        );

        // We need to set ledger/lactency directly. Use update_cluster_health.
        // These calls are async but we need synchronous test setup.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            monitor
                .update_cluster_health("primary", true, primary_ledger, primary_latency_us)
                .await;
            monitor
                .update_cluster_health(
                    "secondary",
                    true,
                    secondary_ledger,
                    secondary_latency_us,
                )
                .await;
        });

        monitor
    }

    #[test]
    fn parse_method_extracts_read_method() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"getLatestLedger","params":{}}"#;
        let (method, kind) = parse_method(body);
        assert_eq!(method.as_deref(), Some("getLatestLedger"));
        assert_eq!(kind, RpcMethodKind::Read);
    }

    #[test]
    fn parse_method_extracts_write_method() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"sendTransaction","params":{"tx":"abc"}}"#;
        let (method, kind) = parse_method(body);
        assert_eq!(method.as_deref(), Some("sendTransaction"));
        assert_eq!(kind, RpcMethodKind::Write);
    }

    #[test]
    fn parse_method_handles_invalid_json() {
        let body = b"not json";
        let (method, kind) = parse_method(body);
        assert_eq!(method, None);
        assert_eq!(kind, RpcMethodKind::Unknown);
    }

    #[test]
    fn parse_method_handles_missing_method_field() {
        let body = br#"{"jsonrpc":"2.0","id":1,"params":{}}"#;
        let (method, kind) = parse_method(body);
        assert_eq!(method, None);
        assert_eq!(kind, RpcMethodKind::Unknown);
    }

    #[test]
    fn rpc_method_kind_classifies_correctly() {
        assert_eq!(RpcMethodKind::from_method("sendTransaction"), RpcMethodKind::Write);
        assert_eq!(RpcMethodKind::from_method("getLatestLedger"), RpcMethodKind::Read);
        assert_eq!(RpcMethodKind::from_method("simulateTransaction"), RpcMethodKind::Read);
        assert_eq!(RpcMethodKind::from_method("unknownMethod"), RpcMethodKind::Unknown);
    }

    #[test]
    fn unknown_method_treated_as_write_for_safety() {
        assert!(RpcMethodKind::Unknown.is_write());
        assert!(RpcMethodKind::Write.is_write());
        assert!(!RpcMethodKind::Read.is_write());
    }

    #[tokio::test]
    async fn write_request_routed_to_primary_when_healthy() {
        let monitor = build_monitor_with_clusters(100, 100, 5_000, 3_000, 10);
        let router = MultiRegionRouter::new(monitor, RouterConfig::default());

        let body = br#"{"jsonrpc":"2.0","id":1,"method":"sendTransaction","params":{}}"#;
        let decision = router.route(body).await;

        assert_eq!(decision.target_cluster_id, "primary");
        assert_eq!(decision.method_kind, RpcMethodKind::Write);
        assert_eq!(decision.reason, RouteReason::PrimaryHealthy);
    }

    #[tokio::test]
    async fn read_request_routed_to_lowest_latency() {
        let monitor = build_monitor_with_clusters(100, 100, 50_000, 3_000, 10);
        let router = MultiRegionRouter::new(monitor, RouterConfig::default());

        let body = br#"{"jsonrpc":"2.0","id":1,"method":"getLatestLedger","params":{}}"#;
        let decision = router.route(body).await;

        // Secondary has lower latency (3ms vs 50ms) and both are synced.
        assert_eq!(decision.target_cluster_id, "secondary");
        assert_eq!(decision.method_kind, RpcMethodKind::Read);
        assert_eq!(decision.reason, RouteReason::LowestLatency);
    }

    #[tokio::test]
    async fn failover_on_10_ledger_sync_delay() {
        // Primary at 100, secondary at 111 → primary lags by 11 > 10.
        let monitor = build_monitor_with_clusters(100, 111, 5_000, 3_000, 10);
        let router = MultiRegionRouter::new(monitor, RouterConfig::default());

        // Write request should failover to secondary (primary is desynced).
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"sendTransaction","params":{}}"#;
        let decision = router.route(body).await;

        assert_eq!(decision.target_cluster_id, "secondary");
        assert_eq!(decision.reason, RouteReason::FailoverWrite);
    }

    #[tokio::test]
    async fn failover_read_on_10_ledger_sync_delay() {
        // Primary at 100, secondary at 111 → primary is desynced (lag 11 > 10).
        let monitor = build_monitor_with_clusters(100, 111, 5_000, 3_000, 10);
        let router = MultiRegionRouter::new(monitor, RouterConfig::default());

        let body = br#"{"jsonrpc":"2.0","id":1,"method":"getLatestLedger","params":{}}"#;
        let decision = router.route(body).await;

        // Primary is desynced — only secondary can serve reads.
        assert_eq!(decision.target_cluster_id, "secondary");
        assert_eq!(decision.reason, RouteReason::LowestLatency);
    }

    #[tokio::test]
    async fn no_cluster_available_when_all_unhealthy() {
        let monitor = ClusterHealthMonitor::new();
        monitor.add_cluster(
            "primary",
            ClusterHealthConfig {
                endpoint: "http://primary:8000".into(),
                circuit_breaker: CircuitBreakerConfig {
                    failure_threshold: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
            true,
        );

        // Force breaker open via 1 failure.
        monitor.record_probe_failure("primary").await;

        let router = MultiRegionRouter::new(
            monitor,
            RouterConfig {
                allow_degraded_fallback: false,
                ..Default::default()
            },
        );

        let body = br#"{"jsonrpc":"2.0","id":1,"method":"getLatestLedger","params":{}}"#;
        let decision = router.route(body).await;

        assert_eq!(decision.reason, RouteReason::NoClusterAvailable);
        assert!(decision.target_cluster_id.is_empty());
    }

    #[tokio::test]
    async fn degraded_fallback_when_no_healthy_cluster() {
        let monitor = ClusterHealthMonitor::new();
        monitor.add_cluster(
            "slow",
            ClusterHealthConfig {
                endpoint: "http://slow:8000".into(),
                max_lag_ledgers: 10,
                ..Default::default()
            },
            true,
        );

        // Make the cluster degraded via high latency.
        monitor.update_cluster_health("slow", true, 100, 600_000).await;

        let router = MultiRegionRouter::new(
            monitor,
            RouterConfig {
                allow_degraded_fallback: true,
                ..Default::default()
            },
        );

        let body = br#"{"jsonrpc":"2.0","id":1,"method":"getLatestLedger","params":{}}"#;
        let decision = router.route(body).await;

        assert_eq!(decision.target_cluster_id, "slow");
        assert_eq!(decision.reason, RouteReason::DegradedFallback);
    }

    #[tokio::test]
    async fn route_eval_under_2ms() {
        // Build a monitor with 5 clusters to simulate a realistic setup.
        let monitor = ClusterHealthMonitor::new();
        for i in 0..5 {
            monitor.add_cluster(
                format!("cluster-{i}"),
                ClusterHealthConfig {
                    endpoint: format!("http://cluster-{i}:8000"),
                    max_lag_ledgers: 10,
                    ..Default::default()
                },
                i == 0,
            );
        }

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            for i in 0..5u64 {
                monitor
                    .update_cluster_health(
                        &format!("cluster-{i}"),
                        true,
                        100 + i,
                        (i + 1) * 1_000,
                    )
                    .await;
            }
        });

        let router = MultiRegionRouter::new(monitor, RouterConfig::default());

        let body = br#"{"jsonrpc":"2.0","id":1,"method":"getLatestLedger","params":{}}"#;

        // Warm up (first call may include one-time allocation).
        let _ = router.route(body).await;

        // Measure.
        let decision = router.route(body).await;

        // The eval_duration_us is measured inside route(). It should be well
        // under 2000us (2ms) for 5 clusters.
        assert!(
            decision.eval_duration_us < 2000,
            "route evaluation took {}us, expected < 2000us",
            decision.eval_duration_us
        );
    }

    #[tokio::test]
    async fn write_failover_prefers_highest_ledger() {
        // Primary is unhealthy, two secondaries with different ledgers.
        let monitor = ClusterHealthMonitor::new();
        monitor.add_cluster(
            "primary",
            ClusterHealthConfig {
                endpoint: "http://primary:8000".into(),
                circuit_breaker: CircuitBreakerConfig {
                    failure_threshold: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
            true,
        );
        monitor.add_cluster(
            "secondary-a",
            ClusterHealthConfig {
                endpoint: "http://secondary-a:8000".into(),
                max_lag_ledgers: 10,
                ..Default::default()
            },
            false,
        );
        monitor.add_cluster(
            "secondary-b",
            ClusterHealthConfig {
                endpoint: "http://secondary-b:8000".into(),
                max_lag_ledgers: 10,
                ..Default::default()
            },
            false,
        );

        // Primary breaker open.
        monitor.record_probe_failure("primary").await;
        // Secondary-A at ledger 105, Secondary-B at ledger 110.
        monitor.update_cluster_health("secondary-a", true, 105, 10_000).await;
        monitor.update_cluster_health("secondary-b", true, 110, 8_000).await;

        let router = MultiRegionRouter::new(monitor, RouterConfig::default());

        let body = br#"{"jsonrpc":"2.0","id":1,"method":"sendTransaction","params":{}}"#;
        let decision = router.route(body).await;

        // Should failover to secondary-b (highest ledger = 110).
        assert_eq!(decision.target_cluster_id, "secondary-b");
        assert_eq!(decision.reason, RouteReason::FailoverWrite);
    }

    #[tokio::test]
    async fn simulate_transaction_routed_as_read() {
        let monitor = build_monitor_with_clusters(100, 100, 50_000, 3_000, 10);
        let router = MultiRegionRouter::new(monitor, RouterConfig::default());

        let body = br#"{"jsonrpc":"2.0","id":1,"method":"simulateTransaction","params":{}}"#;
        let decision = router.route(body).await;

        // simulateTransaction is read-only, should go to lowest latency.
        assert_eq!(decision.target_cluster_id, "secondary");
        assert_eq!(decision.method_kind, RpcMethodKind::Read);
    }

    #[tokio::test]
    async fn unknown_method_routed_as_write() {
        let monitor = build_monitor_with_clusters(100, 100, 5_000, 3_000, 10);
        let router = MultiRegionRouter::new(monitor, RouterConfig::default());

        let body = br#"{"jsonrpc":"2.0","id":1,"method":"customMethod","params":{}}"#;
        let decision = router.route(body).await;

        // Unknown methods default to write → routed to primary.
        assert_eq!(decision.target_cluster_id, "primary");
        assert_eq!(decision.method_kind, RpcMethodKind::Unknown);
    }
}
