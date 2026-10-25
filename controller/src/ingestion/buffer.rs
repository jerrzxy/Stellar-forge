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

//! Bounded ring buffer for raw XDR ledger streams received from Stellar Core.
//!
//! # Design
//!
//! [`LedgerBuffer`] is a bounded, lock-protected circular queue backed by a
//! fixed-capacity `VecDeque`.  The bounded capacity prevents Out-Of-Memory pod
//! terminations during historical catch-up bursts.
//!
//! Three overflow strategies are supported via [`BackpressureStrategy`]:
//!
//! | Strategy     | Overflow behaviour                                   |
//! |--------------|------------------------------------------------------|
//! | `Block`      | `push` awaits until a slot is free (default)         |
//! | `DropNewest` | The incoming frame is silently discarded             |
//! | `DropOldest` | The oldest unprocessed frame is evicted              |
//!
//! Atomic counters track pushed, popped, and dropped frames for observability
//! through `tracing` spans and the [`BufferStats`] snapshot type.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::{Mutex, Notify};
use tracing::{debug, trace, warn};

// ---------------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------------

/// A raw XDR-encoded ledger frame as received from Stellar Core.
///
/// `sequence` is the ledger sequence number (used for ordered processing and
/// deduplication); `xdr_bytes` carries the full XDR payload.
#[derive(Debug, Clone)]
pub struct LedgerFrame {
    /// Stellar ledger sequence number.
    pub sequence: u32,
    /// Raw XDR bytes of the ledger close meta.
    pub xdr_bytes: Vec<u8>,
}

/// How the buffer reacts when it is full and a new frame arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackpressureStrategy {
    /// Block the producer until a consumer drains at least one slot (default).
    #[default]
    Block,
    /// Discard the newly-arrived frame; fastest for non-critical fan-out paths.
    DropNewest,
    /// Evict the oldest unprocessed frame to make room for the new one.
    DropOldest,
}

/// A point-in-time snapshot of buffer statistics.
#[derive(Debug, Clone, Copy)]
pub struct BufferStats {
    /// Total frames successfully enqueued since creation.
    pub pushed: u64,
    /// Total frames successfully dequeued since creation.
    pub popped: u64,
    /// Total frames discarded due to overflow (DropNewest / DropOldest).
    pub dropped: u64,
    /// Current number of frames waiting in the queue.
    pub depth: usize,
    /// Maximum allowed depth (configured capacity).
    pub capacity: usize,
}

// ---------------------------------------------------------------------------
// Internal shared state
// ---------------------------------------------------------------------------

struct Inner {
    queue: VecDeque<LedgerFrame>,
    capacity: usize,
    closed: bool,
}

// ---------------------------------------------------------------------------
// Public handle
// ---------------------------------------------------------------------------

/// A bounded, async-aware ring buffer for [`LedgerFrame`]s.
///
/// Cheap to clone — all clones share the same underlying queue.
#[derive(Clone)]
pub struct LedgerBuffer {
    inner: Arc<Mutex<Inner>>,
    not_empty: Arc<Notify>,
    not_full: Arc<Notify>,
    strategy: BackpressureStrategy,
    // Atomic counters — visible from all clones without locking.
    pushed: Arc<AtomicU64>,
    popped: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    capacity: usize,
}

impl std::fmt::Debug for LedgerBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LedgerBuffer")
            .field("capacity", &self.capacity)
            .field("strategy", &self.strategy)
            .field("pushed", &self.pushed.load(Ordering::Relaxed))
            .field("popped", &self.popped.load(Ordering::Relaxed))
            .field("dropped", &self.dropped.load(Ordering::Relaxed))
            .finish()
    }
}

