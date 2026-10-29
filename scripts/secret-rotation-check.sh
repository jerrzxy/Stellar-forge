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
# Integration check: secret rotation without downtime (issue #1066).
#
# Rotates (patches) a Kubernetes secret and verifies that the consuming
# deployment keeps its available replicas at or above the pre-rotation
# level for the whole observation window.
#
# Usage:
#   scripts/secret-rotation-check.sh [--namespace NS] [--secret NAME] \
#       [--deployment NAME] [--window SECONDS] [--dry-run]
#
#   scripts/secret-rotation-check.sh --validator-rotation \
#       [--namespace NS] [--label-selector SELECTOR] [--stagger SECONDS] \
#       [--window SECONDS] [--ledger-timeout SECONDS] [--dry-run]
#
# Validator seed rotation mode (issue #290):
#   Rotates validator node identities in a staggered fashion across a
#   multi-node cluster, then verifies that:
#     * every node reports the new public key after rotation,
#     * quorum is never lost (each node stays in sync with the network),
#     * ledger advancement continues (no stall) throughout the sequence.
#   The script fails on any quorum loss or ledger stall.
#
# --dry-run validates arguments and prints the plan without needing a
# cluster; CI uses it as a smoke test.
set -euo pipefail

NAMESPACE="stellar-system"
SECRET="stellar-core-secret"
DEPLOYMENT="stellar-k8s-operator"
WINDOW=60
DRY_RUN=0

VALIDATOR_ROTATION=0
LABEL_SELECTOR="app.kubernetes.io/component=validator"
STAGGER=30
LEDGER_TIMEOUT=30

while [[ $# -gt 0 ]]; do
  case "$1" in
    --namespace) NAMESPACE="$2"; shift 2 ;;
    --secret) SECRET="$2"; shift 2 ;;
    --deployment) DEPLOYMENT="$2"; shift 2 ;;
    --window) WINDOW="$2"; shift 2 ;;
    --validator-rotation) VALIDATOR_ROTATION=1; shift ;;
    --label-selector) LABEL_SELECTOR="$2"; shift 2 ;;
    --stagger) STAGGER="$2"; shift 2 ;;
    --ledger-timeout) LEDGER_TIMEOUT="$2"; shift 2 ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

echo "Secret rotation check"
echo "  namespace:  ${NAMESPACE}"
if [[ "${VALIDATOR_ROTATION}" == "1" ]]; then
  echo "  mode:       validator seed rotation"
  echo "  selector:   ${LABEL_SELECTOR}"
  echo "  stagger:    ${STAGGER}s"
  echo "  window:     ${WINDOW}s"
  echo "  ledger t/o: ${LEDGER_TIMEOUT}s"
else
  echo "  secret:     ${SECRET}"
  echo "  deployment: ${DEPLOYMENT}"
  echo "  window:     ${WINDOW}s"
fi

if [[ "${DRY_RUN}" == "1" ]]; then
  echo "Dry run: no cluster interaction performed. Plan:"
  if [[ "${VALIDATOR_ROTATION}" == "1" ]]; then
    echo "  1. list validator pods matching ${LABEL_SELECTOR}"
    echo "  2. record current public key and latest ledger of each node"
    echo "  3. rotate node identities one at a time, ${STAGGER}s apart"
    echo "  4. verify every node reports the new public key"
    echo "  5. poll ledger advancement for ${WINDOW}s; fail on stall"
    echo "     (no ledger progress within ${LEDGER_TIMEOUT}s) or quorum loss"
  else
    echo "  1. record availableReplicas of ${DEPLOYMENT}"
    echo "  2. annotate ${SECRET} with a rotation timestamp"
    echo "  3. poll availableReplicas for ${WINDOW}s and fail on any drop"
  fi
  exit 0
fi

command -v kubectl >/dev/null || { echo "kubectl not found" >&2; exit 2; }

