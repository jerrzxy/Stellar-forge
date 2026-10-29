//! Prometheus metrics exporter for Soroban host function calibration and fee telemetry.
//!
//! Exposes Prometheus gauge and histogram metrics serving:
//! - Measured execution durations of host functions (`soroban_host_function_execution_ns`)
//! - Performance underperformance ratio vs network reference baseline (`soroban_host_function_performance_ratio`)
//! - Calibrated local gas metering multiplier (`soroban_calibrated_gas_metering_multiplier`)
//! - Hardware CPU exhaustion risk level (`soroban_hardware_cpu_exhaustion_risk_level`)
//! - Multi-dimensional fee configuration metrics mapped from stellar-core configuration

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use prometheus_client::encoding::text::encode;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::{error, info};

use super::host_functions::{
    CalibrationSnapshot, FeeConfiguration, HardwareRiskLevel, HostFunctionBenchmark,
    HostFunctionCalibrator, HostFunctionType,
};

/// Metric label set identifying the specific host function.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct HostFunctionLabel {
    pub function_name: String,
}

/// Thread-safe Prometheus calibration metrics state.
pub struct MetricsState {
    pub registry: Registry,
    pub execution_duration_ns: Family<HostFunctionLabel, Gauge<f64, Arc<std::sync::atomic::AtomicU64>>>,
    pub performance_ratio: Family<HostFunctionLabel, Gauge<f64, Arc<std::sync::atomic::AtomicU64>>>,
    pub ops_per_sec: Family<HostFunctionLabel, Gauge<f64, Arc<std::sync::atomic::AtomicU64>>>,
    pub fee_multiplier: Family<HostFunctionLabel, Gauge<f64, Arc<std::sync::atomic::AtomicU64>>>,
    pub composite_performance_ratio: Gauge<f64, Arc<std::sync::atomic::AtomicU64>>,
    pub cpu_exhaustion_risk: Gauge<f64, Arc<std::sync::atomic::AtomicU64>>,
    pub recommended_gas_metering_factor: Gauge<f64, Arc<std::sync::atomic::AtomicU64>>,
    pub isolated_core_id: Gauge<f64, Arc<std::sync::atomic::AtomicU64>>,
    // Stellar Core fee mapping metrics
    pub fee_per_instruction_stroops: Gauge<f64, Arc<std::sync::atomic::AtomicU64>>,
    pub fee_per_read_entry_stroops: Gauge<f64, Arc<std::sync::atomic::AtomicU64>>,
    pub fee_per_write_entry_stroops: Gauge<f64, Arc<std::sync::atomic::AtomicU64>>,
    pub tx_max_instructions: Gauge<f64, Arc<std::sync::atomic::AtomicU64>>,
    pub target_ledger_close_duration_sec: Gauge<f64, Arc<std::sync::atomic::AtomicU64>>,
    pub max_cpu_budget_per_ledger_sec: Gauge<f64, Arc<std::sync::atomic::AtomicU64>>,
}

// Custom atomic f64 gauge wrapper for prometheus-client compatibility
fn create_atomic_f64_gauge() -> Gauge<f64, Arc<std::sync::atomic::AtomicU64>> {
    Gauge::default()
}

