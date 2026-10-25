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
//! `soroban-gateway-proxy` — Multi-region Soroban RPC traffic router (#83).
//!
//! An HTTP proxy that receives JSON-RPC requests from dApps and forwards
//! them to the best available Soroban RPC cluster. Health is continuously
//! monitored in the background via ledger-sequence alignment probes; the
//! routing decision on the hot path reads a pre-computed snapshot and
//! completes in under 2ms.
//!
//! # Routing strategy
//!
//! - **Write methods** (`sendTransaction`): routed to the primary cluster
//!   when healthy. Failover to the next write-capable cluster (highest ledger)
//!   when the primary is desynchronized or breaker is open.
//! - **Read methods** (`getLatestLedger`, `getEvents`, …): routed to the
//!   lowest-latency synced cluster.
//! - **Circuit breaker**: clusters with database corruption or repeated probe
//!   failures are isolated for `open_window` before a half-open trial.
//!
//! # Usage
//!
//! ```text
//! soroban-gateway-proxy \
//!   --listen 0.0.0.0:8080 \
//!   --cluster primary=http://soroban-rpc-us-east:8000 \
//!   --cluster secondary=http://soroban-rpc-eu-west:8000 \
//!   --primary primary \
//!   --max-lag-ledgers 10 \
//!   --probe-interval-secs 5
//! ```
//!
//! All flags can also be set via environment variables (see `--help`).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::{
    body::Bytes,
    extract::State,
    http::{header, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router as AxumRouter,
};
use clap::Parser;
use serde::{Deserialize, Serialize};
use stellar_k8s::gateway::health::{
    CircuitBreakerConfig, ClusterHealthConfig, ClusterHealthMonitor,
};
use stellar_k8s::gateway::router::{MultiRegionRouter, RouteReason, RouterConfig};
use tracing::{error, info, warn};

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "soroban-gateway-proxy",
    version,
    about = "Multi-region Soroban RPC traffic router with automatic failover (#83)",
    long_about = "An HTTP proxy that routes JSON-RPC requests to the best available \
        Soroban RPC cluster. Health is monitored via background ledger-sequence \
        probes; routing decisions complete in under 2ms using a lock-free \
        health snapshot.\n\n\
        EXAMPLES:\n  \
        soroban-gateway-proxy --listen 0.0.0.0:8080 \\\n    \
        --cluster primary=http://soroban-rpc-us-east:8000 \\\n    \
        --cluster secondary=http://soroban-rpc-eu-west:8000 \\\n    \
        --primary primary"
)]
struct Args {
    /// Address to bind the proxy server.
    /// Env: GATEWAY_LISTEN
    #[arg(long, env = "GATEWAY_LISTEN", default_value = "0.0.0.0:8080")]
    listen: String,

