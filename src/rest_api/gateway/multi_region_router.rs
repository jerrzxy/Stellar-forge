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
//! Multi-Region Traffic Router for Soroban RPC
//!
//! Routes client RPC requests across geographically distributed clusters,
//! selecting the lowest-latency fully-synced cluster and isolating clusters
//! experiencing database corruption or sync loss via circuit breakers.
//!
//! ## Design
//!
//! The router maintains per-cluster circuit breakers that open when a cluster
//! exhibits repeated failures or database corruption signals. When the primary
//! cluster falls behind (e.g. 10+ ledger delay), the router instantly routes
//! traffic to the secondary — without manual intervention.
//!
//! ## Request-Path Performance
//!
//! Routing decisions read cached health snapshots and circuit-breaker state
//! from in-memory structures, completing in well under 1 ms for typical
//! cluster counts.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, instrument, warn};

use crate::rest_api::gateway::health::{
    ClusterEndpoint, ClusterHealthChecker, ClusterHealthStatus, HealthEvaluation,
};

// ── Circuit Breaker ───────────────────────────────────────────────────────────

/// Circuit breaker state for a single cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakerState {
    /// Normal operation — traffic is allowed.
    Closed,
    /// Tripped — traffic is blocked until the recovery window elapses.
    Open,
    /// Probing — a single trial request is allowed to test recovery.
    HalfOpen,
}

impl std::fmt::Display for BreakerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(f, "closed"),
            Self::Open => write!(f, "open"),
            Self::HalfOpen => write!(f, "half_open"),
        }
    }
}

/// Per-cluster circuit breaker configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    /// Consecutive failures before the breaker opens.
    pub failure_threshold: u32,
    /// Consecutive successes in HalfOpen before the breaker closes.
    pub success_threshold: u32,
    /// How long the breaker stays open before transitioning to HalfOpen.
    pub open_window_ms: u64,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            success_threshold: 2,
            open_window_ms: 30_000,
        }
    }
}

/// Internal circuit breaker state machine for one cluster.
#[derive(Debug, Clone)]
struct CircuitBreaker {
    config: CircuitBreakerConfig,
    state: BreakerState,
    consecutive_failures: u32,
    consecutive_successes: u32,
    opened_at_ms: Option<u64>,
}

impl CircuitBreaker {
    fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            state: BreakerState::Closed,
            consecutive_failures: 0,
            consecutive_successes: 0,
            opened_at_ms: None,
        }
    }

    /// Check whether the breaker allows a request through.
    fn allow(&mut self, now_ms: u64) -> bool {
        match self.state {
            BreakerState::Closed => true,
            BreakerState::Open => {
                if let Some(opened_at) = self.opened_at_ms {
                    if now_ms.saturating_sub(opened_at) >= self.config.open_window_ms {
                        self.state = BreakerState::HalfOpen;
                        self.consecutive_successes = 0;
                        debug!("circuit breaker transitioning Open → HalfOpen");
                        return true;
                    }
                }
                false
            }
            BreakerState::HalfOpen => true,
        }
    }

    /// Record a success or failure, transitioning state as needed.
    fn on_result(&mut self, success: bool, now_ms: u64) {
        match self.state {
            BreakerState::Closed => {
                if success {
                    self.consecutive_failures = 0;
                } else {
                    self.consecutive_failures += 1;
                    if self.consecutive_failures >= self.config.failure_threshold {
                        self.state = BreakerState::Open;
                        self.opened_at_ms = Some(now_ms);
                        self.consecutive_successes = 0;
                        warn!("circuit breaker opening after {} failures", self.consecutive_failures);
                    }
                }
            }
            BreakerState::Open => {
                if now_ms.saturating_sub(self.opened_at_ms.unwrap_or(now_ms))
                    >= self.config.open_window_ms
                {
                    self.state = BreakerState::HalfOpen;
                    self.consecutive_successes = 0;
                }
            }
            BreakerState::HalfOpen => {
                if success {
                    self.consecutive_successes += 1;
                    if self.consecutive_successes >= self.config.success_threshold {
                        self.state = BreakerState::Closed;
                        self.consecutive_failures = 0;
                        info!("circuit breaker closing after {} successes", self.consecutive_successes);
                    }
                } else {
                    self.state = BreakerState::Open;
                    self.opened_at_ms = Some(now_ms);
                    self.consecutive_failures += 1;
                    warn!("circuit breaker reopening from HalfOpen after failure");
                }
            }
        }
    }

    /// Force the breaker open (e.g. database corruption detected).
    fn force_open(&mut self, now_ms: u64) {
        self.state = BreakerState::Open;
        self.opened_at_ms = Some(now_ms);
        self.consecutive_successes = 0;
    }

    /// Force the breaker closed (manual recovery).
    fn force_close(&mut self) {
        self.state = BreakerState::Closed;
        self.consecutive_failures = 0;
        self.consecutive_successes = 0;
        self.opened_at_ms = None;
    }
}

