//! Low-overhead eBPF network telemetry for Stellar Core pods.

pub mod collector;

pub use collector::{
    CollectorMode, ConnectionSample, EbpfCollector, PodIdentity, ProbeError,
};
