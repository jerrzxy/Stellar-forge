#!/usr/bin/env bash
# Copyright 2024 Stellar-K8s Contributors
# SPDX-License-Identifier: Apache-2.0
#
# End-to-end test (kind): `kubectl drain` of a node running a syncing validator
# must be blocked by the auto-tuned PodDisruptionBudget, and allowed once the
# validators are synced and stable.
#
# Requires: kind, kubectl, cargo, docker. Run from the repository root:
#   controller/tests/e2e/pdb-drain-blocked.sh
set -euo pipefail

CLUSTER="${CLUSTER:-pdb-e2e}"
NS=stellar
HERE="$(cd "$(dirname "$0")" && pwd)"
CTRL_PID=""
KEEP="${KEEP_CLUSTER:-0}"

log()  { printf '\n==> %s\n' "$*"; }
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

cleanup() {
  [ -n "$CTRL_PID" ] && kill "$CTRL_PID" 2>/dev/null || true
  [ "$KEEP" = 1 ] || kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
}
trap cleanup EXIT

wait_pdb() { # <expected maxUnavailable> <timeout seconds>
  for _ in $(seq 1 "$2"); do
    v="$(kubectl -n "$NS" get pdb stellar-validators-testnet-pdb -o jsonpath='{.spec.maxUnavailable}' 2>/dev/null || true)"
    [ "$v" = "$1" ] && return 0
    sleep 1
  done
  fail "PDB maxUnavailable never became $1 (last: '${v:-none}')"
}

set_status() { # <validator> <Ready|Syncing> <lastTransitionTime>
  local ready=True progressing=False phase=Ready
  if [ "$2" = Syncing ]; then ready=False; progressing=True; phase=Syncing; fi
  kubectl -n "$NS" patch stellarnode "$1" --type merge -p "{\"status\":{\"phase\":\"$phase\",\"conditions\":[
    {\"type\":\"Ready\",\"status\":\"$ready\",\"lastTransitionTime\":\"$3\"},
    {\"type\":\"Progressing\",\"status\":\"$progressing\",\"lastTransitionTime\":\"$3\"}]}}"
}

log "Creating kind cluster (1 control plane + 4 workers)"
kind create cluster --name "$CLUSTER" --config "$HERE/kind-config.yaml" --wait 120s

log "Installing a minimal StellarNode CRD and 4 validators"
kubectl apply -f "$HERE/fixtures.yaml"
kubectl -n "$NS" rollout status statefulset/v1 statefulset/v2 statefulset/v3 statefulset/v4 --timeout=180s

NOW="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
OLD="2020-01-01T00:00:00Z"
set_status v1 Syncing "$NOW"
for v in v2 v3 v4; do set_status "$v" Ready "$OLD"; done

log "Starting the PDB controller"
( cd "$HERE/../.." && cargo build --features kube-backend --bin pdb-controller )
PDB_RECONCILE_INTERVAL_SECS=5 RUST_LOG=info "$HERE/../../target/debug/pdb-controller" &
CTRL_PID=$!

log "A validator is syncing -> PDB must block disruption"
wait_pdb 0 60
NODE="$(kubectl -n "$NS" get pod v1-0 -o jsonpath='{.spec.nodeName}')"
echo "v1-0 (syncing) runs on $NODE"

log "kubectl drain of that node must be BLOCKED"
if kubectl drain "$NODE" --ignore-daemonsets --delete-emptydir-data --timeout=30s; then
  fail "drain succeeded although a validator is syncing"
fi
kubectl -n "$NS" get pod v1-0 -o jsonpath='{.metadata.deletionTimestamp}' | grep -q . \
  && fail "syncing validator pod was evicted"
kubectl uncordon "$NODE"
echo "OK: drain blocked while syncing"

log "Validator finishes syncing -> PDB relaxes to 1"
set_status v1 Ready "$OLD"
wait_pdb 1 60

log "kubectl drain now succeeds (one validator at a time)"
kubectl drain "$NODE" --ignore-daemonsets --delete-emptydir-data --timeout=120s \
  || fail "drain still blocked after validators synced and stable"
kubectl uncordon "$NODE"
echo "OK: drain allowed when synced and stable"

log "A validator starts syncing again -> PDB tightens in real time"
set_status v2 Syncing "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
wait_pdb 0 30

log "PASS"
