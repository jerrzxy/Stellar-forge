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
//! Configuration for the Kafka event streaming bridge.

use serde::{Deserialize, Serialize};

use crate::error::{EventBridgeError, Result};

/// Wire format used for event payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventFormat {
    /// Canonical JSON envelope (lossless, schema in `schemas/ledger-event.avsc`).
    Json,
    /// Avro-compatible framing. Until the schema-registry encoder lands (same
    /// state as the SCP analytics pipeline, see
    /// `src/controller/quorum/scp_kafka_stream.rs`), the record body is the
    /// canonical JSON encoding of the event so no data is lost.
    Avro,
}

impl Default for EventFormat {
    fn default() -> Self {
        EventFormat::Json
    }
}

/// Kafka partitioning strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PartitionBy {
    /// Partition by asset code (`XLM` for native, `CODE:ISSUER` for credits).
    Asset,
    /// Partition by Soroban contract ID.
    Contract,
    /// Partition by ledger sequence (all events of one ledger on one partition).
    Ledger,
}

impl Default for PartitionBy {
    fn default() -> Self {
        PartitionBy::Asset
    }
}

/// SASL authentication configuration for the Kafka producer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SaslConfig {
    /// PLAIN, SCRAM-SHA-256 or SCRAM-SHA-512.
    pub mechanism: String,
    pub username: String,
    pub password: String,
}

/// Full configuration for one bridge instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventBridgeConfig {
    /// Bridge instance identifier; part of the Kafka `transactional.id` so a
    /// restarted instance fences its zombie predecessor (exactly-once).
    pub bridge_id: String,

    /// Horizon base URL, e.g. `http://horizon.stellar:8000`.
    pub horizon_url: String,

    /// Kafka bootstrap servers (comma-separated).
    pub bootstrap_servers: String,

    /// Topic that carries the ledger events.
    #[serde(default = "default_topic")]
    pub topic: String,

    /// Compacted topic carrying ledger checkpoints, written inside the same
    /// Kafka transaction as the events themselves.
    #[serde(default = "default_checkpoint_topic")]
    pub checkpoint_topic: String,

    /// Wire format for event payloads.
    #[serde(default)]
    pub format: EventFormat,

    /// Partitioning strategy (by asset or contract ID).
    #[serde(default)]
    pub partition_by: PartitionBy,

    /// FNV-1a seed used for partition selection (mirrors ADR-0005).
    #[serde(default = "default_partition_seed")]
    pub partition_seed: u64,

    /// How often partition counts are re-discovered from broker metadata.
    #[serde(default = "default_metadata_refresh_secs")]
    pub metadata_refresh_interval_secs: u64,

    /// Poll interval for new ledgers while tailing.
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,

    /// Ledger to start from when no checkpoint exists.
    #[serde(default = "default_starting_ledger")]
    pub starting_ledger: u64,

    /// Maximum retries for a failed ledger publish before giving up.
    #[serde(default = "default_max_publish_retries")]
    pub max_publish_retries: u32,

    /// Base backoff (seconds) between publish retries; grows exponentially.
    #[serde(default = "default_retry_backoff_secs")]
    pub retry_backoff_secs: u64,

    /// Timeout for the initial `init_transactions` handshake.
    #[serde(default = "default_transaction_timeout_secs")]
    pub transaction_timeout_secs: u64,

    /// Kafka transaction timeout in milliseconds (`transaction.timeout.ms`).
    #[serde(default = "default_tx_timeout_ms")]
    pub kafka_transaction_timeout_ms: String,

    /// Message compression: none, gzip, snappy, lz4, zstd.
    #[serde(default = "default_compression")]
    pub compression: String,

    /// Batch size in bytes (`batch.size`).
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,

    /// Linger time in milliseconds (`linger.ms`).
    #[serde(default = "default_linger_ms")]
    pub linger_ms: u64,

    /// Timeout for Horizon HTTP requests.
    #[serde(default = "default_http_timeout_secs")]
    pub http_timeout_secs: u64,

    /// Optional SASL configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sasl: Option<SaslConfig>,
}

fn default_topic() -> String {
    "stellar-ledger-events".to_string()
}