// ── Routing Decision ─────────────────────────────────────────────────────────

/// The outcome of a routing decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingDecision {
    /// Selected cluster endpoint.
    pub cluster_id: String,
    /// Endpoint URL to forward to.
    pub endpoint: String,
    /// Region of the selected cluster.
    pub region: String,
    /// Health status of the selected cluster.
    pub health_status: ClusterHealthStatus,
    /// Whether the request was routed to a fallback (non-primary) cluster.
    pub failed_over: bool,
    /// Latency of the selected cluster in ms.
    pub latency_ms: u64,
    /// Ledger sequence of the selected cluster.
    pub ledger_sequence: u64,
    /// Reason for this routing decision.
    pub reason: String,
}

/// Reason the router could not find a routable cluster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoutingError {
    /// No clusters are configured.
    NoClusters,
    /// All clusters are isolated by circuit breakers.
    AllIsolated,
    /// No cluster is healthy or synced enough to serve traffic.
    NoHealthyCluster,
    /// All available clusters exceed the latency threshold.
    AllExceedLatencyThreshold,
}

impl std::fmt::Display for RoutingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoClusters => write!(f, "no clusters configured"),
            Self::AllIsolated => write!(f, "all clusters isolated by circuit breakers"),
            Self::NoHealthyCluster => write!(f, "no healthy cluster available"),
            Self::AllExceedLatencyThreshold => {
                write!(f, "all available clusters exceed latency threshold")
            }
        }
    }
}

impl std::error::Error for RoutingError {}

// ── Multi-Region Router ───────────────────────────────────────────────────────

/// Configuration for the multi-region router.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiRegionRouterConfig {
    /// Primary cluster id — preferred when healthy.
    pub primary_cluster_id: String,
    /// Maximum latency (ms) — clusters above this are skipped.
    pub max_latency_ms: u64,
    /// Per-cluster circuit breaker configuration.
    pub circuit_breaker: CircuitBreakerConfig,
    /// Whether to prefer the primary cluster even if latency is slightly higher.
    pub prefer_primary: bool,
}

impl MultiRegionRouterConfig {
    /// Create a config with `primary_cluster_id` and sensible defaults.
    pub fn new(primary_cluster_id: impl Into<String>) -> Self {
        Self {
            primary_cluster_id: primary_cluster_id.into(),
            max_latency_ms: 2000,
            circuit_breaker: CircuitBreakerConfig::default(),
            prefer_primary: true,
        }
    }
}

/// Multi-region traffic router with circuit-breaker isolation.
///
/// Combines [`ClusterHealthChecker`] health snapshots with per-cluster
/// circuit breakers to make routing decisions.
pub struct MultiRegionRouter {
    config: MultiRegionRouterConfig,
    health_checker: ClusterHealthChecker,
    breakers: Arc<RwLock<HashMap<String, CircuitBreaker>>>,
    /// Tracks routing decisions for observability.
    stats: Arc<RwLock<RouterStats>>,
}

impl MultiRegionRouter {
    /// Create a new multi-region router.
    pub fn new(config: MultiRegionRouterConfig, health_checker: ClusterHealthChecker) -> Self {
        let breaker_config = config.circuit_breaker.clone();
        let breakers: HashMap<String, CircuitBreaker> = health_checker
            .clusters()
            .iter()
            .map(|c| (c.cluster_id.clone(), CircuitBreaker::new(breaker_config.clone())))
            .collect();
        Self {
            config,
            health_checker,
            breakers: Arc::new(RwLock::new(breakers)),
            stats: Arc::new(RwLock::new(RouterStats::default())),
        }
    }