    /// Cluster definition in `name=endpoint` format. Repeat for each cluster.
    /// Env: GATEWAY_CLUSTERS (semicolon-separated: `primary=http://...;secondary=http://...`)
    #[arg(
        long,
        env = "GATEWAY_CLUSTERS",
        value_delimiter = ';',
        num_args = 1..,
        required = false
    )]
    cluster: Vec<String>,

    /// Name of the primary cluster (must match a cluster name).
    /// Env: GATEWAY_PRIMARY
    #[arg(long, env = "GATEWAY_PRIMARY", default_value = "primary")]
    primary: String,

    /// Maximum ledger lag before a cluster is considered desynchronized.
    /// Env: GATEWAY_MAX_LAG_LEDGERS
    #[arg(long, env = "GATEWAY_MAX_LAG_LEDGERS", default_value_t = 10)]
    max_lag_ledgers: u64,

    /// Interval between health probes (seconds).
    /// Env: GATEWAY_PROBE_INTERVAL_SECS
    #[arg(long, env = "GATEWAY_PROBE_INTERVAL_SECS", default_value_t = 5)]
    probe_interval_secs: u64,

    /// Health probe timeout (seconds).
    /// Env: GATEWAY_PROBE_TIMEOUT_SECS
    #[arg(long, env = "GATEWAY_PROBE_TIMEOUT_SECS", default_value_t = 2)]
    probe_timeout_secs: u64,

    /// Circuit-breaker: consecutive probe failures before opening.
    /// Env: GATEWAY_FAILURE_THRESHOLD
    #[arg(long, env = "GATEWAY_FAILURE_THRESHOLD", default_value_t = 3)]
    failure_threshold: u32,

    /// Circuit-breaker: consecutive successes (in half-open) to close.
    /// Env: GATEWAY_SUCCESS_THRESHOLD
    #[arg(long, env = "GATEWAY_SUCCESS_THRESHOLD", default_value_t = 2)]
    success_threshold: u32,

    /// Circuit-breaker: how long the breaker stays open (seconds).
    /// Env: GATEWAY_OPEN_WINDOW_SECS
    #[arg(long, env = "GATEWAY_OPEN_WINDOW_SECS", default_value_t = 30)]
    open_window_secs: u64,

    /// Upstream request timeout (seconds).
    /// Env: GATEWAY_UPSTREAM_TIMEOUT_SECS
    #[arg(long, env = "GATEWAY_UPSTREAM_TIMEOUT_SECS", default_value_t = 10)]
    upstream_timeout_secs: u64,

    /// Allow routing to degraded clusters when no healthy cluster is available.
    /// Env: GATEWAY_ALLOW_DEGRADED_FALLBACK
    #[arg(long, env = "GATEWAY_ALLOW_DEGRADED_FALLBACK", default_value_t = true)]
    allow_degraded_fallback: bool,

    /// Log level (trace / debug / info / warn / error).
    /// Env: RUST_LOG
    #[arg(long, env = "RUST_LOG", default_value = "info")]
    log_level: String,
}

// ── Cluster parsing ──────────────────────────────────────────────────────────

/// Parse a cluster spec string like `name=http://endpoint:8000`.
fn parse_cluster_spec(spec: &str) -> Result<(String, String)> {
    let (name, endpoint) = spec
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("cluster spec must be name=endpoint, got: {spec}"))?;
    let name = name.trim().to_string();
    let endpoint = endpoint.trim().to_string();
    if name.is_empty() {
        anyhow::bail!("cluster name cannot be empty");
    }
    if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
        anyhow::bail!("cluster endpoint must start with http:// or https://, got: {endpoint}");
    }
    Ok((name, endpoint))
}

// ── Shared state ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct ProxyState {
    router: Arc<MultiRegionRouter>,
}

// ── Axum handlers ────────────────────────────────────────────────────────────

/// Proxy a JSON-RPC request to the selected upstream cluster.
async fn proxy_handler(State(state): State<ProxyState>, body: Bytes) -> Response {
    let decision = state.router.route(&body).await;

    if decision.reason == RouteReason::NoClusterAvailable {
        return json_error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            -32603,
            "no healthy cluster available",
            &decision.method,
        );
    }

    let endpoint = decision.target_endpoint.trim_end_matches('/');

    // Add routing observability headers for downstream consumers.
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Ok(v) = HeaderValue::from_str(&decision.target_cluster_id) {
        headers.insert("x-routed-cluster", v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("{}", decision.eval_duration_us)) {
        headers.insert("x-route-eval-us", v);
    }

    let upstream_result = state
        .router
        .http_client()
        .post(endpoint)
        .headers(headers)
        .body(body.to_vec())
        .send()
        .await;

    match upstream_result {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16())
                .unwrap_or(StatusCode::BAD_GATEWAY);
            let resp_headers = resp.headers().clone();
            let resp_body = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    warn!(
                        cluster = %decision.target_cluster_id,
                        error = %e,
                        "failed to read upstream response body"
                    );
                    return json_error_response(
                        StatusCode::BAD_GATEWAY,
                        -32603,
                        "failed to read upstream response",
                        &decision.method,
                    );
                }
            };

            let mut response = Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-routed-cluster", &decision.target_cluster_id)
                .header("x-route-eval-us", decision.eval_duration_us.to_string());

            // Forward relevant upstream headers.
            for key in ["x-request-id", "x-stellar-correlation-id"] {
                if let Some(val) = resp_headers.get(key) {
                    if let Ok(name) = HeaderName::from_bytes(key.as_bytes()) {
                        response = response.header(name, val);
                    }
                }
            }

            response
                .body(axum::body::Body::from(resp_body))
                .unwrap_or_else(|_| {
                    json_error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        -32603,
                        "internal proxy error",
                        &decision.method,
                    )
                })
        }
        Err(e) => {
            warn!(
                cluster = %decision.target_cluster_id,
                endpoint = endpoint,
                error = %e,
                "upstream request failed"
            );
            json_error_response(
                StatusCode::BAD_GATEWAY,
                -32603,
                "upstream RPC unavailable",
                &decision.method,
            )
        }
    }
}