fn default_checkpoint_topic() -> String {
    "stellar-ledger-events.checkpoints".to_string()
}

fn default_partition_seed() -> u64 {
    0xcbf2_9ce4_8422_2325
}

fn default_metadata_refresh_secs() -> u64 {
    15
}

fn default_poll_interval_secs() -> u64 {
    1
}

fn default_starting_ledger() -> u64 {
    1
}

fn default_max_publish_retries() -> u32 {
    5
}

fn default_retry_backoff_secs() -> u64 {
    1
}

fn default_transaction_timeout_secs() -> u64 {
    30
}

fn default_tx_timeout_ms() -> String {
    "60000".to_string()
}

fn default_compression() -> String {
    "snappy".to_string()
}

fn default_batch_size() -> usize {
    1_000_000
}

fn default_linger_ms() -> u64 {
    5
}

fn default_http_timeout_secs() -> u64 {
    30
}

impl Default for EventBridgeConfig {
    fn default() -> Self {
        Self {
            bridge_id: "event-bridge-0".to_string(),
            horizon_url: "http://localhost:8000".to_string(),
            bootstrap_servers: "localhost:9092".to_string(),
            topic: default_topic(),
            checkpoint_topic: default_checkpoint_topic(),
            format: EventFormat::default(),
            partition_by: PartitionBy::default(),
            partition_seed: default_partition_seed(),
            metadata_refresh_interval_secs: default_metadata_refresh_secs(),
            poll_interval_secs: default_poll_interval_secs(),
            starting_ledger: default_starting_ledger(),
            max_publish_retries: default_max_publish_retries(),
            retry_backoff_secs: default_retry_backoff_secs(),
            transaction_timeout_secs: default_transaction_timeout_secs(),
            kafka_transaction_timeout_ms: default_tx_timeout_ms(),
            compression: default_compression(),
            batch_size: default_batch_size(),
            linger_ms: default_linger_ms(),
            http_timeout_secs: default_http_timeout_secs(),
            sasl: None,
        }
    }
}

impl EventBridgeConfig {
    /// Validate cross-field invariants.
    pub fn validate(&self) -> Result<()> {
        if self.bridge_id.trim().is_empty() {
            return Err(EventBridgeError::Config("bridge_id must not be empty".into()));
        }
        if self.bootstrap_servers.trim().is_empty() {
            return Err(EventBridgeError::Config(
                "bootstrap_servers must not be empty".into(),
            ));
        }
        if self.topic.trim().is_empty() {
            return Err(EventBridgeError::Config("topic must not be empty".into()));
        }
        if self.checkpoint_topic == self.topic {
            return Err(EventBridgeError::Config(
                "checkpoint_topic must differ from topic".into(),
            ));
        }
        if self.starting_ledger == 0 {
            return Err(EventBridgeError::Config(
                "starting_ledger must be >= 1".into(),
            ));
        }
        Ok(())
    }

    /// The Kafka `transactional.id` for this instance. Stable across restarts
    /// of the same instance so that a zombie producer is fenced.
    pub fn transactional_id(&self) -> String {
        format!("event-bridge-{}", self.bridge_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_is_valid() {
        let config = EventBridgeConfig::default();
        assert!(config.validate().is_ok());
        assert_eq!(config.topic, "stellar-ledger-events");
        assert_eq!(
            config.checkpoint_topic,
            "stellar-ledger-events.checkpoints"
        );
        assert_eq!(config.format, EventFormat::Json);
        assert_eq!(config.partition_by, PartitionBy::Asset);
    }

    #[test]
    fn test_transactional_id_is_stable() {
        let config = EventBridgeConfig {
            bridge_id: "bridge-a".to_string(),
            ..Default::default()
        };
        assert_eq!(config.transactional_id(), "event-bridge-bridge-a");
    }

    #[test]
    fn test_checkpoint_topic_must_differ() {
        let config = EventBridgeConfig {
            checkpoint_topic: "same-topic".to_string(),
            topic: "same-topic".to_string(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_serde_roundtrip_camel_case() {
        let config = EventBridgeConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("bootstrapServers"));
        assert!(json.contains("checkpointTopic"));
        let parsed: EventBridgeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.topic, config.topic);
    }
}
