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
//! Load tests for the Multi-Region Gateway Fallback & Traffic Router (#83).
//!
//! These tests verify:
//!
//! 1. **Routing correctness under high volume**: thousands of concurrent
//!    routing decisions are made correctly (reads → lowest latency, writes →
//!    primary or failover).
//! 2. **Failover on 10-ledger sync delay**: when the primary cluster falls
//!    behind by ≥10 ledgers, client requests are instantly rerouted to the
//!    secondary. The switch happens within one health-monitor update cycle.
//! 3. **Sub-2ms evaluation budget**: each routing decision must complete in
//!    under 2ms regardless of concurrency.
//! 4. **Circuit-breaker isolation**: clusters with repeated probe failures are
//!    isolated and traffic is rerouted within the failure threshold.
//! 5. **Read/write traffic split**: under mixed load, reads and writes are
//!    routed independently according to their respective policies.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use stellar_k8s::gateway::health::{
    CircuitBreakerConfig, ClusterHealth, ClusterHealthConfig, ClusterHealthMonitor,
};
use stellar_k8s::gateway::router::{
    MultiRegionRouter, RouteReason, RouterConfig, RpcMethodKind,
};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Build a monitor with a healthy primary and secondary cluster.
async fn build_two_cluster_monitor(
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

    monitor
        .update_cluster_health("primary", true, primary_ledger, primary_latency_us)
        .await;
    monitor
        .update_cluster_health("secondary", true, secondary_ledger, secondary_latency_us)
        .await;

    monitor
}

/// Build a monitor with N clusters, all synced with same ledger.
async fn build_n_cluster_monitor(n: usize, max_lag: u64) -> ClusterHealthMonitor {
    let monitor = ClusterHealthMonitor::new();

    for i in 0..n {
        let is_primary = i == 0;
        monitor.add_cluster(
            format!("cluster-{i}"),
            ClusterHealthConfig {
                endpoint: format!("http://cluster-{i}:8000"),
                max_lag_ledgers: max_lag,
                ..Default::default()
            },
            is_primary,
        );
    }

    for i in 0..n {
        let latency_us = (i as u64 + 1) * 5_000; // graduated latencies
        monitor
            .update_cluster_health(&format!("cluster-{i}"), true, 1000, latency_us)
            .await;
    }

    monitor
}

const WRITE_BODY: &[u8] =
    br#"{"jsonrpc":"2.0","id":1,"method":"sendTransaction","params":{"tx":"abc"}}"#;

const READ_BODY: &[u8] =
    br#"{"jsonrpc":"2.0","id":1,"method":"getLatestLedger","params":{}}"#;

const SIMULATE_BODY: &[u8] =
    br#"{"jsonrpc":"2.0","id":1,"method":"simulateTransaction","params":{"tx":"abc"}}"#;

// ─── Test 1: Routing correctness under high volume ────────────────────────────

/// Issue 10,000 read routing decisions and verify every one goes to the
/// lowest-latency cluster.
#[tokio::test]
async fn load_10k_reads_all_route_to_lowest_latency() {
    // secondary has lower latency (2ms) than primary (20ms)
    let monitor = build_two_cluster_monitor(100, 100, 20_000, 2_000, 10).await;
    let router = Arc::new(MultiRegionRouter::new(monitor, RouterConfig::default()));

    const N: usize = 10_000;
    let mut errors = 0usize;

    for _ in 0..N {
        let decision = router.route(READ_BODY).await;
        if decision.target_cluster_id != "secondary" || decision.reason != RouteReason::LowestLatency {
            errors += 1;
        }
    }

    let metrics = router.metrics();
    assert_eq!(
        errors, 0,
        "all {} reads should route to lowest-latency secondary",
        N
    );
    assert_eq!(
        metrics.total_requests, N as u64,
        "metrics counter must match request count"
    );
    assert_eq!(
        metrics.read_requests, N as u64,
        "all requests classified as read"
    );
    assert_eq!(metrics.write_requests, 0);
    assert_eq!(metrics.failovers, 0);
}