    /// Return a reference to the health checker.
    pub fn health_checker(&self) -> &ClusterHealthChecker {
        &self.health_checker
    }

    /// Return a copy of the router configuration.
    pub fn config(&self) -> &MultiRegionRouterConfig {
        &self.config
    }

    /// Make a routing decision for the next request.
    ///
    /// This reads cached health snapshots and circuit-breaker state — no
    /// network I/O — and typically completes in under 1 ms.
    #[instrument(skip(self), level = "debug")]
    pub async fn route(&self) -> Result<RoutingDecision, RoutingError> {
        let route_start = Instant::now();

        let evaluations = self.health_checker.evaluate().await;
        if evaluations.is_empty() {
            return Err(RoutingError::NoClusters);
        }

        let now_ms = route_start.elapsed().as_millis() as u64;
        let mut breakers = self.breakers.write().await;

        // Check for DB corruption — force breakers open immediately.
        for eval in &evaluations {
            if eval.status == ClusterHealthStatus::Isolated {
                if let Some(b) = breakers.get_mut(&eval.cluster_id) {
                    b.force_open(now_ms);
                }
            }
        }

        // Build the list of routable clusters.
        let mut candidates: Vec<(&HealthEvaluation, bool)> = Vec::new();
        for eval in &evaluations {
            // Skip clusters that can't serve traffic at all.
            if !eval.status.can_serve_traffic() {
                continue;
            }
            // Skip clusters above the latency threshold.
            if eval.latency_ms > self.config.max_latency_ms {
                continue;
            }
            // Check circuit breaker.
            let allowed = breakers
                .get_mut(&eval.cluster_id)
                .map(|b| b.allow(now_ms))
                .unwrap_or(true);
            candidates.push((eval, allowed));
        }

        if candidates.is_empty() {
            // Determine the specific error.
            let all_isolated = evaluations
                .iter()
                .all(|e| e.status.is_isolated());
            if all_isolated {
                return Err(RoutingError::AllIsolated);
            }
            let all_high_latency = evaluations
                .iter()
                .filter(|e| e.status.can_serve_traffic())
                .all(|e| e.latency_ms > self.config.max_latency_ms);
            if all_high_latency && evaluations.iter().any(|e| e.status.can_serve_traffic()) {
                return Err(RoutingError::AllExceedLatencyThreshold);
            }
            return Err(RoutingError::NoHealthyCluster);
        }

        // Select the best candidate.
        let is_primary_available = candidates
            .iter()
            .filter(|(_, allowed)| *allowed)
            .any(|(e, _)| e.cluster_id == self.config.primary_cluster_id);

        let selected = if self.config.prefer_primary && is_primary_available {
            // Prefer primary if it's allowed and healthy.
            candidates
                .iter()
                .find(|(e, allowed)| *allowed && e.cluster_id == self.config.primary_cluster_id)
                .map(|(e, _)| *e)
        } else {
            // Otherwise pick the lowest-latency allowed candidate.
            candidates
                .iter()
                .filter(|(_, allowed)| *allowed)
                .min_by_key(|(e, _)| e.latency_ms)
                .map(|(e, _)| *e)
        };

        let selected = selected.ok_or(RoutingError::NoHealthyCluster)?;

        let failed_over = selected.cluster_id != self.config.primary_cluster_id;

        let reason = if failed_over {
            format!(
                "primary '{}' unavailable, routed to '{}'",
                self.config.primary_cluster_id, selected.cluster_id
            )
        } else {
            format!("routed to primary '{}'", selected.cluster_id)
        };

        // Update stats.
        {
            let mut stats = self.stats.write().await;
            stats.total_requests += 1;
            if failed_over {
                stats.failover_requests += 1;
            }
            stats
                .per_cluster
                .entry(selected.cluster_id.clone())
                .and_modify(|c| *c += 1)
                .or_insert(1);
        }

        let elapsed = route_start.elapsed();
        debug!("routing decision in {:?}: {}", elapsed, reason);

        Ok(RoutingDecision {
            cluster_id: selected.cluster_id.clone(),
            endpoint: selected.endpoint.clone(),
            region: self
                .health_checker
                .clusters()
                .iter()
                .find(|c| c.cluster_id == selected.cluster_id)
                .map(|c| c.region.clone())
                .unwrap_or_default(),
            health_status: selected.status.clone(),
            failed_over,
            latency_ms: selected.latency_ms,
            ledger_sequence: selected.ledger_sequence,
            reason,
        })
    }

