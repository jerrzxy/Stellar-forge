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
//! Bridge orchestration: fetch → transform → atomically publish, per ledger.
//!
//! The bridge tails a [`EventSource`] (or backfills a historical range) and
//! publishes every closed ledger as one atomic [`LedgerBatch`] through an
//! [`EventSink`]. Because the Kafka sink commits the batch together with its
//! checkpoint in a single transaction, a crashed or restarted bridge resumes
//! exactly where it left off — never duplicating and never skipping a ledger.

use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::EventBridgeConfig;
use crate::error::{EventBridgeError, Result};
use crate::horizon::{EventSource, HorizonLedgerResource};
use crate::sink::{EventSink, LedgerBatch};
use crate::xdr_transformer::XdrTransformer;

/// Aggregate counters returned by a completed run or backfill.
#[derive(Debug, Clone, Default)]
pub struct BridgeStats {
    /// Ledgers published.
    pub ledgers_processed: u64,
    /// Events published across all ledgers.
    pub events_published: u64,
    /// Wall-clock duration of the run.
    pub elapsed: Duration,
}

impl BridgeStats {
    /// Sustained throughput in events per second.
    pub fn events_per_sec(&self) -> f64 {
        if self.elapsed.as_secs_f64() == 0.0 {
            return 0.0;
        }
        self.events_published as f64 / self.elapsed.as_secs_f64()
    }
}

/// The bridge worker: one source, one sink, one transformer.
pub struct EventBridge<S, Src> {
    source: Src,
    sink: S,
    transformer: XdrTransformer,
    config: EventBridgeConfig,
}

impl<S, Src> EventBridge<S, Src>
where
    S: EventSink,
    Src: EventSource,
{
    /// Wire a bridge instance together.
    pub fn new(source: Src, sink: S, transformer: XdrTransformer, config: EventBridgeConfig) -> Self {
        Self {
            source,
            sink,
            transformer,
            config,
        }
    }

    /// The first ledger to process: explicit override, otherwise the ledger
    /// after the last committed checkpoint, otherwise the configured start.
    pub async fn resume_ledger(&self, override_start: Option<u64>) -> Result<u64> {
        if let Some(start) = override_start {
            return Ok(start);
        }
        match self.sink.last_committed_ledger().await? {
            Some(last) => Ok(last + 1),
            None => Ok(self.config.starting_ledger),
        }
    }

    /// Transform one ledger into an atomic batch and publish it, retrying
    /// with exponential backoff on transient sink failures.
    pub async fn publish_ledger(&self, ledger: &HorizonLedgerResource) -> Result<u64> {
        let events = self.transformer.transform(ledger);
        let event_count = events.len() as u64;
        let batch = LedgerBatch::new(ledger.sequence, &ledger.hash, events);

        let mut attempt = 0u32;
        loop {
            match self.sink.publish_batch(&batch).await {
                Ok(()) => return Ok(event_count),
                Err(e) if attempt < self.config.max_publish_retries => {
                    attempt += 1;
                    let backoff = Duration::from_secs(
                        self.config.retry_backoff_secs.saturating_mul(1u64 << attempt.min(6)),
                    );
                    warn!(
                        ledger = ledger.sequence,
                        attempt,
                        error = %e,
                        backoff_secs = backoff.as_secs(),
                        "publish failed; retrying ledger batch"
                    );
                    tokio::time::sleep(backoff).await;
                }
                Err(e) => {
                    return Err(EventBridgeError::Kafka(format!(
                        "publishing ledger {} failed after {} retries: {e}",
                        ledger.sequence, self.config.max_publish_retries
                    )))
                }
            }
        }
    }

    /// Backfill a closed ledger range (inclusive) through the bridge.
    pub async fn backfill(&self, start_ledger: u64, end_ledger: u64) -> Result<BridgeStats> {
        if end_ledger < start_ledger {
            return Err(EventBridgeError::Config(format!(
                "end ledger {end_ledger} < start ledger {start_ledger}"
            )));
        }
        let started = Instant::now();
        let mut stats = BridgeStats::default();

        for sequence in start_ledger..=end_ledger {
            let ledger = self.source.fetch_ledger(sequence).await?;
            stats.events_published += self.publish_ledger(&ledger).await?;
            stats.ledgers_processed += 1;
            if stats.ledgers_processed % 1000 == 0 {
                info!(
                    ledgers = stats.ledgers_processed,
                    events = stats.events_published,
                    eps = format!("{:.0}", stats.events_per_sec()),
                    "backfill progress"
                );
            }
        }

        stats.elapsed = started.elapsed();
        Ok(stats)
    }

    /// Tail the source until `shutdown` is cancelled, publishing every newly
    /// closed ledger exactly once.
    pub async fn run(&self, shutdown: CancellationToken) -> Result<BridgeStats> {
        let started = Instant::now();
        let mut stats = BridgeStats::default();
        let mut next_ledger = self.resume_ledger(None).await?;

        info!(
            start_ledger = next_ledger,
            topic = %self.config.topic,
            "event bridge started"
        );

        loop {
            if shutdown.is_cancelled() {
                break;
            }

            match self.source.latest_ledger().await {
                Ok(latest) => {
                    while next_ledger <= latest {
                        let ledger = self.source.fetch_ledger(next_ledger).await?;
                        stats.events_published += self.publish_ledger(&ledger).await?;
                        stats.ledgers_processed += 1;
                        next_ledger += 1;
                    }
                }
                Err(e) => {
                    warn!(error = %e, "failed to fetch latest ledger; backing off");
                }
            }

            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(self.config.poll_interval_secs.max(1))) => {}
            }
        }

        stats.elapsed = started.elapsed();
        info!(
            ledgers = stats.ledgers_processed,
            events = stats.events_published,
            eps = format!("{:.0}", stats.events_per_sec()),
            "event bridge stopped"
        );
        Ok(stats)
    }
}

