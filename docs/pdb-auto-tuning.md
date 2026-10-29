# PDB Auto-Tuning Controller

Keeps validators highly available during node maintenance. The controller
(`controller/src/pdb/`) watches `StellarNode` resources, observes each
validator's sync health, and manages one `PodDisruptionBudget` per validator
group (same namespace + network), so `kubectl drain` cannot take down a
validator that is catching up or that quorum depends on.

## Behaviour

| Group state | `maxUnavailable` | Reason code |
|---|---|---|
| Any validator catching up (`Progressing=True`, phase `Syncing`/`Creating`/`Pending`/`Remediating`) | `0` | `catching-up` |
| Any validator health unknown (not Ready, Degraded, Failed, terminating) | `0` | `health-unknown` |
| All synced but not yet stable for the stability window (default 60 s) | `0` | `not-yet-stable` |
| Quorum topology invalid / would allow disjoint quorums | `0` | `topology-unsafe` |
| Removing any one member would break quorum (e.g. 3 validators at the default threshold) | `0` | `quorum-floor` |
| All synced, stable, and quorum survives losing any one validator | `1` | `synced-and-stable` |

The decision is recorded on the PDB in the `stellar.org/pdb-reason`
annotation. Tightening is applied immediately; relaxing only after a
quorum check (`PdbDecision::violates_quorum_invariant`), otherwise the PDB is
held at `0`.

Quorum model: each validator trusts the whole group, with threshold
`n - floor((n-1)/3)` (Byzantine n−f). Override per `StellarNode` with the
annotation `stellar.org/pdb-min-quorum: "<k>"`; for a group the most
conservative (largest) value wins.

Fail-closed: if the cluster can't be observed nothing is changed; only PDBs
labelled `app.kubernetes.io/managed-by=stellar-pdb-controller` are ever
modified or deleted (orphans are garbage-collected).

## Requirements

* Validator pods carry `app.kubernetes.io/name=stellar-node`,
  `stellar.org/node-type=Validator` and `app.kubernetes.io/instance=<name>`.
* Pods are owned by a scalable controller (validators are StatefulSets);
  Kubernetes ignores `maxUnavailable` otherwise.
* Don't create another PDB selecting the same pods (multiple PDBs per pod make
  every eviction fail).
* RBAC: see `controller/deploy/rbac.yaml`.

## Running

```bash
cd controller
cargo run --features kube-backend --bin pdb-controller
# env: PDB_RECONCILE_INTERVAL_SECS (30), PDB_STABILITY_WINDOW_SECS (60), RUST_LOG
```

## Tests

```bash
cd controller
cargo test --lib pdb            # calculator, observation, reconciler logic
cargo test --test pdb_reconciler  # controller loop vs. in-memory cluster + eviction model
controller/tests/e2e/pdb-drain-blocked.sh   # kind: real `kubectl drain` (needs docker, kind, kubectl)
```
