# Royalty Splitter — Test Coverage Guide

This document describes every test in the royalty splitter's test suite, the
invariants each one exercises, and how to run them locally.

---

## Running the tests

```bash
cd contracts/royalty-splitter

# All unit tests + integration tests
cargo test

# Only the distribution math unit tests
cargo test --lib

# Only the integration tests (requires Soroban testutils)
cargo test --test integration
```

Expected output:

```
running 5 tests  (distribution unit tests)
test distribution::test::approvals_must_cover_every_payee         ... ok
test distribution::test::allocations_always_conserve_the_amount   ... ok
test distribution::test::remainder_is_paid_to_the_last_payee      ... ok
test distribution::test::validate_accepts_a_well_formed_split     ... ok
test distribution::test::validate_rejects_bad_splits              ... ok

running 7 tests  (integration tests)
test initialization_rejects_invalid_splits        ... ok
test lifecycle_guards_are_enforced                ... ok
test reconfiguration_redirects_future_payments    ... ok
test reconfiguration_requires_unanimous_approval  ... ok
test preview_matches_settlement_and_strands_no_dust ... ok
test splits_10_000_tokens_without_dust            ... ok
test tracks_payment_counters                      ... ok
```

---

## Unit tests — `src/distribution.rs`

All tests live in the `distribution::test` module inside `src/distribution.rs`.
They target the pure math layer directly, without spinning up a contract or
touching any token.

### `validate_accepts_a_well_formed_split`

Asserts that a 3-payee configuration with shares
`[333_330, 333_330, 333_340]` (summing exactly to `1_000_000`) passes
validation without error.

### `validate_rejects_bad_splits`

Exercises every `SplitError` variant that `validate` can return:

| Case | Expected error |
|---|---|
| Empty `Vec<Payee>` | `EmptyPayees` |
| Shares sum to `900_000` | `SharesDoNotSumToTotal` |
| A payee with `0` shares | `ZeroShare` |
| Two payees with the same `Address` | `DuplicatePayee` |

### `remainder_is_paid_to_the_last_payee`

Sends `10_000` tokens through a `[333_330, 333_330, 333_340]` split and asserts
the exact allocations:

| Payee | Amount |
|---|---|
| 1 | `3_333` |
| 2 | `3_333` |
| 3 (last / dust sink) | `3_334` |

This is the specification scenario. The `1`-unit remainder from truncating the
first two payees is absorbed by the final payee rather than stranded.

### `allocations_always_conserve_the_amount`

Runs a 4-payee split with deliberately asymmetric shares
`[333_333, 111_111, 222_222, 333_334]` across a range of amounts
(`1`, `7`, `99`, `1_000_001`, `u32::MAX` as `i128`) and verifies:

- Every individual allocation is `≥ 0`.
- All allocations sum to exactly `amount`.

This is the **dust-free conservation invariant**.

### `approvals_must_cover_every_payee`

Given a 2-payee config `[A, B]`:

- A list containing only `A` returns `false` from `approvals_cover`.
- A list containing both `A` and `B` returns `true`.

---

## Integration tests — `tests/integration.rs`

Each integration test deploys a fresh `RoyaltySplitter` contract and a real
Stellar Asset Contract (`register_stellar_asset_contract_v2`) to the Soroban
test environment. This exercises the full token transfer path — not a mock.

### `splits_10_000_tokens_without_dust` ★ specification scenario

**Setup:** 3 payees, `[333_330, 333_330, 333_340]` shares. Payer minted
`10_000` tokens.

**Assertions:**

```
creator   balance == 3_333
platform  balance == 3_333
affiliate balance == 3_334   ← its own entitlement + 1 unit of dust
contract  balance == 0       ← zero dust stranded
payer     balance == 0       ← fully debited
sum(all payee balances) == 10_000
```

### `preview_matches_settlement_and_strands_no_dust`

7-payee split with uneven shares summing to `1_000_000`. A prime-ish amount
(`1_000_003`) maximises truncation rounding.

1. Calls `preview(amount)` before the payment.
2. Calls `process_payment`.
3. Asserts each payee's on-chain balance matches the corresponding `preview`
   allocation exactly.
4. Asserts the contract balance is `0`.

### `reconfiguration_requires_unanimous_approval`

Initial split: `[A, B]`. Proposed new split: `[C, D]`.

| Approval list | Expected result |
|---|---|
| Only `A` | `Err(MissingApproval)` |
| `A` + an unrelated address | `Err(MissingApproval)` |
| Both `A` and `B` | `Ok(())` — reconfiguration succeeds |

After success, `get_config()` returns the new `[C, D]` split.

### `reconfiguration_redirects_future_payments`

1. Initialize with `[old_a, old_b]`.
2. Reconfigure to `[new_a (70%), new_b (30%)]` with unanimous approval.
3. Send `1_000` tokens through `process_payment`.

Asserts that `old_a` and `old_b` receive nothing, while
`new_a` receives `700` and `new_b` receives `300`.

### `tracks_payment_counters`

Runs two payments (`1_000` then `1_500`) and asserts:

```
payments_processed() == 2
total_routed()       == 2_500
contract balance     == 0
```

### `initialization_rejects_invalid_splits`

| Configuration | Expected error |
|---|---|
| Shares sum to `900_000` | validation error |
| A payee with `0` shares | validation error |
| Two payees with identical addresses | validation error |

### `lifecycle_guards_are_enforced`

- `process_payment` before `initialize` → error.
- Calling `initialize` a second time → `Err(AlreadyInitialized)`.

---

## Precision model summary

```
TOTAL_SHARES = 1_000_000   (parts per million — 6 decimal places)

split(i) for i in 0..N-1 = floor(amount × shares[i] / 1_000_000)
split(N-1)                = amount − Σ split(0..N-2)   ← absorbs remainder
```

Floating point is never used. All arithmetic is `i128` with
`checked_mul` / `checked_add` / `checked_sub` to surface overflows as
`SplitError::MathOverflow` rather than silently wrapping.
