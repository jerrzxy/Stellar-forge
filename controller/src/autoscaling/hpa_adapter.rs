// Copyright 2024 Stellar-K8s Contributors
// SPDX-License-Identifier: Apache-2.0
//! Custom metrics adapter and latency-based scaling policy.
//!
//! - [`router`] serves `soroban_wasm_latency_ms` via the
//!   `custom.metrics.k8s.io/v1beta2` API shape so an HPA can read it.
//! - [`ScalingPolicy`] implements the operator-side decision logic: scale up
//!   only when p95 latency exceeds the threshold (default 50 ms) for a
//!   sustained window (default 3 minutes), with cool-down periods that
//!   account for pod boot time and prevent over-provisioning on transient
//!   spikes.
//! - [`hpa_manifest`] renders an `autoscaling/v2` HPA with matching behaviour.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{extract::State, routing::get, Json, Router};
use serde_json::{json, Value};

use super::wasm_metrics::{WasmLatencyHistogram, METRIC_NAME};

/// Tunables for latency-driven scaling.
#[derive(Debug, Clone)]
pub struct ScalingConfig {
    /// p95 threshold in ms above which the deployment is considered overloaded.
    pub p95_threshold_ms: f64,
    /// How long the threshold must be continuously breached before scaling up.
    pub breach_window: Duration,
    /// Replicas added per scale-up step.
    pub scale_up_step: u32,
    /// Minimum time between scale-ups (covers pod/node boot time).
    pub scale_up_cooldown: Duration,
    /// Minimum time after any scaling event before scaling down.
    pub scale_down_cooldown: Duration,
    pub min_replicas: u32,
    pub max_replicas: u32,
}

impl Default for ScalingConfig {
    fn default() -> Self {
        Self {
            p95_threshold_ms: 50.0,
            breach_window: Duration::from_secs(180),
            scale_up_step: 3,
            scale_up_cooldown: Duration::from_secs(300),
            scale_down_cooldown: Duration::from_secs(600),
            min_replicas: 2,
            max_replicas: 20,
        }
    }
}

/// Outcome of a policy evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalingDecision {
    Hold,
    ScaleTo(u32),
}

/// Stateful sustained-breach + cool-down scaling policy.
#[derive(Debug)]
pub struct ScalingPolicy {
    cfg: ScalingConfig,
    breach_since: Option<Instant>,
    healthy_since: Option<Instant>,
    last_scale: Option<Instant>,
}

impl ScalingPolicy {
    pub fn new(cfg: ScalingConfig) -> Self {
        Self {
            cfg,
            breach_since: None,
            healthy_since: None,
            last_scale: None,
        }
    }

    pub fn config(&self) -> &ScalingConfig {
        &self.cfg
    }

    /// Evaluates the current p95 against the policy at time `now`.
    pub fn evaluate(&mut self, p95_ms: f64, current: u32, now: Instant) -> ScalingDecision {
        let cfg = &self.cfg;
        let since_scale = self.last_scale.map(|t| now.duration_since(t));

        if p95_ms > cfg.p95_threshold_ms {
            self.healthy_since = None;
            let start = *self.breach_since.get_or_insert(now);
            let sustained = now.duration_since(start) >= cfg.breach_window;
            let cooled = since_scale.map_or(true, |d| d >= cfg.scale_up_cooldown);
            if sustained && cooled && current < cfg.max_replicas {
                let target = current.saturating_add(cfg.scale_up_step).min(cfg.max_replicas);
                self.last_scale = Some(now);
                // Require a fresh sustained window before the next step.
                self.breach_since = None;
                return ScalingDecision::ScaleTo(target);
            }
            return ScalingDecision::Hold;
        }

        self.breach_since = None;
        let start = *self.healthy_since.get_or_insert(now);
        // Scale down one replica at a time, only when latency is well below
        // the threshold and both cool-downs have elapsed.
        let comfortably_low = p95_ms < cfg.p95_threshold_ms * 0.5;
        let cooled = since_scale.map_or(true, |d| d >= cfg.scale_down_cooldown);
        let stable = now.duration_since(start) >= cfg.scale_down_cooldown;
        if comfortably_low && cooled && stable && current > cfg.min_replicas {
            self.last_scale = Some(now);
            self.healthy_since = None;
            return ScalingDecision::ScaleTo(current - 1);
        }
        ScalingDecision::Hold
    }
}

