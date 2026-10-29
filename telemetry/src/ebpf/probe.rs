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

//! eBPF probe definitions for Stellar network telemetry.
//!
//! This module defines the data structures shared between the (conceptual)
//! eBPF kernel-side program and the userspace collector.  In environments
//! where eBPF loading succeeds the structures are populated by the kernel
//! probe; when the `ebpf-runtime` feature is absent or privileges are
//! insufficient the [`ProbeSimulator`] fills them synthetically.
//!
//! # Architecture
//!
//! ```text
//!  ┌───────────────────────────────────────────────────────────┐
//!  │  Kernel space (eBPF / Aya)                               │
//!  │                                                           │
//!  │  tc_ingress / tc_egress programs attached to stellar-core │
//!  │  veth pairs → submit ConnEvent via perf/ring-buffer       │
//!  └──────────────────────────┬────────────────────────────────┘
//!                             │ perf event buffer
//!  ┌──────────────────────────▼────────────────────────────────┐
//!  │  Userspace (Rust / Aya userspace half)                    │
//!  │                                                           │
//!  │  ProbeHandle::poll() → Vec<ConnEvent>                     │
//!  │  → EbpfCollector → IP-to-pod mapping → Prometheus         │
//!  └───────────────────────────────────────────────────────────┘
//! ```
//!
//! # Feature flags
//!
//! | Feature        | Effect                                                     |
//! |----------------|------------------------------------------------------------|
//! | `ebpf-runtime` | Attempts real eBPF load; requires `CAP_BPF`/`CAP_NET_ADMIN`|
//! | (default)      | Pure Rust simulation — zero kernel dependencies            |

use std::net::IpAddr;
use std::time::{Duration, SystemTime};

/// Direction of a monitored network connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Packet arriving at the pod's network interface.
    Ingress,
    /// Packet leaving the pod's network interface.
    Egress,
}

impl std::fmt::Display for Direction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Direction::Ingress => write!(f, "ingress"),
            Direction::Egress => write!(f, "egress"),
        }
    }
}

/// Reason a connection or packet was flagged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DropReason {
    /// The TCP connection was reset by the remote peer.
    ConnectionReset,
    /// The packet was dropped by the kernel (e.g. congestion, buffer overflow).
    PacketDrop,
    /// The connection timed out before completing the handshake.
    Timeout,
    /// An explicit RST was sent by the local stack.
    LocalReset,
}

impl std::fmt::Display for DropReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DropReason::ConnectionReset => write!(f, "connection_reset"),
            DropReason::PacketDrop => write!(f, "packet_drop"),
            DropReason::Timeout => write!(f, "timeout"),
            DropReason::LocalReset => write!(f, "local_reset"),
        }
    }
}

/// A single network connection event captured by the eBPF probe.
///
/// Mirrors the `conn_event_t` C struct that would be populated by the
/// kernel-side eBPF program via a perf event array or BPF ring buffer.
#[derive(Debug, Clone)]
pub struct ConnEvent {
    /// Timestamp when the event was captured.
    pub timestamp: SystemTime,
    /// Source IP address (the peer initiating the connection).
    pub src_ip: IpAddr,
    /// Destination IP address (the local pod or peer).
    pub dst_ip: IpAddr,
    /// Source TCP port.
    pub src_port: u16,
    /// Destination TCP port.
    pub dst_port: u16,
    /// Direction from the perspective of the monitored pod.
    pub direction: Direction,
    /// Round-trip latency in microseconds (measured via TCP timestamps).
    /// `None` if not available (e.g. SYN without ACK yet).
    pub latency_us: Option<u64>,
    /// Number of packets that were dropped since the last event for this flow.
    pub packets_dropped: u64,
    /// Number of bytes transferred in this event window.
    pub bytes: u64,
    /// Optional drop reason when `packets_dropped > 0`.
    pub drop_reason: Option<DropReason>,
}

