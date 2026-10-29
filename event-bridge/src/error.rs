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
//! Error types for the event bridge.

use thiserror::Error;

/// Convenience result alias used across the crate.
pub type Result<T> = std::result::Result<T, EventBridgeError>;

/// Errors surfaced by the event bridge pipeline.
#[derive(Debug, Error)]
pub enum EventBridgeError {
    /// Horizon ingestion endpoint returned an error or unparsable body.
    #[error("Horizon error: {0}")]
    Horizon(String),

    /// Failed to transform a ledger resource into events.
    #[error("transform error: {0}")]
    Transform(String),

    /// Kafka producer / broker error.
    #[error("Kafka error: {0}")]
    Kafka(String),

    /// Serialization failure (event envelope or checkpoint).
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// HTTP transport failure talking to Horizon.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// Invalid configuration supplied to the bridge.
    #[error("config error: {0}")]
    Config(String),

    /// An I/O error (JSONL output, etc).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(feature = "kafka")]
impl From<rdkafka::error::KafkaError> for EventBridgeError {
    fn from(e: rdkafka::error::KafkaError) -> Self {
        EventBridgeError::Kafka(e.to_string())
    }
}
