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
//! `stellar-event-bridge` — Kafka Event Streaming Bridge worker (issue #292).
//!
//! Subcommands:
//!   * `run`      — tail Horizon and stream new ledger events to Kafka.
//!   * `backfill` — stream a historical ledger range to Kafka (exactly once).
//!   * `export`   — dump a ledger range as JSONL + digest for fidelity checks.
//!   * `load-test` — prove the pipeline sustains the required event rate.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::info;
use tracing_subscriber::EnvFilter;
use tokio_util::sync::CancellationToken;

use stellar_event_bridge::config::{EventBridgeConfig, EventFormat, PartitionBy};
use stellar_event_bridge::horizon::{EventSource, HorizonRestSource};
use stellar_event_bridge::kafka_producer::KafkaEventProducer;
use stellar_event_bridge::xdr_transformer::XdrTransformer;
use stellar_event_bridge::{bridge, EventBridge};

#[derive(Parser)]
#[command(
    name = "stellar-event-bridge",
    about = "Kafka Event Streaming Bridge for Horizon ledger events",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Wire format for event payloads.
#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum CliFormat {
    /// Canonical JSON envelope (lossless).
    Json,
    /// Avro-compatible framing (canonical JSON body until the schema
    /// registry encoder lands; see `schemas/ledger-event.avsc`).
    Avro,
}

impl From<CliFormat> for EventFormat {
    fn from(f: CliFormat) -> Self {
        match f {
            CliFormat::Json => EventFormat::Json,
            CliFormat::Avro => EventFormat::Avro,
        }
    }
}

/// Kafka partitioning strategy.
#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum CliPartitionBy {
    Asset,
    Contract,
    Ledger,
}

impl From<CliPartitionBy> for PartitionBy {
    fn from(p: CliPartitionBy) -> Self {
        match p {
            CliPartitionBy::Asset => PartitionBy::Asset,
            CliPartitionBy::Contract => PartitionBy::Contract,
            CliPartitionBy::Ledger => PartitionBy::Ledger,
        }
    }
}

/// Arguments shared by every command that talks to Horizon and/or Kafka.
#[derive(clap::Args, Clone)]
struct BridgeArgs {
    /// Horizon base URL.
    #[arg(long, env = "EVENT_BRIDGE_HORIZON_URL", default_value = "http://localhost:8000")]
    horizon_url: String,

    /// Kafka bootstrap servers (comma-separated).
    #[arg(long, env = "KAFKA_BROKERS", default_value = "localhost:9092")]
    brokers: String,

    /// Kafka topic for ledger events.
    #[arg(long, env = "EVENT_BRIDGE_TOPIC", default_value = "stellar-ledger-events")]
    topic: String,

    /// Compacted checkpoint topic (committed in the same transaction).
    #[arg(long, env = "EVENT_BRIDGE_CHECKPOINT_TOPIC", default_value = "stellar-ledger-events.checkpoints")]
    checkpoint_topic: String,

    /// Bridge instance id (part of the Kafka transactional.id).
    #[arg(long, env = "EVENT_BRIDGE_ID", default_value = "event-bridge-0")]
    bridge_id: String,

    /// Event wire format.
    #[arg(long, value_enum, default_value_t = CliFormat::Json)]
    format: CliFormat,

    /// Partition events by asset or contract ID.
    #[arg(long, value_enum, default_value_t = CliPartitionBy::Asset)]
    partition_by: CliPartitionBy,

    /// Horizon request timeout in seconds.
    #[arg(long, default_value_t = 30)]
    http_timeout_secs: u64,
}

impl BridgeArgs {
    fn to_config(&self) -> EventBridgeConfig {
        EventBridgeConfig {
            bridge_id: self.bridge_id.clone(),
            horizon_url: self.horizon_url.clone(),
            bootstrap_servers: self.brokers.clone(),
            topic: self.topic.clone(),
            checkpoint_topic: self.checkpoint_topic.clone(),
            format: self.format.into(),
            partition_by: self.partition_by.into(),
            http_timeout_secs: self.http_timeout_secs,
            ..Default::default()
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Tail Horizon and stream newly closed ledgers to Kafka (exactly once).
    Run {
        #[command(flatten)]
        args: BridgeArgs,

        /// Ledger to start from when no checkpoint exists.
        #[arg(long, env = "EVENT_BRIDGE_START_LEDGER", default_value_t = 1)]
        start_ledger: u64,

        /// Seconds between polls for new ledgers.
        #[arg(long, default_value_t = 1)]
        poll_interval_secs: u64,
    },
    /// Stream a historical ledger range to Kafka (exactly once).
    Backfill {
        #[command(flatten)]
        args: BridgeArgs,

        /// First ledger of the range (inclusive).
        #[arg(long)]
        start_ledger: u64,

        /// Last ledger of the range (inclusive).
        #[arg(long)]
        end_ledger: u64,
    },
    /// Export a ledger range as JSONL plus a digest, without touching Kafka.
    /// Used to verify data fidelity of the Kafka stream against Horizon.
    Export {
        #[command(flatten)]
        args: BridgeArgs,

        /// First ledger of the range (inclusive).
        #[arg(long)]
        start_ledger: u64,

        /// Last ledger of the range (inclusive).
        #[arg(long)]
        end_ledger: u64,

        /// JSONL output path ("-" writes to stdout).
        #[arg(long, default_value = "-")]
        output: String,
    },
    /// Prove the pipeline sustains the required throughput (in-memory sink).
    LoadTest {
        /// Total synthetic events to produce.
        #[arg(long, default_value_t = 1_000_000)]
        events: u64,

        /// Required sustained events per second.
        #[arg(long, default_value_t = 5_000)]
        target_eps: f64,

        /// Payment operations per synthetic ledger.
        #[arg(long, default_value_t = 4)]
        payments_per_ledger: u32,

        /// Contract events per synthetic ledger.
        #[arg(long, default_value_t = 1)]
        contract_events_per_ledger: u32,
    },
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();

    match cli.command {
        Command::Run {
            args,
            start_ledger,
            poll_interval_secs,
        } => {
            let config = EventBridgeConfig {
                starting_ledger: start_ledger,
                poll_interval_secs,
                ..args.to_config()
            };
            run(config).await
        }
        Command::Backfill {
            args,
            start_ledger,
            end_ledger,
        } => {
            let config = args.to_config();
            backfill(config, start_ledger, end_ledger).await
        }
        Command::Export {
            args,
            start_ledger,
            end_ledger,
            output,
        } => export(args, start_ledger, end_ledger, output).await,
        Command::LoadTest {
            events,
            target_eps,
            payments_per_ledger,
            contract_events_per_ledger,
        } => load_test(events, target_eps, payments_per_ledger, contract_events_per_ledger).await,
    }
}

async fn run(config: EventBridgeConfig) -> anyhow::Result<()> {
    config.validate()?;
    let source = HorizonRestSource::new(
        &config.horizon_url,
        Duration::from_secs(config.http_timeout_secs),
    )?;
    let sink = KafkaEventProducer::new(config.clone())?;
    let bridge = EventBridge::new(source, sink, XdrTransformer::new(), config);

    let shutdown = CancellationToken::new();
    let signal_token = shutdown.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install ctrl-c handler");
        info!("ctrl-c received; committing in-flight ledger and stopping");
        signal_token.cancel();
    });

