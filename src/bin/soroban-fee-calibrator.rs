//! Binary sidecar for Soroban Host Function Benchmarking and Multi-dimensional Fee Calibration.
//!
//! Benchmarks Soroban host functions on isolated CPU cores, aligns physical hardware capabilities
//! with multi-dimensional fees, and exports Prometheus metrics on port 9102.

use clap::Parser;
use std::path::PathBuf;
use stellar_telemetry::calibration::{
    CalibrationExporter, CalibratorConfig, FeeConfiguration,
};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

#[derive(Parser, Debug)]
#[command(
    name = "soroban-fee-calibrator",
    about = "Soroban host function execution time benchmarker and fee calibrator sidecar"
)]
struct Args {
    /// Bind address for Prometheus /metrics endpoint
    #[arg(long, default_value = "0.0.0.0:9102", env = "CALIBRATOR_LISTEN_ADDR")]
    listen_addr: String,

    /// Calibration sweep interval in seconds
    #[arg(long, default_value_t = 15, env = "CALIBRATION_INTERVAL_SECS")]
    interval_secs: u64,

    /// Iterations per host function benchmark
    #[arg(long, default_value_t = 1000, env = "BENCHMARK_ITERATIONS")]
    iterations: u64,

    /// Isolated CPU core ID for benchmarking (isolated from consensus threads)
    #[arg(long, default_value_t = 1, env = "ISOLATED_CORE_ID")]
    isolated_core: usize,

    /// Path to stellar-core configuration or YAML fee config
    #[arg(long, env = "STELLAR_CORE_CONFIG_PATH")]
    config_path: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .json()
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);

    let args = Args::parse();

    info!(
        listen = %args.listen_addr,
        interval_secs = args.interval_secs,
        iterations = args.iterations,
        isolated_core = args.isolated_core,
        "Initializing Soroban Host Function Fee Calibrator sidecar"
    );

    let config = CalibratorConfig {
        listen_addr: args.listen_addr,
        interval_secs: args.interval_secs,
        benchmark_iterations: args.iterations,
        isolated_core_id: Some(args.isolated_core),
        fee_config: FeeConfiguration::default(),
    };

    let exporter = CalibrationExporter::new(config);
    exporter.run().await
}