/// Health endpoint for the proxy itself.
async fn healthz() -> StatusCode {
    StatusCode::OK
}

/// Metrics endpoint — returns router statistics as JSON.
async fn metrics_handler(State(state): State<ProxyState>) -> impl IntoResponse {
    let metrics = state.router.metrics();
    let snapshot = state.router.health_snapshot().await;

    let cluster_health: Vec<serde_json::Value> = snapshot
        .clusters
        .values()
        .map(|e| {
            serde_json::json!({
                "id": e.id,
                "endpoint": e.endpoint,
                "health": format!("{:?}", e.health),
                "breaker": format!("{:?}", e.breaker),
                "ledger_sequence": e.ledger_sequence,
                "latency_us": e.latency_us,
                "is_primary": e.is_primary,
            })
        })
        .collect();

    let body = serde_json::json!({
        "router": metrics,
        "clusters": cluster_health,
        "max_ledger": snapshot.max_ledger,
    });

    axum::Json(body)
}

// ── Response helpers ─────────────────────────────────────────────────────────

fn json_error_response(
    status: StatusCode,
    code: i32,
    message: &str,
    method: &Option<String>,
) -> Response {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": {
            "code": code,
            "message": message,
            "data": {
                "method": method,
            }
        }
    });
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(
            serde_json::to_vec(&body).unwrap_or_default(),
        ))
        .expect("valid error response")
}

// ── Configuration ─────────────────────────────────────────────────────────────

/// Configuration file format for the gateway proxy (optional).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GatewayProxyConfig {
    #[serde(default = "default_listen")]
    listen: String,
    clusters: Vec<NamedCluster>,
    primary: String,
    #[serde(default = "default_max_lag")]
    max_lag_ledgers: u64,
    #[serde(default = "default_probe_interval")]
    probe_interval_secs: u64,
    #[serde(default = "default_probe_timeout")]
    probe_timeout_secs: u64,
    #[serde(default = "default_failure_threshold")]
    failure_threshold: u32,
    #[serde(default = "default_success_threshold")]
    success_threshold: u32,
    #[serde(default = "default_open_window")]
    open_window_secs: u64,
    #[serde(default = "default_upstream_timeout")]
    upstream_timeout_secs: u64,
    #[serde(default = "default_true")]
    allow_degraded_fallback: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NamedCluster {
    name: String,
    endpoint: String,
}

