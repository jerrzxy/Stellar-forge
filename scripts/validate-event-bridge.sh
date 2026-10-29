#!/usr/bin/env bash
# Copyright 2024 Stellar-K8s Contributors
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#
# Validate the Kafka Event Streaming Bridge (issue #292) against a local
# Redpanda/Kafka cluster:
#
#   1. Export a historical ledger range straight from Horizon to JSONL
#      (reference digest of the expected event stream).
#   2. Backfill the same range through the bridge into Kafka (exactly once).
#   3. Consume the topic back with read_committed semantics and verify the
#      event count and event-id set match the Horizon reference exactly.
#
# Requirements: bash, docker, python3, cargo (Rust toolchain).
#
# Usage:
#   HORIZON_URL=https://horizon.stellar.org \
#     START_LEDGER=1000000 END_LEDGER=1030000 \
#     scripts/validate-event-bridge.sh
#
# Environment:
#   HORIZON_URL          Horizon base URL (required)
#   START_LEDGER         First ledger of the range (required)
#   END_LEDGER           Last ledger of the range (required)
#   KAFKA_BROKERS        Bootstrap servers (default: localhost:9092)
#   TOPIC                Events topic (default: stellar-ledger-events)
#   REDPANDA_IMAGE       Redpanda image (default: redpandadata/redpanda:v24.2.4)
#   START_REDPANDA       "true" to start a throwaway container (default: true)
#   CONSUME_TIMEOUT_MS   rpk consume timeout (default: 60000)

set -euo pipefail

readonly DEFAULT_REDPANDA_IMAGE="redpandadata/redpanda:v24.2.4"

HORIZON_URL="${HORIZON_URL:-}"
START_LEDGER="${START_LEDGER:-}"
END_LEDGER="${END_LEDGER:-}"
KAFKA_BROKERS="${KAFKA_BROKERS:-localhost:9092}"
TOPIC="${TOPIC:-stellar-ledger-events}"
REDPANDA_IMAGE="${REDPANDA_IMAGE:-$DEFAULT_REDPANDA_IMAGE}"
START_REDPANDA="${START_REDPANDA:-true}"
CONSUME_TIMEOUT_MS="${CONSUME_TIMEOUT_MS:-60000}"

readonly REDPANDA_CONTAINER="event-bridge-redpanda"

log() { printf '[validate-event-bridge] %s\n' "$*" >&2; }
die() { log "ERROR: $*"; exit 1; }

[[ -n "$HORIZON_URL" ]] || die "HORIZON_URL is required"
[[ -n "$START_LEDGER" ]] || die "START_LEDGER is required"
[[ -n "$END_LEDGER" ]] || die "END_LEDGER is required"
[[ "$START_LEDGER" -le "$END_LEDGER" ]] || die "START_LEDGER must be <= END_LEDGER"
command -v python3 >/dev/null || die "python3 is required"
command -v docker >/dev/null || die "docker is required"

WORKDIR="$(mktemp -d)"
cleanup() {
  rm -rf "$WORKDIR"
  if [[ "$START_REDPANDA" == "true" ]]; then
    docker rm -f "$REDPANDA_CONTAINER" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

if [[ "$START_REDPANDA" == "true" ]]; then
  log "starting throwaway Redpanda container"
  docker rm -f "$REDPANDA_CONTAINER" >/dev/null 2>&1 || true
  docker run -d --name "$REDPANDA_CONTAINER" \
    -p 9092:9092 \
    "$REDPANDA_IMAGE" \
    redpanda start \
    --overprovisioned --smp 1 --memory 512M --reserve-memory 0M \
    --node-id 0 --check=false \
    --kafka-addr internal://0.0.0.0:9092,external://0.0.0.0:19092 \
    --advertise-kafka-addr internal://redpanda:9092,external://localhost:19092 \
    >/dev/null
  log "waiting for Redpanda to accept connections"
  for _ in $(seq 1 30); do
    if docker exec "$REDPANDA_CONTAINER" rpk cluster health >/dev/null 2>&1; then
      break
    fi
    sleep 1
  done
fi

docker exec "$REDPANDA_CONTAINER" rpk topic create "$TOPIC" --partitions 6 >/dev/null 2>&1 || true
docker exec "$REDPANDA_CONTAINER" rpk topic create "${TOPIC}.checkpoints" \
  --partitions 1 --config cleanup.policy=compact >/dev/null 2>&1 || true

log "exporting reference events from Horizon ($START_LEDGER..=$END_LEDGER)"
REFERENCE_JSONL="$WORKDIR/reference.jsonl"
EXPORT_SUMMARY="$(cargo run --quiet --release -p stellar-event-bridge --bin stellar-event-bridge -- \
  export --horizon-url "$HORIZON_URL" \
  --start-ledger "$START_LEDGER" --end-ledger "$END_LEDGER" \
  --output "$REFERENCE_JSONL")"
log "reference: $EXPORT_SUMMARY"

log "backfilling range through the bridge into Kafka"
BACKFILL_SUMMARY="$(cargo run --quiet --release -p stellar-event-bridge --bin stellar-event-bridge -- \
  backfill --horizon-url "$HORIZON_URL" --brokers "$KAFKA_BROKERS" --topic "$TOPIC" \
  --start-ledger "$START_LEDGER" --end-ledger "$END_LEDGER")"
log "backfill: $BACKFILL_SUMMARY"

EXPECTED_EVENTS="$(python3 - "$EXPORT_SUMMARY" <<'PY'
import json, sys
print(json.loads(sys.argv[1])["events"])
PY
)"
[[ "$EXPECTED_EVENTS" -gt 0 ]] || die "reference export produced no events"
log "consuming $EXPECTED_EVENTS events back from Kafka (read_committed)"

CONSUMED_JSONL="$WORKDIR/consumed.jsonl"
docker exec "$REDPANDA_CONTAINER" \
  rpk topic consume "$TOPIC" \
  -n "$EXPECTED_EVENTS" \
  --offset start \
  --format json \
  --read-committed \
  --timeout "${CONSUME_TIMEOUT_MS}ms" >"$CONSUMED_JSONL" || true

python3 - "$REFERENCE_JSONL" "$CONSUMED_JSONL" <<'PY'
import json, sys

reference_path, consumed_path = sys.argv[1], sys.argv[2]

def load_reference(path):
    ids = []
    with open(path) as fh:
        for line in fh:
            line = line.strip()
            if line:
                ids.append(json.loads(line)["eventId"])
    return ids

def load_consumed(path):
    ids = []
    with open(path) as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            record = json.loads(line)
            payload = record.get("payload")
            if isinstance(payload, str):
                payload = json.loads(payload)
            if isinstance(payload, dict) and "eventId" in payload:
                ids.append(payload["eventId"])
    return ids

reference = load_reference(reference_path)
consumed = load_consumed(consumed_path)

print(f"reference events: {len(reference)}")
print(f"consumed  events: {len(consumed)}")

errors = []
if len(consumed) != len(reference):
    errors.append(
        f"count mismatch: Horizon reference has {len(reference)} events, "
        f"Kafka stream delivered {len(consumed)}"
    )
if sorted(consumed) != sorted(reference):
    missing = set(reference) - set(consumed)
    extra = set(consumed) - set(reference)
    errors.append(
        f"event-id set mismatch: {len(missing)} missing, {len(extra)} duplicated/foreign"
    )

if errors:
    for err in errors:
        print(f"FAIL: {err}")
    sys.exit(1)

print("PASS: 100% data fidelity verified against Horizon reference")
PY

log "validation passed"