if [[ "${VALIDATOR_ROTATION}" == "1" ]]; then
  # --- Validator seed rotation (issue #290) --------------------------------
  PODS="$(kubectl -n "${NAMESPACE}" get pods -l "${LABEL_SELECTOR}" \
    -o jsonpath='{.items[*].metadata.name}')"
  if [[ -z "${PODS}" ]]; then
    echo "FAIL: no validator pods match selector ${LABEL_SELECTOR}" >&2
    exit 1
  fi
  read -r -a POD_LIST <<< "${PODS}"
  NODE_COUNT="${#POD_LIST[@]}"
  echo "Validators discovered: ${NODE_COUNT} (${PODS})"

  # Quorum for a Stellar network requires a strict majority of nodes to
  # agree; with staggered rotation we must never drop below that.
  QUORUM=$(( NODE_COUNT / 2 + 1 ))
  echo "Required quorum: ${QUORUM}/${NODE_COUNT}"

  # Returns the latest ledger sequence reported by a node, or empty.
  latest_ledger() {
    kubectl -n "${NAMESPACE}" exec "$1" -- \
      stellar-core http-command info 2>/dev/null \
      | grep -o '"ledger"[^,}]*' | head -n1 | grep -o '[0-9]\+' || true
  }

  # Returns the node public key reported by a node, or empty.
  node_public_key() {
    kubectl -n "${NAMESPACE}" exec "$1" -- \
      stellar-core http-command info 2>/dev/null \
      | grep -o '"publicKey"[^,}]*' | head -n1 \
      | sed 's/.*"publicKey"[": ]*//; s/"//g' || true
  }

  # Records baseline ledger and public keys before any rotation.
  declare -A BASE_LEDGER=()
  declare -A BASE_KEY=()
  for pod in "${POD_LIST[@]}"; do
    BASE_LEDGER["${pod}"]="$(latest_ledger "${pod}")"
    BASE_KEY["${pod}"]="$(node_public_key "${pod}")"
    if [[ -z "${BASE_LEDGER[${pod}]}" ]]; then
      echo "FAIL: could not read ledger from ${pod} before rotation" >&2
      exit 1
    fi
    echo "  ${pod}: ledger=${BASE_LEDGER[${pod}]} key=${BASE_KEY[${pod}]:-unknown}"
  done

  # Verifies ledger advancement across the cluster; fails on stall.
  check_ledger_progress() {
    local phase="$1"
    local start_ledger
    start_ledger="$(latest_ledger "${POD_LIST[0]}")"
    start_ledger="${start_ledger:-0}"
    local waited=0
    while [[ "${waited}" -lt "${LEDGER_TIMEOUT}" ]]; do
      local current
      current="$(latest_ledger "${POD_LIST[0]}")"
      current="${current:-0}"
      if [[ "${current}" -gt "${start_ledger}" ]]; then
        echo "  ledger advanced ${start_ledger} -> ${current} (${phase})"
        return 0
      fi
      sleep 5
      waited=$((waited + 5))
    done
    echo "FAIL: ledger stalled at ${start_ledger} during ${phase}" >&2
    return 1
  }

  # Counts nodes whose latest ledger is within a small window of the max;
  # nodes lagging behind indicate quorum loss / desync.
  check_quorum() {
    local phase="$1"
    local max_ledger=0
    declare -A LEDGERS=()
    for pod in "${POD_LIST[@]}"; do
      local l
      l="$(latest_ledger "${pod}")"
      l="${l:-0}"
      LEDGERS["${pod}"]="${l}"
      if [[ "${l}" -gt "${max_ledger}" ]]; then
        max_ledger="${l}"
      fi
    done
    local in_sync=0
    for pod in "${POD_LIST[@]}"; do
      if [[ $(( max_ledger - LEDGERS[${pod}] )) -le 5 ]]; then
        in_sync=$((in_sync + 1))
      else
        echo "  WARN: ${pod} lagging at ledger ${LEDGERS[${pod}]} (max ${max_ledger})" >&2
      fi
    done
    if [[ "${in_sync}" -lt "${QUORUM}" ]]; then
      echo "FAIL: quorum lost during ${phase}: ${in_sync}/${NODE_COUNT} in sync (need ${QUORUM})" >&2
      return 1
    fi
    echo "  quorum intact: ${in_sync}/${NODE_COUNT} in sync (${phase})"
    return 0
  }

  # Rotate one node at a time, staggered, verifying quorum and ledger
  # advancement between each step.
  for pod in "${POD_LIST[@]}"; do
    echo "Rotating validator identity for ${pod}"
    kubectl -n "${NAMESPACE}" annotate pod "${pod}" \
      "stellar.example.com/rotate-seed=$(date -u +%Y-%m-%dT%H:%M:%SZ)" --overwrite
    kubectl -n "${NAMESPACE}" delete pod "${pod}" --wait=true
    kubectl -n "${NAMESPACE}" wait --for=condition=Ready pod "${pod}" \
      --timeout=300s

    NEW_KEY="$(node_public_key "${pod}")"
    if [[ -z "${NEW_KEY}" ]]; then
      echo "FAIL: ${pod} did not report a public key after rotation" >&2
      exit 1
    fi
    if [[ -n "${BASE_KEY[${pod}]}" && "${NEW_KEY}" == "${BASE_KEY[${pod}]}" ]]; then
      echo "FAIL: ${pod} still reports old public key ${NEW_KEY}" >&2
      exit 1
    fi
    echo "  ${pod}: new public key ${NEW_KEY}"

    check_quorum "after rotating ${pod}" || exit 1
    check_ledger_progress "after rotating ${pod}" || exit 1

    echo "  waiting ${STAGGER}s before next node"
    sleep "${STAGGER}"
  done

  # Final observation window: confirm ledger keeps advancing and quorum
  # holds for the whole window.
  echo "Observing cluster for ${WINDOW}s"
  ELAPSED=0
  while [[ "${ELAPSED}" -lt "${WINDOW}" ]]; do
    check_quorum "observation window" || exit 1
    check_ledger_progress "observation window" || exit 1
    ELAPSED=$((ELAPSED + LEDGER_TIMEOUT))
  done

  echo "PASS: all ${NODE_COUNT} validators rotated identities; ledger advanced without stall"
  exit 0
fi

BASELINE="$(kubectl -n "${NAMESPACE}" get deployment "${DEPLOYMENT}" \
  -o jsonpath='{.status.availableReplicas}')"
BASELINE="${BASELINE:-0}"
if [[ "${BASELINE}" -lt 1 ]]; then
  echo "FAIL: deployment ${DEPLOYMENT} has no available replicas before rotation" >&2
  exit 1
fi
echo "Baseline available replicas: ${BASELINE}"

ROTATED_AT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
kubectl -n "${NAMESPACE}" annotate secret "${SECRET}" \
  "stellar.example.com/rotated-at=${ROTATED_AT}" --overwrite
echo "Secret ${SECRET} rotated at ${ROTATED_AT}"

ELAPSED=0
while [[ "${ELAPSED}" -lt "${WINDOW}" ]]; do
  AVAILABLE="$(kubectl -n "${NAMESPACE}" get deployment "${DEPLOYMENT}" \
    -o jsonpath='{.status.availableReplicas}')"
  AVAILABLE="${AVAILABLE:-0}"
  if [[ "${AVAILABLE}" -lt "${BASELINE}" ]]; then
    echo "FAIL: available replicas dropped to ${AVAILABLE} (baseline ${BASELINE})" >&2
    exit 1
  fi
  sleep 5
  ELAPSED=$((ELAPSED + 5))
done

echo "PASS: no downtime observed during ${WINDOW}s after secret rotation"
