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
//! End-to-end bridge tests: mocked Horizon → transformer → sink, including
//! exactly-once resume behaviour across "restarts".

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use stellar_event_bridge::config::EventBridgeConfig;
use stellar_event_bridge::event::EventPayload;
use stellar_event_bridge::horizon::HorizonRestSource;
use stellar_event_bridge::sink::MemorySink;
use stellar_event_bridge::xdr_transformer::XdrTransformer;
use stellar_event_bridge::{EventBridge, EventSink};

fn ledger_body(sequence: u64) -> serde_json::Value {
    json!({
        "sequence": sequence,
        "hash": format!("ledgerhash-{sequence}"),
        "closed_at": "2026-01-01T00:00:00Z",
        "fee_pool": "2100",
        "max_tx_set_size": 1000,
        "transactions": [
            {
                "hash": format!("tx-{sequence}"),
                "source_account": "GSOURCE",
                "envelope_xdr": "AAAAENv1QzE=",
                "result_xdr": "AAAA",
                "operations": [
                    {
                        "type": "payment",
                        "from": "GSOURCE",
                        "to": "GDEST",
                        "amount": "12.3456789"
                    }
                ],
                "effects": [
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
    })
}

async fn mock_horizon(latest: u64) -> MockServer {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/ledgers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "_embedded": {
                "records": [{ "sequence": latest }]
            }
        })))
        .mount(&server)
        .await;

    for sequence in 1..=latest {
        Mock::given(method("GET"))
            .and(path(format!("/ledgers/{sequence}")))
            .and(query_param("transactions", "true"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ledger_body(sequence)))
            .mount(&server)
            .await;
    }

    server
}

fn source(server: &MockServer) -> HorizonRestSource {
    HorizonRestSource::new(&server.uri(), Duration::from_secs(5)).expect("source")
}

#[tokio::test]
async fn test_backfill_end_to_end() {
    let server = mock_horizon(3).await;
    let sink = Arc::new(MemorySink::new());
    let bridge = EventBridge::new(
        source(&server),
        Arc::clone(&sink) as MemorySinkArc,
        XdrTransformer::new(),
        EventBridgeConfig::default(),
    );

    let stats = bridge.backfill(1, 3).await.unwrap();

    // Per ledger: 1 close + 1 payment + 1 contract event.
    assert_eq!(stats.ledgers_processed, 3);
    assert_eq!(stats.events_published, 9);

    let events = sink.events();
    assert_eq!(events.len(), 9);

    // Deterministic, unique IDs across the whole stream.
    let ids: std::collections::HashSet<_> = events.iter().map(|e| e.event_id.clone()).collect();
    assert_eq!(ids.len(), events.len());

    // Raw XDR passthrough preserved for fidelity verification.
    let transfer = events
        .iter()
        .find(|e| matches!(e.payload, EventPayload::TokenTransfer { .. }))
        .expect("transfer present");
    assert_eq!(transfer.envelope_xdr.as_deref(), Some("AAAAENv1QzE="));

    // Checkpoint follows the last committed ledger.
    assert_eq!(sink.last_committed_ledger().await.unwrap(), Some(3));
}

/// Alias used above so the Arc coercion is explicit.
type MemorySinkArc = Arc<MemorySink>;

#[tokio::test]
async fn test_restart_resumes_from_checkpoint_without_duplicates() {
    let server = mock_horizon(6).await;
    let sink = Arc::new(MemorySink::new());
    let config = EventBridgeConfig::default();

    // First "process life": stream ledgers 1..=3.
    {
        let bridge = EventBridge::new(
            source(&server),
            Arc::clone(&sink) as MemorySinkArc,
            XdrTransformer::new(),
            config.clone(),
        );
        bridge.backfill(1, 3).await.unwrap();
    }
    assert_eq!(sink.last_committed_ledger().await.unwrap(), Some(3));
    assert_eq!(sink.events().len(), 9);

    // Second "process life" (same sink = same committed checkpoint): the
    // bridge resumes after the checkpoint, so no ledger is replayed.
    let bridge = EventBridge::new(
        source(&server),
        Arc::clone(&sink) as MemorySinkArc,
        XdrTransformer::new(),
        config,
    );
    let resume_from = bridge.resume_ledger(None).await.unwrap();
    assert_eq!(resume_from, 4);

    bridge.backfill(resume_from, 6).await.unwrap();
    let events = sink.events();
    assert_eq!(events.len(), 18, "no duplicated events after restart");

    let ledgers: std::collections::HashSet<u64> = events.iter().map(|e| e.ledger).collect();
    assert_eq!(ledgers.len(), 6, "every ledger exactly once");
}

#[tokio::test]
async fn test_run_tails_only_new_ledgers() {
    let server = mock_horizon(4).await;
    let sink = Arc::new(MemorySink::new());
    let bridge = EventBridge::new(
        source(&server),
        Arc::clone(&sink) as MemorySinkArc,
        XdrTransformer::new(),
        EventBridgeConfig {
            starting_ledger: 3,
            poll_interval_secs: 1,
            ..Default::default()
        },
    );

    let shutdown = tokio_util::sync::CancellationToken::new();
    let token = shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        token.cancel();
    });

    let stats = bridge.run(shutdown).await.unwrap();
    assert_eq!(stats.ledgers_processed, 2, "ledgers 3 and 4 only");
    let ledgers: std::collections::HashSet<u64> =
        sink.events().iter().map(|e| e.ledger).collect();
    assert!(ledgers.contains(&3) && ledgers.contains(&4));
    assert!(!ledgers.contains(&1) && !ledgers.contains(&2));
}

#[tokio::test]
async fn test_export_digest_is_deterministic() {
    use sha2::{Digest, Sha256};

    let server = mock_horizon(5).await;
    let transformer = XdrTransformer::new();
    let src = source(&server);

    let digest_of = |src: HorizonRestSource, transformer: XdrTransformer| async move {
        let mut digest = Sha256::new();
        let mut count = 0u64;
        for sequence in 1..=5u64 {
            let ledger = src.fetch_ledger(sequence).await.unwrap();
            for event in transformer.transform(&ledger) {
                digest.update(event.event_id.as_bytes());
                count += 1;
            }
        }
        (hex::encode(digest.finalize()), count)
    };

    let (first, count_a) = digest_of(src.clone(), transformer.clone()).await;
    let (second, count_b) = digest_of(source(&server), XdrTransformer::new()).await;
    assert_eq!(count_a, count_b);
    assert_eq!(first, second);
}
