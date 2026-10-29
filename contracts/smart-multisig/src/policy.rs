//! Programmable `ExecutionPolicy` stored in Instance Storage.
//!
//! # Responsibility
//!
//! This module owns all **policy data types**, their **storage keys**, and the
//! **pure validation functions** that are evaluated *before* any signature
//! loop runs.  Keeping policy checks ahead of signature validation is a
//! deliberate CPU-saving measure: an invalid transaction can be cheaply
//! rejected without iterating over potentially large signer sets.
//!
//! # Policy rules
//!
//! Two independent guards are enforced:
//!
//! 1. **Base-fee guard** — the caller submits the current network base fee
//!    (in stroops) alongside the execution request.  If the submitted fee
//!    exceeds `max_allowed_fee_stroops`, all outbound transfers are paused.
//!    This protects a DAO treasury from gas-spike depletion during severe
//!    network congestion.
//!
//!    > **Why pass the fee as an argument?**  The Soroban SDK 27 host does
//!    > not expose the ledger base fee inside WASM contracts.  The canonical
//!    > approach is for the submitting relayer/signer to include the current
//!    > network fee in the transaction arguments; the contract validates it
//!    > against the stored policy ceiling.  This is consistent with how other
//!    > fee-aware Soroban contracts in this repo handle fee data.
//!
//! 2. **TWAP deviation guard** — if the percentage deviation between the
//!    on-chain spot price (supplied by a cross-contract TWAP oracle call) and
//!    the policy's reference TWAP price exceeds `max_price_deviation_bps`
//!    (in basis points), large asset swaps are blocked to prevent
//!    exploitation during flash-crash or oracle-manipulation windows.
//!
//! Both guards can be disabled independently by setting their threshold to
//! `u32::MAX` (fee) or `u32::MAX` (deviation bps) as a sentinel.
//!
//! # Storage layout
//!
//! All keys live in **Instance Storage** so they share the contract's TTL and
//! cannot silently outlive the owning wallet.
//!
//! | [`DataKey`] variant   | Type              | Description                         |
//! |-----------------------|-------------------|-------------------------------------|
//! | `Signers`             | `Vec<Address>`    | Authorised signer set               |
//! | `Threshold`           | `u32`             | Required signature count            |
//! | `Policy`              | `ExecutionPolicy` | Active programmable policy          |
//! | `TxCounter`           | `u64`             | Monotonic pending-tx counter        |
//! | `PendingTx(id)`       | `PendingTx`       | In-flight transaction record        |
//! | `Signatures(id)`      | `Vec<Address>`    | Collected approvals for a tx        |

#![allow(dead_code)]

use soroban_sdk::{contracterror, contracttype, Address, Env, Vec};

// ---------------------------------------------------------------------------
// Storage keys
// ---------------------------------------------------------------------------

/// All persistent storage keys used by the smart-multisig contract.
///
/// Soroban serialises `#[contracttype]` enum variants by name, so variant
/// names are part of the on-chain storage schema and must never be renamed
/// after deployment.
#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    /// Admin address — set once during construction, may be rotated by admin.
    Admin,
    /// Ordered list of authorised signers.
    Signers,
    /// Minimum number of signer approvals required to execute a transaction.
    Threshold,
    /// Active programmable execution policy.
    Policy,
    /// Auto-incrementing counter used to assign unique IDs to pending txs.
    TxCounter,
    /// Individual pending transaction record keyed by its u64 ID.
    PendingTx(u64),
    /// Set of signer addresses that have approved pending tx `id`.
    Signatures(u64),
}

// ---------------------------------------------------------------------------
// ExecutionPolicy
// ---------------------------------------------------------------------------