    /// Record the result of a request to a cluster (success or failure).
    ///
    /// This updates the circuit breaker for the cluster.
    pub async fn record_result(&self, cluster_id: &str, success: bool) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let mut breakers = self.breakers.write().await;
        if let Some(b) = breakers.get_mut(cluster_id) {
            b.on_result(success, now_ms);
        }

        let mut stats = self.stats.write().await;
        if success {
            stats.total_successes += 1;
        } else {
            stats.total_failures += 1;
        }
    }

    /// Force-open the circuit breaker for a cluster (e.g. manual isolation).
    pub async fn isolate_cluster(&self, cluster_id: &str) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let mut breakers = self.breakers.write().await;
        if let Some(b) = breakers.get_mut(cluster_id) {
            b.force_open(now_ms);
            info!("cluster {} manually isolated", cluster_id);
        }
        // Also mark health as isolated.
        self.health_checker
            .mark_isolated(cluster_id, "manual isolation")
            .await;
    }

    /// Force-close the circuit breaker for a cluster (manual recovery).
    pub async fn recover_cluster(&self, cluster_id: &str) {
        let mut breakers = self.breakers.write().await;
        if let Some(b) = breakers.get_mut(cluster_id) {
            b.force_close();
            info!("cluster {} manually recovered", cluster_id);
        }
        self.health_checker.mark_recovered(cluster_id).await;
    }

    /// Get the current state of a cluster's circuit breaker.
    pub async fn breaker_state(&self, cluster_id: &str) -> Option<BreakerState> {
        self.breakers
            .read()
            .await
            .get(cluster_id)
            .map(|b| b.state)
    }

    /// Get router statistics.
    pub async fn stats(&self) -> RouterStats {
        self.stats.read().await.clone()
    }

    /// Refresh health snapshots in the background.
    pub async fn refresh_health(&self) {
        self.health_checker.refresh().await;
    }
}

/// Aggregate routing statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RouterStats {
    pub total_requests: u64,
    pub failover_requests: u64,
    pub total_successes: u64,
    pub total_failures: u64,
    /// Requests routed per cluster.
    pub per_cluster: HashMap<String, u64>,
}

impl RouterStats {
    /// Failover rate: fraction of requests routed to a non-primary cluster.
    pub fn failover_rate(&self) -> f64 {
        if self.total_requests == 0 {
            return 0.0;
        }
        self.failover_requests as f64 / self.total_requests as f64
    }

    /// Success rate across all clusters.
    pub fn success_rate(&self) -> f64 {
        let total = self.total_successes + self.total_failures;
        if total == 0 {
            return 1.0;
        }
        self.total_successes as f64 / total as f64
    }
}

