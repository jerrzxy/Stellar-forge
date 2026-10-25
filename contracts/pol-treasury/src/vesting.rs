//! # Time-Locked Vesting Module
//!
//! Implements a per-bonder linear vesting schedule for native token payouts.
//!
//! ## Motivation
//!
//! Immediate token issuance on bonding would allow arbitrageurs to dump
//! freshly minted tokens in the same transaction, undermining the treasury's
//! liquidity goals.  By locking payouts behind a time-based vesting cliff /
//! schedule, token supply enters the market gradually.
//!
//! ## Vesting model
//!
//! Each bonder receives a [`VestingSchedule`] keyed by their `Address`:
//!
//! ```text
//! Vesting periods: |----cliff----|-------linear release-------|
//!                  0          cliff_ledger              end_ledger
//! ```
//!
//! * **Cliff** (`cliff_ledgers`): no tokens released before `start + cliff`.
//! * **Linear release**: after the cliff, tokens vest proportionally per
//!   ledger until `start + total_duration_ledgers`.
//! * **Claim**: the bonder calls `claim` at any point after the cliff; only
//!   the unvested remainder is transferred.
//!
//! All arithmetic is integer-only (no floats).
//!
//! ## Storage layout
//!
//! Vesting schedules are stored under persistent storage using a key composed
//! of the `VESTING_` prefix and the serialised `Address`.  This keeps each
//! schedule independent and avoids large vectors.

use soroban_sdk::{contracttype, symbol_short, Address, Env, Symbol};

// ---------------------------------------------------------------------------
// Storage-key prefix (≤ 8 bytes for symbol_short! compat)
// ---------------------------------------------------------------------------

/// Prefix for per-bonder vesting schedule keys in persistent storage.
pub const KEY_VEST_PFX: Symbol = symbol_short!("VEST_");
/// Global: total tokens currently locked in all active schedules.
pub const KEY_VEST_LOCK: Symbol = symbol_short!("VESTLOCK");

// ---------------------------------------------------------------------------
// Vesting schedule record
// ---------------------------------------------------------------------------

/// Immutable vesting schedule created at bond time.
///
/// Once created the `total_amount`, `start_ledger`, `cliff_ledgers`, and
/// `duration_ledgers` are fixed.  Only `claimed_amount` grows.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VestingSchedule {
    /// Beneficiary address — matches the bonder.
    pub beneficiary: Address,
    /// Total native tokens allocated (including cliff tranche), in stroops.
    pub total_amount: i128,
    /// Amount already claimed, in stroops.
    pub claimed_amount: i128,
    /// Ledger sequence number at schedule creation.
    pub start_ledger: u32,
    /// Ledgers after `start_ledger` before any tokens vest (cliff).
    pub cliff_ledgers: u32,
    /// Total vesting duration in ledgers (cliff + linear).
    pub duration_ledgers: u32,
}

// ---------------------------------------------------------------------------
// Initialisation / creation
// ---------------------------------------------------------------------------

/// Create (or accumulate onto) a vesting schedule for `beneficiary`.
///
/// If the beneficiary already has a schedule, the new amount is added to the
/// existing `total_amount` but the schedule timing remains unchanged
/// (subsequent bonds extend the amount, not the duration).
///
/// # Parameters
/// - `beneficiary` — address that will receive vested tokens.
/// - `amount` — native tokens to vest (must be > 0).
/// - `cliff_ledgers` — ledgers before any tokens can be claimed.
/// - `duration_ledgers` — full vesting window; must be > `cliff_ledgers`.
pub(crate) fn create_or_add_schedule(
    env: &Env,
    beneficiary: &Address,
    amount: i128,
    cliff_ledgers: u32,
    duration_ledgers: u32,
) {
    assert!(amount > 0, "vesting: amount must be positive");
    assert!(
        duration_ledgers > cliff_ledgers,
        "vesting: duration must exceed cliff"
    );

    let current_ledger = env.ledger().sequence();

    let vesting_key = vesting_storage_key(env, beneficiary);

    let schedule = if let Some(mut existing) =
        env.storage().persistent().get::<_, VestingSchedule>(&vesting_key)
    {
        // Accumulate on existing schedule (add to unclaimed total)
        existing.total_amount += amount;
        existing
    } else {
        VestingSchedule {
            beneficiary: beneficiary.clone(),
            total_amount: amount,
            claimed_amount: 0,
            start_ledger: current_ledger,
            cliff_ledgers,
            duration_ledgers,
        }
    };

    env.storage().persistent().set(&vesting_key, &schedule);

    // Track global locked amount
    let locked: i128 = env
        .storage()
        .instance()
        .get(&KEY_VEST_LOCK)
        .unwrap_or(0i128);
    env.storage()
        .instance()
        .set(&KEY_VEST_LOCK, &(locked + amount));
}

