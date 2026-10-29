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
//! Event source: the ingestion-side tap.
//!
//! [`EventSource`] abstracts where ledger resources come from. The shipped
//! implementation, [`HorizonRestSource`], reads the ledger resources Horizon
//! derives from Captive Core's execution stream (`GET /ledgers/{seq}` with
//! embedded transactions, operations and effects) — the same data Horizon's
//! Redis ingestion cache is built from. Alternative taps (a direct Redis
//! consumer, a captive-core XDR pipe) implement the same trait.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{EventBridgeError, Result};

/// A closed ledger with everything needed to derive events.
///
/// Field names mirror Horizon's wire format, which is snake_case
/// (e.g. `closed_at`, `fee_pool`, `max_tx_set_size`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HorizonLedgerResource {
    /// Ledger sequence number.
    pub sequence: u64,
    /// Ledger hash (hex).
    pub hash: String,
    /// Close time (RFC 3339); Horizon may report `null` for the newest LED.
    #[serde(default)]
    pub closed_at: Option<DateTime<Utc>>,
    /// Total fees collected (stroops, string).
    #[serde(default)]
    pub fee_pool: Option<String>,
    /// Maximum operations the ledger could hold.
    #[serde(default)]
    pub max_tx_set_size: Option<u32>,
    /// Transactions included in the ledger (when embedded).
    #[serde(default)]
    pub transactions: Vec<HorizonTransaction>,
}

/// One transaction of a closed ledger.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct HorizonTransaction {
    /// Transaction hash (hex).
    pub hash: String,
    /// Account that submitted the transaction.
    #[serde(default)]
    pub source_account: Option<String>,
    /// Base64 XDR envelope (fidelity passthrough).
    #[serde(default)]
    pub envelope_xdr: Option<String>,
    /// Base64 XDR result (fidelity passthrough).
    #[serde(default)]
    pub result_xdr: Option<String>,
    /// Embedded operations (with `?operations=true`).
    #[serde(default)]
    pub operations: Vec<HorizonOperation>,
    /// Embedded effects (with `?effects=true`).
    #[serde(default)]
    pub effects: Vec<HorizonEffect>,
}

/// One operation of a transaction. Field names follow Horizon's JSON.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct HorizonOperation {
    /// Horizon operation type (`payment`, `create_account`, ...).
    #[serde(rename = "type", default)]
    pub operation_type: Option<String>,
    /// Operation source (falls back to the transaction source).
    #[serde(default)]
    pub source_account: Option<String>,
    /// Payer for `payment` operations.
    #[serde(default)]
    pub from: Option<String>,
    /// Payee for `payment` operations.
    #[serde(default)]
    pub to: Option<String>,
    /// Decimal amount exactly as Horizon reports it.
    #[serde(default)]
    pub amount: Option<String>,
    /// Asset code; absent for the native asset.
    #[serde(default)]
    pub asset_code: Option<String>,
    /// Asset issuer; absent for the native asset.
    #[serde(default)]
    pub asset_issuer: Option<String>,
}

/// One effect of a transaction. Field names follow Horizon's JSON.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct HorizonEffect {
    /// Horizon effect type (`account_credited`, `contract_event`, ...).
    #[serde(rename = "type", default)]
    pub effect_type: Option<String>,
    /// Emitting contract (for `contract_event` effects).
    #[serde(default)]
    pub contract_id: Option<String>,
    /// Soroban event type (`fn`, `diagnostic`, ...).
    #[serde(default)]
    pub event_type: Option<String>,
    /// Base64 XDR topics (for `contract_event` effects).
    #[serde(default)]
    pub topic: Option<Vec<String>>,
    /// Base64 XDR event data (for `contract_event` effects).
    #[serde(default)]
    pub data_xdr: Option<String>,
}

/// Response of `GET /ledgers?order=desc&limit=1`.
#[derive(Debug, Deserialize)]
pub struct HorizonLedgersResponse {
    #[serde(rename = "_embedded")]
    pub embedded: HorizonEmbedded,
}