    let stats = bridge.run(shutdown).await?;
    println!(
        "{}",
        json!({
            "ledgersProcessed": stats.ledgers_processed,
            "eventsPublished": stats.events_published,
            "elapsedSecs": stats.elapsed.as_secs_f64(),
            "eventsPerSec": stats.events_per_sec(),
        })
    );
    Ok(())
}

async fn backfill(config: EventBridgeConfig, start_ledger: u64, end_ledger: u64) -> anyhow::Result<()> {
    config.validate()?;
    let source = HorizonRestSource::new(
        &config.horizon_url,
        Duration::from_secs(config.http_timeout_secs),
    )?;
    let sink = KafkaEventProducer::new(config.clone())?;
    let bridge = EventBridge::new(source, sink, XdrTransformer::new(), config);

    let stats = bridge.backfill(start_ledger, end_ledger).await?;
    println!(
        "{}",
        json!({
            "startLedger": start_ledger,
            "endLedger": end_ledger,
            "ledgersProcessed": stats.ledgers_processed,
            "eventsPublished": stats.events_published,
            "elapsedSecs": stats.elapsed.as_secs_f64(),
            "eventsPerSec": stats.events_per_sec(),
        })
    );
    Ok(())
}

async fn export(
    args: BridgeArgs,
    start_ledger: u64,
    end_ledger: u64,
    output: String,
) -> anyhow::Result<()> {
    if end_ledger < start_ledger {
        anyhow::bail!("end ledger {end_ledger} < start ledger {start_ledger}");
    }
    let source = HorizonRestSource::new(
        &args.horizon_url,
        Duration::from_secs(args.http_timeout_secs),
    )?;
    let transformer = XdrTransformer::new();

    let writer: Box<dyn Write> = if output == "-" {
        Box::new(std::io::stdout())
    } else {
        Box::new(File::create(&output)?)
    };
    let mut writer = BufWriter::new(writer);

    let mut digest = Sha256::new();
    let mut events: u64 = 0;
    let mut transfers: u64 = 0;
    let mut contract_events: u64 = 0;

    for sequence in start_ledger..=end_ledger {
        let ledger = source.fetch_ledger(sequence).await?;
        for event in transformer.transform(&ledger) {
            match &event.payload {
                stellar_event_bridge::EventPayload::TokenTransfer { .. } => transfers += 1,
                stellar_event_bridge::EventPayload::ContractEvent { .. } => contract_events += 1,
                _ => {}
            }
            digest.update(event.event_id.as_bytes());
            serde_json::to_writer(&mut writer, &event)?;
            writer.write_all(b"\n")?;
            events += 1;
        }
    }
    writer.flush()?;

    println!(
        "{}",
        json!({
            "startLedger": start_ledger,
            "endLedger": end_ledger,
            "events": events,
            "tokenTransfers": transfers,
            "contractEvents": contract_events,
            "eventStreamDigest": hex::encode(digest.finalize()),
        })
    );
    Ok(())
}

async fn load_test(
    events: u64,
    target_eps: f64,
    payments_per_ledger: u32,
    contract_events_per_ledger: u32,
) -> anyhow::Result<()> {
    let report = bridge::run_load_test(events, target_eps, payments_per_ledger, contract_events_per_ledger)
        .await;
    println!(
        "{}",
        json!({
            "events": report.events,
            "ledgers": report.ledgers,
            "elapsedSecs": report.elapsed_secs,
            "eventsPerSec": report.events_per_sec,
            "targetEps": report.target_eps,
            "passed": report.passed,
        })
    );
    if !report.passed {
        anyhow::bail!(
            "throughput {:.0} events/sec below required {:.0}",
            report.events_per_sec,
            report.target_eps
        );
    }
    Ok(())
}
