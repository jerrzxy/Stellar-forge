//! Core Soroban Host Function benchmarking and hardware calibration.
//!
//! Provides synthetic benchmarks simulating physical CPU execution times of Soroban
//! host functions including:
//! - Cryptographic hashing (SHA-256, Keccak-256)
//! - Elliptic curve / signature operations (Ed25519, Secp256k1)
//! - Ledger reads & writes (simulating ledger KV lookups / tree traversals)
//! - Wasm VM invocation / memory allocation overhead
//!
//! Benchmarking supports thread pinning / CPU core isolation to prevent interference
//! with live Byzantine Fault Tolerant (BFT) consensus threads.

use std::collections::HashMap;
use std::fmt;
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Core Soroban host function categories subject to multi-dimensional fee metering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostFunctionType {
    /// SHA-256 hashing computation.
    CryptoSha256,
    /// Keccak-256 hashing computation.
    CryptoKeccak256,
    /// Ed25519 signature verification.
    CryptoEd25519Verify,
    /// Ledger entry read from state storage.
    LedgerEntryRead,
    /// Ledger entry write / footprint update.
    LedgerEntryWrite,
    /// Vector / Map host value allocation & serialization.
    HostValueAllocation,
    /// Wasm VM byte invocation overhead.
    WasmVmInvocation,
}

impl HostFunctionType {
    /// Return all variants for benchmarking.
    pub fn all() -> &'static [HostFunctionType] {
        &[
            HostFunctionType::CryptoSha256,
            HostFunctionType::CryptoKeccak256,
            HostFunctionType::CryptoEd25519Verify,
            HostFunctionType::LedgerEntryRead,
            HostFunctionType::LedgerEntryWrite,
            HostFunctionType::HostValueAllocation,
            HostFunctionType::WasmVmInvocation,
        ]
    }

    /// String identifier for metrics and logs.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CryptoSha256 => "crypto_sha256",
            Self::CryptoKeccak256 => "crypto_keccak256",
            Self::CryptoEd25519Verify => "crypto_ed25519_verify",
            Self::LedgerEntryRead => "ledger_entry_read",
            Self::LedgerEntryWrite => "ledger_entry_write",
            Self::HostValueAllocation => "host_value_allocation",
            Self::WasmVmInvocation => "wasm_vm_invocation",
        }
    }

    /// Expected standard baseline execution time on reference server hardware (nanoseconds).
    /// Used to calculate performance ratio and underperformance index.
    pub fn reference_baseline_ns(&self) -> f64 {
        match self {
            Self::CryptoSha256 => 2_500.0,         // ~2.5 µs for standard payload
            Self::CryptoKeccak256 => 4_000.0,       // ~4.0 µs
            Self::CryptoEd25519Verify => 55_000.0,  // ~55 µs
            Self::LedgerEntryRead => 12_000.0,      // ~12 µs
            Self::LedgerEntryWrite => 25_000.0,     // ~25 µs
            Self::HostValueAllocation => 1_200.0,   // ~1.2 µs
            Self::WasmVmInvocation => 8_000.0,      // ~8.0 µs
        }
    }
}

impl fmt::Display for HostFunctionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Stellar Core fee configuration mapped to Soroban multi-dimensional fee settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeConfiguration {
    /// Fee per 10,000 instructions in stroops (1 XLM = 10,000,000 stroops).
    pub fee_per_instruction_stroops: f64,
    /// Base fee per ledger read operation.
    pub fee_per_read_entry_stroops: f64,
    /// Base fee per ledger write operation.
    pub fee_per_write_entry_stroops: f64,
    /// Maximum allowed instructions per transaction.
    pub tx_max_instructions: u64,
    /// Target ledger close time in seconds (standard 5.0 s).
    pub target_ledger_close_duration_sec: f64,
    /// Max CPU budget allocated to Soroban execution per ledger (in seconds).
    pub max_cpu_budget_per_ledger_sec: f64,
}