/// Horizon HAL embedded records.
#[derive(Debug, Deserialize)]
pub struct HorizonEmbedded {
    pub records: Vec<HorizonLedgerSummary>,
}

/// Summary record of `GET /ledgers`.
#[derive(Debug, Deserialize)]
pub struct HorizonLedgerSummary {
    pub sequence: u64,
}

/// Where ledger resources come from.
#[async_trait]
pub trait EventSource: Send + Sync {
    /// Sequence number of the latest closed ledger.
    async fn latest_ledger(&self) -> Result<u64>;

    /// Fetch one closed ledger with its transactions, operations and effects.
    async fn fetch_ledger(&self, sequence: u64) -> Result<HorizonLedgerResource>;
}

/// Horizon REST ingestion tap.
#[derive(Debug, Clone)]
pub struct HorizonRestSource {
    client: reqwest::Client,
    base_url: String,
}

impl HorizonRestSource {
    /// Create a source with the given base URL and request timeout.
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| EventBridgeError::Horizon(e.to_string()))?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    fn ledger_url(&self, sequence: u64) -> String {
        format!(
            "{}/ledgers/{}?transactions=true&operations=true&effects=true",
            self.base_url, sequence
        )
    }
}

#[async_trait]
impl EventSource for HorizonRestSource {
    async fn latest_ledger(&self) -> Result<u64> {
        let url = format!("{}/ledgers?order=desc&limit=1", self.base_url);
        let response = self
            .client
            .get(&url)
            .send()
            .await?
            .error_for_status()?;
        let body: HorizonLedgersResponse = response.json().await?;
        body.embedded
            .records
            .first()
            .map(|r| r.sequence)
            .ok_or_else(|| EventBridgeError::Horizon("empty ledger list".to_string()))
    }

    async fn fetch_ledger(&self, sequence: u64) -> Result<HorizonLedgerResource> {
        let response = self
            .client
            .get(self.ledger_url(sequence))
            .send()
            .await?
            .error_for_status()?;
        let ledger: HorizonLedgerResource = response.json().await?;
        if ledger.sequence != sequence {
            return Err(EventBridgeError::Horizon(format!(
                "requested ledger {sequence} but Horizon returned {}",
                ledger.sequence
            )));
        }
        Ok(ledger)
    }
}

/// Deterministic synthetic ledger generator used by load tests, examples and
/// the `load-test` subcommand. Produces the same wire shape Horizon does, so
/// the full transformer path is exercised.
pub mod synthetic {
    use super::*;

    /// Build a synthetic ledger with `payments` payment operations and
    /// `contract_events` Soroban events spread over `tx_count` transactions.
    pub fn ledger(sequence: u64, tx_count: u32, payments: u32, contract_events: u32) -> HorizonLedgerResource {
        let hash = format!("synthetic-ledger-{sequence:016x}");
        let closed_at = Utc::now();

        if tx_count == 0 {
            return HorizonLedgerResource {
                sequence,
                hash,
                closed_at: Some(closed_at),
                fee_pool: Some("2100".to_string()),
                max_tx_set_size: Some(1000),
                transactions: Vec::new(),
            };
        }

        let mut transactions = Vec::with_capacity(tx_count as usize);
        for tx_index in 0..tx_count {
            let tx_hash = format!("{hash}-tx-{tx_index:08x}");
            let mut operations = Vec::new();
            let mut effects = Vec::new();

            // Spread the payments across transactions (round-robin).
            for i in 0..payments {
                if i % tx_count != tx_index {
                    continue;
                }
                operations.push(HorizonOperation {
                    operation_type: Some("payment".to_string()),
                    source_account: None,
                    from: Some(format!("GSENDER{:04}", i % 1000)),
                    to: Some(format!("GRECIPIENT{:04}", i % 1000)),
                    amount: Some("10.0000000".to_string()),
                    // Rotate through a few assets so partition keys vary.
                    asset_code: match i % 3 {
                        0 => None,
                        1 => Some("USDC".to_string()),
                        _ => Some("EURT".to_string()),
                    },
                    asset_issuer: match i % 3 {
                        0 | 1 => None,
                        _ => Some("GISSUER0".to_string()),
                    },
                });
            }

            for i in 0..contract_events {
                if i % tx_count != tx_index {
                    continue;
                }
                effects.push(HorizonEffect {
                    effect_type: Some("contract_event".to_string()),
                    contract_id: Some(format!("CCONTRACT{:04}", i % 100)),
                    event_type: Some("fn".to_string()),
                    topic: Some(vec![format!("dG9waWM=:{i}")]),
                    data_xdr: Some("AAAA".to_string()),
                });
            }

            transactions.push(HorizonTransaction {
                hash: tx_hash,
                source_account: Some(format!("GSOURCE{:04}", tx_index % 1000)),
                envelope_xdr: Some("AAAAENv1QzE=".to_string()),
                result_xdr: Some("AAAA".to_string()),
                operations,
                effects,
            });
        }

        HorizonLedgerResource {
            sequence,
            hash,
            closed_at: Some(closed_at),
            fee_pool: Some("2100".to_string()),
            max_tx_set_size: Some(1000),
            transactions,
        }
    }

