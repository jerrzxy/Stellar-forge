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
//! Transactional Kafka producer with exactly-once semantics (EOS).
//!
//! # How exactly-once is achieved
//!
//! * The producer is configured with a stable `transactional.id` per bridge
//!   instance and `enable.idempotence=true`. A restarted instance fences its
//!   zombie predecessor, so only one writer can ever commit.
//! * All events of one ledger — plus the ledger's checkpoint record — are
//!   sent inside a **single Kafka transaction**. Either the whole ledger (and
//!   its checkpoint) becomes visible to `isolation.level=read_committed`
//!   consumers, or none of it does.
//! * On any failure the transaction is aborted and the bridge retries the
//!   whole ledger batch; Kafka guarantees the aborted records are never
//!   observable.
//! * On restart the bridge resumes from the last committed checkpoint, so
//!   committed ledgers are never re-sent and uncommitted ones are re-sent
//!   in full — exactly once end to end.
//!
//! Consumers MUST set `isolation.level=read_committed`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::config::{EventBridgeConfig, EventFormat, PartitionBy};
use crate::error::{EventBridgeError, Result};
use crate::event::LedgerEvent;
use crate::sink::{EventSink, LedgerBatch};

#[cfg(feature = "kafka")]
use rdkafka::consumer::{Consumer, StreamConsumer};
#[cfg(feature = "kafka")]
use rdkafka::message::{Message, OwnedMessage};
#[cfg(feature = "kafka")]
use rdkafka::producer::{BaseRecord, FutureProducer, Producer};
#[cfg(feature = "kafka")]
use rdkafka::util::Timeout;
#[cfg(feature = "kafka")]
use rdkafka::ClientConfig;

/// Static key of the single checkpoint entry in the compacted checkpoint
/// topic. Compaction keeps only the latest value per key.
const CHECKPOINT_KEY: &str = "checkpoint";

/// Checkpoint record committed in the same transaction as the ledger events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    /// Highest ledger sequence committed atomically with this checkpoint.
    pub ledger: u64,
    /// Ledger hash, useful for gap/corruption detection on resume.
    pub ledger_hash: String,
    /// Wall-clock time the checkpoint committed.
    pub committed_at: DateTime<Utc>,
}

const FNV_OFFSET_BASIS_64: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME_64: u64 = 0x1000_0000_1b3;

/// FNV-1a 64-bit hash used for partition selection (same scheme as ADR-0005
/// and the SCP analytics pipeline).
pub fn fnv1a_64(data: &[u8], seed: u64) -> u64 {
    let mut hash = FNV_OFFSET_BASIS_64 ^ seed;
    for b in data {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(FNV_PRIME_64);
    }
    hash
}

/// Partition counter that can be refreshed from broker metadata at runtime
/// (topics can be re-partitioned while the bridge runs).
#[derive(Debug)]
struct PartitionCounter {
    count: AtomicUsize,
    seed: u64,
}

impl PartitionCounter {
    fn new(count: usize, seed: u64) -> Self {
        Self {
            count: AtomicUsize::new(count.max(1)),
            seed,
        }
    }

    fn partition(&self, key: &str) -> usize {
        let count = self.count.load(Ordering::Relaxed).max(1);
        (fnv1a_64(key.as_bytes(), self.seed) % count as u64) as usize
    }

    fn set_count(&self, count: usize) {
        self.count.store(count.max(1), Ordering::Relaxed);
    }
}

/// Transactional Kafka sink. See the module docs for the EOS design.
pub struct KafkaEventProducer {
    producer: FutureProducer,
    config: EventBridgeConfig,
    partitions: PartitionCounter,
}

impl KafkaEventProducer {
    /// Build the producer and run the `init_transactions` handshake.
    ///
    /// This fences any zombie producer holding the same `transactional.id`,
    /// so it must complete before any batch is published.
    pub fn new(config: EventBridgeConfig) -> Result<Self> {
        config.validate()?;

        let mut client_config = ClientConfig::new();
        client_config
            .set("bootstrap.servers", &config.bootstrap_servers)
            .set("transactional.id", config.transactional_id())
            .set("enable.idempotence", "true")
            .set("acks", "all")
            .set("compression.type", &config.compression)
            .set("batch.size", config.batch_size.to_string())
            .set("linger.ms", config.linger_ms.to_string())
            .set("message.timeout.ms", "60000")
            .set("transaction.timeout.ms", config.kafka_transaction_timeout_ms.clone());

        if let Some(sasl) = &config.sasl {
            client_config
                .set("security.protocol", "SASL_SSL")
                .set("sasl.mechanism", &sasl.mechanism)
                .set("sasl.username", &sasl.username)
                .set("sasl.password", &sasl.password);
        }

        let producer: FutureProducer = client_config
            .create()
            .map_err(|e| EventBridgeError::Kafka(e.to_string()))?;

        producer
            .init_transactions(Timeout::After(Duration::from_secs(
                config.transaction_timeout_secs,
            )))
            .map_err(|e| EventBridgeError::Kafka(format!("init_transactions failed: {e}")))?;

        // Discover the current partition count of the events topic.
        let initial_partitions = Self::fetch_partition_count(&producer, &config.topic)
            .unwrap_or_else(|e| {
                warn!(
                    "initial partition count fetch for {} failed ({e}); assuming 1 partition",
                    config.topic
                );
                1
            });
        info!(
            topic = %config.topic,
            partitions = initial_partitions,
            transactional_id = %config.transactional_id(),
            "transactional Kafka producer ready"
        );

        Ok(Self {
            producer,
            config,
            partitions: PartitionCounter::new(initial_partitions, config.partition_seed),
        })
    }