impl Default for FeeConfiguration {
    fn default() -> Self {
        Self {
            fee_per_instruction_stroops: 25.0,
            fee_per_read_entry_stroops: 100.0,
            fee_per_write_entry_stroops: 500.0,
            tx_max_instructions: 100_000_000,
            target_ledger_close_duration_sec: 5.0,
            max_cpu_budget_per_ledger_sec: 1.0,
        }
    }
}

/// Hardware risk level assessment based on calibration ratios.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HardwareRiskLevel {
    /// Hardware operates well within baseline margins (< 1.25x reference duration).
    Nominal = 0,
    /// Hardware shows moderate latency compared to reference baseline (1.25x - 2.0x).
    Elevated = 1,
    /// Hardware is constrained or throttled, high risk of validator consensus CPU exhaustion (> 2.0x).
    HighRisk = 2,
}

impl HardwareRiskLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Nominal => "NOMINAL",
            Self::Elevated => "ELEVATED",
            Self::HighRisk => "HIGH_RISK",
        }
    }

    pub fn code(&self) -> u32 {
        *self as u32
    }
}

/// Individual benchmark outcome for a host function.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkResult {
    /// Host function type.
    pub function_type: HostFunctionType,
    /// Measured average execution duration per call in nanoseconds.
    pub execution_duration_ns: f64,
    /// Measured throughput in operations per second.
    pub ops_per_sec: f64,
    /// Ratio of measured duration to reference baseline duration (> 1.0 means slower than reference).
    pub performance_ratio: f64,
    /// Number of iterations executed.
    pub iterations: u64,
    /// Timestamp of benchmark run (epoch seconds).
    pub timestamp_sec: u64,
    /// Calibrated local fee multiplier to align physical hardware with network fee model.
    pub calibrated_fee_multiplier: f64,
}

/// Full calibration snapshot across all core host functions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibrationSnapshot {
    /// Individual results per function.
    pub results: HashMap<HostFunctionType, BenchmarkResult>,
    /// Aggregate hardware underperformance factor (average of performance ratios).
    pub composite_performance_ratio: f64,
    /// Hardware risk level for consensus CPU exhaustion.
    pub hardware_risk_level: HardwareRiskLevel,
    /// Suggested operator local gas metering discount / surcharge factor.
    pub recommended_gas_metering_factor: f64,
    /// Effective CPU core affinity / isolation mask utilized.
    pub isolated_core_id: Option<usize>,
    /// Timestamp of the snapshot.
    pub timestamp_sec: u64,
}

/// Configurable benchmark executor for host functions.
#[derive(Debug, Clone)]
pub struct HostFunctionBenchmark {
    /// Number of iterations per benchmark run.
    pub iterations: u64,
    /// Warmup iterations.
    pub warmup_iterations: u64,
    /// Optional CPU core to bind to during benchmarking.
    pub isolated_core: Option<usize>,
}

impl Default for HostFunctionBenchmark {
    fn default() -> Self {
        Self {
            iterations: 1_000,
            warmup_iterations: 100,
            isolated_core: None,
        }
    }
}

impl HostFunctionBenchmark {
    /// Create a new benchmark configuration.
    pub fn new(iterations: u64, isolated_core: Option<usize>) -> Self {
        Self {
            iterations: iterations.max(10),
            warmup_iterations: (iterations / 10).max(5),
            isolated_core,
        }
    }

