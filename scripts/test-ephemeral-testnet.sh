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

set -euo pipefail

# ─── Usage ───────────────────────────────────────────────────────────────────
usage() {
  cat <<EOF
Usage: $0 [options]

End-to-end validation for issue #246 (Ephemeral Testnet Provisioning via CRD).

Provisions an ephemeral single-node Stellar network from a StellarTestnet
manifest, waits for the Soroban RPC endpoint to become Ready and funded
within a time budget (default 30s), deploys the royalty-splitter smart
contract against the generated endpoint, invokes it, then deletes the
manifest and verifies complete garbage collection.

Options:
  -n, --namespace <ns>    Namespace for the testnet (default: stellar-ci)
  -m, --manifest <path>   StellarTestnet manifest
                          (default: controller/examples/stellartestnet.yaml)
  -t, --timeout <secs>    Budget for Ready in seconds (default: 30)
  -k, --keep              Keep the testnet after a successful run
  -p, --port-forward      Reach the RPC through 'kubectl port-forward' instead
                          of status.rpcUrl (for runs outside the cluster,
                          e.g. on a kind host)
  -h, --help              Show this help

Prerequisites:
  kubectl, curl, and the stellar CLI (v25.2+, also used to build
  contracts/royalty-splitter when its wasm artifact is not present).

Examples:
  $0
  $0 --namespace ci-1234 --timeout 45 --keep
EOF
  exit 1
}

# ─── Helpers ─────────────────────────────────────────────────────────────────
log()  { printf '[testnet-e2e] %s\n' "$*"; }
fail() { printf '[testnet-e2e] ERROR: %s\n' "$*" >&2; exit 1; }

require() {
  command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ─── Parse arguments ─────────────────────────────────────────────────────────
NAMESPACE="stellar-ci"
MANIFEST="$REPO_ROOT/controller/examples/stellartestnet.yaml"
TIMEOUT=30
KEEP=false
PORT_FORWARD=false
PF_PID=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    -n|--namespace) NAMESPACE="$2";  shift 2 ;;
    -m|--manifest)  MANIFEST="$2";   shift 2 ;;
    -t|--timeout)   TIMEOUT="$2";    shift 2 ;;
    -k|--keep)      KEEP=true;       shift ;;
    -p|--port-forward) PORT_FORWARD=true; shift ;;
    -h|--help)      usage ;;
    *) echo "Unknown argument: $1"; usage ;;
  esac
done

CRD_MANIFEST="$REPO_ROOT/controller/deploy/stellartestnets.yaml"
CONTRACT_DIR="$REPO_ROOT/contracts/royalty-splitter"
WASM="${WASM:-$CONTRACT_DIR/target/wasm32v1-none/release/royalty_splitter.wasm}"

# ─── Preconditions ───────────────────────────────────────────────────────────
require kubectl
require curl
require stellar
[[ -f "$MANIFEST" ]]    || fail "manifest not found: $MANIFEST"
[[ -f "$CRD_MANIFEST" ]] || fail "CRD manifest not found: $CRD_MANIFEST"

# ─── Install the CRD ─────────────────────────────────────────────────────────
log "installing StellarTestnet CRD"
kubectl apply -f "$CRD_MANIFEST" >/dev/null
kubectl wait --for=condition=Established "crd/stellartestnets.stellar.org" \
  --timeout=30s >/dev/null

# ─── Provision ───────────────────────────────────────────────────────────────
kubectl get namespace "$NAMESPACE" >/dev/null 2>&1 \
  || kubectl create namespace "$NAMESPACE" >/dev/null

log "applying $MANIFEST in namespace $NAMESPACE"
apply_start=$SECONDS
kubectl apply -f "$MANIFEST" -n "$NAMESPACE" >/dev/null

NAME="$(kubectl get -f "$MANIFEST" -n "$NAMESPACE" -o jsonpath='{.metadata.name}')"
log "waiting up to ${TIMEOUT}s for StellarTestnet/$NAME to become Ready"

phase=""
while (( SECONDS - apply_start < TIMEOUT )); do
  phase="$(kubectl get -f "$MANIFEST" -n "$NAMESPACE" \
    -o jsonpath='{.status.phase}' 2>/dev/null || true)"
  [[ "$phase" == "Ready" ]] && break
  sleep 1