// ---------------------------------------------------------------------------
// Claimable amount calculation
// ---------------------------------------------------------------------------

/// Compute the currently claimable (unvested) amount for a given schedule
/// at `current_ledger`.
///
/// Formula:
/// ```text
/// elapsed = current_ledger - start_ledger
///
/// if elapsed < cliff_ledgers:
///     vested = 0
/// elif elapsed >= duration_ledgers:
///     vested = total_amount
/// else:
///     linear_elapsed = elapsed - cliff_ledgers
///     linear_duration = duration_ledgers - cliff_ledgers
///     cliff_tranche = total_amount * cliff_ledgers / duration_ledgers
///     linear_tranche = total_amount - cliff_tranche
///     vested = cliff_tranche
///              + linear_tranche * linear_elapsed / linear_duration
///
/// claimable = vested - claimed_amount
/// ```
pub fn compute_claimable(schedule: &VestingSchedule, current_ledger: u32) -> i128 {
    let elapsed = current_ledger.saturating_sub(schedule.start_ledger);

    let vested = if elapsed < schedule.cliff_ledgers {
        // Before cliff: nothing vested
        0i128
    } else if elapsed >= schedule.duration_ledgers {
        // Fully vested
        schedule.total_amount
    } else {
        let linear_elapsed = elapsed - schedule.cliff_ledgers;
        let linear_duration = schedule.duration_ledgers - schedule.cliff_ledgers;

        // Cliff tranche: proportional to cliff period over total duration
        let cliff_tranche =
            schedule.total_amount * schedule.cliff_ledgers as i128 / schedule.duration_ledgers as i128;
        let linear_tranche = schedule.total_amount - cliff_tranche;

        // Linear portion released so far
        let linear_released =
            linear_tranche * linear_elapsed as i128 / linear_duration as i128;

        cliff_tranche + linear_released
    };

    let claimable = vested - schedule.claimed_amount;
    if claimable < 0 {
        0
    } else {
        claimable
    }
}

// ---------------------------------------------------------------------------
// Claim execution
// ---------------------------------------------------------------------------

/// Execute a claim for `beneficiary`.
///
/// Returns the amount to transfer (may be zero if nothing is claimable yet).
/// Updates the schedule's `claimed_amount` in storage.
pub(crate) fn execute_claim(env: &Env, beneficiary: &Address) -> i128 {
    let vesting_key = vesting_storage_key(env, beneficiary);

    let mut schedule: VestingSchedule = env
        .storage()
        .persistent()
        .get(&vesting_key)
        .expect("vesting: no schedule for beneficiary");

    let current_ledger = env.ledger().sequence();
    let claimable = compute_claimable(&schedule, current_ledger);

    if claimable == 0 {
        return 0;
    }

    schedule.claimed_amount += claimable;
    env.storage().persistent().set(&vesting_key, &schedule);

    // Reduce global locked amount
    let locked: i128 = env
        .storage()
        .instance()
        .get(&KEY_VEST_LOCK)
        .unwrap_or(0i128);
    let new_locked = if locked >= claimable {
        locked - claimable
    } else {
        0
    };
    env.storage().instance().set(&KEY_VEST_LOCK, &new_locked);

    claimable
}

// ---------------------------------------------------------------------------
// Query helpers
// ---------------------------------------------------------------------------

