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
//! Canonical ledger event envelope streamed to Kafka.
//!
//! Every event carries a deterministic `event_id` derived from the ledger
//! hash and the position of the event inside the ledger. Downstream consumers
//! can therefore deduplicate or idempotently upsert without any extra
//! coordination, even if an operator replays a ledger range by hand.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::PartitionBy;

/// An asset denomination for token transfers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum AssetId {
    /// Lumens (XLM).
    Native,
    /// issued credit (asset code + issuer account).
    Credit {
        code: String,
        issuer: String,
    },
}

impl AssetId {
    /// Human-readable asset code; `XLM` for the native asset.
    pub fn code(&self) -> &str {
        match self {
            AssetId::Native => "XLM",
            AssetId::Credit { code, .. } => code.as_str(),
        }
    }

    /// Stable string used as the Kafka partition key for `PartitionBy::Asset`.
    /// Credits disambiguate identical codes from different issuers.
    pub fn partition_code(&self) -> String {
        match self {
            AssetId::Native => "XLM".to_string(),
            AssetId::Credit { code, issuer } => format!("{}:{}", code, issuer),
        }
    }
}

/// The payload carried by a [`LedgerEvent`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum EventPayload {
    /// A ledger closed; emitted exactly once per ledger.
    LedgerClose {
        /// Number of transactions in the ledger set.
        tx_count: u32,
        /// Total fees collected by the ledger (stroops, as string).
        #[serde(skip_serializing_if = "Option::is_none")]
        fee_pool: Option<String>,
        /// Max operations the ledger could hold.
        #[serde(skip_serializing_if = "Option::is_none")]
        max_tx_set_size: Option<u32>,
    },
    /// A payment operation moved value between accounts.
    TokenTransfer {
        /// Source account of the transfer.
        from: String,
        /// Destination account of the transfer.
        to: String,
        /// Asset that moved.
        asset: AssetId,
        /// Amount in decimal units exactly as Horizon reports it (7 dp).
        amount: String,
        /// Amount converted to stroops (integer string) for exact arithmetic.
        amount_stroops: String,
        /// Hash of the transaction that carried the payment.
        tx_hash: String,
        /// Operation index inside the transaction (0-based).
        operation_index: u32,
    },
    /// A Soroban contract event.
    ContractEvent {
        /// Contract that emitted the event.
        contract_id: String,
        /// Event type (`fn`, `diagnostic`, ...).
        #[serde(skip_serializing_if = "Option::is_none")]
        event_type: Option<String>,
        /// Base64-encoded XDR topics.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        topics: Vec<String>,
        /// Base64-encoded XDR event data.
        #[serde(skip_serializing_if = "Option::is_none")]
        data_xdr: Option<String>,
        /// Hash of the transaction that carried the event.
        tx_hash: String,
        /// Index of the contract event inside the transaction (0-based).
        event_index: u32,
    },
}

impl EventPayload {
    /// Short discriminator used when hashing event IDs.
    pub fn kind(&self) -> &'static str {
        match self {
            EventPayload::LedgerClose { .. } => "ledger_close",
            EventPayload::TokenTransfer { .. } => "token_transfer",
            EventPayload::ContractEvent { .. } => "contract_event",
        }
    }
}

/// One ledger-derived event ready for Kafka.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerEvent {
    /// Deterministic SHA-256 id: identical ledger content ⇒ identical id.
    pub event_id: String,
    /// Ledger sequence this event belongs to.
    pub ledger: u64,
    /// Ledger hash (hex) as reported by Horizon.
    pub ledger_hash: String,
    /// Ledger close time.
    pub close_time: DateTime<Utc>,
    /// The event body.
    #[serde(flatten)]
    pub payload: EventPayload,
    /// Base64 envelope XDR of the source transaction (fidelity passthrough).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope_xdr: Option<String>,
    /// Base64 result XDR of the source transaction (fidelity passthrough).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_xdr: Option<String>,
}

impl LedgerEvent {
    /// Build an event with a deterministic `event_id`.
    ///
    /// `discriminator` must uniquely identify the event inside the ledger
    /// (e.g. `close`, `tx:<hash>:transfer:<idx>`, `tx:<hash>:contract:<idx>`).
    pub fn new(
        ledger: u64,
        ledger_hash: &str,
        close_time: DateTime<Utc>,
        discriminator: &str,
        payload: EventPayload,
    ) -> Self {
        let event_id = Self::compute_event_id(ledger_hash, discriminator);
        Self {
            event_id,
            ledger,
            ledger_hash: ledger_hash.to_string(),
            close_time,
            payload,
            envelope_xdr: None,
            result_xdr: None,
        }
    }

