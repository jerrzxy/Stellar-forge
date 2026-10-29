//! Host function benchmarking and calibration module for Soroban fee modeling.
//!
//! Benchmarks core Soroban host functions (e.g. cryptographic hashing, ledger reads/writes,
//! curve operations) on isolated/dedicated CPU cores to avoid interfering with live
//! Byzantine Fault Tolerant consensus execution.

pub mod exporter;
pub mod host_functions;

pub use exporter::{CalibrationExporter, CalibratorConfig, MetricsState};
pub use host_functions::{
    benchmark_all_host_functions, BenchmarkResult, FeeConfiguration, HardwareRiskLevel,
    HostFunctionBenchmark, HostFunctionCalibrator, HostFunctionType,
};
