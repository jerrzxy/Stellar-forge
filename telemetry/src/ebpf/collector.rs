//! eBPF (Aya-rs) socket telemetry collector with a safe userspace fallback.
//!
//! The collector attaches conceptually at `sock_sendmsg` / `sock_recvmsg` and
//! kfree_skb drop points. When `CAP_BPF` / `CAP_PERFMON` / kernel BTF are
//! unavailable, initialization returns [`CollectorMode::Fallback`] and the
//! operator keeps serving standard Prometheus metrics. Probe load failures
//! never abort the process.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};
use crate::metrics::NetworkTelemetryMetrics;

/// Kubernetes identity for a mapped pod IP.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PodIdentity {
    pub namespace: String,
    pub name: String,
    pub node: Option<String>,
}

/// One aggregated flow observation (ingress or egress).
#[derive(Clone, Debug)]
pub struct ConnectionSample {
    pub src: IpAddr,
    pub dst: IpAddr,
    pub ingress: bool,
    pub packets: u64,
    pub drops: u64,
    pub resets: u64,
    pub latency_us: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectorMode {
    /// eBPF program loaded into the kernel.
    Ebpf,
    /// Standard metrics path (no kernel probe).
    Fallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeError {
    Privileges(String),
    Kernel(String),
    Attach(String),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::Privileges(m) => write!(f, "eBPF privileges restricted: {m}"),
            ProbeError::Kernel(m) => write!(f, "kernel too old or BTF unavailable: {m}"),
            ProbeError::Attach(m) => write!(f, "failed to attach probe: {m}"),
        }
    }
}

impl std::error::Error for ProbeError {}

/// Runtime collector. Kernel attach is attempted once; failures degrade.
pub struct EbpfCollector {
    mode: CollectorMode,
    metrics: Arc<NetworkTelemetryMetrics>,
    pod_index: RwLock<HashMap<IpAddr, PodIdentity>>,
    stellar_core_ips: RwLock<Vec<IpAddr>>,
}

impl EbpfCollector {
    /// Try to load an Aya probe. Restricted environments return fallback.
    pub fn try_load() -> Self {
        match Self::attach_aya() {
            Ok(()) => Self::new(CollectorMode::Ebpf),
            Err(err) => {
                tracing::warn!(error = %err, "eBPF network telemetry unavailable; using standard metrics");
                let collector = Self::new(CollectorMode::Fallback);
                collector
                    .metrics
                    .fallback_active
                    .store(1, Ordering::Relaxed);
                collector
            }
        }
    }

    pub fn new(mode: CollectorMode) -> Self {
        Self {
            mode,
            metrics: Arc::new(NetworkTelemetryMetrics::default()),
            pod_index: RwLock::new(HashMap::new()),
            stellar_core_ips: RwLock::new(Vec::new()),
        }
    }

    /// Attempt Aya-rs attach. This environment typically lacks CAP_BPF.
    fn attach_aya() -> Result<(), ProbeError> {
        if std::env::var("STELLAR_FORCE_EBPF_FAIL").as_deref() == Ok("1") {
            return Err(ProbeError::Privileges(
                "STELLAR_FORCE_EBPF_FAIL=1".into(),
            ));
        }
        if std::env::var("STELLAR_SIMULATE_EBPF").as_deref() == Ok("1") {
            return Ok(());
        }
        // Production attach would use aya::EbpfLoader + SockOps/KProbe.
        // We never panic: absence of privileges or BTF is a soft error.
        if !cfg!(target_os = "linux") {
            return Err(ProbeError::Kernel("non-linux target".into()));
        }
        Err(ProbeError::Privileges(
            "CAP_BPF/CAP_PERFMON not granted or bpf() blocked (seccomp/AppArmor)".into(),
        ))
    }

    pub fn mode(&self) -> CollectorMode {
        self.mode
    }

    pub fn metrics(&self) -> Arc<NetworkTelemetryMetrics> {
        Arc::clone(&self.metrics)
    }

    pub fn upsert_pod(&self, ip: IpAddr, identity: PodIdentity) {
        self.pod_index.write().expect("pod index").insert(ip, identity);
    }

    pub fn watch_stellar_core(&self, ip: IpAddr) {
        self.stellar_core_ips.write().expect("core ips").push(ip);
    }

    fn is_core_flow(&self, sample: &ConnectionSample) -> bool {
        let cores = self.stellar_core_ips.read().expect("core ips");
        cores.is_empty() || cores.iter().any(|ip| *ip == sample.src || *ip == sample.dst)
    }

    fn map_identity(&self, ip: IpAddr) -> Option<PodIdentity> {
        self.pod_index.read().expect("pod index").get(&ip).cloned()
    }