/// Result of an in-process load test.
#[derive(Debug, Clone)]
pub struct LoadTestReport {
    /// Events produced by the run.
    pub events: u64,
    /// Ledger batches published.
    pub ledgers: u64,
    /// Wall-clock seconds.
    pub elapsed_secs: f64,
    /// Sustained throughput.
    pub events_per_sec: f64,
    /// Target throughput the run was measured against.
    pub target_eps: f64,
    /// Whether the target was met.
    pub passed: bool,
}

/// Run the full in-process pipeline (synthetic ledger → transformer →
/// serializer → sink) against the 5,000 events/sec review requirement.
///
/// The Kafka broker path is validated separately against a local
/// Redpanda/Kafka cluster (see `scripts/validate-event-bridge.sh`); this
/// harness proves the bridge itself never becomes the bottleneck and never
/// builds memory backpressure: batches are dropped by the sink as soon as
/// they are committed.
pub async fn run_load_test(
    total_events: u64,
    target_eps: f64,
    payments_per_ledger: u32,
    contract_events_per_ledger: u32,
) -> LoadTestReport {
    use crate::horizon::synthetic;

    let sink = crate::sink::MemorySink::new();
    let transformer = XdrTransformer::new();
    let tx_per_ledger = (payments_per_ledger + contract_events_per_ledger).max(1);

    let started = Instant::now();
    let mut events: u64 = 0;
    let mut ledgers: u64 = 0;

    while events < total_events {
        let ledger = synthetic::ledger(
            1_000_000 + ledgers,
            tx_per_ledger,
            payments_per_ledger,
            contract_events_per_ledger,
        );
        let batch_events = transformer.transform(&ledger);
        let count = batch_events.len() as u64;
        let batch = LedgerBatch::new(ledger.sequence, &ledger.hash, batch_events);
        sink.publish_batch(&batch).await.expect("memory sink publish");
        // Drop committed events immediately: no unbounded queue can build.
        sink.take_events();
        events += count;
        ledgers += 1;
    }

    let elapsed_secs = started.elapsed().as_secs_f64();
    let events_per_sec = if elapsed_secs > 0.0 {
        events as f64 / elapsed_secs
    } else {
        f64::INFINITY
    };

    LoadTestReport {
        events,
        ledgers,
        elapsed_secs,
        events_per_sec,
        target_eps,
        passed: events_per_sec >= target_eps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::horizon::synthetic;
    use crate::sink::MemorySink;
    use std::sync::Arc;

    #[tokio::test]
    async fn test_backfill_publishes_every_ledger_once() {
        let sink = Arc::new(MemorySink::new());
        let source = synthetic::SyntheticSource::new(10);
        let bridge = EventBridge::new(
            source,
            Arc::clone(&sink),
            XdrTransformer::new(),
            EventBridgeConfig::default(),
        );

        let stats = bridge.backfill(1, 10).await.unwrap();
        assert_eq!(stats.ledgers_processed, 10);
        // 1 close + 3 payments + 1 contract event per synthetic ledger.
        assert_eq!(stats.events_published, 10 * 5);

        let events = sink.events();
        let unique_ids: std::collections::HashSet<_> =
            events.iter().map(|e| e.event_id.clone()).collect();
        assert_eq!(unique_ids.len(), events.len(), "event ids must be unique");
    }

    #[tokio::test]
    async fn test_resume_uses_checkpoint() {
        let sink = MemorySink::new();
        sink.force_checkpoint(41);
        let bridge = EventBridge::new(
            synthetic::SyntheticSource::new(100),
            &sink,
            XdrTransformer::new(),
            EventBridgeConfig {
                starting_ledger: 1,
                ..Default::default()
            },
        );
        assert_eq!(bridge.resume_ledger(None).await.unwrap(), 42);
        assert_eq!(bridge.resume_ledger(Some(7)).await.unwrap(), 7);
    }

    #[tokio::test]
    async fn test_backfill_rejects_inverted_range() {
        let bridge = EventBridge::new(
            synthetic::SyntheticSource::new(10),
            MemorySink::new(),
            XdrTransformer::new(),
            EventBridgeConfig::default(),
        );
        assert!(bridge.backfill(10, 1).await.is_err());
    }

    #[tokio::test]
    async fn test_load_test_meets_throughput_target() {
        // CI-sized run; the full 1M-event review run lives in
        // tests/load_test.rs (ignored by default) and the `load-test`
        // subcommand of the worker binary.
        let report = run_load_test(50_000, 5_000.0, 4, 1).await;
        assert!(
            report.passed,
            "bridge sustained only {:.0} events/sec (target {})",
            report.events_per_sec, report.target_eps
        );
    }
}
