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

//! Producer-consumer worker pool for XDR ledger stream processing.
//!
//! # Architecture
//!
//! ```text
//!   Stellar Core
//!       │  XDR frames
//!       ▼
//!  ┌──────────────┐           ┌───────────────────────┐
//!  │  ProducerPool │──push──▶│    LedgerBuffer        │
//!  │  (N tasks)    │          │  (bounded ring buffer) │
//!  └──────────────┘           └──────────┬────────────┘
//!                                         │  pop
//!                               ┌─────────▼──────────┐
//!                               │   ConsumerPool      │
//!                               │   (M tasks)         │
//!                               └─────────────────────┘
//!                                         │  LedgerProcessor
//!                                         ▼
//!                                    application logic
//! ```
//!
//! - [`ProducerPool`] accepts raw frames from the network layer and pushes them
//!   into a shared [`LedgerBuffer`].
//! - [`ConsumerPool`] spawns a configurable number of worker tasks.  Each task
//!   loops over `buffer.pop()`, calls the user-supplied [`LedgerProcessor`]
//!   closure, and records per-worker statistics.
//! - [`WorkerPool`] ties both pools together under a single shutdown handle.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::task::JoinHandle;
use tracing::{debug, error, info, instrument, warn};

use crate::ingestion::buffer::{BackpressureStrategy, LedgerBuffer, LedgerFrame};

// ---------------------------------------------------------------------------
// Public trait: LedgerProcessor
// ---------------------------------------------------------------------------

/// Trait implemented by the application logic that processes a ledger frame.
///
/// The trait is `Send + Sync + 'static` so that it can be shared across Tokio
/// worker tasks.  Processing must be async-friendly; long CPU-bound work should
/// be offloaded to [`tokio::task::spawn_blocking`].
#[async_trait::async_trait]
pub trait LedgerProcessor: Send + Sync + 'static {
    /// Process a single [`LedgerFrame`].
    ///
    /// Returning an error logs the failure and increments the error counter but
    /// does **not** stop the worker; it continues processing the next frame.
    async fn process(&self, frame: LedgerFrame) -> anyhow::Result<()>;
}

// ---------------------------------------------------------------------------
// Worker statistics
// ---------------------------------------------------------------------------

/// Aggregate statistics for a single consumer worker.
#[derive(Debug, Clone, Copy)]
pub struct WorkerStats {
    /// Number of frames successfully processed.
    pub processed: u64,
    /// Number of frames that returned an error from [`LedgerProcessor::process`].
    pub errors: u64,
    /// Total processing time in microseconds (across all frames).
    pub total_processing_us: u64,
}

/// Aggregate statistics for the entire [`ConsumerPool`].
#[derive(Debug, Clone)]
pub struct PoolStats {
    /// Per-worker stats, indexed by worker id (0-based).
    pub workers: Vec<WorkerStats>,
}

impl PoolStats {
    /// Sum of processed frames across all workers.
    pub fn total_processed(&self) -> u64 {
        self.workers.iter().map(|w| w.processed).sum()
    }

    /// Sum of errors across all workers.
    pub fn total_errors(&self) -> u64 {
        self.workers.iter().map(|w| w.errors).sum()
    }

    /// Average processing latency in microseconds, or 0 if nothing was processed.
    pub fn avg_latency_us(&self) -> u64 {
        let total_processed = self.total_processed();
        if total_processed == 0 {
            return 0;
        }
        let total_us: u64 = self.workers.iter().map(|w| w.total_processing_us).sum();
        total_us / total_processed
    }
}

// ---------------------------------------------------------------------------
// Internal per-worker counters (shared via Arc)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct WorkerCounters {
    processed: AtomicU64,
    errors: AtomicU64,
    total_us: AtomicU64,
}

// ---------------------------------------------------------------------------
// ProducerPool
// ---------------------------------------------------------------------------

/// A thin wrapper that lets multiple producer tasks push frames into the shared
/// buffer without needing to hold direct references to it.
///
/// In practice the operator receives XDR frames from a Stellar Core HTTP stream
/// or Kafka topic.  Each ingestion source can hold a clone of the
/// [`ProducerHandle`] (returned by [`ProducerPool::handle`]) and push frames
/// independently.
pub struct ProducerPool {
    buffer: LedgerBuffer,
}

impl ProducerPool {
    /// Construct a producer pool backed by the given buffer.
    pub fn new(buffer: LedgerBuffer) -> Self {
        Self { buffer }
    }