impl LedgerBuffer {
    /// Create a new buffer with the given `capacity` and `strategy`.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is zero.
    pub fn new(capacity: usize, strategy: BackpressureStrategy) -> Self {
        assert!(capacity > 0, "LedgerBuffer capacity must be > 0");
        Self {
            inner: Arc::new(Mutex::new(Inner {
                queue: VecDeque::with_capacity(capacity),
                capacity,
                closed: false,
            })),
            not_empty: Arc::new(Notify::new()),
            not_full: Arc::new(Notify::new()),
            strategy,
            pushed: Arc::new(AtomicU64::new(0)),
            popped: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
            capacity,
        }
    }

    /// Enqueue `frame` according to the configured [`BackpressureStrategy`].
    ///
    /// - `Block`: suspends until a slot is available, then enqueues.
    /// - `DropNewest`: returns immediately, incrementing the drop counter if full.
    /// - `DropOldest`: evicts the head of the queue, then enqueues.
    ///
    /// Returns `true` if the frame was accepted, `false` if it was dropped.
    pub async fn push(&self, frame: LedgerFrame) -> bool {
        match self.strategy {
            BackpressureStrategy::Block => self.push_blocking(frame).await,
            BackpressureStrategy::DropNewest => self.push_drop_newest(frame).await,
            BackpressureStrategy::DropOldest => self.push_drop_oldest(frame).await,
        }
    }

    /// Dequeue the oldest frame from the buffer, suspending until one is
    /// available or until [`LedgerBuffer::close`] is called.
    ///
    /// Returns `None` when the buffer has been closed **and** drained.
    pub async fn pop(&self) -> Option<LedgerFrame> {
        loop {
            {
                let mut guard = self.inner.lock().await;
                if let Some(frame) = guard.queue.pop_front() {
                    let depth = guard.queue.len();
                    drop(guard);
                    self.popped.fetch_add(1, Ordering::Relaxed);
                    // Notify a blocked producer that a slot opened.
                    self.not_full.notify_one();
                    trace!(
                        sequence = frame.sequence,
                        depth,
                        "LedgerBuffer: popped frame"
                    );
                    return Some(frame);
                }
                if guard.closed {
                    return None;
                }
            }
            // Queue is empty and not closed — wait for a push.
            self.not_empty.notified().await;
        }
    }

    /// Try to dequeue a frame without blocking.
    ///
    /// Returns `None` immediately if the queue is empty.
    pub async fn try_pop(&self) -> Option<LedgerFrame> {
        let mut guard = self.inner.lock().await;
        if let Some(frame) = guard.queue.pop_front() {
            drop(guard);
            self.popped.fetch_add(1, Ordering::Relaxed);
            self.not_full.notify_one();
            return Some(frame);
        }
        None
    }

    /// Signal that no more frames will be pushed.
    ///
    /// All pending [`pop`](Self::pop) calls will drain the remaining frames
    /// and then receive `None`.
    pub async fn close(&self) {
        let mut guard = self.inner.lock().await;
        guard.closed = true;
        drop(guard);
        // Wake all sleeping consumers.
        self.not_empty.notify_waiters();
        debug!("LedgerBuffer: closed");
    }

    /// Returns `true` if the buffer has been closed.
    pub async fn is_closed(&self) -> bool {
        self.inner.lock().await.closed
    }

    /// Returns the current number of frames in the buffer.
    pub async fn len(&self) -> usize {
        self.inner.lock().await.queue.len()
    }

