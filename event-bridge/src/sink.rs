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
//! Sink abstraction: where ledger batches are committed.
//!
//! The bridge publishes one [`LedgerBatch`] at a time. A batch must be
//! committed **atomically together with its checkpoint** — that is what makes
//! the pipeline exactly-once across restarts. [`crate::kafka_producer`] does
//! this with a Kafka transaction; [`MemorySink`] models the same contract for
//! tests and load tests.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;

use crate::error::Result;
use crate::event::LedgerEvent;

/// All events derived from one closed ledger, published atomically.
#[derive(Debug, Clone)]
pub struct LedgerBatch {
    /// Ledger sequence.
    pub ledger: u64,
    /// Ledger hash as reported by the source.
    pub ledger_hash: String,
    /// Events in deterministic order.
    pub events: Vec<LedgerEvent>,
}

impl LedgerBatch {
    /// Build a batch from a ledger and its events.
    pub fn new(ledger: u64, ledger_hash: impl Into<String>, events: Vec<LedgerEvent>) -> Self {
        Self {
            ledger,
            ledger_hash: ledger_hash.into(),
            events,
        }
    }
}

/// Atomic commit target for ledger batches.
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Atomically publish every event of `batch` **and** record the batch's
    /// ledger as the latest committed checkpoint. Either all of it is visible
    /// to `read_committed` consumers or none of it is.
    async fn publish_batch(&self, batch: &LedgerBatch) -> Result<()>;

    /// Highest ledger sequence whose batch has been committed, if any.
    async fn last_committed_ledger(&self) -> Result<Option<u64>>;
}

/// In-memory sink used by tests, load tests and dry runs.
///
/// Mirrors the atomicity contract of the Kafka sink: events and checkpoint
/// land together under one lock acquisition.
#[derive(Debug, Default)]
pub struct MemorySink {
    events: Mutex<Vec<LedgerEvent>>,
    checkpoint: AtomicU64,
    batches: AtomicU64,
}

impl MemorySink {
    /// Create an empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// All events committed so far, in commit order.
    pub fn events(&self) -> Vec<LedgerEvent> {
        self.events.lock().expect("memory sink lock").clone()
    }

    /// Number of committed batches.
    pub fn batch_count(&self) -> u64 {
        self.batches.load(Ordering::SeqCst)
    }

    /// Drain all committed events (keeping the checkpoint in place).
    pub fn take_events(&self) -> Vec<LedgerEvent> {
        std::mem::take(&mut *self.events.lock().expect("memory sink lock"))
    }

    /// Simulate a committed checkpoint (for restart/resume tests).
    pub fn force_checkpoint(&self, ledger: u64) {
        self.checkpoint.store(ledger, Ordering::SeqCst);
    }
}

/// Delegation so bridges can share a sink via reference.
#[async_trait]
impl<T: EventSink + ?Sized> EventSink for &T {
    async fn publish_batch(&self, batch: &LedgerBatch) -> Result<()> {
        (**self).publish_batch(batch).await
    }

    async fn last_committed_ledger(&self) -> Result<Option<u64>> {
        (**self).last_committed_ledger().await
    }
}

/// Delegation so bridges and callers can share a sink via `Arc`.
#[async_trait]
impl<T: EventSink + ?Sized> EventSink for std::sync::Arc<T> {
    async fn publish_batch(&self, batch: &LedgerBatch) -> Result<()> {
        (**self).publish_batch(batch).await
    }

    async fn last_committed_ledger(&self) -> Result<Option<u64>> {
        (**self).last_committed_ledger().await
    }
}

#[async_trait]
impl EventSink for MemorySink {
    async fn publish_batch(&self, batch: &LedgerBatch) -> Result<()> {
        let mut events = self.events.lock().expect("memory sink lock");
        events.extend(batch.events.iter().cloned());
        self.checkpoint.store(batch.ledger, Ordering::SeqCst);
        self.batches.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn last_committed_ledger(&self) -> Result<Option<u64>> {
        let ledger = self.checkpoint.load(Ordering::SeqCst);
        Ok(if ledger == 0 { None } else { Some(ledger) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventPayload;
    use chrono::Utc;

    fn batch(ledger: u64) -> LedgerBatch {
        let event = LedgerEvent::new(
            ledger,
            "hash",
            Utc::now(),
            "close",
            EventPayload::LedgerClose {
                tx_count: 0,
                fee_pool: None,
                max_tx_set_size: None,
            },
        );
        LedgerBatch::new(ledger, "hash", vec![event])
    }

    #[tokio::test]
    async fn test_memory_sink_commits_atomically() {
        let sink = MemorySink::new();
        assert_eq!(sink.last_committed_ledger().await.unwrap(), None);
        sink.publish_batch(&batch(10)).await.unwrap();
        sink.publish_batch(&batch(11)).await.unwrap();
        assert_eq!(sink.last_committed_ledger().await.unwrap(), Some(11));
        assert_eq!(sink.events().len(), 2);
        assert_eq!(sink.batch_count(), 2);
    }
}
