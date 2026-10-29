# Kafka Event Streaming Bridge

The **Event Streaming Bridge** (issue #292) streams Stellar ledger events — ledger closes,
token transfers, and Soroban contract events — from a Horizon ingestion endpoint directly
into Kafka with **exactly-once** delivery semantics. Enterprise backends and data lakes get
realtime ledger data at streaming latency instead of polling the Horizon REST API.

- Crate: `event-bridge/` (`stellar-event-bridge`)
- Worker binary: `stellar-event-bridge`
- Key modules: `event-bridge/src/kafka_producer.rs`, `event-bridge/src/xdr_transformer.rs`

## Architecture

```text
┌───────────────────────────┐
│ Captive Core /            │
│ Horizon (ingestion)       │
└──────────┬────────────────┘
           │ REST: ledger resources
           │ (transactions, operations, effects + base64 XDR)
           ▼
┌───────────────────────┐     ┌────────────────────────┐
│ horizon.rs            │────▶│ xdr_transformer.rs     │
│ EventSource           │     │ XdrTransformer         │
└───────────────────────┘     └───────────┬────────────┘
                                          │ Vec<LedgerEvent>
                                          ▼
                            ┌──────────────────────────┐
                            │ bridge.rs                │
                            │ per-ledger batching      │
                            └──────────┬───────────────┘
                                       │ EventSink::publish_batch
                                       ▼
                            ┌──────────────────────────┐
                            │ kafka_producer.rs        │
                            │ Kafka transaction        │
                            │ (events + checkpoint)    │
                            └──────────┬───────────────┘
                                       ▼
                            ┌──────────────────────────┐
                            │ Kafka / Redpanda         │
                            │ events topic + compacted │
                            │ checkpoint topic         │
                            └──────────────────────────┘
```

The bridge taps the ledger resources Horizon derives from Captive Core's execution
stream — the same data that populates Horizon's Redis ingestion cache. The
`EventSource` trait abstracts the tap, so a direct Redis or captive-core XDR pipe can be
plugged in without touching the rest of the pipeline.

## Event model

Every event is a `LedgerEvent` envelope:

| Field | Description |
| --- | --- |
| `eventId` | Deterministic SHA-256 of `<ledgerHash>:<discriminator>` — identical ledger content always yields identical IDs |
| `ledger` | Ledger sequence number |
| `ledgerHash` | Ledger hash (hex) reported by Horizon |
| `closeTime` | Ledger close time (RFC 3339) |
| `kind` | `ledgerClose` \| `tokenTransfer` \| `contractEvent` |
| `envelopeXdr`, `resultXdr` | Base64 XDR of the source transaction, passed through verbatim for fidelity checks |

Partition keys follow the configured strategy:

- `asset` (default) — `XLM` for native, `CODE:ISSUER` for credit assets, ledger sequence otherwise
- `contract` — Soroban contract ID, ledger sequence otherwise
- `ledger` — ledger sequence (all events of one ledger land on one partition)

The FNV-1a partition mapping follows [ADR-0005](adr/0005-dynamic-kafka-partitioning.md)
and re-discovers the partition count from broker metadata, so re-partitioned topics are
handled without a restart.

## Exactly-once delivery

Exactly-once is implemented with Kafka transactions (EOS):

1. The producer holds a stable `transactional.id` per bridge instance with
   `enable.idempotence=true`. A restarted instance **fences** its zombie predecessor.
2. All events of one ledger **and** the ledger's checkpoint record are sent inside a
   single Kafka transaction. Either the whole ledger is visible to
   `isolation.level=read_committed` consumers, or none of it is.
3. On failure the transaction is aborted and the whole ledger batch is retried with
   exponential backoff. Aborted records are never observable.
4. On restart the bridge reads the last committed checkpoint from the compacted
   `<topic>.checkpoints` topic and resumes at `checkpoint + 1`. Committed ledgers are
   never re-sent; uncommitted ledgers are re-sent in full.

**Consumers must set `isolation.level=read_committed`.** Downstream idempotency is also
possible without coordination thanks to the deterministic `eventId`.

## Running the worker

```bash
# Tail a Horizon endpoint and stream to Kafka
cargo run --release -p stellar-event-bridge --bin stellar-event-bridge -- \
    run --horizon-url http://horizon:8000 --brokers kafka:9092 \
        --partition-by asset

# Backfill a historical range (exactly once, resumable)
cargo run --release -p stellar-event-bridge --bin stellar-event-bridge -- \
    backfill --start-ledger 1000000 --end-ledger 1001000

# Export a range as JSONL + digest for fidelity verification (no Kafka needed)
cargo run --release -p stellar-event-bridge --bin stellar-event-bridge -- \
    export --start-ledger 1000000 --end-ledger 1001000 --output events.jsonl
```

All flags also have environment variables (`EVENT_BRIDGE_HORIZON_URL`,
`KAFKA_BROKERS`, `EVENT_BRIDGE_TOPIC`, `EVENT_BRIDGE_CHECKPOINT_TOPIC`,
`EVENT_BRIDGE_ID`, `EVENT_BRIDGE_START_LEDGER`), which is how the sample
deployment in `config/samples/event-bridge-example.yaml` configures the
worker.

### Configuration reference

| Flag / env | Default | Description |
| --- | --- | --- |
| `--horizon-url` / `EVENT_BRIDGE_HORIZON_URL` | `http://localhost:8000` | Horizon base URL |
| `--brokers` / `KAFKA_BROKERS` | `localhost:9092` | Kafka bootstrap servers |
| `--topic` / `EVENT_BRIDGE_TOPIC` | `stellar-ledger-events` | Events topic |
| `--checkpoint-topic` / `EVENT_BRIDGE_CHECKPOINT_TOPIC` | `stellar-ledger-events.checkpoints` | Compacted checkpoint topic |
| `--bridge-id` / `EVENT_BRIDGE_ID` | `event-bridge-0` | Instance id (part of `transactional.id`) |
| `--format` | `json` | `json` or `avro` (see limitations) |
| `--partition-by` | `asset` | `asset`, `contract` or `ledger` |
| `--start-ledger` / `EVENT_BRIDGE_START_LEDGER` | `1` | First ledger when no checkpoint exists |

## Validation

### Data fidelity (1M historical transactions)

`scripts/validate-event-bridge.sh` drives the full check against a local Redpanda
container:

1. Export the ledger range from Horizon to JSONL and compute the event-stream digest.
2. Backfill the same range through the bridge into Kafka.
3. Consume the topic back (`read_committed`) and compare event count **and** digest
   against the Horizon-side export — any loss, duplication or reordering fails the script.

```bash
HORIZON_URL=https://horizon.stellar.org \
  START_LEDGER=1000000 END_LEDGER=1030000 \
  scripts/validate-event-bridge.sh
```

### Load test (5,000 events/sec)

The review requirement — sustaining 5,000 events/sec without memory backpressure — is
covered by two harnesses:

```bash
# In-process pipeline harness (review scale, 1M events)
cargo test -p stellar-event-bridge --test load_test -- --ignored --nocapture

# Same harness via the worker binary
cargo run --release -p stellar-event-bridge --bin stellar-event-bridge -- \
    load-test --events 1000000 --target-eps 5000
```

A CI-sized subset (50k events at the same 5,000 eps floor) runs on every `cargo test`.
Batch memory is bounded by construction: the sink commits each ledger batch and drops it,
so no unbounded queue can form between the transformer and the producer.

## Consumer guidance

- Set `isolation.level=read_committed` on every consumer of the events topic.
- Use the event key semantics: keys are asset codes (`XLM`, `USDC:GISSUER…`), contract
  IDs, or ledger sequences, so per-key ordering is preserved by Kafka.
- Deduplicate on `eventId` if your sink cannot rely on `read_committed` alone.

## Limitations / future work

- **Avro**: `--format avro` currently emits the canonical JSON encoding of the event
  (identical transitional state as the SCP analytics pipeline). The canonical Avro schema
  is published in `schemas/ledger-event.avsc` for registration; the binary encoder +
  schema registry integration will follow.
- **Redis tap**: the `EventSource` trait is in place; a direct consumer of Horizon's
  Redis ingestion cache is a drop-in future implementation.