impl MetricsState {
    pub fn new() -> Self {
        let mut registry = Registry::default();

        let execution_duration_ns: Family<HostFunctionLabel, Gauge<f64, Arc<std::sync::atomic::AtomicU64>>> =
            Family::new_with_constructor(create_atomic_f64_gauge);

        let performance_ratio: Family<HostFunctionLabel, Gauge<f64, Arc<std::sync::atomic::AtomicU64>>> =
            Family::new_with_constructor(create_atomic_f64_gauge);

        let ops_per_sec: Family<HostFunctionLabel, Gauge<f64, Arc<std::sync::atomic::AtomicU64>>> =
            Family::new_with_constructor(create_atomic_f64_gauge);

        let fee_multiplier: Family<HostFunctionLabel, Gauge<f64, Arc<std::sync::atomic::AtomicU64>>> =
            Family::new_with_constructor(create_atomic_f64_gauge);

        let composite_performance_ratio = create_atomic_f64_gauge();
        let cpu_exhaustion_risk = create_atomic_f64_gauge();
        let recommended_gas_metering_factor = create_atomic_f64_gauge();
        let isolated_core_id = create_atomic_f64_gauge();

        let fee_per_instruction_stroops = create_atomic_f64_gauge();
        let fee_per_read_entry_stroops = create_atomic_f64_gauge();
        let fee_per_write_entry_stroops = create_atomic_f64_gauge();
        let tx_max_instructions = create_atomic_f64_gauge();
        let target_ledger_close_duration_sec = create_atomic_f64_gauge();
        let max_cpu_budget_per_ledger_sec = create_atomic_f64_gauge();

        registry.register(
            "soroban_host_function_execution_ns",
            "Benchmarked execution duration in nanoseconds per host function call",
            execution_duration_ns.clone(),
        );
        registry.register(
            "soroban_host_function_performance_ratio",
            "Ratio of observed duration to baseline network reference (> 1.0 indicates slower execution)",
            performance_ratio.clone(),
        );
        registry.register(
            "soroban_host_function_ops_per_sec",
            "Calculated throughput capacity in operations per second for the host function",
            ops_per_sec.clone(),
        );
        registry.register(
            "soroban_calibrated_fee_multiplier",
            "Recommended local fee multiplier to align physical hardware capacity with network baseline",
            fee_multiplier.clone(),
        );
        registry.register(
            "soroban_hardware_composite_performance_ratio",
            "Composite hardware performance ratio across all host functions (> 2.0 indicates high risk)",
            composite_performance_ratio.clone(),
        );
        registry.register(
            "soroban_hardware_cpu_exhaustion_risk_level",
            "Hardware risk level for consensus CPU exhaustion: 0=NOMINAL, 1=ELEVATED, 2=HIGH_RISK",
            cpu_exhaustion_risk.clone(),
        );
        registry.register(
            "soroban_recommended_gas_metering_factor",
            "Dynamically adjusted gas metering calibration factor for local validator operator",
            recommended_gas_metering_factor.clone(),
        );
        registry.register(
            "soroban_calibrator_isolated_core_id",
            "CPU core ID isolated for calibrator benchmark execution (-1 if unpinned)",
            isolated_core_id.clone(),
        );

        // Multi-dimensional fee config metrics
        registry.register(
            "soroban_fee_config_per_instruction_stroops",
            "Configured Stellar Core fee in stroops per 10k instructions",
            fee_per_instruction_stroops.clone(),
        );
        registry.register(
            "soroban_fee_config_read_entry_stroops",
            "Configured Stellar Core base fee in stroops per ledger read entry",
            fee_per_read_entry_stroops.clone(),
        );
        registry.register(
            "soroban_fee_config_write_entry_stroops",
            "Configured Stellar Core base fee in stroops per ledger write entry",
            fee_per_write_entry_stroops.clone(),
        );
        registry.register(
            "soroban_fee_config_tx_max_instructions",
            "Maximum allowed instructions per transaction from stellar-core config",
            tx_max_instructions.clone(),
        );
        registry.register(
            "soroban_fee_config_target_ledger_close_duration_sec",
            "Target ledger close duration in seconds",
            target_ledger_close_duration_sec.clone(),
        );
        registry.register(
            "soroban_fee_config_max_cpu_budget_per_ledger_sec",
            "Max CPU budget in seconds allocated for Soroban transaction execution per ledger",
            max_cpu_budget_per_ledger_sec.clone(),
        );

        Self {
            registry,
            execution_duration_ns,
            performance_ratio,
            ops_per_sec,
            fee_multiplier,
            composite_performance_ratio,
            cpu_exhaustion_risk,
            recommended_gas_metering_factor,
            isolated_core_id,
            fee_per_instruction_stroops,
            fee_per_read_entry_stroops,
            fee_per_write_entry_stroops,
            tx_max_instructions,
            target_ledger_close_duration_sec,
            max_cpu_budget_per_ledger_sec,
        }
    }

