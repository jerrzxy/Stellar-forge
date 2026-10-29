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
//! Kafka Event Streaming Bridge for Horizon (issue #292).
//!
//! Streams Stellar ledger events — ledger closes, token transfers, and Soroban
//! contract events — from a Horizon ingestion endpoint straight into Kafka with
//! **exactly-once** delivery semantics, so downstream enterprise systems never
//! observe a duplicated or lost event.
//!
//! # Architecture
//!
//! ```text
//! ┌───────────────────────┐
//! │ Captive Core /        │
//! │ Horizon (ingestion)   │
//! └──────────┬────────────┘
//!            │ REST (ledger resources: transactions, operations, effects)
//!            ▼
//! ┌───────────────────────┐     ┌──────────────────────┐
//! │ horizon.rs            │────▶│ xdr_transformer.rs   │
//! │ EventSource           │     │ XdrTransformer       │
//! └───────────────────────┘     └──────────┬───────────┘
//!                                          │ Vec<LedgerEvent>
//!                                          ▼
//!                            ┌──────────────────────────┐
//!                            │ bridge.rs                │
//!                            │ per-ledger orchestration │
//!                            └──────────┬───────────────┘
//!                                       │ EventSink::publish_batch
//!                                       ▼
//!                            ┌──────────────────────────┐
//!                            │ kafka_producer.rs        │
//!                            │ transactional producer   │
//!                            └──────────┬───────────────┘
//!                                       │ Kafka transaction
//!                                       ▼
//!                            ┌──────────────────────────┐
//!                            │ Kafka / Redpanda         │
//!                            │ topic + checkpoint topic │
//!                            └──────────────────────────┘
//! ```
//!
//! # Exactly-once delivery
//!
//! Every ledger is published inside a single Kafka transaction together with a
//! checkpoint record (see [`kafka_producer`]). Consumers must be configured
//! with `isolation.level=read_committed` to honor the atomicity boundary.
//! A restart resumes from the last *committed* checkpoint, so ledgers are never
//! replayed and never skipped.
//!
//! # Feature flags
//!
//! * `kafka` (default) — enables the transactional Kafka producer. Without it
//!   the crate still compiles and the transformer/tests can be used standalone.

pub mod bridge;
pub mod config;
pub mod error;
pub mod event;
pub mod horizon;
pub mod sink;
pub mod xdr_transformer;

#[cfg(feature = "kafka")]
pub mod kafka_producer;

pub use bridge::{BridgeStats, EventBridge};
pub use config::{EventBridgeConfig, EventFormat, PartitionBy};
pub use error::{EventBridgeError, Result};
pub use event::{AssetId, EventPayload, LedgerEvent};
pub use horizon::{EventSource, HorizonRestSource};
pub use sink::{EventSink, MemorySink};
pub use xdr_transformer::XdrTransformer;

#[cfg(feature = "kafka")]
pub use kafka_producer::{Checkpoint, KafkaEventProducer};