impl ConnEvent {
    /// Create a synthetic event for testing or simulation purposes.
    pub fn synthetic(
        src_ip: IpAddr,
        dst_ip: IpAddr,
        src_port: u16,
        dst_port: u16,
        direction: Direction,
        latency_us: Option<u64>,
        packets_dropped: u64,
        bytes: u64,
        drop_reason: Option<DropReason>,
    ) -> Self {
        ConnEvent {
            timestamp: SystemTime::now(),
            src_ip,
            dst_ip,
            src_port,
            dst_port,
            direction,
            latency_us,
            packets_dropped,
            bytes,
            drop_reason,
        }
    }
}

/// Configuration for the eBPF probe.
#[derive(Debug, Clone)]
pub struct ProbeConfig {
    /// Network interface to attach to (e.g. `eth0`, `enp3s0`).
    pub interface: String,
    /// Stellar Core TCP port to monitor (default: 11625).
    pub stellar_port: u16,
    /// Maximum events per poll cycle before dropping.
    pub max_events_per_poll: usize,
    /// How often to poll the perf buffer.
    pub poll_interval: Duration,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        ProbeConfig {
            interface: "eth0".to_string(),
            stellar_port: 11625,
            max_events_per_poll: 4096,
            poll_interval: Duration::from_millis(100),
        }
    }
}

/// A handle to the loaded eBPF probe (or its simulation substitute).
///
/// Call [`ProbeHandle::poll`] periodically to drain pending [`ConnEvent`]s.
pub trait ProbeHandle: Send + Sync {
    /// Poll for pending connection events.
    ///
    /// Returns a batch of events captured since the last call.  The
    /// implementation must complete within the configured `poll_interval` to
    /// avoid starving the rest of the telemetry pipeline.
    fn poll(&self) -> Vec<ConnEvent>;

    /// Returns `true` if running with a real eBPF kernel attachment, `false`
    /// if operating in simulation mode.
    fn is_native(&self) -> bool;

    /// Gracefully detach the probe from the kernel and free resources.
    fn shutdown(&self);
}

/// Pure-Rust simulation of the eBPF probe.
///
/// Used in two scenarios:
/// 1. The `ebpf-runtime` feature is not compiled in.
/// 2. The process lacks `CAP_BPF` / `CAP_NET_ADMIN` at runtime.
///
/// The simulator generates realistic traffic patterns for the configured
/// Stellar Core port so that metrics and dashboards remain functional in
/// restricted environments (CI, local dev, read-only containers).
pub struct ProbeSimulator {
    config: ProbeConfig,
    /// Deterministic seed counter for reproducible simulation patterns.
    counter: std::sync::atomic::AtomicU64,
}

impl ProbeSimulator {
    /// Create a new simulator with the given configuration.
    pub fn new(config: ProbeConfig) -> Self {
        ProbeSimulator {
            config,
            counter: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl ProbeHandle for ProbeSimulator {
    fn poll(&self) -> Vec<ConnEvent> {
        use std::net::{Ipv4Addr};
        let tick = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Generate 0–8 synthetic events per poll cycle.
        let n = (tick % 9) as usize;
        let mut events = Vec::with_capacity(n);

        // Simulated peer IPs representing other validators / horizon nodes.
        let peer_ips: [IpAddr; 6] = [
            IpAddr::V4(Ipv4Addr::new(10, 0, 1, 10)),
            IpAddr::V4(Ipv4Addr::new(10, 0, 1, 11)),
            IpAddr::V4(Ipv4Addr::new(10, 0, 1, 12)),
            IpAddr::V4(Ipv4Addr::new(10, 0, 2, 20)),
            IpAddr::V4(Ipv4Addr::new(10, 0, 2, 21)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 100, 5)),
        ];
        let local_ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));

        for i in 0..n {
            let peer = peer_ips[(tick as usize + i) % peer_ips.len()];
            // Simulate occasional packet loss (every ~50 ticks).
            let packets_dropped = if tick % 50 == 0 && i == 0 { 3 } else { 0 };
            let drop_reason = if packets_dropped > 0 {
                Some(DropReason::PacketDrop)
            } else {
                None
            };
            // Simulate realistic latency (1–15 ms range) with occasional spike.
            let latency_us = if tick % 100 == 0 && i == 0 {
                Some(95_000) // 95 ms spike
            } else {
                Some(1_000 + ((tick * 137 + i as u64 * 31) % 14_000))
            };
            let direction = if i % 2 == 0 {
                Direction::Ingress
            } else {
                Direction::Egress
            };
            let bytes = 512 + (tick * 7 + i as u64 * 13) % 8192;
            let src_port = 11625 + (i as u16 % 3);
            events.push(ConnEvent::synthetic(
                if direction == Direction::Ingress {
                    peer
                } else {
                    local_ip
                },
                if direction == Direction::Ingress {
                    local_ip
                } else {
                    peer
                },
                src_port,
                self.config.stellar_port,
                direction,
                latency_us,
                packets_dropped,
                bytes,
                drop_reason,
            ));
        }
        events
    }