    /// Record a calibration snapshot into Prometheus gauges.
    pub fn update_snapshot(&self, snapshot: &CalibrationSnapshot, fee_config: &FeeConfiguration) {
        for (func_type, res) in &snapshot.results {
            let label = HostFunctionLabel {
                function_name: func_type.as_str().to_string(),
            };

            self.execution_duration_ns
                .get_or_create(&label)
                .set(res.execution_duration_ns);

            self.performance_ratio
                .get_or_create(&label)
                .set(res.performance_ratio);

            self.ops_per_sec
                .get_or_create(&label)
                .set(res.ops_per_sec);

            self.fee_multiplier
                .get_or_create(&label)
                .set(res.calibrated_fee_multiplier);
        }

        self.composite_performance_ratio
            .set(snapshot.composite_performance_ratio);

        self.cpu_exhaustion_risk
            .set(snapshot.hardware_risk_level.code() as f64);

        self.recommended_gas_metering_factor
            .set(snapshot.recommended_gas_metering_factor);

        self.isolated_core_id.set(
            snapshot
                .isolated_core_id
                .map(|c| c as f64)
                .unwrap_or(-1.0),
        );

        // Record fee settings
        self.fee_per_instruction_stroops
            .set(fee_config.fee_per_instruction_stroops);
        self.fee_per_read_entry_stroops
            .set(fee_config.fee_per_read_entry_stroops);
        self.fee_per_write_entry_stroops
            .set(fee_config.fee_per_write_entry_stroops);
        self.tx_max_instructions
            .set(fee_config.tx_max_instructions as f64);
        self.target_ledger_close_duration_sec
            .set(fee_config.target_ledger_close_duration_sec);
        self.max_cpu_budget_per_ledger_sec
            .set(fee_config.max_cpu_budget_per_ledger_sec);
    }

    /// Render metrics to Prometheus text exposition format.
    pub fn encode_metrics(&self) -> String {
        let mut buf = String::new();
        if let Err(e) = encode(&mut buf, &self.registry) {
            error!("Failed to encode calibration metrics: {}", e);
            return format!("# encoding error: {e}\n");
        }
        buf
    }
}

impl Default for MetricsState {
    fn default() -> Self {
        Self::new()
    }
}

/// Sidecar configuration for the calibration exporter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibratorConfig {
    /// Bind address for the calibration HTTP /metrics endpoint.
    pub listen_addr: String,
    /// Calibration interval in seconds.
    pub interval_secs: u64,
    /// Number of iterations per benchmark run.
    pub benchmark_iterations: u64,
    /// Isolated CPU core ID to pin benchmarks to.
    pub isolated_core_id: Option<usize>,
    /// Stellar-core fee configuration settings.
    pub fee_config: FeeConfiguration,
}

impl Default for CalibratorConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:9102".to_string(),
            interval_secs: 15,
            benchmark_iterations: 1_000,
            isolated_core_id: Some(1), // Default to core 1 to avoid consensus on core 0
            fee_config: FeeConfiguration::default(),
        }
    }
}

/// The calibration exporter engine managing the background calibrator and HTTP server.
pub struct CalibrationExporter {
    config: CalibratorConfig,
    metrics: Arc<MetricsState>,
    calibrator: Arc<HostFunctionCalibrator>,
    latest_snapshot: Arc<RwLock<Option<CalibrationSnapshot>>>,
}

impl CalibrationExporter {
    pub fn new(config: CalibratorConfig) -> Self {
        let metrics = Arc::new(MetricsState::new());
        let benchmark = HostFunctionBenchmark::new(
            config.benchmark_iterations,
            config.isolated_core_id,
        );
        let calibrator = Arc::new(HostFunctionCalibrator::new(
            benchmark,
            config.fee_config.clone(),
        ));

        Self {
            config,
            metrics,
            calibrator,
            latest_snapshot: Arc::new(RwLock::new(None)),
        }
    }

    /// Run calibration once and update the metrics state.
    pub fn calibrate_once(&self) -> CalibrationSnapshot {
        let snapshot = self.calibrator.calibrate_all();
        self.metrics.update_snapshot(&snapshot, self.calibrator.fee_config());
        snapshot
    }

    /// Access reference to metrics state.
    pub fn metrics(&self) -> Arc<MetricsState> {
        Arc::clone(&self.metrics)
    }