done
ready_in=$(( SECONDS - apply_start ))

if [[ "$phase" != "Ready" ]]; then
  kubectl get stellartestnet "$NAME" -n "$NAMESPACE" -o yaml || true
  kubectl get deploy,svc -n "$NAMESPACE" -l "stellar.org/testnet=$NAME" \
    -o wide || true
  kubectl logs -n stellar-testnet-system deploy/stellar-testnet-operator \
    --tail=50 2>/dev/null || true
  fail "not Ready within ${TIMEOUT}s (phase=${phase:-none}, waited ${ready_in}s)"
fi
log "StellarTestnet Ready in ${ready_in}s (budget ${TIMEOUT}s)"

# ─── Read connection details out of status ───────────────────────────────────
RPC_URL="$(kubectl get -f "$MANIFEST" -n "$NAMESPACE" -o jsonpath='{.status.rpcUrl}')"
PASSPHRASE="$(kubectl get -f "$MANIFEST" -n "$NAMESPACE" -o jsonpath='{.status.networkPassphrase}')"
DEPLOYER_G="$(kubectl get -f "$MANIFEST" -n "$NAMESPACE" \
  -o jsonpath='{.status.accounts[?(@.name=="deployer")].publicKey}')"
DEPLOYER_S="$(kubectl get -f "$MANIFEST" -n "$NAMESPACE" \
  -o jsonpath='{.status.accounts[?(@.name=="deployer")].secretKey}')"

[[ -n "$RPC_URL" ]]    || fail "status.rpcUrl is empty"

if [[ "$PORT_FORWARD" == true ]]; then
  LOCAL_PORT="${LOCAL_PORT:-18000}"
  kubectl port-forward -n "$NAMESPACE" "svc/${NAME}-rpc" "${LOCAL_PORT}:8000" >/dev/null 2>&1 &
  PF_PID=$!
  trap 'kill "$PF_PID" 2>/dev/null || true' EXIT
  for _ in $(seq 1 20); do
    curl -fs --max-time 1 -H 'Content-Type: application/json' \
      -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' \
      "http://127.0.0.1:${LOCAL_PORT}/rpc" >/dev/null 2>&1 && break
    sleep 0.5
  done
  RPC_URL="http://127.0.0.1:${LOCAL_PORT}/rpc"
fi
[[ -n "$DEPLOYER_S" ]] || fail "status did not publish the deployer secret key"
log "rpc endpoint:   $RPC_URL"
log "deployer:       $DEPLOYER_G"

# Every declared account must be funded before Ready was reported.
funded="$(kubectl get -f "$MANIFEST" -n "$NAMESPACE" \
  -o jsonpath='{.status.accounts[*].funded}')"
case " $funded " in
  *" false "*) fail "status reports unfunded accounts: $funded" ;;
esac
log "accounts funded: $funded"

# ─── Probe the Soroban RPC endpoint directly ─────────────────────────────────
health="$(curl -sf --max-time 5 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' "$RPC_URL")"
grep -q '"status":"healthy"' <<<"$health" || fail "getHealth not healthy: $health"

network="$(curl -sf --max-time 5 -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getNetwork"}' "$RPC_URL")"
grep -Fq "$PASSPHRASE" <<<"$network" \
  || fail "RPC reports a different passphrase (expected: $PASSPHRASE)"
log "getHealth healthy, network passphrase matches spec"

# ─── Build the royalty-splitter contract (same toolchain as CI) ──────────────
if [[ ! -f "$WASM" ]]; then
  log "building royalty-splitter wasm (stellar contract build)"
  (cd "$CONTRACT_DIR" && stellar contract build) \
    || fail "contract build failed (rustup target add wasm32v1-none?)"
fi
[[ -f "$WASM" ]] || fail "wasm artifact missing: $WASM"

CI_BOT_G="$(kubectl get -f "$MANIFEST" -n "$NAMESPACE" \
  -o jsonpath='{.status.accounts[?(@.name=="ci-bot")].publicKey}')"
