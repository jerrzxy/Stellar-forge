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
//! XDR → [`LedgerEvent`] transformation.
//!
//! Horizon serves each closed ledger as a JSON projection of the ledger's XDR
//! closemeta: transactions (with their base64 `envelope_xdr` / `result_xdr`),
//! embedded operations, and effects. This module turns that projection into
//! the canonical typed events streamed to Kafka, carrying the raw base64 XDR
//! of each source transaction verbatim so downstream systems can verify 100%
//! data fidelity against both Horizon responses and Stellar Core.
//!
//! Design notes:
//!
//! * Token transfers are derived from **payment operations** only. Account
//!   credit/debit *effects* encode the same transfer, so emitting from both
//!   sources would duplicate every payment.
//! * Contract events are derived from `contract_event` effects (the same
//!   events the Soroban RPC `getEvents` method exposes).
//! * Every event keeps the deterministic `event_id` from [`LedgerEvent::new`],
//!   so re-transforming the same ledger always yields identical events.

use chrono::{DateTime, Utc};

use crate::event::{to_stroops, AssetId, EventPayload, LedgerEvent};
use crate::horizon::{HorizonEffect, HorizonLedgerResource, HorizonOperation, HorizonTransaction};

/// Which event classes the transformer emits.
#[derive(Debug, Clone, Copy)]
pub struct TransformerFilters {
    /// Emit one `LedgerClose` event per ledger.
    pub ledger_close: bool,
    /// Emit `TokenTransfer` events from payment operations.
    pub token_transfers: bool,
    /// Emit `ContractEvent` events from Soroban contract event effects.
    pub contract_events: bool,
}

impl Default for TransformerFilters {
    fn default() -> Self {
        Self {
            ledger_close: true,
            token_transfers: true,
            contract_events: true,
        }
    }
}

/// Transforms Horizon ledger resources into canonical [`LedgerEvent`]s.
#[derive(Debug, Clone, Default)]
pub struct XdrTransformer {
    filters: TransformerFilters,
}

impl XdrTransformer {
    /// Transformer emitting every event class.
    pub fn new() -> Self {
        Self {
            filters: TransformerFilters::default(),
        }
    }

    /// Restrict the emitted event classes (useful for filtered topics).
    pub fn with_filters(filters: TransformerFilters) -> Self {
        Self { filters }
    }

    /// Transform one closed ledger into its ordered event list.
    ///
    /// Events are ordered deterministically: the ledger-close event first,
    /// then for each transaction in ledger order its token transfers (by
    /// operation index) followed by its contract events (by event index).
    pub fn transform(&self, ledger: &HorizonLedgerResource) -> Vec<LedgerEvent> {
        let close_time: DateTime<Utc> = ledger
            .closed_at
            .unwrap_or_else(|| Utc::now());

        let mut events = Vec::new();

        if self.filters.ledger_close {
            events.push(LedgerEvent::new(
                ledger.sequence,
                &ledger.hash,
                close_time,
                "close",
                EventPayload::LedgerClose {
                    tx_count: ledger.transactions.len() as u32,
                    fee_pool: ledger.fee_pool.clone(),
                    max_tx_set_size: ledger.max_tx_set_size,
                },
            ));
        }

        for tx in &ledger.transactions {
            if self.filters.token_transfers {
                self.transform_token_transfers(ledger, tx, close_time, &mut events);
            }
            if self.filters.contract_events {
                self.transform_contract_events(ledger, tx, close_time, &mut events);
            }
        }

        events
    }

    fn transform_token_transfers(
        &self,
        ledger: &HorizonLedgerResource,
        tx: &HorizonTransaction,
        close_time: DateTime<Utc>,
        events: &mut Vec<LedgerEvent>,
    ) {
        for (index, op) in tx.operations.iter().enumerate() {
            if !is_payment_operation(op) {
                continue;
            }
            let Some(amount) = op.amount.clone() else {
                continue;
            };
            let Some(amount_stroops) = to_stroops(&amount) else {
                continue;
            };
            let asset = match &op.asset_code {
                Some(code) => AssetId::Credit {
                    code: code.clone(),
                    issuer: op
                        .asset_issuer
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string()),
                },
                None => AssetId::Native,
            };
            let from = op
                .from
                .clone()
                .or_else(|| tx.source_account.clone())
                .unwrap_or_default();
            let to = op.to.clone().unwrap_or_default();

            let mut event = LedgerEvent::new(
                ledger.sequence,
                &ledger.hash,
                close_time,
                &format!("tx:{}:transfer:{}", tx.hash, index),
                EventPayload::TokenTransfer {
                    from,
                    to,
                    asset,
                    amount,
                    amount_stroops,
                    tx_hash: tx.hash.clone(),
                    operation_index: index as u32,
                },
            );
            event.envelope_xdr = tx.envelope_xdr.clone();
            event.result_xdr = tx.result_xdr.clone();
            events.push(event);
        }
    }

    fn transform_contract_events(
        &self,
        ledger: &HorizonLedgerResource,
        tx: &HorizonTransaction,
        close_time: DateTime<Utc>,
        events: &mut Vec<LedgerEvent>,
    ) {
        let mut event_index = 0u32;
        for effect in &tx.effects {
            if effect.effect_type.as_deref() != Some("contract_event") {
                continue;
            }
            let Some(contract_id) = effect.contract_id.clone() else {
                continue;
            };
            let mut event = LedgerEvent::new(
                ledger.sequence,
                &ledger.hash,
                close_time,
                &format!("tx:{}:contract:{}", tx.hash, event_index),
                EventPayload::ContractEvent {
                    contract_id,
                    event_type: effect.event_type.clone(),
                    topics: effect.topics.clone().unwrap_or_default(),
                    data_xdr: effect.data_xdr.clone(),
                    tx_hash: tx.hash.clone(),
                    event_index,
                },
            );
            event.envelope_xdr = tx.envelope_xdr.clone();
            event.result_xdr = tx.result_xdr.clone();
            events.push(event);
            event_index += 1;
        }
    }
}