    fn fetch_partition_count(producer: &FutureProducer, topic: &str) -> Result<usize> {
        let metadata = producer.fetch_metadata(Some(topic), Duration::from_secs(5))?;
        let topic_meta = metadata.topics().iter().find(|t| t.name() == topic);
        match topic_meta {
            Some(t) if !t.partitions().is_empty() => Ok(t.partitions().len()),
            _ => Err(EventBridgeError::Kafka(format!(
                "topic {topic} not found in broker metadata"
            ))),
        }
    }

    /// Serialize one event according to the configured wire format.
    ///
    /// `avro` currently emits the canonical JSON encoding of the event (same
    /// transitional state as the SCP analytics pipeline) so no data is lost;
    /// the binary Avro encoder + schema registry integration is tracked
    /// separately. The canonical Avro schema lives in
    /// `schemas/ledger-event.avsc`.
    fn serialize_event(&self, event: &LedgerEvent) -> Result<Vec<u8>> {
        match self.config.format {
            EventFormat::Json | EventFormat::Avro => Ok(serde_json::to_vec(event)?),
        }
    }

    /// Queue one record inside the current transaction, retrying once with a
    /// refreshed partition count when the broker rejects the partition.
    fn send_in_transaction<'a>(
        &'a self,
        topic: &'a str,
        key: &str,
        payload: &'a [u8],
    ) -> Result<()> {
        let mut partition = self.partitions.partition(key);
        let record = BaseRecord::to(topic)
            .partition(partition as i32)
            .key(key)
            .payload(payload);

        if let Err((e, _)) = self.producer.send_result(record) {
            // The partition count may have changed (topic re-partitioned or
            // auto-created); refresh and retry once with the new mapping.
            match Self::fetch_partition_count(&self.producer, topic) {
                Ok(n) => self.partitions.set_count(n),
                Err(refresh_err) => {
                    warn!("partition refresh failed during retry: {refresh_err}");
                }
            }
            let new_partition = self.partitions.partition(key);
            if new_partition == partition {
                return Err(EventBridgeError::Kafka(format!(
                    "send to {topic}[{partition}] failed: {e}"
                )));
            }
            partition = new_partition;
            let retry = BaseRecord::to(topic)
                .partition(partition as i32)
                .key(key)
                .payload(payload);
            if let Err((e, _)) = self.producer.send_result(retry) {
                return Err(EventBridgeError::Kafka(format!(
                    "send to {topic}[{partition}] failed after retry: {e}"
                )));
            }
        }
        Ok(())
    }

    /// Queue the checkpoint record inside the current transaction. The
    /// checkpoint topic normally has a single partition; keying it with
    /// [`CHECKPOINT_KEY`] keeps every checkpoint on one partition under the
    /// default partitioner regardless of the topic's partition count.
    fn send_checkpoint_in_transaction(&self, payload: &[u8]) -> Result<()> {
        let record = BaseRecord::to(&self.config.checkpoint_topic)
            .key(CHECKPOINT_KEY)
            .payload(payload);
        if let Err((e, _)) = self.producer.send_result(record) {
            return Err(EventBridgeError::Kafka(format!(
                "checkpoint send to {} failed: {e}",
                self.config.checkpoint_topic
            )));
        }
        Ok(())
    }

    fn abort_quietly(&self) {
        if let Err(e) = self
            .producer
            .abort_transaction(Timeout::After(Duration::from_secs(10)))
        {
            warn!("abort_transaction failed (broker will abort on timeout): {e}");
        }
    }

    /// Commit one ledger batch (events + checkpoint) in a single transaction.
    pub fn commit_batch_transactional(&self, batch: &LedgerBatch) -> Result<()> {
        self.producer.begin_transaction()?;

        for event in &batch.events {
            let payload = self.serialize_event(event)?;
            let key = event.partition_key(self.config.partition_by);
            if let Err(e) = self.send_in_transaction(&self.config.topic, &key, &payload) {
                self.abort_quietly();
                return Err(e);
            }
        }

        // The checkpoint rides the same transaction: consumers that read the
        // checkpoint topic with read_committed see events iff they see this.
        // The checkpoint topic is separate (and compacted, usually
        // single-partition), so it is queued through the broker's default
        // key-based partitioner instead of the events-topic partition
        // counter.
        let checkpoint = Checkpoint {
            ledger: batch.ledger,
            ledger_hash: batch.ledger_hash.clone(),
            committed_at: Utc::now(),
        };
        let payload = serde_json::to_vec(&checkpoint)?;
        if let Err(e) = self.send_checkpoint_in_transaction(&payload) {
            self.abort_quietly();
            return Err(e);
        }

        match self.producer.commit_transaction(Timeout::After(Duration::from_secs(15))) {
            Ok(()) => {
                debug!(
                    ledger = batch.ledger,
                    events = batch.events.len(),
                    "ledger transaction committed"
                );
                Ok(())
            }
            Err(e) => {
                self.abort_quietly();
                Err(EventBridgeError::Kafka(format!(
                    "commit_transaction failed: {e}"
                )))
            }
        }
    }

    /// Flush pending batches (best-effort shutdown helper).
    pub fn flush(&self) -> Result<()> {
        self.producer
            .flush(Timeout::After(Duration::from_secs(10)))?;
        Ok(())
    }

    /// Read the last committed checkpoint from the compacted checkpoint
    /// topic. The reader never commits offsets, so every call re-reads the
    /// (compacted, tiny) topic from the beginning and takes the latest record.
    async fn read_last_checkpoint(&self) -> Result<Option<Checkpoint>> {
        let group_id = format!("{}-checkpoint-reader", self.config.transactional_id());

        let mut client_config = ClientConfig::new();
        client_config
            .set("group.id", group_id)
            .set("bootstrap.servers", &self.config.bootstrap_servers)
            .set("enable.auto.commit", "false")
            .set("auto.offset.reset", "earliest")
            .set("isolation.level", "read_committed");

        if let Some(sasl) = &self.config.sasl {
            client_config
                .set("security.protocol", "SASL_SSL")
                .set("sasl.mechanism", &sasl.mechanism)
                .set("sasl.username", &sasl.username)
                .set("sasl.password", &sasl.password);
        }

        let consumer: StreamConsumer = client_config
            .create()
            .map_err(|e| EventBridgeError::Kafka(e.to_string()))?;
        consumer.subscribe(&[self.config.checkpoint_topic.as_str()])?;

        let deadline = Duration::from_secs(10);
        let mut latest: Option<Checkpoint> = None;

        loop {
            match timeout(deadline, consumer.recv()).await {
                Ok(Ok(message)) => {
                    if let Some(parsed) = parse_checkpoint(&message) {
                        match &latest {
                            Some(existing) if existing.ledger >= parsed.ledger => {}
                            _ => latest = Some(parsed),
                        }
                    }
                }
                Ok(Err(e)) => {
                    return Err(EventBridgeError::Kafka(format!(
                        "checkpoint read failed: {e}"
                    )))
                }
                // Timeout: drained everything currently committed.
                Err(_) => break,
            }
        }

        Ok(latest)
    }
}

