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

//! Stellar Core XDR ledger ingestion subsystem.
//!
//! This module implements the **Stellar Core Ingestion Ring Buffer & Stream
//! Manager** described in issue #75.  It decouples the reception of raw XDR
//! ledger frames from their downstream processing, preventing ingestion
//! back-pressure from stalling the operator reconciliation loop.
//!
//! # Module layout
//!
//! | Sub-module     | Responsibility                                           |
//! |----------------|----------------------------------------------------------|
//! | [`buffer`]     | Bounded ring buffer; configurable backpressure strategy  |
//! | [`worker`]     | Producer / consumer worker pool; per-worker statistics   |
//!
//! # Quick-start
//!
//! ```rust,no_run
//! use controller::ingestion::{StreamManager, StreamConfig};
//! use controller::ingestion::buffer::{BackpressureStrategy, LedgerFrame};
//! use controller::ingestion::worker::LedgerProcessor;
//!
//! struct MyProcessor;
//!
//! #[async_trait::async_trait]
//! impl LedgerProcessor for MyProcessor {
//!     async fn process(&self, frame: LedgerFrame) -> anyhow::Result<()> {
//!         tracing::info!(sequence = frame.sequence, "received ledger");
//!         Ok(())
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let config = StreamConfig {
//!         buffer_capacity: 4096,
//!         worker_concurrency: 8,
//!         backpressure: BackpressureStrategy::Block,
//!     };
//!     let (manager, producer) = StreamManager::start(config, MyProcessor);
//!
//!     // Feed frames from Stellar Core …
//!     producer.push(LedgerFrame { sequence: 1, xdr_bytes: vec![] }).await;
//!
//!     // Graceful shutdown
//!     producer.close().await;
//!     let stats = manager.join().await;
//!     println!("processed: {}", stats.total_processed());
//!     Ok(())
//! }
//! ```

pub mod buffer;
pub mod worker;

use tracing::info;

use crate::ingestion::buffer::BackpressureStrategy;
use crate::ingestion::worker::{LedgerProcessor, PoolStats, ProducerHandle, WorkerPool};

// ---------------------------------------------------------------------------
// StreamConfig
// ---------------------------------------------------------------------------

/// Configuration for the ingestion stream manager.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// Maximum number of [`LedgerFrame`](buffer::LedgerFrame)s held in the
    /// ring buffer at any one time.  Frames beyond this limit are handled
    /// according to the [`backpressure`](Self::backpressure) strategy.
    ///
    /// Default: `4096`.
    pub buffer_capacity: usize,

    /// Number of parallel consumer tasks draining the ring buffer.
    ///
    /// Rule of thumb: set to the number of available vCPUs for CPU-bound
    /// processors, or to 2×vCPUs for I/O-bound processors.
    ///
    /// Default: `4`.
    pub worker_concurrency: usize,

    /// How the buffer handles overflow when it is full.
    ///
    /// Default: [`BackpressureStrategy::Block`].
    pub backpressure: BackpressureStrategy,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            buffer_capacity: 4096,
            worker_concurrency: 4,
            backpressure: BackpressureStrategy::Block,
        }
    }
}

// ---------------------------------------------------------------------------
// StreamManager
// ---------------------------------------------------------------------------

/// High-level coordinator for the ingestion ring buffer and worker pool.
///
/// [`StreamManager`] is the primary entry point for the ingestion subsystem.
/// It owns the [`WorkerPool`] and provides a clean lifecycle API:
/// [`StreamManager::start`] → push frames via [`ProducerHandle`] → call
/// [`StreamManager::join`] to await graceful shutdown.
pub struct StreamManager {
    pool: WorkerPool,
}

impl StreamManager {
    /// Start the stream manager with the given configuration and processor.
    ///
    /// Returns:
    /// - a [`StreamManager`] that owns the pool lifecycle, and
    /// - a [`ProducerHandle`] that callers use to push ledger frames.
    pub fn start<P: LedgerProcessor>(config: StreamConfig, processor: P) -> (Self, ProducerHandle) {
        info!(
            buffer_capacity = config.buffer_capacity,
            worker_concurrency = config.worker_concurrency,
            backpressure = ?config.backpressure,
            "StreamManager: starting ingestion subsystem"
        );

        let pool = WorkerPool::builder()
            .capacity(config.buffer_capacity)
            .concurrency(config.worker_concurrency)
            .strategy(config.backpressure)
            .build(processor);

        let producer = pool.producer();
        (Self { pool }, producer)
    }

    /// Wait for all consumer workers to drain the buffer and return aggregate
    /// statistics.  This call blocks until the [`ProducerHandle`] (or any
    /// clone thereof) has been closed via [`ProducerHandle::close`].
    pub async fn join(self) -> PoolStats {
        let stats = self.pool.join().await;
        info!(
            total_processed = stats.total_processed(),
            total_errors = stats.total_errors(),
            avg_latency_us = stats.avg_latency_us(),
            "StreamManager: all workers finished"
        );
        stats
    }