    /// Attempt to pin current thread to `isolated_core` if configured and supported.
    pub fn apply_thread_affinity(&self) {
        if let Some(core_id) = self.isolated_core {
            #[cfg(target_os = "linux")]
            {
                unsafe {
                    let mut set: libc::cpu_set_t = std::mem::zeroed();
                    libc::CPU_ZERO(&mut set);
                    libc::CPU_SET(core_id, &mut set);
                    let res = libc::sched_setaffinity(
                        0,
                        std::mem::size_of::<libc::cpu_set_t>(),
                        &set as *const libc::cpu_set_t,
                    );
                    if res == 0 {
                        debug!("Pinned benchmark worker thread to isolated CPU core {}", core_id);
                    } else {
                        warn!("sched_setaffinity to core {} failed: errno {}", core_id, res);
                    }
                }
            }
            #[cfg(target_os = "windows")]
            {
                use std::mem::size_of;
                #[allow(non_snake_case)]
                extern "system" {
                    fn GetCurrentThread() -> *mut std::ffi::c_void;
                    fn SetThreadAffinityMask(
                        hThread: *mut std::ffi::c_void,
                        dwThreadAffinityMask: usize,
                    ) -> usize;
                }
                if core_id < (size_of::<usize>() * 8) {
                    let mask = 1usize << core_id;
                    unsafe {
                        let prev = SetThreadAffinityMask(GetCurrentThread(), mask);
                        if prev != 0 {
                            debug!("Set Windows thread affinity mask to 0x{:X} (core {})", mask, core_id);
                        }
                    }
                }
            }
            #[cfg(not(any(target_os = "linux", target_os = "windows")))]
            {
                debug!("CPU isolation pinning is a no-op on this OS platform (core {})", core_id);
            }
        }
    }

    /// Run benchmark for a specific host function type.
    pub fn benchmark_function(&self, func: HostFunctionType) -> BenchmarkResult {
        self.apply_thread_affinity();

        // 1. Warmup
        for _ in 0..self.warmup_iterations {
            execute_synthetic_host_function(func);
        }

        // 2. Timed run
        let start = Instant::now();
        for _ in 0..self.iterations {
            execute_synthetic_host_function(func);
        }
        let elapsed = start.elapsed();

        let total_ns = elapsed.as_nanos() as f64;
        let avg_duration_ns = total_ns / (self.iterations as f64);
        let ops_per_sec = if avg_duration_ns > 0.0 {
            1_000_000_000.0 / avg_duration_ns
        } else {
            0.0
        };

        let ref_ns = func.reference_baseline_ns();
        let performance_ratio = avg_duration_ns / ref_ns;

        // Calibrated fee multiplier: if the machine is 2x slower, calibrate
        // local gas metering by a factor of 2x to reflect true compute consumption.
        let calibrated_fee_multiplier = performance_ratio.max(0.1);

        let now_sec = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        BenchmarkResult {
            function_type: func,
            execution_duration_ns: avg_duration_ns,
            ops_per_sec,
            performance_ratio,
            iterations: self.iterations,
            timestamp_sec: now_sec,
            calibrated_fee_multiplier,
        }
    }
}

