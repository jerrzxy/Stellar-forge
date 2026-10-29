// Copyright 2024 Stellar-K8s Contributors
// SPDX-License-Identifier: Apache-2.0
//! WASM execution latency histogram.
//!
//! Records host-function execution times for Soroban contract invocations and
//! exposes them in Prometheus text format as the `soroban_wasm_latency_ms`
//! histogram, plus pre-computed `p50`/`p95`/`p99` gauges so the custom
//! metrics adapter (and the HPA) can consume a single scalar.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

/// Metric name exposed to Prometheus and the custom metrics API.
pub const METRIC_NAME: &str = "soroban_wasm_latency_ms";

/// Default bucket upper bounds in milliseconds.
pub const DEFAULT_BUCKETS_MS: &[f64] = &[
    1.0, 2.5, 5.0, 10.0, 20.0, 30.0, 40.0, 50.0, 75.0, 100.0, 250.0, 500.0, 1000.0,
];

/// Lock-free cumulative histogram of WASM execution latencies.
#[derive(Debug)]
pub struct WasmLatencyHistogram {
    bounds: Vec<f64>,
    /// Non-cumulative per-bucket counts; last slot is the `+Inf` bucket.
    counts: Vec<AtomicU64>,
    /// Sum of observations in microseconds (integer for atomic storage).
    sum_us: AtomicU64,
    total: AtomicU64,
}

impl Default for WasmLatencyHistogram {
    fn default() -> Self {
        Self::new(DEFAULT_BUCKETS_MS.to_vec())
    }
}

/// Snapshot of latency quantiles in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatencyQuantiles {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub count: u64,
}

impl WasmLatencyHistogram {
    /// Creates a histogram with the given (ascending) bucket bounds.
    pub fn new(mut bounds: Vec<f64>) -> Self {
        bounds.retain(|b| b.is_finite() && *b > 0.0);
        bounds.sort_by(|a, b| a.partial_cmp(b).unwrap());
        bounds.dedup();
        let counts = (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect();
        Self {
            bounds,
            counts,
            sum_us: AtomicU64::new(0),
            total: AtomicU64::new(0),
        }
    }

    /// Records one host-function execution latency in milliseconds.
    pub fn observe(&self, latency_ms: f64) {
        if !latency_ms.is_finite() || latency_ms < 0.0 {
            return;
        }
        let idx = self
            .bounds
            .iter()
            .position(|b| latency_ms <= *b)
            .unwrap_or(self.bounds.len());
        self.counts[idx].fetch_add(1, Ordering::Relaxed);
        self.sum_us
            .fetch_add((latency_ms * 1000.0) as u64, Ordering::Relaxed);
        self.total.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a [`std::time::Duration`].
    pub fn observe_duration(&self, d: std::time::Duration) {
        self.observe(d.as_secs_f64() * 1000.0);
    }

    /// Total observations.
    pub fn count(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Estimates quantile `q` (0..=1) using linear interpolation within the
    /// bucket, matching Prometheus' `histogram_quantile` semantics.
    pub fn quantile(&self, q: f64) -> f64 {
        let counts: Vec<u64> = self
            .counts
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect();
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return 0.0;
        }
        let rank = q.clamp(0.0, 1.0) * total as f64;
        let mut cumulative = 0u64;
        for (i, c) in counts.iter().enumerate() {
            let prev = cumulative;
            cumulative += c;
            if (cumulative as f64) >= rank && *c > 0 {
                if i == self.bounds.len() {
                    // +Inf bucket: return the highest finite bound.
                    return *self.bounds.last().unwrap_or(&0.0);
                }
                let lower = if i == 0 { 0.0 } else { self.bounds[i - 1] };
                let upper = self.bounds[i];
                let frac = (rank - prev as f64) / *c as f64;
                return lower + (upper - lower) * frac.clamp(0.0, 1.0);
            }
        }
        *self.bounds.last().unwrap_or(&0.0)
    }

    /// Returns p50/p95/p99.
    pub fn quantiles(&self) -> LatencyQuantiles {
        LatencyQuantiles {
            p50: self.quantile(0.50),
            p95: self.quantile(0.95),
            p99: self.quantile(0.99),
            count: self.count(),
        }
    }

    /// Clears all observations (used to start a fresh scrape window).
    pub fn reset(&self) {
        for c in &self.counts {
            c.store(0, Ordering::Relaxed);
        }
        self.sum_us.store(0, Ordering::Relaxed);
        self.total.store(0, Ordering::Relaxed);
    }

    /// Renders the histogram and quantile gauges in Prometheus text format.
    pub fn render_prometheus(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "# HELP {METRIC_NAME} Soroban WASM host-function execution latency in milliseconds."
        );
        let _ = writeln!(out, "# TYPE {METRIC_NAME} histogram");
        let mut cumulative = 0u64;
        for (i, b) in self.bounds.iter().enumerate() {
            cumulative += self.counts[i].load(Ordering::Relaxed);
            let _ = writeln!(out, "{METRIC_NAME}_bucket{{le=\"{b}\"}} {cumulative}");
        }
        cumulative += self.counts[self.bounds.len()].load(Ordering::Relaxed);
        let _ = writeln!(out, "{METRIC_NAME}_bucket{{le=\"+Inf\"}} {cumulative}");
        let sum_ms = self.sum_us.load(Ordering::Relaxed) as f64 / 1000.0;
        let _ = writeln!(out, "{METRIC_NAME}_sum {sum_ms}");
        let _ = writeln!(out, "{METRIC_NAME}_count {cumulative}");

        let q = self.quantiles();
        let _ = writeln!(
            out,
            "# HELP {METRIC_NAME}_quantile Pre-computed WASM latency quantiles (ms)."
        );
        let _ = writeln!(out, "# TYPE {METRIC_NAME}_quantile gauge");
        for (label, v) in [("0.5", q.p50), ("0.95", q.p95), ("0.99", q.p99)] {
            let _ = writeln!(out, "{METRIC_NAME}_quantile{{quantile=\"{label}\"}} {v}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_histogram_reports_zero() {
        let h = WasmLatencyHistogram::default();
        assert_eq!(h.quantiles().p95, 0.0);
    }

    #[test]
    fn quantiles_are_monotonic_and_bounded() {
        let h = WasmLatencyHistogram::default();
        for i in 0..1000 {
            h.observe((i % 100) as f64);
        }
        let q = h.quantiles();
        assert!(q.p50 <= q.p95 && q.p95 <= q.p99);
        assert!(q.p95 > 50.0 && q.p95 <= 100.0);
        assert_eq!(q.count, 1000);
    }

    #[test]
    fn ignores_invalid_observations() {
        let h = WasmLatencyHistogram::default();
        h.observe(f64::NAN);
        h.observe(-1.0);
        assert_eq!(h.count(), 0);
    }

    #[test]
    fn renders_prometheus_text() {
        let h = WasmLatencyHistogram::default();
        h.observe(3.0);
        h.observe(2000.0);
        let text = h.render_prometheus();
        assert!(text.contains("soroban_wasm_latency_ms_bucket{le=\"5\"} 1"));
        assert!(text.contains("soroban_wasm_latency_ms_bucket{le=\"+Inf\"} 2"));
        assert!(text.contains("soroban_wasm_latency_ms_quantile{quantile=\"0.95\"}"));
    }
}