/// Return the [`VestingSchedule`] for `beneficiary`, or `None`.
pub(crate) fn get_schedule(env: &Env, beneficiary: &Address) -> Option<VestingSchedule> {
    let vesting_key = vesting_storage_key(env, beneficiary);
    env.storage().persistent().get(&vesting_key)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Build the persistent storage key for a beneficiary's vesting schedule.
///
/// We compose `(KEY_VEST_PFX, beneficiary)` as the key tuple so each
/// schedule occupies a unique storage slot.
fn vesting_storage_key(env: &Env, beneficiary: &Address) -> (Symbol, Address) {
    (KEY_VEST_PFX, beneficiary.clone())
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Env};

    /// Construct a minimal schedule for testing (no persistent env required).
    fn make_schedule(
        total: i128,
        claimed: i128,
        start: u32,
        cliff: u32,
        duration: u32,
    ) -> VestingSchedule {
        let env = Env::default();
        VestingSchedule {
            beneficiary: soroban_sdk::Address::generate(&env),
            total_amount: total,
            claimed_amount: claimed,
            start_ledger: start,
            cliff_ledgers: cliff,
            duration_ledgers: duration,
        }
    }

    // -----------------------------------------------------------------------
    // Property: before cliff, claimable = 0
    // -----------------------------------------------------------------------
    #[test]
    fn claimable_zero_before_cliff() {
        let s = make_schedule(1_000_000, 0, 100, 200, 1_000);
        // Current ledger = 150 (before cliff at 100+200=300)
        assert_eq!(compute_claimable(&s, 150), 0);
    }

    // -----------------------------------------------------------------------
    // Property: after full duration, claimable = total_amount - claimed
    // -----------------------------------------------------------------------
    #[test]
    fn claimable_full_after_duration() {
        let s = make_schedule(1_000_000, 0, 100, 200, 1_000);
        // At start + duration = 1100, fully vested
        assert_eq!(compute_claimable(&s, 1100), 1_000_000);
    }

    // -----------------------------------------------------------------------
    // Property: claimable never decreases as ledger increases (monotone)
    // -----------------------------------------------------------------------
    #[test]
    fn claimable_monotone_over_time() {
        let s = make_schedule(1_000_000, 0, 0, 100, 500);
        let mut prev_claimable = 0i128;
        for ledger in 0u32..=600 {
            let c = compute_claimable(&s, ledger);
            assert!(
                c >= prev_claimable,
                "claimable decreased at ledger {}: {} -> {}",
                ledger,
                prev_claimable,
                c
            );
            prev_claimable = c;
        }
    }

    // -----------------------------------------------------------------------
    // Property: at cliff boundary, exactly cliff_tranche is claimable
    // -----------------------------------------------------------------------
    #[test]
    fn claimable_at_cliff_boundary() {
        let total = 1_000_000i128;
        let cliff = 200u32;
        let duration = 1_000u32;
        let s = make_schedule(total, 0, 0, cliff, duration);

        // At the cliff ledger, cliff_tranche should be released
        let cliff_tranche = total * cliff as i128 / duration as i128;
        let claimable_at_cliff = compute_claimable(&s, cliff);

        assert_eq!(
            claimable_at_cliff, cliff_tranche,
            "expected cliff_tranche={} at ledger={}, got {}",
            cliff_tranche, cliff, claimable_at_cliff
        );
    }

    // -----------------------------------------------------------------------
    // Property: total claimable sums to exactly total_amount over full duration
    // -----------------------------------------------------------------------
    #[test]
    fn total_claimable_equals_total_amount() {
        let s = make_schedule(1_000_000, 0, 0, 50, 200);
        let final_claimable = compute_claimable(&s, 200);
        assert_eq!(final_claimable, 1_000_000);
    }

    // -----------------------------------------------------------------------
    // Property: already-claimed amount is subtracted from claimable
    // -----------------------------------------------------------------------
    #[test]
    fn claimed_amount_reduces_claimable() {
        let s = make_schedule(1_000_000, 500_000, 0, 0, 200);
        let c = compute_claimable(&s, 200);
        assert_eq!(c, 500_000);
    }
}
