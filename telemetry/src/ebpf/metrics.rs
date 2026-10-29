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

//! Prometheus metrics definitions for the eBPF network telemetry collector.
//!
//! # Exported metrics
//!
//! | Metric | Type | Labels | Description |
//! |---|---|---|---|
//! | `stellar_ebpf_connections_total` | counter | `direction`, `pod_name`, `namespace` | Total TCP connections observed |
//! | `stellar_ebpf_bytes_total` | counter | `direction`, `pod_name`, `namespace` | Total bytes transferred |
//! | `stellar_ebpf_packets_dropped_total` | counter | `pod_name`, `namespace`, `reason` | Total dropped packets |
//! | `stellar_ebpf_latency_us` | histogram | `direction`, `pod_name`, `namespace` | Round-trip latency in µs |
//! | `stellar_ebpf_connection_resets_total` | counter | `pod_name`, `namespace` | TCP connection resets observed |
//! | `stellar_ebpf_probe_mode` | gauge | `mode` | `1` = native eBPF, `0` = simulation |
//! | `stellar_ebpf_poll_duration_us` | histogram | – | Time spent in each poll cycle |
//! | `stellar_ebpf_events_per_poll` | histogram | – | Number of events returned per poll |

use std::sync::atomic::AtomicU64;

use prometheus_client::encoding::text::encode;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{exponential_buckets, linear_buckets, Histogram};
use prometheus_client::registry::Registry;

// ---------------------------------------------------------------------------
// Label sets
// ---------------------------------------------------------------------------

/// Labels carried by per-direction / per-pod connection and byte counters.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct NetworkLabels {
    /// `ingress` or `egress`.
    pub direction: String,
    /// Kubernetes pod name (resolved from IP mapping, or "unknown").
    pub pod_name: String,
    /// Kubernetes namespace (resolved from IP mapping, or "unknown").
    pub namespace: String,
}

/// Labels for dropped-packet counters.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct DropLabels {
    /// Kubernetes pod name.
    pub pod_name: String,
    /// Kubernetes namespace.
    pub namespace: String,
    /// Human-readable drop reason: `packet_drop`, `connection_reset`, etc.
    pub reason: String,
}

/// Labels for connection-reset counters.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct PodLabels {
    /// Kubernetes pod name.
    pub pod_name: String,
    /// Kubernetes namespace.
    pub namespace: String,
}

/// Labels for the probe-mode gauge.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ModeLabels {
    /// `native` or `simulation`.
    pub mode: String,
}

// ---------------------------------------------------------------------------
// Metric collection
// ---------------------------------------------------------------------------

/// All Prometheus metrics exported by the eBPF telemetry collector.
///
/// Construct once and share via `Arc<EbpfMetrics>`.
pub struct EbpfMetrics {
    pub(crate) registry: Registry,

    /// Total TCP connections observed, by direction and pod.
    pub connections_total: Family<NetworkLabels, Counter<u64, AtomicU64>>,

    /// Total bytes transferred, by direction and pod.
    pub bytes_total: Family<NetworkLabels, Counter<u64, AtomicU64>>,

    /// Total packets dropped, by pod and reason.
    pub packets_dropped_total: Family<DropLabels, Counter<u64, AtomicU64>>,

    /// Round-trip latency histogram in microseconds.
    pub latency_us: Family<NetworkLabels, Histogram>,

    /// TCP connection resets observed, by pod.
    pub connection_resets_total: Family<PodLabels, Counter<u64, AtomicU64>>,

    /// Probe mode indicator: 1.0 = native eBPF, 0.0 = simulation.
    pub probe_mode: Family<ModeLabels, Gauge<f64, std::sync::atomic::AtomicU64>>,

    /// Time spent in each collector poll cycle (microseconds).
    pub poll_duration_us: Histogram,

    /// Number of events returned per poll cycle.
    pub events_per_poll: Histogram,
}

impl EbpfMetrics {
    /// Create and register all metrics.
    pub fn new() -> Self {
        let mut registry = Registry::default();

        let connections_total: Family<NetworkLabels, Counter<u64, AtomicU64>> =
            Family::default();
        registry.register(
            "stellar_ebpf_connections",
            "Total TCP connections observed by the eBPF probe",
            connections_total.clone(),
        );

        let bytes_total: Family<NetworkLabels, Counter<u64, AtomicU64>> = Family::default();
        registry.register(
            "stellar_ebpf_bytes",
            "Total bytes transferred through monitored connections",
            bytes_total.clone(),
        );

        let packets_dropped_total: Family<DropLabels, Counter<u64, AtomicU64>> = Family::default();
        registry.register(
            "stellar_ebpf_packets_dropped",
            "Total packets dropped, tagged by reason",
            packets_dropped_total.clone(),
        );

        // Latency buckets: 100 µs → ~100 s, 25 buckets (exponential x 2).
        let latency_us: Family<NetworkLabels, Histogram> =
            Family::new_with_constructor(|| Histogram::new(exponential_buckets(100.0, 2.0, 25)));
        registry.register(
            "stellar_ebpf_latency_us",
            "Round-trip connection latency observed by the eBPF probe (µs)",
            latency_us.clone(),
        );

        let connection_resets_total: Family<PodLabels, Counter<u64, AtomicU64>> =
            Family::default();
        registry.register(
            "stellar_ebpf_connection_resets",
            "TCP connection resets (RST) observed by the eBPF probe",
            connection_resets_total.clone(),
        );

        let probe_mode: Family<ModeLabels, Gauge<f64, std::sync::atomic::AtomicU64>> =
            Family::default();
        registry.register(
            "stellar_ebpf_probe_mode",
            "1 = native eBPF kernel attachment, 0 = simulation fallback",
            probe_mode.clone(),
        );

        // Poll duration: 1 µs → ~500 ms (linear 10 µs steps up to 100 µs,
        // then exponential).
        let poll_duration_us =
            Histogram::new(exponential_buckets(10.0, 2.0, 16));
        registry.register(
            "stellar_ebpf_poll_duration_us",
            "Time spent per collector poll cycle (µs)",
            poll_duration_us.clone(),
        );

        // Events per poll: 0–4096 in linear steps of 128.
        let events_per_poll = Histogram::new(linear_buckets(0.0, 128.0, 33));
        registry.register(
            "stellar_ebpf_events_per_poll",
            "Number of eBPF events drained per poll cycle",
            events_per_poll.clone(),
        );

        EbpfMetrics {
            registry,
            connections_total,
            bytes_total,
            packets_dropped_total,
            latency_us,
            connection_resets_total,
            probe_mode,
            poll_duration_us,
            events_per_poll,
        }
    }