/// Programmable execution policy stored in Instance Storage.
///
/// The policy is consulted **before** the signature validation loop so that
/// network-condition violations cause an immediate, cheap revert without
/// wasting CPU on cryptographic verification.
///
/// ## Fields
///
/// * `max_allowed_fee_stroops` — Ledger base-fee ceiling (in stroops).
///   If `env.ledger().base_fee()` exceeds this value, all outbound transfers
///   are paused.  Set to `u32::MAX` to disable the fee guard.
///
/// * `twap_oracle` — `Address` of the external TWAP oracle contract.
///   The contract must expose a `get_twap() → i128` function that returns
///   the current time-weighted average price (scaled by 1 × 10⁷ = one XLM
///   stroop equivalent).  Set to the contract's own address to disable
///   oracle verification (self-call always returns the reference price).
///
/// * `reference_twap_price` — The baseline TWAP price (same scale as the
///   oracle) captured when the policy was last configured.  Swaps are
///   blocked when the live oracle price deviates beyond
///   `max_price_deviation_bps` from this reference.
///
/// * `max_price_deviation_bps` — Maximum tolerated TWAP deviation expressed
///   in basis points (1/10 000).  E.g. `500` == 5%.  Set to `10_000` or
///   higher to allow any deviation (effectively disabled).
///
/// ## GitOps payload approval fields
///
/// These fields gate the emission of GitOps deployment trigger events.  A
/// proposed infrastructure payload (identified by an IPFS hash or Git commit
/// SHA) must be approved by at least `gitops_threshold` distinct members of
/// `gitops_signers` before it can be emitted.  Each approval is bound to a
/// monotonically increasing `gitops_nonce` and must fall inside a bounded
/// ledger window so that stale or replayed approvals are rejected.
///
/// * `gitops_threshold` — Minimum number of authorised GitOps signers that
///   must approve a payload hash before emission.  Must be ≥ 1 and ≤ the
///   length of `gitops_signers`.
///
/// * `gitops_signers` — Authorised GitOps approver set.  Only addresses in
///   this set may contribute approvals.
///
/// * `gitops_nonce` — Monotonic replay-protection counter.  Every emitted
///   payload consumes exactly one nonce; approvals are only valid for the
///   current nonce value.
///
/// * `gitops_window_ledgers` — Maximum age (in ledgers) of an approval
///   relative to the ledger at which it was recorded.  Approvals older than
///   this window are rejected as stale.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionPolicy {
    /// Ledger base-fee ceiling (stroops). Transfers blocked above this.
    pub max_allowed_fee_stroops: u32,
    /// Address of the external TWAP oracle contract.
    pub twap_oracle: Address,
    /// Baseline TWAP price captured at policy configuration time.
    pub reference_twap_price: i128,
    /// Maximum tolerated TWAP deviation in basis points (e.g. 500 = 5 %).
    pub max_price_deviation_bps: u32,
    /// Minimum number of GitOps signer approvals required to emit a payload.
    pub gitops_threshold: u32,
    /// Authorised GitOps approver set.
    pub gitops_signers: Vec<Address>,
    /// Monotonic replay-protection nonce for GitOps payload emissions.
    pub gitops_nonce: u64,
    /// Maximum age (in ledgers) of a GitOps approval before it is stale.
    pub gitops_window_ledgers: u32,
}

// ---------------------------------------------------------------------------
// Pending transaction record
// ---------------------------------------------------------------------------

/// An outbound token transfer proposed by a signer and awaiting threshold
/// approval.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingTx {
    /// Unique identifier assigned at proposal time.
    pub id: u64,
    /// Address of the SEP-41 / Stellar token contract to transfer.
    pub token: Address,
    /// Destination account.
    pub to: Address,
    /// Amount in the token's smallest unit (stroops for XLM).
    pub amount: i128,
    /// Signer that originally proposed this transaction.
    pub proposer: Address,
    /// Whether this transaction has already been executed.
    pub executed: bool,
}

// ---------------------------------------------------------------------------
// Error codes
// ---------------------------------------------------------------------------

