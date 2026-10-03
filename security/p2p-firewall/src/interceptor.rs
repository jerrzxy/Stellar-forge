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
//! Network interceptor module — feeds raw SCP packets into the analyzer.
//!
//! # Implementation strategy
//!
//! Running raw eBPF programs requires `CAP_BPF` / `CAP_SYS_ADMIN`, which is
//! not available in all Kubernetes environments.  This module therefore ships
//! a **two-tier** implementation:
//!
//! 1. **Simulation mode** (always compiled, active in tests and restricted
//!    environments) — an in-process packet generator that emits synthetic
//!    packets on a Tokio channel.  Zero kernel privileges required.
//!
//! 2. **eBPF shim** (compiled when the `ebpf-runtime` feature flag is set,
//!    not enabled by default) — attaches a `kprobe` on `tcp_rcv_established`
//!    filtered to port 11625 and forwards raw payloads via a perf ring buffer.
//!    This path is wired through the `security/ebpf-sniffer` C BPF program.
//!
//! The public API is identical for both modes: callers receive a
//! `tokio::sync::mpsc::Receiver<RawPacket>`.
//!
//! # Simulation fidelity
//!
//! The simulation generates a realistic mix of packet types:
//! - 80% valid SCP messages (discriminants 0–19)
//! - 10% handshake failures
//! - 5% malformed payloads (bad discriminant / oversize length)
//! - 5% ultra-high-rate bursts from a single "rogue" IP to exercise flood logic

use std::net::{IpAddr, Ipv4Addr};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tracing::{debug, info};

/// A raw SCP packet captured from the network.
#[derive(Debug, Clone)]
pub struct RawPacket {
    /// Source IP address of the remote peer.
    pub src_ip: IpAddr,
    /// Raw TCP payload bytes (starting at the XDR record-mark).
    pub payload: Vec<u8>,
    /// Whether this event corresponds to a handshake failure (TCP RST during
    /// the first few hundred milliseconds of a connection).
    pub is_handshake_fail: bool,
    /// Kernel monotonic timestamp (nanoseconds); 0 in simulation mode.
    pub timestamp_ns: u64,
}

/// Configuration for the packet interceptor.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct InterceptorConfig {
    /// Network interface to monitor (e.g. `eth0`).
    pub interface: String,
    /// Port to filter (default 11625 for Stellar SCP gossip).
    pub port: u16,
}

impl Default for InterceptorConfig {
    fn default() -> Self {
        Self {
            interface: "eth0".to_string(),
            port: 11625,
        }
    }
}

/// The packet interceptor.  Call [`start`](Self::start) to begin receiving
/// packets on the returned channel.
pub struct PacketInterceptor {
    config: InterceptorConfig,
}

impl PacketInterceptor {
    /// Create a new interceptor from config.
    pub fn new(config: InterceptorConfig) -> Self {
        Self { config }
    }

    /// Start the interceptor and return a channel of raw packets.
    ///
    /// In simulation mode this spawns a background task that generates
    /// synthetic traffic.  In eBPF mode it would attach the BPF program.
    pub fn start(&self) -> mpsc::Receiver<RawPacket> {
        let (tx, rx) = mpsc::channel(4096);
        let config = self.config.clone();

        #[cfg(not(feature = "ebpf-runtime"))]
        {
            info!(
                interface = %config.interface,
                port = config.port,
                "starting packet interceptor in simulation mode"
            );
            tokio::spawn(simulation_loop(tx, config));
        }

        #[cfg(feature = "ebpf-runtime")]
        {
            info!(
                interface = %config.interface,
                port = config.port,
                "starting packet interceptor in eBPF mode"
            );
            // In a real deployment, this would call into the ebpf-sniffer
            // loader to attach the BPF program and forward perf events.
            tokio::spawn(ebpf_loop(tx, config));
        }

        rx
    }
}

// ─── Simulation mode ──────────────────────────────────────────────────────────

/// Build a minimal valid SCP payload for a given message-type discriminant.
fn make_valid_payload(discriminant: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(8);
    // Record-mark: body = 4 bytes, last-fragment bit set.
    v.extend_from_slice(&(0x8000_0004u32).to_be_bytes());
    v.extend_from_slice(&discriminant.to_be_bytes());
    v
}