    /// Non-blocking snapshot of current worker statistics.
    pub fn stats(&self) -> PoolStats {
        self.pool.stats_snapshot()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingestion::buffer::{BackpressureStrategy, LedgerFrame};
    use crate::ingestion::worker::LedgerProcessor;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// Processor that counts received frames.
    struct CountingProcessor(Arc<Mutex<u32>>);

    #[async_trait::async_trait]
    impl LedgerProcessor for CountingProcessor {
        async fn process(&self, _frame: LedgerFrame) -> anyhow::Result<()> {
            *self.0.lock().await += 1;
            Ok(())
        }
    }

    fn make_frame(seq: u32) -> LedgerFrame {
        LedgerFrame {
            sequence: seq,
            xdr_bytes: vec![seq as u8; 64],
        }
    }

    // ── Smoke test ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stream_manager_end_to_end() {
        let counter = Arc::new(Mutex::new(0u32));
        let proc = CountingProcessor(Arc::clone(&counter));

        let config = StreamConfig {
            buffer_capacity: 256,
            worker_concurrency: 2,
            backpressure: BackpressureStrategy::Block,
        };

        let (manager, producer) = StreamManager::start(config, proc);

        const N: u32 = 200;
        for i in 0..N {
            producer.push(make_frame(i)).await;
        }
        producer.close().await;

        let stats = manager.join().await;
        assert_eq!(stats.total_processed(), N as u64);
        assert_eq!(stats.total_errors(), 0);
        assert_eq!(*counter.lock().await, N);
    }

    // ── Default config ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_default_stream_config() {
        let cfg = StreamConfig::default();
        assert_eq!(cfg.buffer_capacity, 4096);
        assert_eq!(cfg.worker_concurrency, 4);
        assert_eq!(cfg.backpressure, BackpressureStrategy::Block);
    }

    // ── Stats snapshot ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stats_snapshot_non_zero_after_processing() {
        let counter = Arc::new(Mutex::new(0u32));
        let proc = CountingProcessor(Arc::clone(&counter));

        let config = StreamConfig {
            buffer_capacity: 512,
            worker_concurrency: 2,
            backpressure: BackpressureStrategy::Block,
        };

        let (manager, producer) = StreamManager::start(config, proc);

        for i in 0..100 {
            producer.push(make_frame(i)).await;
        }
        // Let workers make some progress.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let snap = manager.stats();
        assert!(snap.total_processed() > 0, "should have processed some frames");

        producer.close().await;
        manager.join().await;
    }

    // ── DropOldest overflow ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_drop_oldest_overflow_does_not_stall() {
        let counter = Arc::new(Mutex::new(0u32));
        let proc = CountingProcessor(Arc::clone(&counter));

        let config = StreamConfig {
            buffer_capacity: 8,
            worker_concurrency: 1,
            backpressure: BackpressureStrategy::DropOldest,
        };

        let (manager, producer) = StreamManager::start(config, proc);

        // Push significantly more frames than the buffer can hold.
        for i in 0..500 {
            producer.push(make_frame(i)).await;
        }
        producer.close().await;

        let stats = manager.join().await;
        // Cannot have processed more than what was pushed.
        assert!(stats.total_processed() <= 500);
        // The subsystem must not have stalled; it completes.
    }

    // ── Benchmark-style throughput test (10 000 frames/minute ≈ 167/s) ──────

    /// Validates that the stream manager can sustain at least 10 000 frames/min
    /// (≈ 167 frames/second) as required by issue #75.
    #[tokio::test]
    async fn test_throughput_10k_ledgers_per_minute() {
        use std::time::Instant;

        let counter = Arc::new(Mutex::new(0u32));
        let proc = CountingProcessor(Arc::clone(&counter));

        let config = StreamConfig {
            buffer_capacity: 4096,
            worker_concurrency: 4,
            backpressure: BackpressureStrategy::Block,
        };

        let (manager, producer) = StreamManager::start(config, proc);

        const TOTAL: u32 = 10_000;
        let t0 = Instant::now();

        for i in 0..TOTAL {
            producer.push(make_frame(i)).await;
        }
        producer.close().await;

        let stats = manager.join().await;
        let elapsed = t0.elapsed();

        assert_eq!(stats.total_processed(), TOTAL as u64, "all frames processed");

        // 10 000 frames in under 60 s satisfies the 10 000 frames/min requirement.
        assert!(
            elapsed.as_secs() < 60,
            "throughput requirement not met: elapsed {:?} for {} frames",
            elapsed,
            TOTAL
        );

        println!(
            "[throughput] {} frames in {:?} ({:.0} frames/s, avg latency {} µs)",
            TOTAL,
            elapsed,
            TOTAL as f64 / elapsed.as_secs_f64(),
            stats.avg_latency_us(),
        );
    }
}