/// Helper: build a [`MultiRegionRouter`] from a list of cluster endpoints.
pub fn build_router(
    primary_cluster_id: &str,
    clusters: Vec<ClusterEndpoint>,
    router_config: Option<MultiRegionRouterConfig>,
) -> MultiRegionRouter {
    let health_config = crate::rest_api::gateway::health::HealthCheckerConfig::default();
    let health_checker = ClusterHealthChecker::new(health_config, clusters);
    let config = router_config.unwrap_or_else(|| MultiRegionRouterConfig::new(primary_cluster_id));
    MultiRegionRouter::new(config, health_checker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rest_api::gateway::health::{
        ClusterEndpoint, ClusterHealthChecker, ClusterHealthStatus, HealthCheckerConfig,
        HealthSnapshot,
    };

    fn make_clusters() -> Vec<ClusterEndpoint> {
        vec![
            ClusterEndpoint::new("primary", "http://primary:8000", "us-east-1", 1),
            ClusterEndpoint::new("secondary", "http://secondary:8000", "eu-west-1", 2),
        ]
    }

    async fn set_healthy(checker: &ClusterHealthChecker, cluster: &str, ledger: u64, latency: u64) {
        checker
            .update_snapshot(
                cluster,
                HealthSnapshot {
                    cluster_id: cluster.into(),
                    endpoint: format!("http://{cluster}:8000"),
                    ledger_sequence: ledger,
                    network_ledger_sequence: ledger,
                    ledger_lag: 0,
                    latency_ms: latency,
                    is_catching_up: false,
                    db_corruption_detected: false,
                    status: ClusterHealthStatus::Healthy,
                    checked_at: chrono::Utc::now(),
                },
            )
            .await;
    }

    async fn set_unsynchronized(
        checker: &ClusterHealthChecker,
        cluster: &str,
        ledger: u64,
        network: u64,
    ) {
        checker
            .update_snapshot(
                cluster,
                HealthSnapshot {
                    cluster_id: cluster.into(),
                    endpoint: format!("http://{cluster}:8000"),
                    ledger_sequence: ledger,
                    network_ledger_sequence: network,
                    ledger_lag: network.saturating_sub(ledger),
                    latency_ms: 50,
                    is_catching_up: false,
                    db_corruption_detected: false,
                    status: ClusterHealthStatus::Unsynchronized,
                    checked_at: chrono::Utc::now(),
                },
            )
            .await;
    }

    // ── CircuitBreaker unit tests ──

    #[test]
    fn test_circuit_breaker_starts_closed() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig::default());
        assert_eq!(cb.state, BreakerState::Closed);
    }

    #[test]
    fn test_circuit_breaker_opens_after_threshold() {
        let mut cb = CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 3,
            success_threshold: 2,
            open_window_ms: 1000,
        });
        // Two failures: not yet open.
        cb.on_result(false, 0);
        cb.on_result(false, 0);
        assert_eq!(cb.state, BreakerState::Closed);
        // Third failure opens the breaker.
        cb.on_result(false, 0);
        assert_eq!(cb.state, BreakerState::Open);
    }

    #[test]
    fn test_circuit_breaker_half_open_after_window() {
        let mut cb = CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 1,
            success_threshold: 2,
            open_window_ms: 100,
        });
        cb.on_result(false, 0);
        assert_eq!(cb.state, BreakerState::Open);
        // After the window, allow() should transition to HalfOpen.
        let allowed = cb.allow(200);
        assert!(allowed);
        assert_eq!(cb.state, BreakerState::HalfOpen);
    }

    #[test]
    fn test_circuit_breaker_closes_after_success_threshold() {
        let mut cb = CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 1,
            success_threshold: 2,
            open_window_ms: 100,
        });
        cb.on_result(false, 0);
        cb.allow(200); // HalfOpen
        cb.on_result(true, 200);
        cb.on_result(true, 200);
        assert_eq!(cb.state, BreakerState::Closed);
    }

    #[test]
    fn test_circuit_breaker_reopens_on_half_open_failure() {
        let mut cb = CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 1,
            success_threshold: 2,
            open_window_ms: 100,
        });
        cb.on_result(false, 0); // Open
        cb.allow(200); // HalfOpen
        cb.on_result(false, 200); // Fail → Open
        assert_eq!(cb.state, BreakerState::Open);
    }

    #[test]
    fn test_circuit_breaker_force_open() {
        let mut cb = CircuitBreaker::new(CircuitBreakerConfig::default());
        cb.force_open(0);
        assert_eq!(cb.state, BreakerState::Open);
    }

    #[test]
    fn test_circuit_breaker_force_close() {
        let mut cb = CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 1,
            success_threshold: 2,
            open_window_ms: 100,
        });
        cb.force_open(0);
        cb.force_close();
        assert_eq!(cb.state, BreakerState::Closed);
    }

    // ── Router routing tests ──

    #[tokio::test]
    async fn test_route_to_primary_when_healthy() {
        let clusters = make_clusters();
        let health_checker =
            ClusterHealthChecker::new(HealthCheckerConfig::default(), clusters);
        set_healthy(&health_checker, "primary", 1000, 30).await;
        set_healthy(&health_checker, "secondary", 1000, 50).await;

        let router = MultiRegionRouter::new(
            MultiRegionRouterConfig::new("primary"),
            health_checker,
        );

        let decision = router.route().await.unwrap();
        assert_eq!(decision.cluster_id, "primary");
        assert!(!decision.failed_over);
        assert_eq!(decision.health_status, ClusterHealthStatus::Healthy);
    }

    #[tokio::test]
    async fn test_failover_when_primary_unsynchronized() {
        let clusters = make_clusters();
        let health_checker =
            ClusterHealthChecker::new(HealthCheckerConfig::default(), clusters);

        // Primary is 15 ledgers behind (lag > threshold of 10)
        set_unsynchronized(&health_checker, "primary", 985, 1000).await;
        set_healthy(&health_checker, "secondary", 1000, 50).await;

        let router = MultiRegionRouter::new(
            MultiRegionRouterConfig::new("primary"),
            health_checker,
        );

        let decision = router.route().await.unwrap();
        assert_eq!(decision.cluster_id, "secondary");
        assert!(decision.failed_over);
        assert_eq!(decision.health_status, ClusterHealthStatus::Healthy);
    }

    #[tokio::test]
    async fn test_simulate_10_ledger_sync_delay_failover() {
        // Reproduce the issue scenario: simulate a 10-ledger sync delay
        // on the Primary cluster and verify client requests route instantly
        // to the Secondary cluster.
        let clusters = make_clusters();
        let config = HealthCheckerConfig {
            max_ledger_lag: 10,
            ..Default::default()
        };
        let health_checker = ClusterHealthChecker::new(config, clusters);

        // Network ledger = 1000, primary at 1000 (fully synced)
        set_healthy(&health_checker, "primary", 1000, 20).await;
        set_healthy(&health_checker, "secondary", 1000, 40).await;

        let router = MultiRegionRouter::new(
            MultiRegionRouterConfig::new("primary"),
            health_checker.clone(),
        );

        // Initially, traffic goes to primary.
        let d1 = router.route().await.unwrap();
        assert_eq!(d1.cluster_id, "primary");
        assert!(!d1.failed_over);

        // Simulate 10-ledger delay on primary (network advances to 1010,
        // primary stays at 1000 → lag = 10, still within threshold).
        set_healthy(&health_checker, "primary", 1000, 20).await;
        {
            let mut snapshots = health_checker.get_all_snapshots().await;
            for snap in &mut snapshots {
                if snap.cluster_id == "primary" {
                    snap.network_ledger_sequence = 1010;
                    snap.ledger_lag = 10;
                }
                if snap.cluster_id == "secondary" {
                    snap.network_ledger_sequence = 1010;
                    snap.ledger_sequence = 1010;
                    snap.ledger_lag = 0;
                }
            }
            for snap in snapshots {
                health_checker.update_snapshot(&snap.cluster_id, snap).await;
            }
        }
        let d2 = router.route().await.unwrap();
        // At lag=10 (threshold), primary is still healthy → routed to primary.
        assert_eq!(d2.cluster_id, "primary");

        // Now delay by 11 ledgers (> threshold).
        {
            let mut snapshots = health_checker.get_all_snapshots().await;
            for snap in &mut snapshots {
                if snap.cluster_id == "primary" {
                    snap.network_ledger_sequence = 1011;
                    snap.ledger_lag = 11;
                    snap.status = ClusterHealthStatus::Unsynchronized;
                }
            }
            for snap in snapshots {
                health_checker.update_snapshot(&snap.cluster_id, snap).await;
            }
        }

        // Instantly route to secondary.
        let d3 = router.route().await.unwrap();
        assert_eq!(d3.cluster_id, "secondary");
        assert!(d3.failed_over);
        assert_eq!(d3.health_status, ClusterHealthStatus::Healthy);
    }

    #[tokio::test]
    async fn test_route_when_all_unsynchronized_returns_error() {
        let clusters = make_clusters();
        let health_checker =
            ClusterHealthChecker::new(HealthCheckerConfig::default(), clusters);
        set_unsynchronized(&health_checker, "primary", 900, 1000).await;
        set_unsynchronized(&health_checker, "secondary", 950, 1000).await;

        let router = MultiRegionRouter::new(
            MultiRegionRouterConfig::new("primary"),
            health_checker,
        );

        let result = router.route().await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), RoutingError::NoHealthyCluster);
    }

    #[tokio::test]
    async fn test_isolate_cluster_via_db_corruption() {
        let clusters = make_clusters();
        let health_checker =
            ClusterHealthChecker::new(HealthCheckerConfig::default(), clusters);
        set_healthy(&health_checker, "primary", 1000, 20).await;
        set_healthy(&health_checker, "secondary", 1000, 50).await;

        let router = MultiRegionRouter::new(
            MultiRegionRouterConfig::new("primary"),
            health_checker,
        );

        // Simulate database corruption on primary.
        router.isolate_cluster("primary").await;

        // Primary breaker should be open.
        assert_eq!(
            router.breaker_state("primary").await,
            Some(BreakerState::Open)
        );

        // Traffic should fail over to secondary.
        let decision = router.route().await.unwrap();
        assert_eq!(decision.cluster_id, "secondary");
        assert!(decision.failed_over);
    }

    #[tokio::test]
    async fn test_recover_cluster_re_enables_routing() {
        let clusters = make_clusters();
        let health_checker =
            ClusterHealthChecker::new(HealthCheckerConfig::default(), clusters);
        set_healthy(&health_checker, "primary", 1000, 20).await;
        set_healthy(&health_checker, "secondary", 1000, 50).await;

        let router = MultiRegionRouter::new(
            MultiRegionRouterConfig::new("primary"),
            health_checker,
        );

        // Isolate primary.
        router.isolate_cluster("primary").await;
        let d1 = router.route().await.unwrap();
        assert_eq!(d1.cluster_id, "secondary");

        // Recover primary.
        router.recover_cluster("primary").await;
        assert_eq!(
            router.breaker_state("primary").await,
            Some(BreakerState::Closed)
        );

        // Re-set primary health to healthy (recover marks it Unknown).
        router
            .health_checker()
            .update_snapshot(
                "primary",
                HealthSnapshot {
                    cluster_id: "primary".into(),
                    endpoint: "http://primary:8000".into(),
                    ledger_sequence: 1000,
                    network_ledger_sequence: 1000,
                    ledger_lag: 0,
                    latency_ms: 20,
                    is_catching_up: false,
                    db_corruption_detected: false,
                    status: ClusterHealthStatus::Healthy,
                    checked_at: chrono::Utc::now(),
                },
            )
            .await;

        let d2 = router.route().await.unwrap();
        assert_eq!(d2.cluster_id, "primary");
        assert!(!d2.failed_over);
    }

    #[tokio::test]
    async fn test_record_result_updates_breaker() {
        let clusters = make_clusters();
        let health_checker =
            ClusterHealthChecker::new(HealthCheckerConfig::default(), clusters);
        set_healthy(&health_checker, "primary", 1000, 20).await;

        let config = MultiRegionRouterConfig {
            primary_cluster_id: "primary".into(),
            max_latency_ms: 2000,
            circuit_breaker: CircuitBreakerConfig {
                failure_threshold: 3,
                success_threshold: 2,
                open_window_ms: 30_000,
            },
            prefer_primary: true,
        };
        let router = MultiRegionRouter::new(config, health_checker);

        // Record 3 failures → breaker opens.
        router.record_result("primary", false).await;
        router.record_result("primary", false).await;
        assert_eq!(
            router.breaker_state("primary").await,
            Some(BreakerState::Closed)
        );
        router.record_result("primary", false).await;
        assert_eq!(
            router.breaker_state("primary").await,
            Some(BreakerState::Open)
        );

        // Primary breaker is open → traffic should fail to secondary if available.
        // But secondary is unknown → NoHealthyCluster.
        let result = router.route().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_route_prefers_lowest_latency_without_primary_preference() {
        let clusters = vec![
            ClusterEndpoint::new("primary", "http://primary:8000", "us-east-1", 1),
            ClusterEndpoint::new("secondary", "http://secondary:8000", "eu-west-1", 2),
            ClusterEndpoint::new("tertiary", "http://tertiary:8000", "ap-south-1", 3),
        ];
        let health_checker =
            ClusterHealthChecker::new(HealthCheckerConfig::default(), clusters);
        set_healthy(&health_checker, "primary", 1000, 100).await;
        set_healthy(&health_checker, "secondary", 1000, 30).await;
        set_healthy(&health_checker, "tertiary", 1000, 60).await;

        let config = MultiRegionRouterConfig {
            primary_cluster_id: "primary".into(),
            max_latency_ms: 2000,
            circuit_breaker: CircuitBreakerConfig::default(),
            prefer_primary: false, // Don't prefer primary → pick lowest latency.
        };
        let router = MultiRegionRouter::new(config, health_checker);

        let decision = router.route().await.unwrap();
        assert_eq!(decision.cluster_id, "secondary"); // 30ms is lowest
        assert!(decision.failed_over);
    }

    #[tokio::test]
    async fn test_route_excludes_high_latency_clusters() {
        let clusters = make_clusters();
        let health_checker =
            ClusterHealthChecker::new(HealthCheckerConfig::default(), clusters);
        set_healthy(&health_checker, "primary", 1000, 5000).await;
        set_healthy(&health_checker, "secondary", 1000, 50).await;

        let config = MultiRegionRouterConfig {
            primary_cluster_id: "primary".into(),
            max_latency_ms: 1000,
            ..MultiRegionRouterConfig::new("primary")
        };
        let router = MultiRegionRouter::new(config, health_checker);

        let decision = router.route().await.unwrap();
        assert_eq!(decision.cluster_id, "secondary");
        assert!(decision.failed_over);
    }

    #[tokio::test]
    async fn test_router_stats_track_failovers() {
        let clusters = make_clusters();
        let health_checker =
            ClusterHealthChecker::new(HealthCheckerConfig::default(), clusters);
        set_healthy(&health_checker, "primary", 1000, 20).await;
        set_healthy(&health_checker, "secondary", 1000, 50).await;

        let router = MultiRegionRouter::new(
            MultiRegionRouterConfig::new("primary"),
            health_checker,
        );

        // Route 5 times to primary.
        for _ in 0..5 {
            router.route().await.unwrap();
        }

        // Isolate primary and route 3 times to secondary.
        router.isolate_cluster("primary").await;
        for _ in 0..3 {
            router.route().await.unwrap();
        }

        let stats = router.stats().await;
        assert_eq!(stats.total_requests, 8);
        assert_eq!(stats.failover_requests, 3);
        assert!((stats.failover_rate() - 0.375).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_build_router_helper() {
        let clusters = make_clusters();
        let router = build_router("primary", clusters, None);
        assert_eq!(router.config().primary_cluster_id, "primary");
        assert_eq!(router.health_checker().clusters().len(), 2);
    }

    #[tokio::test]
    async fn test_no_clusters_returns_error() {
        let health_checker =
            ClusterHealthChecker::new(HealthCheckerConfig::default(), vec![]);
        let router = MultiRegionRouter::new(
            MultiRegionRouterConfig::new("primary"),
            health_checker,
        );
        let result = router.route().await;
        assert_eq!(result.unwrap_err(), RoutingError::NoClusters);
    }

    #[tokio::test]
    async fn test_all_isolated_returns_all_isolated_error() {
        let clusters = make_clusters();
        let health_checker =
            ClusterHealthChecker::new(HealthCheckerConfig::default(), clusters);

        // Mark both as isolated.
        health_checker.mark_isolated("primary", "corruption").await;
        health_checker.mark_isolated("secondary", "corruption").await;

        let router = MultiRegionRouter::new(
            MultiRegionRouterConfig::new("primary"),
            health_checker,
        );

        let result = router.route().await;
        assert_eq!(result.unwrap_err(), RoutingError::AllIsolated);
    }

    #[tokio::test]
    async fn test_routing_decision_completes_under_1ms() {
        let clusters: Vec<ClusterEndpoint> = (0..10)
            .map(|i| {
                ClusterEndpoint::new(
                    format!("cluster-{i}"),
                    format!("http://c{i}:8000"),
                    "region",
                    i,
                )
            })
            .collect();
        let health_checker =
            ClusterHealthChecker::new(HealthCheckerConfig::default(), clusters);

        for i in 0..10u64 {
            set_healthy(&health_checker, &format!("cluster-{i}"), 1000, i * 10).await;
        }

        let router = MultiRegionRouter::new(
            MultiRegionRouterConfig {
                prefer_primary: false,
                ..MultiRegionRouterConfig::new("cluster-0")
            },
            health_checker,
        );

        let start = Instant::now();
        let _decision = router.route().await.unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(1),
            "routing decision took {:?}, expected < 1ms",
            elapsed
        );
    }
}