/// Execute synthetic workload representing the physical computation of Soroban host functions.
#[inline(never)]
pub fn execute_synthetic_host_function(func: HostFunctionType) {
    match func {
        HostFunctionType::CryptoSha256 => {
            // SHA-256 simulation: 64 rounds of message expansion & bitwise operations
            let mut state: [u32; 8] = [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
                0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
            ];
            let data = [0x5au8; 64];
            for chunk in data.chunks_exact(4) {
                let w = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                state[0] = state[0].wrapping_add(w).rotate_right(7) ^ state[1].rotate_right(18);
                state[1] = state[1].wrapping_add(state[2]);
            }
            black_box(state);
        }
        HostFunctionType::CryptoKeccak256 => {
            // Keccak round simulation: 5x5 lane array theta, rho, pi, chi, iota operations
            let mut state = [0u64; 25];
            for i in 0..25 {
                state[i] ^= (i as u64).wrapping_mul(0x9e3779b97f4a7c15);
            }
            for round in 0..12 {
                for i in 0..25 {
                    state[i] = state[i].rotate_left(((round + i) % 64) as u32) ^ state[(i + 1) % 25];
                }
            }
            black_box(state);
        }
        HostFunctionType::CryptoEd25519Verify => {
            // Simulates Curve25519 point multiplication / scalar arithmetic
            let mut scalar: [u64; 4] = [0xdeadbeef, 0xcafebabe, 0x12345678, 0x87654321];
            let mut point: [u64; 4] = [1, 2, 3, 4];
            for _ in 0..30 {
                for i in 0..4 {
                    point[i] = point[i].wrapping_mul(scalar[i]).wrapping_add(0xed25519);
                    scalar[i] = scalar[i].rotate_right(13) ^ point[i];
                }
            }
            black_box((scalar, point));
        }
        HostFunctionType::LedgerEntryRead => {
            // Simulates indexed B-tree / Radix tree binary search over state storage
            let mut table = [0u32; 128];
            for i in 0..128 {
                table[i] = (i * 37) as u32;
            }
            let key = 113u32;
            let mut found = 0u32;
            let mut low = 0;
            let mut high = 127;
            while low <= high {
                let mid = (low + high) / 2;
                if table[mid] == key {
                    found = table[mid];
                    break;
                } else if table[mid] < key {
                    low = mid + 1;
                } else {
                    if mid == 0 { break; }
                    high = mid - 1;
                }
            }
            black_box(found);
        }
        HostFunctionType::LedgerEntryWrite => {
            // Simulates key hashing, state modification, and dirty page serialization
            let mut buffer = [0u8; 256];
            for i in 0..256 {
                buffer[i] = ((i ^ 0xaa) as u8).wrapping_add((i % 7) as u8);
            }
            let mut checksum: u32 = 0;
            for byte in &buffer {
                checksum = checksum.wrapping_mul(31).wrapping_add(*byte as u32);
            }
            black_box((buffer, checksum));
        }
        HostFunctionType::HostValueAllocation => {
            // Simulates Soroban Val/Object vector representation unpacking
            let mut vec_repr = [0u64; 16];
            for i in 0..16 {
                vec_repr[i] = (i as u64) << 32 | (0xbeef);
            }
            black_box(vec_repr);
        }
        HostFunctionType::WasmVmInvocation => {
            // Simulates Wasm guest-to-host call frame boundary & stack validation
            let mut stack = [0i64; 32];
            for sp in 0..32 {
                stack[sp] = (sp as i64) * -17;
            }
            let acc = stack.iter().fold(0i64, |acc, x| acc.wrapping_add(*x));
            black_box(acc);
        }
    }
}

/// Continuous Calibrator Engine.
pub struct HostFunctionCalibrator {
    benchmark: HostFunctionBenchmark,
    fee_config: FeeConfiguration,
    running: Arc<AtomicBool>,
}

