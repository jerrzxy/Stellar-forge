//! Operator telemetry engine metrics for eBPF and fallback collectors.

use std::sync::atomic::{AtomicU64, Ordering};

/// Aggregated socket-level counters exposed as Prometheus text.
#[derive(Debug, Default)]
pub struct NetworkTelemetryMetrics {
    pub packets_in: AtomicU64,
    pub packets_out: AtomicU64,
    pub drops: AtomicU64,
    pub resets: AtomicU64,
    pub latency_us_sum: AtomicU64,
    pub latency_samples: AtomicU64,
    pub mapped_pods: AtomicU64,
    pub unmapped_ips: AtomicU64,
    pub fallback_active: AtomicU64,
}

impl NetworkTelemetryMetrics {
    pub fn record_latency_us(&self, latency_us: u64) {
        self.latency_us_sum.fetch_add(latency_us, Ordering::Relaxed);
        self.latency_samples.fetch_add(1, Ordering::Relaxed);
    }

    pub fn mean_latency_us(&self) -> u64 {
        let n = self.latency_samples.load(Ordering::Relaxed);
        if n == 0 {
            0
        } else {
            self.latency_us_sum.load(Ordering::Relaxed) / n
        }
    }

    pub fn encode_prometheus(&self) -> String {
        format!(
            "# HELP stellar_ebpf_packets_in_total Ingress packets observed at the socket layer.\n\
             # TYPE stellar_ebpf_packets_in_total counter\n\
             stellar_ebpf_packets_in_total {}\n\
             # HELP stellar_ebpf_packets_out_total Egress packets observed at the socket layer.\n\
             # TYPE stellar_ebpf_packets_out_total counter\n\
             stellar_ebpf_packets_out_total {}\n\
             # HELP stellar_ebpf_packet_drops_total Packet drops attributed to Stellar Core peers.\n\
             # TYPE stellar_ebpf_packet_drops_total counter\n\
             stellar_ebpf_packet_drops_total {}\n\
             # HELP stellar_ebpf_connection_resets_total TCP RSTs involving Stellar Core pods.\n\
             # TYPE stellar_ebpf_connection_resets_total counter\n\
             stellar_ebpf_connection_resets_total {}\n\
             # HELP stellar_ebpf_latency_us_avg Mean socket RTT sample in microseconds.\n\
             # TYPE stellar_ebpf_latency_us_avg gauge\n\
             stellar_ebpf_latency_us_avg {}\n\
             # HELP stellar_ebpf_pod_mapped_total Flows mapped to Kubernetes Pod identities.\n\
             # TYPE stellar_ebpf_pod_mapped_total counter\n\
             stellar_ebpf_pod_mapped_total {}\n\
             # HELP stellar_ebpf_unmapped_ips_total Flows that could not be mapped to a Pod.\n\
             # TYPE stellar_ebpf_unmapped_ips_total counter\n\
             stellar_ebpf_unmapped_ips_total {}\n\
             # HELP stellar_ebpf_fallback_active 1 when the collector is using standard metrics fallback.\n\
             # TYPE stellar_ebpf_fallback_active gauge\n\
             stellar_ebpf_fallback_active {}\n",
            self.packets_in.load(Ordering::Relaxed),
            self.packets_out.load(Ordering::Relaxed),
            self.drops.load(Ordering::Relaxed),
            self.resets.load(Ordering::Relaxed),
            self.mean_latency_us(),
            self.mapped_pods.load(Ordering::Relaxed),
            self.unmapped_ips.load(Ordering::Relaxed),
            self.fallback_active.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prometheus_text_includes_core_series() {
        let m = NetworkTelemetryMetrics::default();
        m.drops.store(7, Ordering::Relaxed);
        let text = m.encode_prometheus();
        assert!(text.contains("stellar_ebpf_packet_drops_total 7"));
        assert!(text.contains("stellar_ebpf_fallback_active"));
    }
}