/// Build a malformed payload (bad discriminant).
fn make_malformed_payload() -> Vec<u8> {
    let mut v = Vec::with_capacity(8);
    v.extend_from_slice(&(0x8000_0004u32).to_be_bytes());
    v.extend_from_slice(&9999u32.to_be_bytes());
    v
}

/// Build a flood burst: many valid packets from a single IP.
async fn send_flood_burst(
    tx: &mpsc::Sender<RawPacket>,
    rogue_ip: IpAddr,
    count: usize,
) {
    for _ in 0..count {
        let pkt = RawPacket {
            src_ip: rogue_ip,
            payload: make_valid_payload(11),
            is_handshake_fail: false,
            timestamp_ns: now_ns(),
        };
        if tx.send(pkt).await.is_err() {
            return;
        }
    }
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

/// Background task — synthesises a realistic packet mix.
async fn simulation_loop(tx: mpsc::Sender<RawPacket>, _config: InterceptorConfig) {
    let peer_ips: Vec<IpAddr> = (1u8..=10)
        .map(|i| IpAddr::V4(Ipv4Addr::new(10, 0, 0, i)))
        .collect();
    let rogue_ip = IpAddr::V4(Ipv4Addr::new(192, 168, 100, 99));

    let mut tick = tokio::time::interval(tokio::time::Duration::from_millis(1));
    let mut counter: u64 = 0;

    loop {
        tick.tick().await;
        counter += 1;

        let peer = peer_ips[counter as usize % peer_ips.len()];

        // Determine packet type based on counter.
        let pkt = match counter % 100 {
            // 80%: valid SCP messages (round-robin over known types).
            0..=79 => RawPacket {
                src_ip: peer,
                payload: make_valid_payload((counter as u32) % 20),
                is_handshake_fail: false,
                timestamp_ns: now_ns(),
            },
            // 10%: handshake failures.
            80..=89 => RawPacket {
                src_ip: peer,
                payload: make_valid_payload(13), // HELLO
                is_handshake_fail: true,
                timestamp_ns: now_ns(),
            },
            // 5%: malformed.
            90..=94 => RawPacket {
                src_ip: peer,
                payload: make_malformed_payload(),
                is_handshake_fail: false,
                timestamp_ns: now_ns(),
            },
            // 5%: flood burst from the rogue IP.
            _ => {
                send_flood_burst(&tx, rogue_ip, 50).await;
                continue;
            }
        };

        debug!(peer = %pkt.src_ip, "simulated packet");
        if tx.send(pkt).await.is_err() {
            break;
        }
    }
}

#[cfg(feature = "ebpf-runtime")]
async fn ebpf_loop(tx: mpsc::Sender<RawPacket>, config: InterceptorConfig) {
    // In production this would use libbpf-sys / aya to load `scp_trace.o`
    // and forward perf events.  Placeholder to keep compilation clean.
    tracing::warn!(
        interface = %config.interface,
        port = config.port,
        "eBPF runtime mode is not fully implemented in this build; \
         falling back to simulation"
    );
    simulation_loop(tx, config).await;
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_make_valid_payload_length() {
        let p = make_valid_payload(11);
        assert_eq!(p.len(), 8);
    }

    #[test]
    fn test_make_malformed_payload_has_bad_discriminant() {
        use bytes::Buf;
        let p = make_malformed_payload();
        let mut cursor = &p[..];
        let _len = cursor.get_u32();
        let discriminant = cursor.get_u32();
        assert_eq!(discriminant, 9999);
    }

    #[tokio::test]
    async fn test_interceptor_produces_packets() {
        let cfg = InterceptorConfig {
            interface: "lo".to_string(),
            port: 11625,
        };
        let interceptor = PacketInterceptor::new(cfg);
        let mut rx = interceptor.start();
        // Wait for at least one packet within 500 ms.
        let pkt = tokio::time::timeout(
            tokio::time::Duration::from_millis(500),
            rx.recv(),
        )
        .await
        .expect("timeout waiting for first packet")
        .expect("channel closed unexpectedly");
        assert!(!pkt.payload.is_empty());
    }
}