[[ -n "$CI_BOT_G" ]] || fail "status did not publish the ci-bot account"
CI_BOT_S="$(kubectl get -f "$MANIFEST" -n "$NAMESPACE" \
  -o jsonpath='{.status.accounts[?(@.name=="ci-bot")].secretKey}')"
[[ -n "$CI_BOT_S" ]] || fail "status did not publish the ci-bot secret key"

# ─── Deploy the contract against the ephemeral network ───────────────────────
# The deploy transaction doubles as the proof that the deployer account is
# funded: an unfunded source cannot submit anything.
NET_ARGS=(--source "$DEPLOYER_S" --rpc-url "$RPC_URL" \
  --network-passphrase "$PASSPHRASE")

log "uploading royalty-splitter wasm"
WASM_HASH="$(stellar contract upload "${NET_ARGS[@]}" --wasm "$WASM")"
log "wasm hash: $WASM_HASH"

log "deploying royalty-splitter"
CONTRACT_ID="$(stellar contract deploy "${NET_ARGS[@]}" --wasm-hash "$WASM_HASH")"
log "contract id: $CONTRACT_ID"

# initialize needs every payee's consent, so the deployer (the transaction
# source) is the sole payee; ci-bot then pays through the splitter, which
# proves the second generated account is funded and can sign.
log "invoking initialize (deployer 100%)"
PAYEES="[{\"address\":\"$DEPLOYER_G\",\"shares\":1000000}]"
stellar contract invoke "${NET_ARGS[@]}" --send=yes --id "$CONTRACT_ID" -- \
  initialize --payees "$PAYEES" >/dev/null

preview="$(stellar contract invoke "${NET_ARGS[@]}" --id "$CONTRACT_ID" -- \
  preview --amount 1000)"
grep -Eq '"amount":"?1000"?' <<<"$preview" \
  || fail "preview returned '$preview', expected 1000 routed to the deployer"

# The native-asset Stellar Asset Contract is not deployed on a fresh
# standalone network; deploying it is idempotent from our point of view.
XLM_SAC="$(stellar contract id asset --asset native \
  --rpc-url "$RPC_URL" --network-passphrase "$PASSPHRASE")"
stellar contract asset deploy --asset native "${NET_ARGS[@]}" >/dev/null 2>&1 || true

log "ci-bot pays 1000 stroops through the splitter"
stellar contract invoke --source "$CI_BOT_S" --rpc-url "$RPC_URL" \
  --network-passphrase "$PASSPHRASE" --send=yes --id "$CONTRACT_ID" -- \
  process_payment --from "$CI_BOT_G" --asset "$XLM_SAC" --amount 1000 >/dev/null

processed="$(stellar contract invoke "${NET_ARGS[@]}" --id "$CONTRACT_ID" -- payments_processed)"
routed="$(stellar contract invoke "${NET_ARGS[@]}" --id "$CONTRACT_ID" -- total_routed)"
[[ "$processed" == "1" && "${routed//\"/}" == "1000" ]] \
  || fail "splitter counters: processed=$processed routed=$routed (expected 1/1000)"
log "contract state verified (1 payment, 1000 stroops routed)"

# ─── Teardown and garbage collection ─────────────────────────────────────────
if [[ "$KEEP" == true ]]; then
  log "keeping StellarTestnet/$NAME in namespace $NAMESPACE (--keep)"
else
  log "deleting StellarTestnet/$NAME"
  delete_start=$SECONDS
  kubectl delete -f "$MANIFEST" -n "$NAMESPACE" --wait=true --timeout=60s >/dev/null

  leftovers="$(kubectl get deploy,svc,ingresses -n "$NAMESPACE" \
    -l "stellar.org/testnet=$NAME" -o name 2>/dev/null || true)"
  [[ -z "$leftovers" ]] || fail "resources survived deletion: $leftovers"

  if kubectl get stellartestnet "$NAME" -n "$NAMESPACE" >/dev/null 2>&1; then
    fail "StellarTestnet/$NAME still present after delete returned"
  fi
  log "teardown verified in $(( SECONDS - delete_start ))s: no owned resources left"
fi

log "PASS: provisioned (${ready_in}s), deployed contract, torn down cleanly"