impl HostFunctionCalibrator {
    /// Create a new calibrator instance with fee configuration.
    pub fn new(benchmark: HostFunctionBenchmark, fee_config: FeeConfiguration) -> Self {
        Self {
            benchmark,
            fee_config,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Access current fee configuration.
    pub fn fee_config(&self) -> &FeeConfiguration {
        &self.fee_config
    }

    /// Update fee configuration dynamically (e.g. from live stellar-core config).
    pub fn set_fee_config(&mut self, config: FeeConfiguration) {
        self.fee_config = config;
    }

    /// Execute a full calibration sweep across all host functions.
    pub fn calibrate_all(&self) -> CalibrationSnapshot {
        let mut results = HashMap::new();
        let mut total_ratio = 0.0;
        let count = HostFunctionType::all().len() as f64;

        for &func in HostFunctionType::all() {
            let res = self.benchmark.benchmark_function(func);
            total_ratio += res.performance_ratio;
            results.insert(func, res);
        }

        let composite_ratio = if count > 0.0 {
            total_ratio / count
        } else {
            1.0
        };

        // Determine risk level based on composite performance ratio
        let hardware_risk_level = if composite_ratio > 2.0 {
            HardwareRiskLevel::HighRisk
        } else if composite_ratio > 1.25 {
            HardwareRiskLevel::Elevated
        } else {
            HardwareRiskLevel::Nominal
        };

        // Calibrated metering factor: maps hardware underperformance directly to
        // fee logic so operators can throttle or price transactions safely.
        let recommended_gas_metering_factor = composite_ratio;

        let now_sec = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        CalibrationSnapshot {
            results,
            composite_performance_ratio: composite_ratio,
            hardware_risk_level,
            recommended_gas_metering_factor,
            isolated_core_id: self.benchmark.isolated_core,
            timestamp_sec: now_sec,
        }
    }

    /// Start a continuous calibration background loop with given interval.
    pub fn start_continuous<F>(
        &self,
        interval: Duration,
        mut callback: F,
    ) -> Arc<AtomicBool>
    where
        F: FnMut(CalibrationSnapshot) + Send + 'static,
    {
        self.running.store(true, Ordering::SeqCst);
        let running_clone = Arc::clone(&self.running);
        let bench = self.benchmark.clone();
        let fee_cfg = self.fee_config.clone();

        tokio::spawn(async move {
            let calibrator = HostFunctionCalibrator::new(bench, fee_cfg);
            info!("Continuous host function calibration sidecar loop started (interval: {:?})", interval);

            while running_clone.load(Ordering::Relaxed) {
                let snapshot = calibrator.calibrate_all();
                callback(snapshot);
                tokio::time::sleep(interval).await;
            }

            info!("Continuous host function calibration loop terminated");
        });

        Arc::clone(&self.running)
    }

    /// Stop continuous calibration loop.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

/// Convenience function to benchmark all host functions immediately.
pub fn benchmark_all_host_functions(iterations: u64, isolated_core: Option<usize>) -> CalibrationSnapshot {
    let bench = HostFunctionBenchmark::new(iterations, isolated_core);
    let calibrator = HostFunctionCalibrator::new(bench, FeeConfiguration::default());
    calibrator.calibrate_all()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_host_functions_defined() {
        let all = HostFunctionType::all();
        assert_eq!(all.len(), 7);
        for f in all {
            assert!(f.reference_baseline_ns() > 0.0);
            assert!(!f.as_str().is_empty());
        }
    }

    #[test]
    fn test_single_benchmark_run() {
        let bench = HostFunctionBenchmark::new(100, None);
        let res = bench.benchmark_function(HostFunctionType::CryptoSha256);
        assert_eq!(res.function_type, HostFunctionType::CryptoSha256);
        assert!(res.execution_duration_ns > 0.0);
        assert!(res.ops_per_sec > 0.0);
        assert!(res.performance_ratio > 0.0);
        assert_eq!(res.iterations, 100);
    }

    #[test]
    fn test_calibrator_sweep() {
        let bench = HostFunctionBenchmark::new(50, None);
        let calibrator = HostFunctionCalibrator::new(bench, FeeConfiguration::default());
        let snapshot = calibrator.calibrate_all();

        assert_eq!(snapshot.results.len(), 7);
        assert!(snapshot.composite_performance_ratio > 0.0);
        assert!(matches!(
            snapshot.hardware_risk_level,
            HardwareRiskLevel::Nominal | HardwareRiskLevel::Elevated | HardwareRiskLevel::HighRisk
        ));
    }

    #[test]
    fn test_risk_level_mapping() {
        assert_eq!(HardwareRiskLevel::Nominal.code(), 0);
        assert_eq!(HardwareRiskLevel::Elevated.code(), 1);
        assert_eq!(HardwareRiskLevel::HighRisk.code(), 2);
        assert_eq!(HardwareRiskLevel::HighRisk.as_str(), "HIGH_RISK");
    }

    #[test]
    fn test_thread_affinity_execution() {
        let bench = HostFunctionBenchmark::new(20, Some(0));
        let res = bench.benchmark_function(HostFunctionType::LedgerEntryRead);
        assert!(res.execution_duration_ns > 0.0);
    }

    #[test]
    fn test_synthetic_execution_no_panic() {
        for &func in HostFunctionType::all() {
            execute_synthetic_host_function(func);
        }
    }
}