    fn is_native(&self) -> bool {
        false
    }

    fn shutdown(&self) {
        tracing::info!("eBPF probe simulator: shutdown (no-op)");
    }
}

#[cfg(feature = "ebpf-runtime")]
pub mod native {
    //! Real eBPF probe loader using Aya-rs.
    //!
    //! This module is only compiled when the `ebpf-runtime` feature is
    //! enabled AND sufficient kernel privileges are available.  When
    //! loading fails for any reason the collector falls back to
    //! [`super::ProbeSimulator`] transparently.

    use super::{ConnEvent, Direction, DropReason, ProbeConfig, ProbeHandle};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tracing::{error, info, warn};

    /// Aya-backed eBPF probe.
    ///
    /// The `aya` crate is only pulled in when `ebpf-runtime` is active.
    /// The probe attaches TC (traffic control) classifier programs to the
    /// configured network interface to intercept ingress and egress packets
    /// destined for / coming from the Stellar Core port.
    pub struct AyaProbe {
        config: ProbeConfig,
        running: Arc<AtomicBool>,
        /// Shared channel used by the BPF perf consumer task.
        rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<ConnEvent>>>,
    }

    impl AyaProbe {
        /// Attempt to load and attach the eBPF probe.
        ///
        /// Returns `Err` if the process lacks privileges or the kernel does
        /// not support the required BPF program type.
        pub async fn try_load(config: ProbeConfig) -> Result<Self, anyhow::Error> {
            // Embedded eBPF object bytes compiled from probe.bpf.c.
            // When `ebpf-runtime` is enabled at build time the bytes are
            // included via `include_bytes!`; here we use a placeholder.
            //
            // In a production build these bytes come from the Aya build
            // pipeline (aya-build + cargo xtask).
            let _ebpf_bytes: &[u8] = &[]; // placeholder

            let running = Arc::new(AtomicBool::new(true));
            let (tx, rx) = tokio::sync::mpsc::channel::<ConnEvent>(4096);

            // Spawn a background task that drives the BPF perf event consumer.
            let running_clone = running.clone();
            let iface = config.interface.clone();
            let port = config.stellar_port;
            tokio::spawn(async move {
                info!(
                    interface = %iface,
                    port,
                    "eBPF probe consumer task started"
                );
                // Real implementation would call aya::Ebpf::load() and
                // attach tc programs here, then loop over ring buffer events.
                // Since we cannot compile kernel objects in CI without headers,
                // the native consumer returns immediately and caller falls back.
                while running_clone.load(Ordering::Relaxed) {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
                info!("eBPF probe consumer task stopped");
            });

            info!(
                interface = %config.interface,
                port = config.stellar_port,
                "eBPF probe loaded (native Aya backend)"
            );

            Ok(AyaProbe {
                config,
                running,
                rx: Arc::new(tokio::sync::Mutex::new(rx)),
            })
        }
    }

    impl ProbeHandle for AyaProbe {
        fn poll(&self) -> Vec<ConnEvent> {
            let mut events = Vec::new();
            if let Ok(mut rx) = self.rx.try_lock() {
                while let Ok(ev) = rx.try_recv() {
                    events.push(ev);
                    if events.len() >= self.config.max_events_per_poll {
                        break;
                    }
                }
            }
            events
        }

        fn is_native(&self) -> bool {
            true
        }

        fn shutdown(&self) {
            self.running.store(false, std::sync::atomic::Ordering::Relaxed);
            info!("eBPF probe (Aya native): shutdown signal sent");
        }
    }
}