/// Shared adapter state.
#[derive(Clone)]
pub struct AdapterState {
    pub histogram: Arc<WasmLatencyHistogram>,
    pub namespace: String,
    pub deployment: String,
}

/// Builds the custom metric `MetricValueList` for the p95 latency.
pub fn metric_value_list(state: &AdapterState) -> Value {
    let p95 = state.histogram.quantiles().p95;
    // Kubernetes quantities: use milli-units to preserve sub-ms precision.
    let milli = (p95 * 1000.0).round() as u64;
    json!({
        "kind": "MetricValueList",
        "apiVersion": "custom.metrics.k8s.io/v1beta2",
        "metadata": {},
        "items": [{
            "describedObject": {
                "kind": "Deployment",
                "namespace": state.namespace,
                "name": state.deployment,
                "apiVersion": "apps/v1"
            },
            "metric": { "name": METRIC_NAME, "selector": { "matchLabels": { "quantile": "0.95" } } },
            "timestamp": "1970-01-01T00:00:00Z",
            "windowSeconds": 60,
            "value": format!("{milli}m")
        }]
    })
}

/// HTTP router for the adapter (`/metrics` + custom metrics API endpoints).
pub fn router(state: AdapterState) -> Router {
    Router::new()
        .route(
            "/metrics",
            get(|State(s): State<AdapterState>| async move { s.histogram.render_prometheus() }),
        )
        .route(
            "/apis/custom.metrics.k8s.io/v1beta2",
            get(|| async {
                Json(json!({
                    "kind": "APIResourceList",
                    "apiVersion": "v1",
                    "groupVersion": "custom.metrics.k8s.io/v1beta2",
                    "resources": [{
                        "name": format!("deployments.apps/{METRIC_NAME}"),
                        "singularName": "",
                        "namespaced": true,
                        "kind": "MetricValueList",
                        "verbs": ["get"]
                    }]
                }))
            }),
        )
        .route(
            "/apis/custom.metrics.k8s.io/v1beta2/namespaces/{ns}/deployments.apps/{name}/soroban_wasm_latency_ms",
            get(|State(s): State<AdapterState>| async move { Json(metric_value_list(&s)) }),
        )
        .with_state(state)
}