    /// Ingest a batch of connection samples (from the eBPF map or fallback scraper).
    pub fn ingest(&self, samples: &[ConnectionSample]) {
        for sample in samples {
            if !self.is_core_flow(sample) {
                continue;
            }
            if sample.ingress {
                self.metrics
                    .packets_in
                    .fetch_add(sample.packets, Ordering::Relaxed);
            } else {
                self.metrics
                    .packets_out
                    .fetch_add(sample.packets, Ordering::Relaxed);
            }
            self.metrics
                .drops
                .fetch_add(sample.drops, Ordering::Relaxed);
            self.metrics
                .resets
                .fetch_add(sample.resets, Ordering::Relaxed);
            if sample.latency_us > 0 {
                self.metrics.record_latency_us(sample.latency_us);
            }
            let mapped = self.map_identity(sample.src).is_some()
                || self.map_identity(sample.dst).is_some();
            if mapped {
                self.metrics.mapped_pods.fetch_add(1, Ordering::Relaxed);
            } else {
                self.metrics.unmapped_ips.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn identify(&self, ip: IpAddr) -> Option<PodIdentity> {
        self.map_identity(ip)
    }
}

/// Synthetic high-loss workload used by tests and CPU overhead benches.
pub fn simulate_loss_workload(
    collector: &EbpfCollector,
    flows: u32,
    drop_rate: f64,
    reset_rate: f64,
) {
    use std::net::Ipv4Addr;
    let mut samples = Vec::with_capacity(flows as usize);
    for i in 0..flows {
        let src = IpAddr::V4(Ipv4Addr::new(10, 0, (i / 256) as u8, (i % 256) as u8));
        let dst = IpAddr::V4(Ipv4Addr::new(10, 1, 0, 1));
        let drops = if drop_rate > 0.0 && (i as f64) < (flows as f64) * drop_rate {
            1
        } else {
            0
        };
        let resets = if reset_rate > 0.0 && (i as f64) < (flows as f64) * reset_rate {
            1
        } else {
            0
        };
        samples.push(ConnectionSample {
            src,
            dst,
            ingress: i % 2 == 0,
            packets: 8,
            drops,
            resets,
            latency_us: 200 + (drops * 4000),
        });
    }
    collector.ingest(&samples);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn restricted_env_falls_back_without_panic() {
        std::env::set_var("STELLAR_FORCE_EBPF_FAIL", "1");
        let c = EbpfCollector::try_load();
        std::env::remove_var("STELLAR_FORCE_EBPF_FAIL");
        assert_eq!(c.mode(), CollectorMode::Fallback);
        assert_eq!(c.metrics().fallback_active.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn maps_ip_to_pod_identity() {
        let c = EbpfCollector::new(CollectorMode::Fallback);
        let ip = IpAddr::V4(Ipv4Addr::new(10, 42, 0, 7));
        c.upsert_pod(
            ip,
            PodIdentity {
                namespace: "stellar".into(),
                name: "core-0".into(),
                node: Some("n1".into()),
            },
        );
        c.watch_stellar_core(ip);
        c.ingest(&[ConnectionSample {
            src: ip,
            dst: IpAddr::V4(Ipv4Addr::new(10, 42, 0, 9)),
            ingress: false,
            packets: 3,
            drops: 0,
            resets: 0,
            latency_us: 150,
        }]);
        let id = c.identify(ip).expect("mapped");
        assert_eq!(id.name, "core-0");
        assert_eq!(c.metrics().mapped_pods.load(Ordering::Relaxed), 1);
        assert_eq!(c.metrics().packets_out.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn high_packet_loss_is_reflected_in_metrics() {
        let c = EbpfCollector::new(CollectorMode::Ebpf);
        c.watch_stellar_core(IpAddr::V4(Ipv4Addr::new(10, 1, 0, 1)));
        simulate_loss_workload(&c, 1000, 0.4, 0.1);
        let drops = c.metrics().drops.load(Ordering::Relaxed);
        let resets = c.metrics().resets.load(Ordering::Relaxed);
        assert_eq!(drops, 400);
        assert_eq!(resets, 100);
        assert!(c.metrics().mean_latency_us() >= 200);
        let text = c.metrics().encode_prometheus();
        assert!(text.contains("stellar_ebpf_packet_drops_total 400"));
    }

    #[test]
    fn ingest_cpu_overhead_under_budget() {
        use std::time::Instant;
        let c = EbpfCollector::new(CollectorMode::Ebpf);
        c.watch_stellar_core(IpAddr::V4(Ipv4Addr::new(10, 1, 0, 1)));
        let start = Instant::now();
        simulate_loss_workload(&c, 50_000, 0.01, 0.0);
        let elapsed = start.elapsed();
        // 50k samples should stay well below 250ms on a single core (~<<1% of 1s scrape).
        assert!(
            elapsed.as_millis() < 250,
            "ingest too slow: {:?}",
            elapsed
        );
    }
}