    /// Return a cheap-to-clone handle for pushing frames.
    pub fn handle(&self) -> ProducerHandle {
        ProducerHandle {
            buffer: self.buffer.clone(),
        }
    }
}

/// A clonable handle for a single producer to push frames.
#[derive(Clone)]
pub struct ProducerHandle {
    buffer: LedgerBuffer,
}

impl ProducerHandle {
    /// Push a frame into the buffer.  Behaviour on overflow depends on the
    /// [`BackpressureStrategy`] the buffer was created with.
    pub async fn push(&self, frame: LedgerFrame) -> bool {
        self.buffer.push(frame).await
    }

    /// Signal no more frames will arrive (closes the underlying buffer).
    ///
    /// All consumer workers will drain remaining frames and then stop.
    pub async fn close(&self) {
        self.buffer.close().await;
    }
}

// ---------------------------------------------------------------------------
// ConsumerPool
// ---------------------------------------------------------------------------

/// A pool of `concurrency` async worker tasks that drain a [`LedgerBuffer`].
pub struct ConsumerPool<P: LedgerProcessor> {
    buffer: LedgerBuffer,
    processor: Arc<P>,
    concurrency: usize,
}

impl<P: LedgerProcessor> ConsumerPool<P> {
    /// Create a new consumer pool.
    ///
    /// # Arguments
    ///
    /// * `buffer`      — shared ring buffer to drain.
    /// * `processor`   — application logic called for each frame.
    /// * `concurrency` — number of parallel worker tasks (≥ 1).
    ///
    /// # Panics
    ///
    /// Panics if `concurrency` is zero.
    pub fn new(buffer: LedgerBuffer, processor: P, concurrency: usize) -> Self {
        assert!(concurrency > 0, "concurrency must be >= 1");
        Self {
            buffer,
            processor: Arc::new(processor),
            concurrency,
        }
    }

    /// Spawn `concurrency` worker tasks and return a [`ConsumerHandle`] that
    /// can be used to await their completion and gather statistics.
    pub fn start(self) -> ConsumerHandle {
        let mut handles: Vec<JoinHandle<()>> = Vec::with_capacity(self.concurrency);
        let counters: Vec<Arc<WorkerCounters>> = (0..self.concurrency)
            .map(|_| Arc::new(WorkerCounters::default()))
            .collect();

        for (id, counter) in counters.iter().enumerate() {
            let buf = self.buffer.clone();
            let proc = Arc::clone(&self.processor);
            let cntr = Arc::clone(counter);

            let handle = tokio::spawn(async move {
                run_worker(id, buf, proc, cntr).await;
            });
            handles.push(handle);
        }

        info!(
            concurrency = self.concurrency,
            "ConsumerPool: started worker pool"
        );

        ConsumerHandle {
            handles,
            counters,
        }
    }
}

/// A handle returned by [`ConsumerPool::start`].
///
/// Await [`ConsumerHandle::join`] to wait for all workers to finish (i.e., the
/// buffer is closed and drained).
pub struct ConsumerHandle {
    handles: Vec<JoinHandle<()>>,
    counters: Vec<Arc<WorkerCounters>>,
}

impl ConsumerHandle {
    /// Wait for all workers to finish and return aggregate statistics.
    pub async fn join(self) -> PoolStats {
        for (i, h) in self.handles.into_iter().enumerate() {
            if let Err(e) = h.await {
                error!(worker = i, error = ?e, "ConsumerPool: worker task panicked");
            }
        }
        let workers = self
            .counters
            .iter()
            .map(|c| WorkerStats {
                processed: c.processed.load(Ordering::Relaxed),
                errors: c.errors.load(Ordering::Relaxed),
                total_processing_us: c.total_us.load(Ordering::Relaxed),
            })
            .collect();
        PoolStats { workers }
    }

    /// Return a snapshot of current worker statistics without blocking.
    pub fn stats_snapshot(&self) -> PoolStats {
        let workers = self
            .counters
            .iter()
            .map(|c| WorkerStats {
                processed: c.processed.load(Ordering::Relaxed),
                errors: c.errors.load(Ordering::Relaxed),
                total_processing_us: c.total_us.load(Ordering::Relaxed),
            })
            .collect();
        PoolStats { workers }
    }
}

// ---------------------------------------------------------------------------
// Worker loop
// ---------------------------------------------------------------------------