    /// Returns `true` if the buffer contains no frames.
    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.queue.is_empty()
    }

    /// Returns a point-in-time snapshot of buffer statistics.
    pub async fn stats(&self) -> BufferStats {
        let guard = self.inner.lock().await;
        BufferStats {
            pushed: self.pushed.load(Ordering::Relaxed),
            popped: self.popped.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            depth: guard.queue.len(),
            capacity: guard.capacity,
        }
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    async fn push_blocking(&self, frame: LedgerFrame) -> bool {
        loop {
            {
                let mut guard = self.inner.lock().await;
                if guard.closed {
                    warn!("LedgerBuffer: push after close — dropping frame");
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                    return false;
                }
                if guard.queue.len() < guard.capacity {
                    let seq = frame.sequence;
                    guard.queue.push_back(frame);
                    let depth = guard.queue.len();
                    drop(guard);
                    self.pushed.fetch_add(1, Ordering::Relaxed);
                    self.not_empty.notify_one();
                    trace!(sequence = seq, depth, "LedgerBuffer: pushed (block)");
                    return true;
                }
                // Full — release the lock and wait for a slot.
            }
            debug!("LedgerBuffer: full, producer blocking");
            self.not_full.notified().await;
        }
    }

    async fn push_drop_newest(&self, frame: LedgerFrame) -> bool {
        let mut guard = self.inner.lock().await;
        if guard.closed {
            warn!("LedgerBuffer: push after close — dropping frame");
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if guard.queue.len() >= guard.capacity {
            warn!(
                sequence = frame.sequence,
                "LedgerBuffer: full, dropping newest frame (DropNewest strategy)"
            );
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let seq = frame.sequence;
        guard.queue.push_back(frame);
        let depth = guard.queue.len();
        drop(guard);
        self.pushed.fetch_add(1, Ordering::Relaxed);
        self.not_empty.notify_one();
        trace!(sequence = seq, depth, "LedgerBuffer: pushed (drop-newest)");
        true
    }

    async fn push_drop_oldest(&self, frame: LedgerFrame) -> bool {
        let mut guard = self.inner.lock().await;
        if guard.closed {
            warn!("LedgerBuffer: push after close — dropping frame");
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if guard.queue.len() >= guard.capacity {
            // Evict the oldest frame to make room.
            if let Some(evicted) = guard.queue.pop_front() {
                warn!(
                    evicted_sequence = evicted.sequence,
                    incoming_sequence = frame.sequence,
                    "LedgerBuffer: full, evicting oldest frame (DropOldest strategy)"
                );
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
        let seq = frame.sequence;
        guard.queue.push_back(frame);
        let depth = guard.queue.len();
        drop(guard);
        self.pushed.fetch_add(1, Ordering::Relaxed);
        self.not_empty.notify_one();
        trace!(sequence = seq, depth, "LedgerBuffer: pushed (drop-oldest)");
        true
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a simple frame with given sequence number.
    fn frame(seq: u32) -> LedgerFrame {
        LedgerFrame {
            sequence: seq,
            xdr_bytes: vec![seq as u8; 16],
        }
    }

    // ── Basic push / pop ────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_push_pop_fifo_order() {
        let buf = LedgerBuffer::new(8, BackpressureStrategy::Block);
        for i in 0..4 {
            buf.push(frame(i)).await;
        }
        for i in 0..4 {
            let f = buf.pop().await.expect("frame expected");
            assert_eq!(f.sequence, i, "FIFO order violated");
        }
    }

    #[tokio::test]
    async fn test_len_and_is_empty() {
        let buf = LedgerBuffer::new(4, BackpressureStrategy::DropNewest);
        assert!(buf.is_empty().await);
        buf.push(frame(1)).await;
        assert_eq!(buf.len().await, 1);
        buf.pop().await;
        assert!(buf.is_empty().await);
    }

    // ── DropNewest ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_drop_newest_strategy() {
        let buf = LedgerBuffer::new(2, BackpressureStrategy::DropNewest);
        assert!(buf.push(frame(1)).await, "first push should succeed");
        assert!(buf.push(frame(2)).await, "second push should succeed");
        // Buffer is now full; the third frame must be dropped.
        assert!(!buf.push(frame(3)).await, "overflow frame should be dropped");

        let stats = buf.stats().await;
        assert_eq!(stats.dropped, 1);
        assert_eq!(stats.pushed, 2);
        assert_eq!(stats.depth, 2);

        // The two accepted frames must be sequence 1 and 2 (not 3).
        assert_eq!(buf.pop().await.unwrap().sequence, 1);
        assert_eq!(buf.pop().await.unwrap().sequence, 2);
    }

    // ── DropOldest ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_drop_oldest_strategy() {
        let buf = LedgerBuffer::new(2, BackpressureStrategy::DropOldest);
        buf.push(frame(1)).await;
        buf.push(frame(2)).await;
        // Buffer is full; seq=1 should be evicted.
        buf.push(frame(3)).await;

        let stats = buf.stats().await;
        assert_eq!(stats.dropped, 1);

        // Remaining frames should be 2 and 3.
        assert_eq!(buf.pop().await.unwrap().sequence, 2);
        assert_eq!(buf.pop().await.unwrap().sequence, 3);
    }

    // ── Close semantics ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_close_drains_remaining_frames() {
        let buf = LedgerBuffer::new(4, BackpressureStrategy::Block);
        buf.push(frame(10)).await;
        buf.push(frame(11)).await;
        buf.close().await;

        // Already-queued frames should still be returned.
        assert_eq!(buf.pop().await.unwrap().sequence, 10);
        assert_eq!(buf.pop().await.unwrap().sequence, 11);
        // After draining, pop must return None.
        assert!(buf.pop().await.is_none(), "closed+empty buffer must yield None");
    }

    #[tokio::test]
    async fn test_push_after_close_dropped() {
        let buf = LedgerBuffer::new(4, BackpressureStrategy::DropNewest);
        buf.close().await;
        let accepted = buf.push(frame(99)).await;
        assert!(!accepted, "push after close should always fail");
        assert_eq!(buf.stats().await.dropped, 1);
    }

    // ── Concurrent push / pop ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_concurrent_producers_and_consumer() {
        use tokio::task::JoinSet;

        const NUM_PRODUCERS: u32 = 4;
        const FRAMES_PER_PRODUCER: u32 = 100;
        const TOTAL: u32 = NUM_PRODUCERS * FRAMES_PER_PRODUCER;

        let buf = LedgerBuffer::new(64, BackpressureStrategy::Block);
        let buf_c = buf.clone();

        // Spawn a consumer that collects all sequences.
        let consumer = tokio::spawn(async move {
            let mut sequences = Vec::with_capacity(TOTAL as usize);
            while sequences.len() < TOTAL as usize {
                if let Some(f) = buf_c.pop().await {
                    sequences.push(f.sequence);
                }
            }
            sequences
        });

        // Spawn producers.
        let mut set = JoinSet::new();
        for p in 0..NUM_PRODUCERS {
            let buf_p = buf.clone();
            set.spawn(async move {
                for i in 0..FRAMES_PER_PRODUCER {
                    buf_p.push(frame(p * FRAMES_PER_PRODUCER + i)).await;
                }
            });
        }
        while set.join_next().await.is_some() {}
        buf.close().await;

        let sequences = consumer.await.expect("consumer panicked");
        assert_eq!(sequences.len(), TOTAL as usize, "all frames must be received");

        let stats = buf.stats().await;
        assert_eq!(stats.dropped, 0, "no frames should be dropped in Block mode");
    }

    // ── Stats ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stats_counters() {
        let buf = LedgerBuffer::new(4, BackpressureStrategy::DropNewest);
        for i in 0..4 {
            buf.push(frame(i)).await;
        }
        buf.push(frame(99)).await; // one drop

        for _ in 0..4 {
            buf.pop().await;
        }

        let s = buf.stats().await;
        assert_eq!(s.pushed, 4);
        assert_eq!(s.popped, 4);
        assert_eq!(s.dropped, 1);
        assert_eq!(s.depth, 0);
        assert_eq!(s.capacity, 4);
    }

    // ── try_pop ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_try_pop_returns_none_when_empty() {
        let buf = LedgerBuffer::new(4, BackpressureStrategy::Block);
        assert!(buf.try_pop().await.is_none());
        buf.push(frame(1)).await;
        assert!(buf.try_pop().await.is_some());
        assert!(buf.try_pop().await.is_none());
    }
}
