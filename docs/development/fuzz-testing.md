# Operator State Verification and Fuzz Testing Guide

This guide explains how to use property-based testing and state-machine fuzzing to prove that the Stellar-K8s operator handles arbitrary, malformed, and adversarial `StellarNode` CRD mutations without panicking, entering an undefined state, or violating critical safety invariants.

---

## Why Fuzz Test a Kubernetes Operator?

Kubernetes operators run indefinitely against live clusters. Any unhandled edge case in the reconciliation loop becomes a production outage. Common failure vectors include:

- Malformed user-supplied YAML that passes schema validation but fails semantic checks.
- Concurrent spec changes mid-reconcile that leave resources in an inconsistent state.
- Retention policy bugs that cause accidental PVC deletion.
- Node type / config mismatches (e.g. a `Horizon` spec with a `validatorConfig` block).

Property-based fuzzing forces the operator to process millions of such inputs automatically, shrinking failing cases to minimal reproducers.

---

## Tools and Approach

| Layer | Tool | Purpose |
|-------|------|---------|
| Property-based testing | [`proptest`](https://docs.rs/proptest) | Generate structured random `StellarNodeSpec` values |
| State-machine fuzzing | Custom proptest strategies | Model multi-step event sequences |
| Coverage-guided fuzzing | `cargo-fuzz` (libFuzzer) | Optional raw-byte fuzz targets |
| Snapshot testing | Inline `assert_eq!` | Pin expected validation error messages |

The primary harness lives in `tests/fuzz/crd_mutations.rs` and is gated behind the `reconciler-fuzz` Cargo feature.

---

## Setting Up

### Prerequisites

- Rust 1.92+ (`rustup update stable`)
- All workspace dependencies resolved (`cargo fetch`)
- Optional: `cargo install cargo-fuzz` for libFuzzer targets

### Running the property tests

```bash
# Run all fuzz/property tests
cargo test -p stellar-k8s --features reconciler-fuzz --test crd_mutations

# Increase case count for deeper coverage
PROPTEST_CASES=10000 cargo test -p stellar-k8s --features reconciler-fuzz --test crd_mutations

# Show verbose output including shrunk failing cases
cargo test -p stellar-k8s --features reconciler-fuzz --test crd_mutations -- --nocapture
```

### Running the existing reconciler fuzz tests

```bash
# Original reconciler state-machine tests
cargo test -p stellar-k8s --features reconciler-fuzz --test reconciler_fuzz
```

---

## Understanding the Test Harness

### `tests/fuzz/crd_mutations.rs`

This file contains the `crd_mutations` integration test suite. It is structured in three layers:

#### Layer 1: Spec Generation Strategies

`proptest` strategies produce valid-looking `StellarNodeSpec` values by composing smaller generators:

```rust
fn arb_node_type() -> impl Strategy<Value = NodeType> {
    prop_oneof![
        Just(NodeType::Validator),
        Just(NodeType::Horizon),
        Just(NodeType::SorobanRpc),
    ]
}

fn arb_storage_config() -> impl Strategy<Value = StorageConfig> {
    (arb_retention_policy(), "[0-9]{1,4}Gi")
        .prop_map(|(retention_policy, size)| StorageConfig { ... })
}
```

Strategies are composable: `arb_validator_spec()`, `arb_horizon_spec()`, and `arb_soroban_spec()` each build a well-typed spec for their node type.

#### Layer 2: Mutation Strategies

Mutations represent realistic user actions: changing the version, toggling `suspended`, adjusting replicas, or swapping the `nodeType` entirely. The `arb_mutation()` strategy produces values of:

```rust
enum SpecMutation {
    ChangeReplicas(i32),
    ToggleSuspended,
    ChangeVersion(String),
    ChangeNetwork(StellarNetwork),
    ChangeRetentionPolicy(RetentionPolicy),
}
```

#### Layer 3: Invariant Assertions

Each test body calls `spec.validate()` and asserts the safety invariants defined in the next section. It **never** unwraps `Result` unconditionally — a `validate()` returning `Err` is an expected, safe outcome. An unexpected *panic* is the failure condition proptest catches.

---

## State Verification Assertions (Invariant Matrix)

The following table defines every invariant checked by `tests/fuzz/crd_mutations.rs`. Each invariant is either tested directly or is a property of the validation logic under test.

| # | Invariant | Tested by | Failure means |
|---|-----------|-----------|---------------|
| I-1 | `spec.validate()` must **never panic** regardless of input | `prop_spec_validation_never_panics` | Operator crash on bad user input |
| I-2 | A deleted PVC must **never be recreated** if `retentionPolicy: Delete` | `prop_delete_retention_no_pvc_recreate` | Data-loss / orphaned storage cost |
| I-3 | A `Validator` spec without `validatorConfig` must return a **validation error**, not `Ok(())` | `prop_validator_requires_validator_config` | Silent misconfiguration |
| I-4 | A `Horizon` spec without `horizonConfig` must return a **validation error** | `prop_horizon_requires_horizon_config` | Silent misconfiguration |
| I-5 | A `SorobanRpc` spec without `sorobanConfig` must return a **validation error** | `prop_soroban_requires_soroban_config` | Silent misconfiguration |
| I-6 | `replicas <= 0` must produce a **validation error** (not a panic) | `prop_invalid_replica_count_is_error` | Zero-replica deployment silently created |
| I-7 | `network: Custom` without `customNetworkPassphrase` must produce a **validation error** | `prop_custom_network_requires_passphrase` | Node connects to wrong/no network |
| I-8 | Multi-step event sequences must **converge** (validate returns Ok or Err within bounded time) | `prop_event_sequence_converges` | Reconciler hangs / livelock |
| I-9 | Validation errors must carry a human-readable `how_to_fix` string | `prop_validation_errors_have_guidance` | Operator logs are unhelpful |
| I-10 | Swapping `nodeType` mid-stream must produce a deterministic outcome (error or ok, no panic) | `prop_node_type_swap_is_safe` | Reconciler panic on live update |

---

## Differentiating Rejection Categories

The issue distinguishes two failure categories. Tests must respect this boundary:

### Category A: Expected Kubernetes API Rejections (Validation Webhook)

These are caught **before** the operator reconcile loop runs. The admission webhook or CRD schema rejects the manifest. Examples:

- `spec.replicas` is a string instead of an integer → OpenAPI schema validation rejects it.
- A required field like `spec.nodeType` is missing → CRD `x-kubernetes-validations` rejects it.
- `spec.storage.size` does not match the `^[0-9]+[KMGT]i?$` pattern → Schema pattern check.

**In tests:** These cases are exercised by the webhook conformance tests in `tests/admission_webhook_conformance.rs`, not by the fuzz harness. The fuzz harness only generates type-safe Rust values; it cannot produce structurally invalid YAML.

### Category B: Fatal Internal Operator Panics

These occur inside the reconcile loop when the operator receives a spec that passed webhook validation but triggers unexpected logic paths. Examples:

- `nodeType: Validator` but `validatorConfig` is `None` → operator dereferences `None` with `unwrap()` → **panic**.
- `retentionPolicy: Delete` but PVC ownership tracking is missing → **data loss** (not a panic, but a correctness violation).
- An empty string version `""` triggers an unchecked `.split('.').nth(0).unwrap()` → **panic**.

**In tests:** The fuzz harness in `tests/fuzz/crd_mutations.rs` targets exactly this category. All `proptest!` tests use `catch_unwind` semantics (proptest panics fail the test) so any operator panic is immediately surfaced.

---

## Injecting a Logic Bug to Validate the Harness

The issue requires that you can introduce a deliberate bug and verify the fuzzer catches it. Here is the recommended procedure:

### Step 1: Introduce the bug

In `src/crd/stellar_node.rs`, locate the `validate()` method. Find the Validator config check:

```rust
// Existing safe code:
if self.node_type == NodeType::Validator && self.validator_config.is_none() {
    errors.push(SpecValidationError::new(
        "validatorConfig",
        "Validator nodes require validatorConfig",
        "Add a validatorConfig block with at least seedSecretRef",
    ));
}
```

**Comment it out** (the bug: the check is removed):

```rust
// BUG: intentionally removed for fuzzer validation
// if self.node_type == NodeType::Validator && self.validator_config.is_none() {
//     errors.push(...)
// }
```

Then in `src/controller/reconciler.rs` (or wherever the reconciler accesses `validator_config`), add a panic:

```rust
// Simulated downstream panic when the invariant is violated:
let vc = spec.validator_config.as_ref().expect("validator_config must be set for Validator nodes");
```

### Step 2: Run the fuzzer

```bash
cargo test -p stellar-k8s --features reconciler-fuzz --test crd_mutations prop_validator_requires_validator_config
```

### Step 3: Observe the failure

proptest prints a minimal counterexample immediately:

```
thread 'prop_validator_requires_validator_config' panicked at 'validator_config must be set for Validator nodes'
Minimal failing input:
  spec = StellarNodeSpec { node_type: Validator, validator_config: None, ... }
```

### Step 4: Revert the bug

```bash
git diff src/crd/stellar_node.rs   # verify
git restore src/crd/stellar_node.rs src/controller/reconciler.rs
```

---

## Generating Large-Scale Random YAML Configurations

For integration-level testing beyond the unit fuzz harness, you can generate random YAML manifests at scale:

```bash
# Generate 1000 random StellarNode manifests and validate them against the CRD schema
PROPTEST_CASES=1000 cargo test -p stellar-k8s --features reconciler-fuzz \
  --test crd_mutations prop_generated_yaml_valid_schema -- --nocapture

# Pipe generated YAML to kubectl for dry-run admission validation
# (requires a running cluster with the CRD installed)
cargo run --bin stellar-operator -- generate-test-manifests --count 100 \
  | kubectl apply --dry-run=server -f -
```

---

## Environment Variables

| Variable | Default | Effect |
|----------|---------|--------|
| `PROPTEST_CASES` | 256 | Number of random cases per `proptest!` test |
| `PROPTEST_MAX_SHRINK_ITERS` | 65536 | Maximum shrink iterations for failing cases |
| `PROPTEST_SEED` | random | Fixed seed for deterministic reproduction |

### Reproducing a specific failure

When proptest finds a failure, it prints a seed and the failing input. To reproduce:

```bash
# From the failure output: seed = 0xdeadbeef...
PROPTEST_SEED=0xdeadbeef... cargo test -p stellar-k8s --features reconciler-fuzz \
  --test crd_mutations prop_spec_validation_never_panics
```

proptest also persists failure seeds in `.proptest-regressions/` (committed to the repo) so CI always re-runs known-bad inputs.

---

## Adding New Invariants

To add a new invariant to the harness:

1. Identify the state boundary in natural language: _"A node with `maintenanceMode: true` must not have `autoscaling` enabled simultaneously."_
2. Write a minimal failing example (unit test in `src/crd/stellar_node.rs` or `src/crd/tests.rs`).
3. Encode it as a proptest strategy in `tests/fuzz/crd_mutations.rs`.
4. Add a row to the invariant matrix table above.
5. Verify the fuzzer catches it by temporarily removing the validation rule.

---

## CI Integration

The fuzz tests run in the `test` job of `.github/workflows/ci.yml`:

```yaml
- name: Run fuzz / property tests
  run: |
    PROPTEST_CASES=512 cargo test \
      -p stellar-k8s \
      --features reconciler-fuzz \
      --test crd_mutations \
      --test reconciler_fuzz
```

Failures produce a `proptest-regressions/` artifact that is uploaded and pinned in subsequent runs.

---

## Related Documentation

- [`docs/fuzzing.md`](../fuzzing.md) — Quick-start for the original `reconciler_fuzz` tests.
- [`tests/reconciler_fuzz.rs`](../../tests/reconciler_fuzz.rs) — Original reconciler state-machine fuzzer.
- [`tests/fuzz/crd_mutations.rs`](../../tests/fuzz/crd_mutations.rs) — CRD mutation harness (this issue).
- [`src/crd/stellar_node.rs`](../../src/crd/stellar_node.rs) — `StellarNodeSpec::validate()` implementation.
- [`docs/EDGE_CASES.md`](../EDGE_CASES.md) — Catalogue of known edge cases and their handling.
- [`docs/FORMAL_VERIFICATION.md`](../FORMAL_VERIFICATION.md) — TLA+ model for the reconciler state machine.
