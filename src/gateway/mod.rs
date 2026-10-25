//! Gateway module: HTTP proxy middleware for Soroban RPC nodes.
//!
//! Provides request rate-limiting, IP tracking, dynamic throttling based
//! on live CPU utilisation metrics, multi-region health monitoring with
//! circuit breakers, and intelligent traffic routing with automatic failover.
//!
//! # Modules
//!
//! - [`ratelimit`]: sliding-window per-IP tracker + CPU-aware engine
//! - [`health`]: multi-cluster health monitor with circuit breaker
//! - [`router`]: multi-region traffic router with failover (#83)

pub mod health;
pub mod ratelimit;
pub mod router;

pub use health::{
    BreakerState, BreakerState_, CircuitBreakerConfig, ClusterHealth, ClusterHealthConfig,
    ClusterHealthEntry, ClusterHealthMonitor, ClusterId, HealthSnapshot, ProbeError,
};
pub use ratelimit::{
    extract_client_ip, EngineMetrics, RateLimitConfig, RateLimitDecision, RateLimitEngine,
    SlidingWindowTracker,
};
pub use router::{
    AtomicRouterMetrics, ForwardError, MultiRegionRouter, NoClusterError, RouteReason,
    RouterConfig, RouterMetrics, RoutingDecision, RpcMethodKind,
};