#[instrument(skip(buffer, processor, counters), fields(worker_id = id))]
async fn run_worker<P: LedgerProcessor>(
    id: usize,
    buffer: LedgerBuffer,
    processor: Arc<P>,
    counters: Arc<WorkerCounters>,
) {
    info!(worker_id = id, "worker starting");
    let mut frames_this_batch = 0u64;

    loop {
        match buffer.pop().await {
            None => {
                // Buffer closed and drained.
                info!(
                    worker_id = id,
                    frames_this_batch, "worker shutting down — buffer closed"
                );
                break;
            }
            Some(frame) => {
                let seq = frame.sequence;
                let t0 = Instant::now();

                match processor.process(frame).await {
                    Ok(()) => {
                        let elapsed_us = t0.elapsed().as_micros() as u64;
                        counters.processed.fetch_add(1, Ordering::Relaxed);
                        counters.total_us.fetch_add(elapsed_us, Ordering::Relaxed);
                        frames_this_batch += 1;
                        debug!(
                            worker_id = id,
                            sequence = seq,
                            elapsed_us,
                            "worker: frame processed"
                        );
                    }
                    Err(e) => {
                        counters.errors.fetch_add(1, Ordering::Relaxed);
                        warn!(
                            worker_id = id,
                            sequence = seq,
                            error = %e,
                            "worker: processor returned error, continuing"
                        );
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// WorkerPool — convenience façade
// ---------------------------------------------------------------------------

/// Convenience wrapper that combines a [`ProducerPool`] and [`ConsumerPool`]
/// behind a single, unified handle.
///
/// # Example
///
/// ```rust,no_run
/// use controller::ingestion::buffer::{BackpressureStrategy, LedgerBuffer, LedgerFrame};
/// use controller::ingestion::worker::{WorkerPool, LedgerProcessor};
///
/// struct NoOpProcessor;
///
/// #[async_trait::async_trait]
/// impl LedgerProcessor for NoOpProcessor {
///     async fn process(&self, _frame: LedgerFrame) -> anyhow::Result<()> {
///         Ok(())
///     }
/// }
///
/// #[tokio::main]
/// async fn main() {
///     let pool = WorkerPool::builder()
///         .capacity(1024)
///         .concurrency(4)
///         .strategy(BackpressureStrategy::Block)
///         .build(NoOpProcessor);
///
///     let producer = pool.producer();
///     producer.push(LedgerFrame { sequence: 1, xdr_bytes: vec![] }).await;
///     producer.close().await;
///
///     let stats = pool.join().await;
///     println!("processed: {}", stats.total_processed());
/// }
/// ```
pub struct WorkerPool {
    producer: ProducerHandle,
    handle: ConsumerHandle,
}

impl WorkerPool {
    /// Return a builder for ergonomic configuration.
    pub fn builder() -> WorkerPoolBuilder {
        WorkerPoolBuilder::default()
    }

    /// Return a clonable producer handle.
    pub fn producer(&self) -> ProducerHandle {
        self.producer.clone()
    }

    /// Wait for all consumer workers to finish and collect statistics.
    pub async fn join(self) -> PoolStats {
        self.handle.join().await
    }

    /// Non-blocking snapshot of current statistics.
    pub fn stats_snapshot(&self) -> PoolStats {
        self.handle.stats_snapshot()
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Builder for [`WorkerPool`].
#[derive(Debug)]
pub struct WorkerPoolBuilder {
    capacity: usize,
    concurrency: usize,
    strategy: BackpressureStrategy,
}

impl Default for WorkerPoolBuilder {
    fn default() -> Self {
        Self {
            capacity: 8192,
            concurrency: 4,
            strategy: BackpressureStrategy::Block,
        }
    }
}

impl WorkerPoolBuilder {
    /// Set the buffer capacity (maximum number of in-flight frames).
    pub fn capacity(mut self, capacity: usize) -> Self {
        self.capacity = capacity;
        self
    }

    /// Set the number of parallel consumer tasks.
    pub fn concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency;
        self
    }

    /// Set the backpressure strategy used when the buffer is full.
    pub fn strategy(mut self, strategy: BackpressureStrategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// Build the [`WorkerPool`] with the supplied processor.
    pub fn build<P: LedgerProcessor>(self, processor: P) -> WorkerPool {
        let buffer = LedgerBuffer::new(self.capacity, self.strategy);
        let producer_pool = ProducerPool::new(buffer.clone());
        let producer = producer_pool.handle();
        let consumer_pool = ConsumerPool::new(buffer, processor, self.concurrency);
        let handle = consumer_pool.start();
        WorkerPool { producer, handle }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    // A simple processor that records all sequence numbers it receives.
    struct CollectingProcessor {
        received: Arc<Mutex<Vec<u32>>>,
    }

    #[async_trait::async_trait]
    impl LedgerProcessor for CollectingProcessor {
        async fn process(&self, frame: LedgerFrame) -> anyhow::Result<()> {
            self.received.lock().await.push(frame.sequence);
            Ok(())
        }
    }

    // A processor that always errors.
    struct FailingProcessor;

    #[async_trait::async_trait]
    impl LedgerProcessor for FailingProcessor {
        async fn process(&self, _frame: LedgerFrame) -> anyhow::Result<()> {
            Err(anyhow::anyhow!("deliberate processing error"))
        }
    }

    fn frame(seq: u32) -> LedgerFrame {
        LedgerFrame {
            sequence: seq,
            xdr_bytes: vec![seq as u8; 32],
        }
    }

    // ── Basic end-to-end ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_worker_pool_processes_all_frames() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let proc = CollectingProcessor {
            received: Arc::clone(&received),
        };

        let pool = WorkerPool::builder()
            .capacity(64)
            .concurrency(2)
            .strategy(BackpressureStrategy::Block)
            .build(proc);

        let producer = pool.producer();
        for i in 0..50 {
            producer.push(frame(i)).await;
        }
        producer.close().await;

        let stats = pool.join().await;
        assert_eq!(stats.total_processed(), 50, "all 50 frames must be processed");
        assert_eq!(stats.total_errors(), 0);

        let mut seqs = received.lock().await.clone();
        seqs.sort();
        let expected: Vec<u32> = (0..50).collect();
        assert_eq!(seqs, expected);
    }

    // ── Error resilience ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_worker_continues_after_processor_error() {
        let pool = WorkerPool::builder()
            .capacity(32)
            .concurrency(1)
            .build(FailingProcessor);

        let producer = pool.producer();
        for i in 0..10 {
            producer.push(frame(i)).await;
        }
        producer.close().await;

        let stats = pool.join().await;
        assert_eq!(stats.total_errors(), 10, "all frames should produce errors");
        assert_eq!(stats.total_processed(), 0);
    }

    // ── DropNewest backpressure ─────────────────────────────────────────────

    #[tokio::test]
    async fn test_drop_newest_backpressure_in_worker_pool() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let proc = CollectingProcessor {
            received: Arc::clone(&received),
        };

        // Tiny buffer so overflow is inevitable.
        let pool = WorkerPool::builder()
            .capacity(4)
            .concurrency(1)
            .strategy(BackpressureStrategy::DropNewest)
            .build(proc);

        let producer = pool.producer();
        // Push 20 frames into a capacity-4 buffer with one slow consumer.
        for i in 0..20u32 {
            producer.push(frame(i)).await;
        }
        producer.close().await;

        let stats = pool.join().await;
        // Drops happen at the buffer level; the pool only counts what it processes.
        // The number processed must not exceed the total pushed.
        assert!(
            stats.total_processed() <= 20,
            "processed cannot exceed total pushed"
        );
        assert_eq!(stats.total_errors(), 0, "CollectingProcessor never errors");
    }

    // ── Stats snapshot ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stats_snapshot_available_before_join() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let proc = CollectingProcessor {
            received: Arc::clone(&received),
        };

        let pool = WorkerPool::builder()
            .capacity(128)
            .concurrency(2)
            .build(proc);

        let producer = pool.producer();
        for i in 0..20 {
            producer.push(frame(i)).await;
        }

        // Allow workers a moment to drain.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let snap = pool.stats_snapshot();
        assert!(snap.total_processed() > 0, "snapshot should show progress");

        producer.close().await;
        pool.join().await;
    }

    // ── Concurrent producers ────────────────────────────────────────────────

    #[tokio::test]
    async fn test_multiple_producers_single_consumer() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let proc = CollectingProcessor {
            received: Arc::clone(&received),
        };

        let pool = WorkerPool::builder()
            .capacity(256)
            .concurrency(1)
            .strategy(BackpressureStrategy::Block)
            .build(proc);

        let mut tasks = tokio::task::JoinSet::new();
        for p in 0..4u32 {
            let producer = pool.producer();
            tasks.spawn(async move {
                for i in 0..25u32 {
                    producer.push(frame(p * 25 + i)).await;
                }
            });
        }
        while tasks.join_next().await.is_some() {}
        pool.producer().close().await;

        let stats = pool.join().await;
        assert_eq!(stats.total_processed(), 100);
    }
}