fn default_listen() -> String {
    "0.0.0.0:8080".into()
}
fn default_max_lag() -> u64 {
    10
}
fn default_probe_interval() -> u64 {
    5
}
fn default_probe_timeout() -> u64 {
    2
}
fn default_failure_threshold() -> u32 {
    3
}
fn default_success_threshold() -> u32 {
    2
}
fn default_open_window() -> u64 {
    30
}
fn default_upstream_timeout() -> u64 {
    10
}
fn default_true() -> bool {
    true
}

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize tracing.
    let log_level = args.log_level.parse().unwrap_or(tracing::Level::INFO);
    stellar_k8s::logging::init_binary_subscriber(
        log_level,
        stellar_k8s::logging::LogOutputFormat::Json,
    );

    // Parse cluster specs.
    if args.cluster.is_empty() {
        error!("at least one --cluster must be specified");
        anyhow::bail!("at least one --cluster must be specified");
    }

    let mut clusters: Vec<(String, String)> = Vec::new();
    for spec in &args.cluster {
        match parse_cluster_spec(spec) {
            Ok((name, endpoint)) => {
                info!(cluster = %name, endpoint = %endpoint, "registered cluster");
                clusters.push((name, endpoint));
            }
            Err(e) => {
                error!(spec = spec, error = %e, "failed to parse cluster spec");
                anyhow::bail!("invalid cluster spec '{spec}': {e}");
            }
        }
    }

    // Verify the primary cluster exists.
    if !clusters.iter().any(|(name, _)| name == &args.primary) {
        error!(
            primary = %args.primary,
            "primary cluster not found in cluster list"
        );
        anyhow::bail!(
            "primary cluster '{}' not found in cluster list",
            args.primary
        );
    }

    // Build the health monitor.
    let monitor = ClusterHealthMonitor::new();
    let circuit_breaker = CircuitBreakerConfig {
        failure_threshold: args.failure_threshold,
        success_threshold: args.success_threshold,
        open_window: Duration::from_secs(args.open_window_secs),
    };

    for (name, endpoint) in &clusters {
        let config = ClusterHealthConfig {
            endpoint: endpoint.clone(),
            probe_interval: Duration::from_secs(args.probe_interval_secs),
            probe_timeout: Duration::from_secs(args.probe_timeout_secs),
            max_lag_ledgers: args.max_lag_ledgers,
            circuit_breaker: circuit_breaker.clone(),
        };
        monitor.add_cluster(name.clone(), config, name == &args.primary);
    }

    monitor.start().await;

    // Build the router.
    let router_config = RouterConfig {
        max_lag_ledgers: args.max_lag_ledgers,
        allow_degraded_fallback: args.allow_degraded_fallback,
        upstream_timeout: Duration::from_secs(args.upstream_timeout_secs),
    };
    let router = Arc::new(MultiRegionRouter::new(monitor, router_config));

    info!(
        listen = %args.listen,
        cluster_count = clusters.len(),
        primary = %args.primary,
        max_lag_ledgers = args.max_lag_ledgers,
        probe_interval_secs = args.probe_interval_secs,
        "soroban-gateway-proxy starting"
    );

    // Build the Axum router.
    let state = ProxyState {
        router: router.clone(),
    };
    let app = AxumRouter::new()
        .route("/", post(proxy_handler))
        .route("/healthz", get(healthz))
        .route("/readyz", get(healthz))
        .route("/metrics", get(metrics_handler))
        .with_state(state);

    let address: SocketAddr = args.listen.parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    info!(addr = %address, "soroban-gateway-proxy listening");
    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cluster_spec_valid() {
        let (name, endpoint) = parse_cluster_spec("primary=http://rpc:8000").unwrap();
        assert_eq!(name, "primary");
        assert_eq!(endpoint, "http://rpc:8000");
    }

    #[test]
    fn parse_cluster_spec_https() {
        let (name, endpoint) =
            parse_cluster_spec("secondary=https://rpc.example.com:8000").unwrap();
        assert_eq!(name, "secondary");
        assert_eq!(endpoint, "https://rpc.example.com:8000");
    }

    #[test]
    fn parse_cluster_spec_missing_equals() {
        assert!(parse_cluster_spec("just-a-name").is_err());
    }

    #[test]
    fn parse_cluster_spec_empty_name() {
        assert!(parse_cluster_spec("=http://rpc:8000").is_err());
    }

    #[test]
    fn parse_cluster_spec_missing_scheme() {
        assert!(parse_cluster_spec("primary=rpc:8000").is_err());
    }

    #[test]
    fn parse_cluster_spec_trims_whitespace() {
        let (name, endpoint) =
            parse_cluster_spec("  primary  =  http://rpc:8000  ").unwrap();
        assert_eq!(name, "primary");
        assert_eq!(endpoint, "http://rpc:8000");
    }

    #[test]
    fn gateway_proxy_config_deserializes() {
        let yaml = r#"
listen: "0.0.0.0:8080"
clusters:
  - name: primary
    endpoint: "http://rpc-us-east:8000"
  - name: secondary
    endpoint: "http://rpc-eu-west:8000"
primary: primary
max_lag_ledgers: 10
probe_interval_secs: 5
"#;
        let config: GatewayProxyConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.listen, "0.0.0.0:8080");
        assert_eq!(config.clusters.len(), 2);
        assert_eq!(config.primary, "primary");
        assert_eq!(config.max_lag_ledgers, 10);
    }
}