fn parse_checkpoint(message: &OwnedMessage) -> Option<Checkpoint> {
    let payload = message.payload()?;
    serde_json::from_slice::<Checkpoint>(payload).ok()
}

#[async_trait]
impl EventSink for KafkaEventProducer {
    async fn publish_batch(&self, batch: &LedgerBatch) -> Result<()> {
        self.commit_batch_transactional(batch)
    }

    async fn last_committed_ledger(&self) -> Result<Option<u64>> {
        Ok(self.read_last_checkpoint().await?.map(|c| c.ledger))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fnv1a_64_stability() {
        // Same constants as ADR-0005 / src/kafka/producer.rs.
        assert_eq!(fnv1a_64(b"hello", 0), 0xa430d84680aabd0b);
        assert_eq!(fnv1a_64(b"hello", 0), fnv1a_64(b"hello", 0));
        assert_ne!(fnv1a_64(b"hello", 0), fnv1a_64(b"hellp", 0));
    }

    #[test]
    fn test_partition_counter_range() {
        let counter = PartitionCounter::new(12, 42);
        for key in ["USDC:GI", "XLM", "CCONTRACT", "42"] {
            let p = counter.partition(key);
            assert!(p < 12, "partition {p} out of range for key {key}");
        }
    }

    #[test]
    fn test_partition_counter_zero_count_is_clamped() {
        let counter = PartitionCounter::new(0, 1);
        assert_eq!(counter.partition("k"), 0);
        counter.set_count(0);
        assert_eq!(counter.partition("k"), 0);
    }

    #[test]
    fn test_checkpoint_serde() {
        let checkpoint = Checkpoint {
            ledger: 99,
            ledger_hash: "abc".to_string(),
            committed_at: Utc::now(),
        };
        let json = serde_json::to_string(&checkpoint).unwrap();
        assert!(json.contains("\"ledger\":99"));
        let back: Checkpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back.ledger, 99);
    }

    #[test]
    fn test_transactional_id_prefix() {
        let config = EventBridgeConfig {
            bridge_id: "hz-1".to_string(),
            ..Default::default()
        };
        assert_eq!(config.transactional_id(), "event-bridge-hz-1");
    }

    // The remaining tests require a live broker; see tests/ and
    // scripts/validate-event-bridge.sh for the Redpanda-based validation.
}