    /// In-memory [`EventSource`] serving synthetic ledgers; used by unit
    /// tests and offline dry runs. Each ledger carries 3 payment operations
    /// and 1 contract event over 3 transactions (5 events including the
    /// ledger-close event).
    #[derive(Debug, Clone)]
    pub struct SyntheticSource {
        latest: u64,
    }

    impl SyntheticSource {
        /// Serve synthetic ledgers `1..=latest`.
        pub fn new(latest: u64) -> Self {
            Self { latest }
        }
    }

    #[async_trait]
    impl EventSource for SyntheticSource {
        async fn latest_ledger(&self) -> Result<u64> {
            Ok(self.latest)
        }

        async fn fetch_ledger(&self, sequence: u64) -> Result<HorizonLedgerResource> {
            if sequence == 0 || sequence > self.latest {
                return Err(EventBridgeError::Horizon(format!(
                    "ledger {sequence} out of synthetic range 1..={}",
                    self.latest
                )));
            }
            Ok(ledger(sequence, 3, 3, 1))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parses_horizon_ledger_shape() {
        // Horizon serves snake_case JSON, e.g. `closed_at` / `fee_pool`.
        let json = r#"{
            "sequence": 7,
            "hash": "abc",
            "closed_at": "2026-01-01T00:00:00Z",
            "fee_pool": "1",
            "max_tx_set_size": 500,
            "transactions": []
        }"#;
        let parsed: HorizonLedgerResource = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.sequence, 7);
        assert_eq!(parsed.max_tx_set_size, Some(500));
        assert!(parsed.closed_at.is_some());
    }

    #[test]
    fn test_parses_minimal_ledger() {
        // Newest LEDs may omit closed_at; unknown fields must be ignored.
        let json = r#"{
            "sequence": 8,
            "hash": "def",
            "closed_at": null,
            "paging_token": "8",
            "protocol_version": 21
        }"#;
        let parsed: HorizonLedgerResource = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.sequence, 8);
        assert!(parsed.closed_at.is_none());
        assert!(parsed.transactions.is_empty());
    }

    #[test]
    fn test_operation_type_aliased() {
        let json = r#"{"type": "payment", "from": "G1", "to": "G2", "amount": "1.0000000"}"#;
        let op: HorizonOperation = serde_json::from_str(json).unwrap();
        assert_eq!(op.operation_type.as_deref(), Some("payment"));
        assert_eq!(op.from.as_deref(), Some("G1"));
    }

    #[test]
    fn test_synthetic_ledger_shape() {
        let ledger = synthetic::ledger(100, 10, 100, 20);
        assert_eq!(ledger.sequence, 100);
        assert_eq!(ledger.transactions.len(), 10);
        let payments: usize = ledger
            .transactions
            .iter()
            .map(|tx| tx.operations.len())
            .sum();
        let events: usize = ledger.transactions.iter().map(|tx| tx.effects.len()).sum();
        assert_eq!(payments, 100);
        assert_eq!(events, 20);
    }
}