/// All error conditions surfaced by the smart-multisig contract.
///
/// Error codes are `#[repr(u32)]` so they appear as typed contract errors in
/// XDR diagnostics and can be matched by client SDKs.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PolicyError {
    /// Contract has already been initialised.
    AlreadyInitialized = 1,
    /// Contract has not been initialised yet.
    NotInitialized = 2,
    /// Caller is not authorised to perform this action.
    Unauthorized = 3,
    /// `threshold` must be ≥ 1 and ≤ number of signers.
    InvalidThreshold = 4,
    /// The signer set must not be empty.
    EmptySignerSet = 5,
    /// The referenced pending transaction does not exist.
    TxNotFound = 6,
    /// The transaction has already been executed.
    TxAlreadyExecuted = 7,
    /// The signer has already approved this transaction.
    AlreadySigned = 8,
    /// Insufficient approvals to execute (threshold not met).
    ThresholdNotMet = 9,
    // ---- Policy violation codes (must come before signature loop) ----
    /// Current ledger base fee exceeds `max_allowed_fee_stroops`.
    /// All outbound transfers are paused until congestion subsides.
    FeeTooHigh = 10,
    /// Live TWAP oracle price deviates from the reference by more than
    /// `max_price_deviation_bps` basis points.
    TwapDeviationTooLarge = 11,
    /// Transfer amount is zero or negative.
    InvalidAmount = 12,
    /// Arithmetic overflow in policy calculation.
    Overflow = 13,
    // ---- GitOps payload approval codes ----
    /// `gitops_threshold` must be ≥ 1 and ≤ number of GitOps signers.
    InvalidGitopsThreshold = 14,
    /// The GitOps signer set must not be empty.
    EmptyGitopsSignerSet = 15,
    /// Caller is not an authorised GitOps signer.
    NotGitopsSigner = 16,
    /// The supplied nonce does not match the policy's current nonce.
    InvalidGitopsNonce = 17,
    /// The approval is older than `gitops_window_ledgers` and is stale.
    StaleGitopsApproval = 18,
    /// The GitOps payload hash has already been approved by this signer.
    DuplicateGitopsApproval = 19,
    /// The GitOps payload hash is empty.
    EmptyGitopsPayload = 20,
}

// ---------------------------------------------------------------------------
// Instance storage helpers
// ---------------------------------------------------------------------------

/// Convenience: extend the instance storage TTL on every mutating call.
///
/// Using the maximum ledger values keeps the contract alive as long as
/// possible, consistent with other contracts in this repo (governance-vote,
/// staking-vault).
pub fn refresh_ttl(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(17_280 * 30, 17_280 * 30);
}

/// Persist the `ExecutionPolicy` in Instance Storage.
pub fn save_policy(env: &Env, policy: &ExecutionPolicy) {
    env.storage().instance().set(&DataKey::Policy, policy);
}

/// Load the `ExecutionPolicy` from Instance Storage.
///
/// Returns `Err(PolicyError::NotInitialized)` if no policy has been stored.
pub fn load_policy(env: &Env) -> Result<ExecutionPolicy, PolicyError> {
    env.storage()
        .instance()
        .get(&DataKey::Policy)
        .ok_or(PolicyError::NotInitialized)
}

/// Persist a `PendingTx`.
pub fn save_pending_tx(env: &Env, tx: &PendingTx) {
    env.storage()
        .instance()
        .set(&DataKey::PendingTx(tx.id), tx);
}

/// Load a `PendingTx` by ID.
pub fn load_pending_tx(env: &Env, id: u64) -> Result<PendingTx, PolicyError> {
    env.storage()
        .instance()
        .get(&DataKey::PendingTx(id))
        .ok_or(PolicyError::TxNotFound)
}

/// Load the current signer approval list for `tx_id`.
pub fn load_signatures(env: &Env, tx_id: u64) -> Vec<Address> {
    env.storage()
        .instance()
        .get(&DataKey::Signatures(tx_id))
        .unwrap_or_else(|| Vec::new(env))
}

/// Persist the signer approval list for `tx_id`.
pub fn save_signatures(env: &Env, tx_id: u64, sigs: &Vec<Address>) {
    env.storage()
        .instance()
        .set(&DataKey::Signatures(tx_id), sigs);
}

// ---------------------------------------------------------------------------
// Policy validation — fee guard
// ---------------------------------------------------------------------------