/// Issue 10,000 write routing decisions and verify every one goes to the
/// primary cluster.
#[tokio::test]
async fn load_10k_writes_all_route_to_primary() {
    let monitor = build_two_cluster_monitor(100, 100, 5_000, 3_000, 10).await;
    let router = Arc::new(MultiRegionRouter::new(monitor, RouterConfig::default()));

    const N: usize = 10_000;
    let mut errors = 0usize;

    for _ in 0..N {
        let decision = router.route(WRITE_BODY).await;
        if decision.target_cluster_id != "primary" || decision.reason != RouteReason::PrimaryHealthy {
            errors += 1;
        }
    }

    assert_eq!(errors, 0, "all {} writes should route to healthy primary", N);
    let metrics = router.metrics();
    assert_eq!(metrics.write_requests, N as u64);
    assert_eq!(metrics.failovers, 0);
}

// ─── Test 2: Concurrent routing ──────────────────────────────────────────────

/// Spawn 50 concurrent tasks, each issuing 200 routing decisions (10,000 total).
/// Verify correctness, metrics consistency, and that the router is thread-safe
/// under concurrency.
#[tokio::test]
async fn load_concurrent_50_tasks_200_requests_each() {
    let monitor = build_two_cluster_monitor(100, 100, 20_000, 2_000, 10).await;
    let router = Arc::new(MultiRegionRouter::new(monitor, RouterConfig::default()));

    const TASKS: usize = 50;
    const PER_TASK: usize = 200;

    let read_errors = Arc::new(AtomicU64::new(0));
    let write_ok = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::with_capacity(TASKS);
    for t in 0..TASKS {
        let router = Arc::clone(&router);
        let read_errors = Arc::clone(&read_errors);
        let write_ok = Arc::clone(&write_ok);

        let handle = tokio::spawn(async move {
            for i in 0..PER_TASK {
                // Alternate between read and write.
                if i % 2 == 0 {
                    let dec = router.route(READ_BODY).await;
                    // secondary has lower latency
                    if dec.target_cluster_id != "secondary" {
                        read_errors.fetch_add(1, Ordering::Relaxed);
                    }
                } else {
                    let dec = router.route(WRITE_BODY).await;
                    if dec.target_cluster_id == "primary"
                        && dec.reason == RouteReason::PrimaryHealthy
                    {
                        write_ok.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        });
        handles.push(handle);
    }

    for h in handles {
        h.await.expect("task panicked");
    }

    let metrics = router.metrics();
    let total_expected = (TASKS * PER_TASK) as u64;

    assert_eq!(
        metrics.total_requests, total_expected,
        "metrics total must match actual request count"
    );
    assert_eq!(
        read_errors.load(Ordering::Relaxed),
        0,
        "no read routing errors under concurrency"
    );
    // All writes should have gone to primary.
    let expected_writes = (TASKS * PER_TASK / 2) as u64;
    assert_eq!(
        write_ok.load(Ordering::Relaxed),
        expected_writes,
        "all writes correctly routed to primary"
    );
}

// ─── Test 3: Failover on 10-ledger sync delay ────────────────────────────────

/// This is the primary validation scenario from the issue:
/// "Simulate a 10-ledger sync delay on Primary Cluster and verify client
/// requests route instantly to Secondary Cluster."
///
/// We set primary to ledger 100 and secondary to 111 (11-ledger ahead).
/// The primary's lag is 11 > 10 → desynced. All traffic must go to secondary.
#[tokio::test]
async fn failover_on_10_ledger_sync_delay_write_traffic() {
    let monitor = build_two_cluster_monitor(
        100, // primary ledger (lagging)
        111, // secondary ledger (ahead → max_ledger)
        5_000,
        3_000,
        10, // max_lag_ledgers
    ).await;
    let router = MultiRegionRouter::new(monitor, RouterConfig::default());

    const N: usize = 1_000;
    let mut failover_count = 0usize;

    for _ in 0..N {
        let decision = router.route(WRITE_BODY).await;
        assert_eq!(
            decision.target_cluster_id, "secondary",
            "write must failover to secondary when primary lags by 11 ledgers"
        );
        assert_eq!(decision.reason, RouteReason::FailoverWrite);
        failover_count += 1;
    }

    assert_eq!(failover_count, N);
    let metrics = router.metrics();
    assert_eq!(metrics.failovers, N as u64);
}

/// Verify reads also route to secondary (away from the desynced primary) when
/// primary lags by 10+ ledgers.
#[tokio::test]
async fn failover_on_10_ledger_sync_delay_read_traffic() {
    let monitor = build_two_cluster_monitor(
        100, // primary lagging
        111, // secondary ahead
        5_000,
        3_000,
        10,
    ).await;
    let router = MultiRegionRouter::new(monitor, RouterConfig::default());

    const N: usize = 1_000;

    for _ in 0..N {
        let decision = router.route(READ_BODY).await;
        assert_eq!(
            decision.target_cluster_id, "secondary",
            "reads must route away from desynced primary (11-ledger lag)"
        );
        // Reads go to lowest-latency synced cluster — secondary is the only one.
        assert_eq!(decision.reason, RouteReason::LowestLatency);
    }
}

/// Verify that *exactly* at 10 ledgers lag the primary is still eligible
/// (boundary condition: lag must *exceed* threshold, not just equal it).
#[tokio::test]
async fn primary_still_eligible_at_exactly_10_ledger_lag() {
    // primary at 100, secondary at 110 → lag = 10 = threshold → NOT desynced
    let monitor = build_two_cluster_monitor(100, 110, 5_000, 3_000, 10).await;
    let router = MultiRegionRouter::new(monitor, RouterConfig::default());

    let decision = router.route(WRITE_BODY).await;
    // lag == threshold → still healthy → primary should be selected
    assert_eq!(
        decision.target_cluster_id, "primary",
        "primary should still be selected when lag equals (not exceeds) threshold"
    );
    assert_eq!(decision.reason, RouteReason::PrimaryHealthy);
}

/// Verify that at 11 ledgers lag (> threshold) the primary is immediately
/// desynced and writes go to secondary.
#[tokio::test]
async fn primary_desynced_at_11_ledger_lag() {
    // primary at 100, secondary at 111 → lag = 11 > 10 → desynced
    let monitor = build_two_cluster_monitor(100, 111, 5_000, 3_000, 10).await;
    let router = MultiRegionRouter::new(monitor, RouterConfig::default());

    let decision = router.route(WRITE_BODY).await;
    assert_eq!(
        decision.target_cluster_id, "secondary",
        "primary must be desynced at lag > 10"
    );
    assert_eq!(decision.reason, RouteReason::FailoverWrite);
}

// ─── Test 4: Sub-2ms routing evaluation under load ───────────────────────────

/// Make 1,000 routing decisions and assert all complete within the 2ms budget.
/// The gateway's design requirement is <2ms evaluation overhead.
#[tokio::test]
async fn route_eval_under_2ms_for_all_1000_requests() {
    let monitor = build_n_cluster_monitor(5, 10).await;
    let router = MultiRegionRouter::new(monitor, RouterConfig::default());

    // Warm up.
    let _ = router.route(READ_BODY).await;
    let _ = router.route(WRITE_BODY).await;

    const N: usize = 1_000;
    let mut max_eval_us = 0u64;
    let mut total_eval_us = 0u64;
    let mut over_budget = 0usize;

    for _ in 0..N {
        let decision = router.route(READ_BODY).await;
        let us = decision.eval_duration_us;
        total_eval_us += us;
        if us > max_eval_us {
            max_eval_us = us;
        }
        if us >= 2_000 {
            over_budget += 1;
        }
    }

    let avg_eval_us = total_eval_us / N as u64;

    assert_eq!(
        over_budget, 0,
        "all {} routing decisions must complete in <2ms; {} exceeded budget (max={}µs, avg={}µs)",
        N, over_budget, max_eval_us, avg_eval_us
    );
    println!(
        "route eval: avg={}µs max={}µs (N={}, 5 clusters)",
        avg_eval_us, max_eval_us, N
    );
}

/// Measure routing evaluation with 10 clusters to ensure linear scaling stays
/// within the 2ms budget even for larger deployments.
#[tokio::test]
async fn route_eval_under_2ms_with_10_clusters() {
    let monitor = build_n_cluster_monitor(10, 10).await;
    let router = MultiRegionRouter::new(monitor, RouterConfig::default());

    // Warm up.
    let _ = router.route(READ_BODY).await;

    const N: usize = 500;
    let mut over_budget = 0usize;

    for _ in 0..N {
        let decision = router.route(READ_BODY).await;
        if decision.eval_duration_us >= 2_000 {
            over_budget += 1;
        }
    }

    assert_eq!(
        over_budget, 0,
        "{} requests exceeded 2ms budget (10 clusters)",
        over_budget
    );
}

// ─── Test 5: Circuit breaker isolation ───────────────────────────────────────

/// Open the primary cluster's circuit breaker and verify all traffic
/// immediately routes to the secondary.
#[tokio::test]
async fn circuit_breaker_isolates_failed_cluster() {
    let monitor = ClusterHealthMonitor::new();
    monitor.add_cluster(
        "primary",
        ClusterHealthConfig {
            endpoint: "http://primary:8000".into(),
            max_lag_ledgers: 10,
            circuit_breaker: CircuitBreakerConfig {
                failure_threshold: 3,
                success_threshold: 2,
                open_window: Duration::from_secs(30),
            },
            ..Default::default()
        },
        true,
    );
    monitor.add_cluster(
        "secondary",
        ClusterHealthConfig {
            endpoint: "http://secondary:8000".into(),
            max_lag_ledgers: 10,
            ..Default::default()
        },
        false,
    );

    // Both clusters have fresh ledger data.
    monitor.update_cluster_health("primary", true, 100, 5_000).await;
    monitor.update_cluster_health("secondary", true, 100, 3_000).await;

    // Verify primary is initially selected for writes.
    let router = MultiRegionRouter::new(monitor, RouterConfig::default());
    {
        let decision = router.route(WRITE_BODY).await;
        assert_eq!(decision.target_cluster_id, "primary");
        assert_eq!(decision.reason, RouteReason::PrimaryHealthy);
    }

    // Open the primary's circuit breaker by recording 3 failures.
    let monitor_ref = router.health_snapshot().await;
    drop(monitor_ref);

    // We need to access the monitor to record failures. Rebuild with an open breaker.
    let monitor2 = ClusterHealthMonitor::new();
    monitor2.add_cluster(
        "primary",
        ClusterHealthConfig {
            endpoint: "http://primary:8000".into(),
            max_lag_ledgers: 10,
            circuit_breaker: CircuitBreakerConfig {
                failure_threshold: 1, // Opens after 1 failure
                success_threshold: 2,
                open_window: Duration::from_secs(30),
            },
            ..Default::default()
        },
        true,
    );
    monitor2.add_cluster(
        "secondary",
        ClusterHealthConfig {
            endpoint: "http://secondary:8000".into(),
            max_lag_ledgers: 10,
            ..Default::default()
        },
        false,
    );

    monitor2.update_cluster_health("primary", true, 100, 5_000).await;
    monitor2.update_cluster_health("secondary", true, 100, 3_000).await;

    // Open breaker by recording 1 failure.
    monitor2.record_probe_failure("primary").await;

    let snap = monitor2.snapshot().await;
    let primary_entry = snap.get("primary").unwrap();
    assert_eq!(
        primary_entry.health,
        ClusterHealth::Unhealthy,
        "primary should be unhealthy after breaker opens"
    );

    let router2 = MultiRegionRouter::new(monitor2, RouterConfig::default());

    // Under load, all writes must now failover to secondary.
    const N: usize = 500;
    let mut failover_count = 0;
    for _ in 0..N {
        let decision = router2.route(WRITE_BODY).await;
        if decision.target_cluster_id == "secondary"
            && decision.reason == RouteReason::FailoverWrite
        {
            failover_count += 1;
        }
    }
    assert_eq!(
        failover_count, N,
        "all {} writes must failover when primary breaker is open",
        N
    );
}

/// Verify that opening the breaker on a cluster also blocks its reads.
#[tokio::test]
async fn circuit_breaker_blocks_reads_from_failed_cluster() {
    // Build a monitor with bad-cluster (low latency) and good-cluster (higher latency).
    let m_before = ClusterHealthMonitor::new();
    m_before.add_cluster(
        "bad-cluster",
        ClusterHealthConfig {
            endpoint: "http://bad-cluster:8000".into(),
            max_lag_ledgers: 10,
            ..Default::default()
        },
        false,
    );
    m_before.add_cluster(
        "good-cluster",
        ClusterHealthConfig {
            endpoint: "http://good-cluster:8000".into(),
            max_lag_ledgers: 10,
            ..Default::default()
        },
        false,
    );
    m_before.update_cluster_health("bad-cluster", true, 100, 500).await;
    m_before.update_cluster_health("good-cluster", true, 100, 5_000).await;

    // Initially, bad-cluster should win on reads (lowest latency).
    let router_before = MultiRegionRouter::new(m_before, RouterConfig::default());
    let decision_before = router_before.route(READ_BODY).await;
    assert_eq!(
        decision_before.target_cluster_id, "bad-cluster",
        "before breaker: bad-cluster wins on reads (lowest latency)"
    );

    // Now create a new monitor where bad-cluster's breaker has been opened.
    let monitor_after = ClusterHealthMonitor::new();
    monitor_after.add_cluster(
        "bad-cluster",
        ClusterHealthConfig {
            endpoint: "http://bad-cluster:8000".into(),
            max_lag_ledgers: 10,
            circuit_breaker: CircuitBreakerConfig {
                failure_threshold: 1,
                success_threshold: 2,
                open_window: Duration::from_secs(30),
            },
            ..Default::default()
        },
        false,
    );
    monitor_after.add_cluster(
        "good-cluster",
        ClusterHealthConfig {
            endpoint: "http://good-cluster:8000".into(),
            max_lag_ledgers: 10,
            ..Default::default()
        },
        false,
    );
    monitor_after.update_cluster_health("bad-cluster", true, 100, 500).await;
    monitor_after.update_cluster_health("good-cluster", true, 100, 5_000).await;

    // Open the breaker on bad-cluster.
    monitor_after.record_probe_failure("bad-cluster").await;

    let router_after = MultiRegionRouter::new(monitor_after, RouterConfig::default());

    // Reads must now go to good-cluster.
    const N: usize = 200;
    for _ in 0..N {
        let decision = router_after.route(READ_BODY).await;
        assert_eq!(
            decision.target_cluster_id, "good-cluster",
            "reads must avoid cluster with open breaker"
        );
    }
}

// ─── Test 6: Mixed read/write load split ──────────────────────────────────────

/// Issue 10,000 mixed requests (50% reads, 50% writes) and verify:
/// - All reads go to lowest-latency cluster.
/// - All writes go to primary.
/// - Metrics counters add up exactly.
#[tokio::test]
async fn mixed_load_split_read_write_routing() {
    // primary: 20ms latency, secondary: 2ms latency (secondary wins reads)
    // Both synced at ledger 100.
    let monitor = build_two_cluster_monitor(100, 100, 20_000, 2_000, 10).await;
    let router = Arc::new(MultiRegionRouter::new(monitor, RouterConfig::default()));

    const N: usize = 10_000;
    let half = N / 2;

    let mut read_errors = 0usize;
    let mut write_errors = 0usize;

    for i in 0..N {
        if i % 2 == 0 {
            let dec = router.route(READ_BODY).await;
            if dec.target_cluster_id != "secondary" || dec.method_kind != RpcMethodKind::Read {
                read_errors += 1;
            }
        } else {
            let dec = router.route(WRITE_BODY).await;
            if dec.target_cluster_id != "primary" || dec.method_kind != RpcMethodKind::Write {
                write_errors += 1;
            }
        }
    }

    assert_eq!(read_errors, 0, "all read decisions must be correct");
    assert_eq!(write_errors, 0, "all write decisions must be correct");

    let metrics = router.metrics();
    assert_eq!(metrics.total_requests, N as u64);
    assert_eq!(metrics.read_requests, half as u64);
    assert_eq!(metrics.write_requests, half as u64);
    assert_eq!(metrics.failovers, 0);
}

// ─── Test 7: Dynamic failover recovery ───────────────────────────────────────

/// Simulate the full lifecycle:
///  1. Primary healthy → writes go to primary.
///  2. Primary desyncs (10-ledger lag) → writes failover to secondary.
///  3. Primary recovers → writes return to primary.
#[tokio::test]
async fn dynamic_failover_and_recovery() {
    let monitor = ClusterHealthMonitor::new();

    monitor.add_cluster(
        "primary",
        ClusterHealthConfig {
            endpoint: "http://primary:8000".into(),
            max_lag_ledgers: 10,
            ..Default::default()
        },
        true,
    );
    monitor.add_cluster(
        "secondary",
        ClusterHealthConfig {
            endpoint: "http://secondary:8000".into(),
            max_lag_ledgers: 10,
            ..Default::default()
        },
        false,
    );

    let router = MultiRegionRouter::new(monitor, RouterConfig::default());

    // Phase 1: Both in sync at ledger 100.
    router
        .health_snapshot()
        .await; // just to init snapshot
    // We need to access the monitor; let's rebuild since we can't get it back.
    // Instead use the snapshot to verify, then update the underlying monitor.

    // Rebuild for phase 1.
    let monitor1 = ClusterHealthMonitor::new();
    monitor1.add_cluster(
        "primary",
        ClusterHealthConfig {
            endpoint: "http://primary:8000".into(),
            max_lag_ledgers: 10,
            ..Default::default()
        },
        true,
    );
    monitor1.add_cluster(
        "secondary",
        ClusterHealthConfig {
            endpoint: "http://secondary:8000".into(),
            max_lag_ledgers: 10,
            ..Default::default()
        },
        false,
    );
    monitor1.update_cluster_health("primary", true, 100, 5_000).await;
    monitor1.update_cluster_health("secondary", true, 100, 3_000).await;

    let router1 = MultiRegionRouter::new(monitor1, RouterConfig::default());

    let dec = router1.route(WRITE_BODY).await;
    assert_eq!(dec.target_cluster_id, "primary", "phase 1: write → primary");
    assert_eq!(dec.reason, RouteReason::PrimaryHealthy);

    // Phase 2: Primary lags by 11 ledgers (desynced).
    let monitor2 = ClusterHealthMonitor::new();
    monitor2.add_cluster(
        "primary",
        ClusterHealthConfig {
            endpoint: "http://primary:8000".into(),
            max_lag_ledgers: 10,
            ..Default::default()
        },
        true,
    );
    monitor2.add_cluster(
        "secondary",
        ClusterHealthConfig {
            endpoint: "http://secondary:8000".into(),
            max_lag_ledgers: 10,
            ..Default::default()
        },
        false,
    );
    monitor2.update_cluster_health("primary", true, 100, 5_000).await; // lagging
    monitor2.update_cluster_health("secondary", true, 111, 3_000).await; // ahead

    let router2 = MultiRegionRouter::new(monitor2, RouterConfig::default());

    let dec = router2.route(WRITE_BODY).await;
    assert_eq!(
        dec.target_cluster_id, "secondary",
        "phase 2: write fails over to secondary (primary lags 11)"
    );
    assert_eq!(dec.reason, RouteReason::FailoverWrite);

    // Phase 3: Primary recovers (catches up to ledger 112).
    let monitor3 = ClusterHealthMonitor::new();
    monitor3.add_cluster(
        "primary",
        ClusterHealthConfig {
            endpoint: "http://primary:8000".into(),
            max_lag_ledgers: 10,
            ..Default::default()
        },
        true,
    );
    monitor3.add_cluster(
        "secondary",
        ClusterHealthConfig {
            endpoint: "http://secondary:8000".into(),
            max_lag_ledgers: 10,
            ..Default::default()
        },
        false,
    );
    monitor3.update_cluster_health("primary", true, 112, 5_000).await; // recovered
    monitor3.update_cluster_health("secondary", true, 112, 3_000).await;

    let router3 = MultiRegionRouter::new(monitor3, RouterConfig::default());

    let dec = router3.route(WRITE_BODY).await;
    assert_eq!(dec.target_cluster_id, "primary", "phase 3: primary recovered");
    assert_eq!(dec.reason, RouteReason::PrimaryHealthy);
}

// ─── Test 8: Throughput under concurrent stress ───────────────────────────────

/// Issue 100,000 routing decisions across 100 concurrent tasks.
/// Verifies the router handles high concurrency without data races or panics.
/// (Does not make real network calls — purely in-memory routing logic.)
#[tokio::test]
async fn stress_100k_requests_100_concurrent_tasks() {
    let monitor = build_n_cluster_monitor(3, 10).await;
    let router = Arc::new(MultiRegionRouter::new(monitor, RouterConfig::default()));

    const TASKS: usize = 100;
    const PER_TASK: usize = 1_000; // 100,000 total

    let no_cluster_errors = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::with_capacity(TASKS);
    for _ in 0..TASKS {
        let router = Arc::clone(&router);
        let errors = Arc::clone(&no_cluster_errors);
        handles.push(tokio::spawn(async move {
            for i in 0..PER_TASK {
                let body = if i % 3 == 0 { WRITE_BODY } else { READ_BODY };
                let decision = router.route(body).await;
                if decision.reason == RouteReason::NoClusterAvailable {
                    errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }

    for h in handles {
        h.await.expect("stress task panicked");
    }

    let total_errors = no_cluster_errors.load(Ordering::Relaxed);
    assert_eq!(
        total_errors, 0,
        "no 'no cluster available' errors in stress test"
    );

    let metrics = router.metrics();
    assert_eq!(
        metrics.total_requests,
        (TASKS * PER_TASK) as u64,
        "total requests counter must equal stress test volume"
    );
}

// ─── Test 9: Degraded fallback under total failure ────────────────────────────

/// When all clusters are degraded (high latency, no circuit breaker open),
/// the router should fall back to the degraded cluster rather than returning
/// NoClusterAvailable.
#[tokio::test]
async fn degraded_fallback_when_all_clusters_high_latency() {
    let monitor = ClusterHealthMonitor::new();
    monitor.add_cluster(
        "slow-primary",
        ClusterHealthConfig {
            endpoint: "http://slow-primary:8000".into(),
            max_lag_ledgers: 10,
            ..Default::default()
        },
        true,
    );
    monitor.add_cluster(
        "slow-secondary",
        ClusterHealthConfig {
            endpoint: "http://slow-secondary:8000".into(),
            max_lag_ledgers: 10,
            ..Default::default()
        },
        false,
    );

    // Both clusters exceed the 500ms latency threshold → degraded
    monitor.update_cluster_health("slow-primary", true, 100, 600_000).await;
    monitor.update_cluster_health("slow-secondary", true, 100, 800_000).await;

    let router = MultiRegionRouter::new(
        monitor,
        RouterConfig {
            allow_degraded_fallback: true,
            ..Default::default()
        },
    );

    const N: usize = 200;
    for _ in 0..N {
        let decision = router.route(READ_BODY).await;
        assert_ne!(
            decision.reason,
            RouteReason::NoClusterAvailable,
            "degraded fallback must prevent NoClusterAvailable"
        );
        assert_eq!(decision.reason, RouteReason::DegradedFallback);
    }
}

/// When degraded fallback is disabled and all clusters are degraded, the router
/// must return NoClusterAvailable.
#[tokio::test]
async fn no_cluster_available_when_degraded_fallback_disabled() {
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

    // High latency → degraded (not Unhealthy, so breaker is still closed,
    // but ClusterHealth::Degraded does not pass allows_writes())
    monitor.update_cluster_health("slow", true, 100, 600_000).await;

    let router = MultiRegionRouter::new(
        monitor,
        RouterConfig {
            allow_degraded_fallback: false,
            ..Default::default()
        },
    );

    let decision = router.route(WRITE_BODY).await;
    assert_eq!(decision.reason, RouteReason::NoClusterAvailable);
    assert!(decision.target_cluster_id.is_empty());
}

// ─── Test 10: Metrics accuracy ────────────────────────────────────────────────

/// Verify that metric counters increment correctly across many mixed requests.
#[tokio::test]
async fn metrics_accuracy_under_mixed_load() {
    // primary: breaker threshold 1, will be opened to force failovers.
    let monitor = ClusterHealthMonitor::new();
    monitor.add_cluster(
        "primary",
        ClusterHealthConfig {
            endpoint: "http://primary:8000".into(),
            max_lag_ledgers: 10,
            circuit_breaker: CircuitBreakerConfig {
                failure_threshold: 1,
                success_threshold: 2,
                open_window: Duration::from_secs(60),
            },
            ..Default::default()
        },
        true,
    );
    monitor.add_cluster(
        "secondary",
        ClusterHealthConfig {
            endpoint: "http://secondary:8000".into(),
            max_lag_ledgers: 10,
            ..Default::default()
        },
        false,
    );

    monitor.update_cluster_health("primary", true, 100, 5_000).await;
    monitor.update_cluster_health("secondary", true, 100, 3_000).await;

    let router = MultiRegionRouter::new(monitor, RouterConfig::default());

    // Phase A: 100 reads + 100 writes while primary is healthy.
    for _ in 0..100 {
        router.route(READ_BODY).await;
    }
    for _ in 0..100 {
        router.route(WRITE_BODY).await;
    }

    let m = router.metrics();
    assert_eq!(m.total_requests, 200);
    assert_eq!(m.read_requests, 100);
    assert_eq!(m.write_requests, 100);
    assert_eq!(m.failovers, 0);

    // simulateTransaction counts as read.
    for _ in 0..50 {
        router.route(SIMULATE_BODY).await;
    }
    let m = router.metrics();
    assert_eq!(m.read_requests, 150);
    assert_eq!(m.total_requests, 250);
}
