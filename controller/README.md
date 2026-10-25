# Stellar-K8s Controller

A Kubernetes operator that provisions **ephemeral, single-node Stellar
networks** from a `StellarTestnet` custom resource — isolated testnets for
CI/CD pipelines that come up in under 30 seconds and disappear completely on
delete.

```
StellarTestnet ──► reconcile loop ──► Deployment (stellar/quickstart: Core + Soroban RPC)
                                       ├── Service <name>-rpc  (RPC + friendbot, :8000)
                                       ├── Service <name>-core (Core admin, :11626)
                                       └── Ingress <name>-rpc  (optional)
```

## What it does

- **CRD** (`src/crd/testnet.rs`): `StellarTestnet` defines genesis ledger
  configuration (network passphrase, ledger protocol version, memory- or
  disk-backed storage), pre-funded test accounts, the Soroban RPC interface
  (optional Ingress), the container image, and an optional
  `ttlSecondsAfterReady` safety net for CI jobs.
- **Reconciliation** (`src/testnet/provisioner.rs`): creates/updates the
  child resources with server-side apply, waits for the readiness probe
  (which gates on a healthy Soroban RPC), verifies the network reports the
  configured passphrase, funds every declared account through the network's
  own friendbot, and publishes everything into `status`.
- **Status**: `phase`, `rpcUrl`, `friendbotUrl`, `coreAdminUrl`,
  `externalRpcUrl`, `networkPassphrase`, `rootAccount`, funded keypairs
  (`accounts[].secretKey` for generated keys) and a `Ready` condition for
  `kubectl wait`.
- **Teardown**: every child carries an owner reference **and** a shared
  `stellar.org/testnet=<name>` label; a finalizer sweeps owned
  resources before the object is released, so deleting a `StellarTestnet`
  garbage-collects the whole network (no cluster bloat). `ttlSecondsAfterReady`
  deletes the resource automatically even if the CI job dies first.
- **CRD install**: the operator applies its own CRD at startup
  (`stellar-testnet-operator crd` prints the YAML).

## Key derivation

- The **genesis root account** is `sha256(network passphrase)` interpreted
  as an ed25519 seed — the same derivation Stellar Core uses at
  `startNewLedger` — so `status.rootAccount` is always correct for the
  configured passphrase (verified in unit tests against the documented
  standalone root `GBZXN7PIRZGNMHGA7MUUUF4GWPY5AYPV6LY4UV2GL6VJGIQRXFDNMADI`).
- **Pre-funded accounts** are derived deterministically from
  `(passphrase, namespace, testnet name, account name)`, so repeated
  reconciles yield the same keypair and two testnets never collide.

## Install

```bash
# 1. Build and push the operator image
docker build -t <registry>/stellar-testnet-operator:latest controller

# 2. RBAC + operator (point the image at your registry first)
kubectl apply -f controller/deploy/rbac.yaml
kubectl apply -f controller/deploy/operator.yaml

# 3. CRD (optional — the operator installs it at startup, but applying it
#    first lets kubectl accept a StellarTestnet immediately)
kubectl apply -f controller/deploy/stellartestnets.yaml
```

## Use

```bash
kubectl apply -f controller/examples/stellartestnet.yaml -n stellar-ci
kubectl get stellartestnet -n stellar-ci        # Phase / RPC / Age columns
kubectl wait --for=condition=Ready stellartestnet/pr-246 -n stellar-ci --timeout=40s

# Connection details and funded keys:
kubectl get stellartestnet pr-246 -n stellar-ci \
  -o jsonpath='{.status.rpcUrl}{"\n"}{.status.accounts[*].secretKey}{"\n"}'

# Teardown (owner references + finalizer remove everything):
kubectl delete stellartestnet pr-246 -n stellar-ci
```

## Test scripts

`scripts/test-ephemeral-testnet.sh` is the end-to-end validation for this
feature. It applies the example manifest, asserts the network is `Ready`
and every account funded **within 30 seconds**, probes the Soroban RPC
(`getHealth`, `getNetwork`), then builds, uploads and deploys
`contracts/royalty-splitter` against the ephemeral endpoint, initializes it
with the generated `deployer` account, has `ci-bot` route a native-asset
payment through it, and finally verifies that deletion leaves no owned
resources behind:

```bash
./scripts/test-ephemeral-testnet.sh                 # 30s budget, cleans up
./scripts/test-ephemeral-testnet.sh --keep --timeout 45
./scripts/test-ephemeral-testnet.sh --port-forward  # from outside the cluster (e.g. kind)
```

Prerequisites: `kubectl`, `curl` and the `stellar` CLI v25.2+ (also used to
build the contract wasm if absent).

## Development

```bash
cargo test
cargo run --bin stellar-testnet-operator -- crd > deploy/stellartestnets.yaml  # after CRD changes
```