/// Assert that `submitted_fee_stroops` does not exceed the policy ceiling.
///
/// The Soroban SDK 27 host does not expose the ledger base fee inside WASM
/// contracts. The submitting relayer/signer passes the current network base
/// fee (in stroops) as a transaction argument; the contract validates it
/// against the stored policy ceiling.  This check runs **first**, before any
/// signature iteration, so a policy violation causes a cheap, immediate revert.
///
/// # Errors
///
/// * [`PolicyError::FeeTooHigh`] — `submitted_fee_stroops` >
///   `policy.max_allowed_fee_stroops`.
pub fn assert_fee_within_policy(
    submitted_fee_stroops: u32,
    policy: &ExecutionPolicy,
) -> Result<(), PolicyError> {
    if submitted_fee_stroops > policy.max_allowed_fee_stroops {
        return Err(PolicyError::FeeTooHigh);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Policy validation — TWAP deviation guard
// ---------------------------------------------------------------------------

/// Compute the absolute percentage deviation (in basis points) between
/// `live_price` and `reference_price`.
///
/// ```text
/// deviation_bps = abs(live - reference) * 10_000 / reference
/// ```
///
/// Returns `Err(PolicyError::Overflow)` if any intermediate multiplication
/// would overflow `i128`.
///
/// Returns `Err(PolicyError::NotInitialized)` if `reference_price` is zero
/// (guard cannot be computed).
pub fn compute_deviation_bps(
    live_price: i128,
    reference_price: i128,
) -> Result<u32, PolicyError> {
    if reference_price == 0 {
        return Err(PolicyError::NotInitialized);
    }
    let diff = (live_price - reference_price).abs();
    // Multiply first to preserve precision, then divide.
    let numerator = diff
        .checked_mul(10_000)
        .ok_or(PolicyError::Overflow)?;
    let bps = numerator / reference_price.abs();
    // Safe cast: result fits in u32 for any realistic price pair.
    Ok(bps.min(u32::MAX as i128) as u32)
}

/// Assert that `live_price` does not deviate from `policy.reference_twap_price`
/// beyond `policy.max_price_deviation_bps`.
///
/// # Errors
///
/// * [`PolicyError::TwapDeviationTooLarge`] — deviation exceeds the policy limit.
/// * [`PolicyError::Overflow`] / [`PolicyError::NotInitialized`] — propagated
///   from [`compute_deviation_bps`].
pub fn assert_twap_within_policy(
    policy: &ExecutionPolicy,
    live_price: i128,
) -> Result<(), PolicyError> {
    // If the operator set max_price_deviation_bps to u32::MAX the guard is
    // explicitly disabled; skip the division entirely.
    if policy.max_price_deviation_bps == u32::MAX {
        return Ok(());
    }
    let deviation = compute_deviation_bps(live_price, policy.reference_twap_price)?;
    if deviation > policy.max_price_deviation_bps {
        return Err(PolicyError::TwapDeviationTooLarge);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Policy validation — GitOps payload approval guards
// ---------------------------------------------------------------------------

/// Assert that the GitOps approval configuration is internally consistent.
///
/// # Errors
///
/// * [`PolicyError::EmptyGitopsSignerSet`] — `gitops_signers` is empty.
/// * [`PolicyError::InvalidGitopsThreshold`] — `gitops_threshold` is zero or
///   exceeds the number of authorised GitOps signers.
pub fn assert_gitops_config_valid(policy: &ExecutionPolicy) -> Result<(), PolicyError> {
    let signer_count = policy.gitops_signers.len();
    if signer_count == 0 {
        return Err(PolicyError::EmptyGitopsSignerSet);
    }
    if policy.gitops_threshold == 0 || policy.gitops_threshold > signer_count {
        return Err(PolicyError::InvalidGitopsThreshold);
    }
    Ok(())
}

/// Assert that `signer` is a member of the authorised GitOps approver set.
///
/// # Errors
///
/// * [`PolicyError::NotGitopsSigner`] — `signer` is not authorised.
pub fn assert_gitops_signer(policy: &ExecutionPolicy, signer: &Address) -> Result<(), PolicyError> {
    if !policy.gitops_signers.contains(signer) {
        return Err(PolicyError::NotGitopsSigner);
    }
    Ok(())
}

/// Assert that `nonce` matches the policy's current GitOps nonce.
///
/// Binding approvals to the current nonce prevents replay of previously
/// emitted GitOps trigger events.
///
/// # Errors
///
/// * [`PolicyError::InvalidGitopsNonce`] — `nonce` != `policy.gitops_nonce`.
pub fn assert_gitops_nonce(policy: &ExecutionPolicy, nonce: u64) -> Result<(), PolicyError> {
    if nonce != policy.gitops_nonce {
        return Err(PolicyError::InvalidGitopsNonce);
    }
    Ok(())
}

/// Assert that an approval recorded at `approved_at_ledger` is still fresh.
///
/// An approval is stale when the current ledger is more than
/// `gitops_window_ledgers` ahead of the ledger at which it was recorded.
/// A zero window disables the staleness check.
///
/// # Errors
///
/// * [`PolicyError::StaleGitopsApproval`] — the approval has expired.
pub fn assert_gitops_fresh(
    policy: &ExecutionPolicy,
    approved_at_ledger: u32,
    current_ledger: u32,
) -> Result<(), PolicyError> {
    if policy.gitops_window_ledgers == 0 {
        return Ok(());
    }
    // Saturating subtraction: a future-dated approval is treated as fresh
    // (age 0) rather than wrapping around.
    let age = current_ledger.saturating_sub(approved_at_ledger);
    if age > policy.gitops_window_ledgers {
        return Err(PolicyError::StaleGitopsApproval);
    }
    Ok(())
}

/// Assert that `payload_hash` is non-empty.
///
/// The payload hash is expected to be an IPFS CID or Git commit SHA encoded
/// as bytes; an empty hash can never identify a deployment target.
///
/// # Errors
///
/// * [`PolicyError::EmptyGitopsPayload`] — `payload_hash` has zero length.
pub fn assert_gitops_payload_nonempty(payload_hash: &soroban_sdk::Bytes) -> Result<(), PolicyError> {
    if payload_hash.is_empty() {
        return Err(PolicyError::EmptyGitopsPayload);
    }
    Ok(())
}

/// Assert that the collected GitOps approvals meet the policy threshold.
///
/// # Errors
///
/// * [`PolicyError::ThresholdNotMet`] — fewer approvals than
///   `policy.gitops_threshold`.
pub fn assert_gitops_threshold_met(
    policy: &ExecutionPolicy,
    approval_count: u32,
) -> Result<(), PolicyError> {
    if approval_count < policy.gitops_threshold {
        return Err(PolicyError::ThresholdNotMet);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests for pure policy logic
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::Address as _;

    #[test]
    fn fee_guard_passes_at_threshold() {
        // Equal to max is still allowed — test via the deviation helper since
        // the fee guard no longer depends on env.ledger().
        let env = soroban_sdk::Env::default();
        let policy = ExecutionPolicy {
            max_allowed_fee_stroops: 100,
            twap_oracle: soroban_sdk::Address::generate(&env),
            reference_twap_price: 1_000_000,
            max_price_deviation_bps: 500,
            gitops_threshold: 1,
            gitops_signers: Vec::new(&env),
            gitops_nonce: 0,
            gitops_window_ledgers: 0,
        };
        // Fee equal to ceiling should pass.
        assert!(assert_fee_within_policy(100, &policy).is_ok());
        // Fee above ceiling should fail.
        assert_eq!(
            assert_fee_within_policy(101, &policy),
            Err(PolicyError::FeeTooHigh)
        );
    }

    #[test]
    fn deviation_bps_exact_5_percent() {
        // 1_050_000 vs 1_000_000 → 5% = 500 bps
        let bps = compute_deviation_bps(1_050_000, 1_000_000).unwrap();
        assert_eq!(bps, 500);
    }

    #[test]
    fn deviation_bps_zero_when_equal() {
        let bps = compute_deviation_bps(1_000_000, 1_000_000).unwrap();
        assert_eq!(bps, 0);
    }

    #[test]
    fn deviation_bps_below_reference() {
        // 950_000 vs 1_000_000 → 5% = 500 bps (abs)
        let bps = compute_deviation_bps(950_000, 1_000_000).unwrap();
        assert_eq!(bps, 500);
    }

    #[test]
    fn deviation_bps_zero_reference_errors() {
        let result = compute_deviation_bps(1_000, 0);
        assert_eq!(result, Err(PolicyError::NotInitialized));
    }

    #[test]
    fn twap_guard_disabled_when_max() {
        let env = soroban_sdk::Env::default();
        let policy = ExecutionPolicy {
            max_allowed_fee_stroops: 100,
            twap_oracle: soroban_sdk::Address::generate(&env),
            reference_twap_price: 1_000_000,
            max_price_deviation_bps: u32::MAX,
            gitops_threshold: 1,
            gitops_signers: Vec::new(&env),
            gitops_nonce: 0,
            gitops_window_ledgers: 0,
        };
        // Any live price should pass when guard is disabled.
        assert!(assert_twap_within_policy(&policy, 9_999_999_999).is_ok());
    }

    #[test]
    fn twap_guard_blocks_large_deviation() {
        let env = soroban_sdk::Env::default();
        let policy = ExecutionPolicy {
            max_allowed_fee_stroops: 100,
            twap_oracle: soroban_sdk::Address::generate(&env),
            reference_twap_price: 1_000_000,
            max_price_deviation_bps: 500, // 5%
            gitops_threshold: 1,
            gitops_signers: Vec::new(&env),
            gitops_nonce: 0,
            gitops_window_ledgers: 0,
        };
        // 1_100_000 → 10% deviation → blocked
        let result = assert_twap_within_policy(&policy, 1_100_000);
        assert_eq!(result, Err(PolicyError::TwapDeviationTooLarge));
    }

    // ---- GitOps policy helpers ----

    fn gitops_policy(env: &soroban_sdk::Env, threshold: u32, signers: Vec<Address>) -> ExecutionPolicy {
        ExecutionPolicy {
            max_allowed_fee_stroops: 100,
            twap_oracle: soroban_sdk::Address::generate(env),
            reference_twap_price: 1_000_000,
            max_price_deviation_bps: 500,
            gitops_threshold: threshold,
            gitops_signers: signers,
            gitops_nonce: 7,
            gitops_window_ledgers: 100,
        }
    }

    #[test]
    fn gitops_config_rejects_empty_signers() {
        let env = soroban_sdk::Env::default();
        let policy = gitops_policy(&env, 1, Vec::new(&env));
        assert_eq!(
            assert_gitops_config_valid(&policy),
            Err(PolicyError::EmptyGitopsSignerSet)
        );
    }

    #[test]
    fn gitops_config_rejects_bad_threshold() {
        let env = soroban_sdk::Env::default();
        let mut signers = Vec::new(&env);
        signers.push_back(soroban_sdk::Address::generate(&env));
        let policy = gitops_policy(&env, 2, signers);
        assert_eq!(
            assert_gitops_config_valid(&policy),
            Err(PolicyError::InvalidGitopsThreshold)
        );
    }

    #[test]
    fn gitops_config_accepts_valid() {
        let env = soroban_sdk::Env::default();
        let mut signers = Vec::new(&env);
        signers.push_back(soroban_sdk::Address::generate(&env));
        signers.push_back(soroban_sdk::Address::generate(&env));
        let policy = gitops_policy(&env, 2, signers);
        assert!(assert_gitops_config_valid(&policy).is_ok());
    }

    #[test]
    fn gitops_signer_membership() {
        let env = soroban_sdk::Env::default();
        let member = soroban_sdk::Address::generate(&env);
        let outsider = soroban_sdk::Address::generate(&env);
        let mut signers = Vec::new(&env);
        signers.push_back(member.clone());
        let policy = gitops_policy(&env, 1, signers);
        assert!(assert_gitops_signer(&policy, &member).is_ok());
        assert_eq!(
            assert_gitops_signer(&policy, &outsider),
            Err(PolicyError::NotGitopsSigner)
        );
    }

    #[test]
    fn gitops_nonce_must_match() {
        let env = soroban_sdk::Env::default();
        let policy = gitops_policy(&env, 1, Vec::new(&env));
        assert!(assert_gitops_nonce(&policy, 7).is_ok());
        assert_eq!(
            assert_gitops_nonce(&policy, 6),
            Err(PolicyError::InvalidGitopsNonce)
        );
    }

    #[test]
    fn gitops_freshness_window() {
        let env = soroban_sdk::Env::default();
        let policy = gitops_policy(&env, 1, Vec::new(&env));
        // Within window.
        assert!(assert_gitops_fresh(&policy, 1_000, 1_050).is_ok());
        // Exactly at window boundary is still fresh.
        assert!(assert_gitops_fresh(&policy, 1_000, 1_100).is_ok());
        // Beyond window is stale.
        assert_eq!(
            assert_gitops_fresh(&policy, 1_000, 1_101),
            Err(PolicyError::StaleGitopsApproval)
        );
    }

    #[test]
    fn gitops_payload_must_be_nonempty() {
        let env = soroban_sdk::Env::default();
        let empty = soroban_sdk::Bytes::new(&env);
        assert_eq!(
            assert_gitops_payload_nonempty(&empty),
            Err(PolicyError::EmptyGitopsPayload)
        );
        let mut data = soroban_sdk::Bytes::new(&env);
        data.push_back(0xab);
        assert!(assert_gitops_payload_nonempty(&data).is_ok());
    }

    #[test]
    fn gitops_threshold_met() {
        let env = soroban_sdk::Env::default();
        let policy = gitops_policy(&env, 2, Vec::new(&env));
        assert!(assert_gitops_threshold_met(&policy, 2).is_ok());
        assert_eq!(
            assert_gitops_threshold_met(&policy, 1),
            Err(PolicyError::ThresholdNotMet)
        );
    }
}