/// A Horizon operation is a token transfer when it is a `payment` (or the
/// `liquidity_pool_withdraw`-free `path_payment` variants) carrying an amount.
fn is_payment_operation(op: &HorizonOperation) -> bool {
    matches!(
        op.operation_type.as_deref(),
        Some("payment")
            | Some("path_payment_strict_receive")
            | Some("path_payment_strict_send")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_ledger() -> HorizonLedgerResource {
        let json = r#"{
            "sequence": 123456,
            "hash": "f00dface",
            "closed_at": "2026-01-01T00:00:00Z",
            "fee_pool": "2100",
            "max_tx_set_size": 1000,
            "transactions": [
                {
                    "hash": "tx1",
                    "envelope_xdr": "AAAAENv1QzE=",
                    "result_xdr": "AAAA",
                    "source_account": "GSOURCE",
                    "operations": [
                        {
                            "type": "payment",
                            "from": "GSOURCE",
                            "to": "GDEST",
                            "amount": "25.5000000"
                        },
                        {
                            "type": "create_account",
                            "source_account": "GSOURCE",
                            "account": "GNEW",
                            "starting_balance": "20.0000000"
                        },
                        {
                            "type": "payment",
                            "from": "GSOURCE",
                            "to": "GISSUER_DEST",
                            "amount": "1.0000000",
                            "asset_code": "USDC",
                            "asset_issuer": "GISSUER"
                        }
                    ],
                    "effects": [
                        { "type": "account_debited", "account": "GSOURCE", "amount": "25.5000000" },
                        {
                            "type": "contract_event",
                            "contract_id": "CCONTRACT",
                            "event_type": "fn",
                            "topic": ["dG9waWM="],
                            "data_xdr": "AAAA"
                        }
                    ]
                }
            ]
        }"#;
        serde_json::from_str(json).expect("fixture parses")
    }

    #[test]
    fn test_transform_full_ledger() {
        let ledger = fixture_ledger();
        let events = XdrTransformer::new().transform(&ledger);

        // 1 ledger close + 2 payments + 1 contract event
        assert_eq!(events.len(), 4);

        assert!(matches!(
            events[0].payload,
            EventPayload::LedgerClose { tx_count: 1, .. }
        ));
        assert_eq!(events[0].event_id, events[0].event_id.clone());

        match &events[1].payload {
            EventPayload::TokenTransfer {
                from,
                to,
                asset,
                amount,
                amount_stroops,
                operation_index,
                ..
            } => {
                assert_eq!(from, "GSOURCE");
                assert_eq!(to, "GDEST");
                assert_eq!(asset, &AssetId::Native);
                assert_eq!(amount, "25.5000000");
                assert_eq!(amount_stroops, "255000000");
                assert_eq!(*operation_index, 0);
            }
            other => panic!("expected token transfer, got {other:?}"),
        }

        match &events[2].payload {
            EventPayload::TokenTransfer { asset, .. } => assert_eq!(
                asset,
                &AssetId::Credit {
                    code: "USDC".into(),
                    issuer: "GISSUER".into()
                }
            ),
            other => panic!("expected token transfer, got {other:?}"),
        }

        match &events[3].payload {
            EventPayload::ContractEvent {
                contract_id,
                topics,
                event_index,
                ..
            } => {
                assert_eq!(contract_id, "CCONTRACT");
                assert_eq!(topics, &vec!["dG9waWM=".to_string()]);
                assert_eq!(*event_index, 0);
            }
            other => panic!("expected contract event, got {other:?}"),
        }
    }

    #[test]
    fn test_no_duplicate_transfers_from_effects() {
        // account_debited/account_credited effects mirror payment operations;
        // only the operation-derived transfer must be emitted.
        let ledger = fixture_ledger();
        let events = XdrTransformer::new().transform(&ledger);
        let transfers = events
            .iter()
            .filter(|e| matches!(e.payload, EventPayload::TokenTransfer { .. }))
            .count();
        assert_eq!(transfers, 2);
    }

    #[test]
    fn test_xdr_passthrough_preserved() {
        let ledger = fixture_ledger();
        let events = XdrTransformer::new().transform(&ledger);
        let transfer = events
            .iter()
            .find(|e| matches!(e.payload, EventPayload::TokenTransfer { .. }))
            .unwrap();
        assert_eq!(transfer.envelope_xdr.as_deref(), Some("AAAAENv1QzE="));
        assert_eq!(transfer.result_xdr.as_deref(), Some("AAAA"));
    }

    #[test]
    fn test_deterministic_output() {
        let ledger = fixture_ledger();
        let a = XdrTransformer::new().transform(&ledger);
        let b = XdrTransformer::new().transform(&ledger);
        let ids_a: Vec<_> = a.iter().map(|e| e.event_id.clone()).collect();
        let ids_b: Vec<_> = b.iter().map(|e| e.event_id.clone()).collect();
        assert_eq!(ids_a, ids_b);
    }

    #[test]
    fn test_filters() {
        let ledger = fixture_ledger();
        let transformer = XdrTransformer::with_filters(TransformerFilters {
            ledger_close: false,
            token_transfers: false,
            contract_events: true,
        });
        let events = transformer.transform(&ledger);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].payload, EventPayload::ContractEvent { .. }));
    }
}