    /// Start the HTTP server and background continuous calibration.
    pub async fn run(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let addr: SocketAddr = self.config.listen_addr.parse()?;
        let metrics_state = Arc::clone(&self.metrics);
        let latest_snapshot_ref = Arc::clone(&self.latest_snapshot);
        let calibrator_ref = Arc::clone(&self.calibrator);
        let interval = std::time::Duration::from_secs(self.config.interval_secs.max(1));

        // Initial synchronous calibration before serving
        let initial_snapshot = self.calibrator.calibrate_all();
        self.metrics.update_snapshot(&initial_snapshot, self.calibrator.fee_config());
        *self.latest_snapshot.write().await = Some(initial_snapshot);

        // Spawn continuous calibration task
        let metrics_state_bg = Arc::clone(&metrics_state);
        let latest_bg = Arc::clone(&latest_snapshot_ref);
        let fee_config = self.config.fee_config.clone();

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let snapshot = calibrator_ref.calibrate_all();
                metrics_state_bg.update_snapshot(&snapshot, &fee_config);
                *latest_bg.write().await = Some(snapshot);
            }
        });

        // Setup Axum HTTP router
        let app_state = Arc::new(ServerContext {
            metrics: metrics_state,
            latest: latest_snapshot_ref,
        });

        let app = Router::new()
            .route("/metrics", get(metrics_handler))
            .route("/healthz", get(health_handler))
            .route("/calibration/snapshot", get(snapshot_handler))
            .with_state(app_state);

        let listener = TcpListener::bind(addr).await?;
        info!("Soroban Host Function Calibrator exporter listening on http://{}", addr);

        axum::serve(listener, app).await?;
        Ok(())
    }
}

struct ServerContext {
    metrics: Arc<MetricsState>,
    latest: Arc<RwLock<Option<CalibrationSnapshot>>>,
}

async fn metrics_handler(State(ctx): State<Arc<ServerContext>>) -> impl IntoResponse {
    let body = ctx.metrics.encode_metrics();
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        body,
    )
}

async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

async fn snapshot_handler(State(ctx): State<Arc<ServerContext>>) -> impl IntoResponse {
    let guard = ctx.latest.read().await;
    match guard.as_ref() {
        Some(snapshot) => (
            StatusCode::OK,
            [("content-type", "application/json")],
            serde_json::to_string_pretty(snapshot).unwrap_or_default(),
        ),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            [("content-type", "application/json")],
            "{}".to_string(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_state_recording() {
        let metrics = MetricsState::new();
        let bench = HostFunctionBenchmark::new(20, None);
        let calibrator = HostFunctionCalibrator::new(bench, FeeConfiguration::default());
        let snapshot = calibrator.calibrate_all();

        metrics.update_snapshot(&snapshot, calibrator.fee_config());
        let encoded = metrics.encode_metrics();

        assert!(encoded.contains("soroban_host_function_execution_ns"));
        assert!(encoded.contains("soroban_host_function_performance_ratio"));
        assert!(encoded.contains("soroban_calibrated_fee_multiplier"));
        assert!(encoded.contains("soroban_hardware_cpu_exhaustion_risk_level"));
        assert!(encoded.contains("soroban_fee_config_per_instruction_stroops"));
    }

    #[test]
    fn test_calibrator_flags_high_risk_on_constrained_hardware() {
        let metrics = MetricsState::new();
        let mut snapshot = CalibrationSnapshot {
            results: std::collections::HashMap::new(),
            composite_performance_ratio: 3.5, // 3.5x slower (e.g. Raspberry Pi)
            hardware_risk_level: HardwareRiskLevel::HighRisk,
            recommended_gas_metering_factor: 3.5,
            isolated_core_id: Some(2),
            timestamp_sec: 1700000000,
        };

        // Populate results
        for &func in HostFunctionType::all() {
            snapshot.results.insert(
                func,
                super::super::host_functions::BenchmarkResult {
                    function_type: func,
                    execution_duration_ns: func.reference_baseline_ns() * 3.5,
                    ops_per_sec: 1_000_000_000.0 / (func.reference_baseline_ns() * 3.5),
                    performance_ratio: 3.5,
                    iterations: 100,
                    timestamp_sec: 1700000000,
                    calibrated_fee_multiplier: 3.5,
                },
            );
        }

        metrics.update_snapshot(&snapshot, &FeeConfiguration::default());
        let encoded = metrics.encode_metrics();

        assert!(encoded.contains("soroban_hardware_cpu_exhaustion_risk_level 2"));
        assert!(encoded.contains("soroban_hardware_composite_performance_ratio 3.5"));
    }

    #[tokio::test]
    async fn test_exporter_http_endpoints() {
        let config = CalibratorConfig {
            listen_addr: "127.0.0.1:0".to_string(),
            interval_secs: 10,
            benchmark_iterations: 10,
            isolated_core_id: None,
            fee_config: FeeConfiguration::default(),
        };

        let exporter = CalibrationExporter::new(config);
        let snapshot = exporter.calibrate_once();
        assert_eq!(snapshot.results.len(), 7);

        let metrics_text = exporter.metrics().encode_metrics();
        assert!(metrics_text.contains("soroban_host_function_execution_ns"));
    }
}