    /// SHA-256 hex id over `<ledger_hash>:<discriminator>`.
    pub fn compute_event_id(ledger_hash: &str, discriminator: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(ledger_hash.as_bytes());
        hasher.update(b":");
        hasher.update(discriminator.as_bytes());
        hex::encode(hasher.finalize())
    }

    /// Kafka partition key for the configured strategy.
    pub fn partition_key(&self, strategy: PartitionBy) -> String {
        match (strategy, &self.payload) {
            (PartitionBy::Asset, EventPayload::TokenTransfer { asset, .. }) => {
                asset.partition_code()
            }
            (PartitionBy::Contract, EventPayload::ContractEvent { contract_id, .. }) => {
                contract_id.clone()
            }
            _ => self.ledger.to_string(),
        }
    }
}

/// Convert a Horizon decimal amount (string, 7 decimal places) into stroops
/// (integer string) without touching floating point, so no precision is lost.
///
/// Returns `None` when the input is not a plain decimal number.
pub fn to_stroops(amount: &str) -> Option<String> {
    let (int_part, frac_part) = match amount.split_once('.') {
        Some((i, f)) => (i, f),
        None => (amount, ""),
    };
    if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if frac_part.len() > 7 || !frac_part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut frac = frac_part.to_string();
    while frac.len() < 7 {
        frac.push('0');
    }
    let combined = format!("{}{}", int_part, frac);
    // Strip leading zeros but keep at least one digit.
    let trimmed = combined.trim_start_matches('0');
    Some(if trimmed.is_empty() { "0".to_string() } else { trimmed.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_id_is_deterministic() {
        let a = LedgerEvent::compute_event_id("abc", "close");
        let b = LedgerEvent::compute_event_id("abc", "close");
        let c = LedgerEvent::compute_event_id("abc", "other");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64); // sha256 hex
    }

    #[test]
    fn test_partition_key_by_asset() {
        let event = LedgerEvent::new(
            42,
            "hash",
            Utc::now(),
            "tx:t:transfer:0",
            EventPayload::TokenTransfer {
                from: "GA".into(),
                to: "GB".into(),
                asset: AssetId::Credit {
                    code: "USDC".into(),
                    issuer: "GI".into(),
                },
                amount: "10.0000000".into(),
                amount_stroops: "100000000".into(),
                tx_hash: "t".into(),
                operation_index: 0,
            },
        );
        assert_eq!(event.partition_key(PartitionBy::Asset), "USDC:GI");
        assert_eq!(event.partition_key(PartitionBy::Ledger), "42");
    }

    #[test]
    fn test_partition_key_native_asset() {
        let event = LedgerEvent::new(
            7,
            "hash",
            Utc::now(),
            "close",
            EventPayload::LedgerClose {
                tx_count: 3,
                fee_pool: None,
                max_tx_set_size: None,
            },
        );
        assert_eq!(event.partition_key(PartitionBy::Asset), "7");
    }

    #[test]
    fn test_partition_key_by_contract() {
        let event = LedgerEvent::new(
            9,
            "hash",
            Utc::now(),
            "tx:t:contract:0",
            EventPayload::ContractEvent {
                contract_id: "CCONTRACT".into(),
                event_type: Some("fn".into()),
                topics: vec!["dG9waWM=".into()],
                data_xdr: None,
                tx_hash: "t".into(),
                event_index: 0,
            },
        );
        assert_eq!(event.partition_key(PartitionBy::Contract), "CCONTRACT");
    }

    #[test]
    fn test_to_stroops_exact() {
        assert_eq!(to_stroops("100.0000000").as_deref(), Some("100000000"));
        assert_eq!(to_stroops("0.0000001").as_deref(), Some("1"));
        assert_eq!(to_stroops("1.5").as_deref(), Some("15000000"));
        assert_eq!(to_stroops("12345678901.1234567").as_deref(), Some("123456789011234567"));
        assert_eq!(to_stroops("0").as_deref(), Some("0"));
        assert_eq!(to_stroops("1.12345678"), None); // 8 dp: too precise
        assert_eq!(to_stroops("abc"), None);
        assert_eq!(to_stroops(""), None);
    }

    #[test]
    fn test_event_serde_roundtrip() {
        let event = LedgerEvent::new(
            1,
            "hash",
            Utc::now(),
            "close",
            EventPayload::LedgerClose {
                tx_count: 1,
                fee_pool: Some("100".into()),
                max_tx_set_size: Some(1000),
            },
        );
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"kind\":\"ledgerClose\""));
        let back: LedgerEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }
}