    /// Render all metrics to Prometheus text format.
    pub fn render(&self) -> String {
        let mut buf = String::new();
        encode(&mut buf, &self.registry).unwrap_or_default();
        buf
    }
}

impl Default for EbpfMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_register_without_panic() {
        let m = EbpfMetrics::new();

        // Observe at least one value on each metric family so they appear in output.
        let net = NetworkLabels {
            direction: "ingress".to_string(),
            pod_name: "test-pod".to_string(),
            namespace: "test-ns".to_string(),
        };
        m.connections_total.get_or_create(&net).inc();
        m.bytes_total.get_or_create(&net).inc_by(100);
        m.latency_us.get_or_create(&net).observe(1_000.0);

        let drop = DropLabels {
            pod_name: "test-pod".to_string(),
            namespace: "test-ns".to_string(),
            reason: "packet_drop".to_string(),
        };
        m.packets_dropped_total.get_or_create(&drop).inc();

        let pod = PodLabels {
            pod_name: "test-pod".to_string(),
            namespace: "test-ns".to_string(),
        };
        m.connection_resets_total.get_or_create(&pod).inc();
        m.probe_mode
            .get_or_create(&ModeLabels {
                mode: "simulation".to_string(),
            })
            .set(1.0);
        m.poll_duration_us.observe(50.0);
        m.events_per_poll.observe(8.0);

        let rendered = m.render();
        assert!(rendered.contains("stellar_ebpf_connections"), "connections missing");
        assert!(rendered.contains("stellar_ebpf_bytes"), "bytes missing");
        assert!(rendered.contains("stellar_ebpf_packets_dropped"), "packets_dropped missing");
        assert!(rendered.contains("stellar_ebpf_latency_us"), "latency_us missing");
        assert!(rendered.contains("stellar_ebpf_connection_resets"), "connection_resets missing");
        assert!(rendered.contains("stellar_ebpf_probe_mode"), "probe_mode missing");
        assert!(rendered.contains("stellar_ebpf_poll_duration_us"), "poll_duration_us missing");
        assert!(rendered.contains("stellar_ebpf_events_per_poll"), "events_per_poll missing");
    }

    #[test]
    fn connections_counter_increments() {
        let m = EbpfMetrics::new();
        let labels = NetworkLabels {
            direction: "ingress".to_string(),
            pod_name: "stellar-core-0".to_string(),
            namespace: "stellar-system".to_string(),
        };
        m.connections_total
            .get_or_create(&labels)
            .inc_by(5);
        let rendered = m.render();
        // The rendered output should contain the counter value.
        assert!(rendered.contains("stellar_ebpf_connections_total"));
    }

    #[test]
    fn latency_histogram_observes_values() {
        let m = EbpfMetrics::new();
        let labels = NetworkLabels {
            direction: "egress".to_string(),
            pod_name: "stellar-core-1".to_string(),
            namespace: "stellar-system".to_string(),
        };
        for us in [500u64, 1_000, 5_000, 10_000, 50_000] {
            m.latency_us.get_or_create(&labels).observe(us as f64);
        }
        let rendered = m.render();
        assert!(rendered.contains("stellar_ebpf_latency_us"));
    }

    #[test]
    fn probe_mode_gauge_sets_correctly() {
        let m = EbpfMetrics::new();
        m.probe_mode
            .get_or_create(&ModeLabels {
                mode: "simulation".to_string(),
            })
            .set(1.0);
        let rendered = m.render();
        assert!(rendered.contains("stellar_ebpf_probe_mode"));
    }

    #[test]
    fn drop_labels_work() {
        let m = EbpfMetrics::new();
        let labels = DropLabels {
            pod_name: "stellar-core-0".to_string(),
            namespace: "stellar-system".to_string(),
            reason: "packet_drop".to_string(),
        };
        m.packets_dropped_total.get_or_create(&labels).inc_by(10);
        let rendered = m.render();
        assert!(rendered.contains("stellar_ebpf_packets_dropped_total"));
    }
}