/// Renders an `autoscaling/v2` HPA that targets p95 latency with
/// behaviour mirroring [`ScalingConfig`].
pub fn hpa_manifest(namespace: &str, deployment: &str, cfg: &ScalingConfig) -> Value {
    json!({
        "apiVersion": "autoscaling/v2",
        "kind": "HorizontalPodAutoscaler",
        "metadata": { "name": format!("{deployment}-wasm-latency"), "namespace": namespace },
        "spec": {
            "scaleTargetRef": { "apiVersion": "apps/v1", "kind": "Deployment", "name": deployment },
            "minReplicas": cfg.min_replicas,
            "maxReplicas": cfg.max_replicas,
            "metrics": [{
                "type": "Object",
                "object": {
                    "describedObject": { "apiVersion": "apps/v1", "kind": "Deployment", "name": deployment },
                    "metric": { "name": METRIC_NAME, "selector": { "matchLabels": { "quantile": "0.95" } } },
                    "target": { "type": "Value", "value": format!("{}m", (cfg.p95_threshold_ms * 1000.0) as u64) }
                }
            }],
            "behavior": {
                "scaleUp": {
                    "stabilizationWindowSeconds": cfg.breach_window.as_secs(),
                    "selectPolicy": "Min",
                    "policies": [{ "type": "Pods", "value": cfg.scale_up_step, "periodSeconds": cfg.scale_up_cooldown.as_secs() }]
                },
                "scaleDown": {
                    "stabilizationWindowSeconds": cfg.scale_down_cooldown.as_secs(),
                    "selectPolicy": "Min",
                    "policies": [{ "type": "Pods", "value": 1, "periodSeconds": cfg.scale_down_cooldown.as_secs() }]
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn requires_three_minutes_of_sustained_breach() {
        let mut p = ScalingPolicy::new(ScalingConfig::default());
        let t0 = Instant::now();
        assert_eq!(p.evaluate(80.0, 3, t0), ScalingDecision::Hold);
        assert_eq!(p.evaluate(80.0, 3, t0 + secs(120)), ScalingDecision::Hold);
        assert_eq!(p.evaluate(80.0, 3, t0 + secs(180)), ScalingDecision::ScaleTo(6));
    }

    #[test]
    fn transient_spike_resets_window() {
        let mut p = ScalingPolicy::new(ScalingConfig::default());
        let t0 = Instant::now();
        p.evaluate(80.0, 3, t0);
        p.evaluate(10.0, 3, t0 + secs(100));
        assert_eq!(p.evaluate(80.0, 3, t0 + secs(200)), ScalingDecision::Hold);
        assert_eq!(p.evaluate(80.0, 3, t0 + secs(379)), ScalingDecision::Hold);
        assert_eq!(p.evaluate(80.0, 3, t0 + secs(380)), ScalingDecision::ScaleTo(6));
    }

    #[test]
    fn cooldown_prevents_over_provisioning() {
        let mut p = ScalingPolicy::new(ScalingConfig::default());
        let t0 = Instant::now();
        p.evaluate(80.0, 3, t0);
        assert_eq!(p.evaluate(80.0, 3, t0 + secs(180)), ScalingDecision::ScaleTo(6));
        // New breach window satisfied at +360 but cool-down (300s) is from +180 => ok at +480.
        p.evaluate(80.0, 6, t0 + secs(181));
        assert_eq!(p.evaluate(80.0, 6, t0 + secs(400)), ScalingDecision::Hold);
        assert_eq!(p.evaluate(80.0, 6, t0 + secs(480)), ScalingDecision::ScaleTo(9));
    }

    #[test]
    fn respects_max_and_scales_down_gradually() {
        let cfg = ScalingConfig { max_replicas: 4, ..Default::default() };
        let mut p = ScalingPolicy::new(cfg);
        let t0 = Instant::now();
        p.evaluate(80.0, 3, t0);
        assert_eq!(p.evaluate(80.0, 3, t0 + secs(180)), ScalingDecision::ScaleTo(4));
        p.evaluate(5.0, 4, t0 + secs(200));
        assert_eq!(p.evaluate(5.0, 4, t0 + secs(700)), ScalingDecision::Hold);
        assert_eq!(p.evaluate(5.0, 4, t0 + secs(800)), ScalingDecision::ScaleTo(3));
    }

    #[test]
    fn metric_value_and_manifest_shape() {
        let h = Arc::new(WasmLatencyHistogram::default());
        for _ in 0..100 {
            h.observe(45.0);
        }
        let s = AdapterState { histogram: h, namespace: "stellar".into(), deployment: "soroban-rpc".into() };
        let v = metric_value_list(&s);
        assert_eq!(v["items"][0]["metric"]["name"], METRIC_NAME);
        assert!(v["items"][0]["value"].as_str().unwrap().ends_with('m'));

        let m = hpa_manifest("stellar", "soroban-rpc", &ScalingConfig::default());
        assert_eq!(m["spec"]["metrics"][0]["object"]["target"]["value"], "50000m");
        assert_eq!(m["spec"]["behavior"]["scaleUp"]["stabilizationWindowSeconds"], 180);
        assert_eq!(m["spec"]["behavior"]["scaleUp"]["policies"][0]["value"], 3);
    }
}
