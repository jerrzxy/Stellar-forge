//! # Bonding Curve Module
//!
//! Implements the dynamic discount bonding curve for the POL Treasury Manager.
//!
//! ## Design
//!
//! Users sell reserve assets (e.g. XLM, USDC) to the protocol treasury in
//! exchange for native DAO tokens.  To incentivise bonding over open-market
//! purchases, the protocol issues native tokens at a discount relative to the
//! current market price.  This discount shrinks deterministically as the
//! treasury accumulates more reserves — preventing unlimited dilution.
//!
//! ### Discount formula (no floating-point)
//!
//! ```text
//! discount_bps = MAX_DISCOUNT_BPS
//!                - (reserve_balance * MAX_DISCOUNT_BPS / MAX_RESERVE_TARGET)
//! ```
//!
//! All arithmetic is in basis-points (BPS, 1/10 000).  At zero reserves the
//! discount is `MAX_DISCOUNT_BPS` (e.g. 3 000 = 30 %).  As reserves grow to
//! `MAX_RESERVE_TARGET` the discount linearly decays to zero.  Any reserve
//! beyond the target yields a discount of exactly zero (no premium charge).
//!
//! ### Token payout
//!
//! ```text
//! payout = reserve_in * (10_000 + discount_bps) / 10_000
//!          * price_inverse_bps / 10_000
//! ```
//!
//! In practice the caller passes `price_inverse_bps` representing
//! `10_000 * native_token_price / reserve_price` so the entire calculation
//! stays in integer arithmetic.
//!
//! ## Constraints
//!
//! * No floating-point — all math uses `i128` fixed-point integers.
//! * Overflow safety: Soroban `profile.release` sets `overflow-checks = true`;
//!   additionally each multiplication is bounded by parameter ranges.
//! * All public functions are `pub(crate)` to keep the surface area minimal.

use soroban_sdk::{contracterror, contracttype, symbol_short, Env, Symbol};

// ---------------------------------------------------------------------------
// Storage-key constants (≤ 9 bytes each)
// ---------------------------------------------------------------------------

/// Maximum discount in basis-points (3 000 = 30 %).
pub const KEY_MAX_DISC: Symbol = symbol_short!("MAX_DISC");
/// Maximum reserve target (denominator of the decay formula), i128 stroops.
pub const KEY_MAX_RES: Symbol = symbol_short!("MAX_RES");
/// Cumulative reserve balance held by the treasury, i128 stroops.
pub const KEY_RES_BAL: Symbol = symbol_short!("RES_BAL");
/// Total native tokens ever issued via bonding (excludes vested but unpaid), i128.
pub const KEY_TOT_BOND: Symbol = symbol_short!("TOT_BOND");

// ---------------------------------------------------------------------------
// Hard limits
// ---------------------------------------------------------------------------

/// Maximum allowed discount: 50 % (5 000 BPS).
pub const MAX_ALLOWED_DISCOUNT_BPS: u32 = 5_000;
/// Basis-points denominator.
pub const BPS_DENOM: i128 = 10_000;

// ---------------------------------------------------------------------------
// Error variants contributed by this module
// ---------------------------------------------------------------------------

/// Bond-module specific errors (values starting at 100 to avoid collision).
#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BondError {
    /// The bond module has not been initialised yet.
    NotInitialized = 100,
    /// The bond module has already been initialised.
    AlreadyInitialized = 101,
    /// The `max_discount_bps` parameter exceeds the hard cap of 5 000.
    DiscountTooHigh = 102,
    /// The `max_reserve_target` must be strictly positive.
    InvalidReserveTarget = 103,
    /// The `reserve_in` must be strictly positive.
    ZeroReserveIn = 104,
    /// Computed payout overflows i128 — inputs are unreasonably large.
    PayoutOverflow = 105,
    /// The `price_inverse_bps` must be strictly positive.
    InvalidPrice = 106,
}

// ---------------------------------------------------------------------------
// Configuration snapshot (returned for off-chain inspection)
// ---------------------------------------------------------------------------

/// Read-only snapshot of current bonding-curve parameters and state.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BondState {
    /// Maximum discount at zero reserves, in basis-points.
    pub max_discount_bps: u32,
    /// Reserve level at which the discount decays to zero, in stroops.
    pub max_reserve_target: i128,
    /// Current reserve balance held in the treasury, in stroops.
    pub reserve_balance: i128,
    /// Total native tokens ever issued via bonding, in stroops.
    pub total_bonded: i128,
    /// Current effective discount in basis-points.
    pub current_discount_bps: u32,
}

// ---------------------------------------------------------------------------
// Initialisation
// ---------------------------------------------------------------------------

/// Initialise bond parameters.  Must be called exactly once before any bond.
///
/// # Parameters
/// - `max_discount_bps` — maximum discount at zero reserves (≤ 5 000 BPS).
/// - `max_reserve_target` — reserve level at which discount decays to zero.
pub(crate) fn initialize(env: &Env, max_discount_bps: u32, max_reserve_target: i128) {
    assert!(
        !env.storage().instance().has(&KEY_MAX_DISC),
        "bond: already initialized"
    );
    assert!(
        max_discount_bps <= MAX_ALLOWED_DISCOUNT_BPS,
        "bond: discount too high"
    );
    assert!(max_reserve_target > 0, "bond: invalid reserve target");

    env.storage()
        .instance()
        .set(&KEY_MAX_DISC, &max_discount_bps);
    env.storage()
        .instance()
        .set(&KEY_MAX_RES, &max_reserve_target);
    env.storage().instance().set(&KEY_RES_BAL, &0i128);
    env.storage().instance().set(&KEY_TOT_BOND, &0i128);
}

// ---------------------------------------------------------------------------
// Discount calculation
// ---------------------------------------------------------------------------

/// Compute the current discount in basis-points given `reserve_balance`.
///
/// Formula (linear decay, no floating-point):
/// ```text
/// discount_bps = max_discount_bps
///                - (reserve_balance * max_discount_bps / max_reserve_target)
/// ```
/// Saturates at zero — discount never goes negative.
///
/// # Arguments
/// - `reserve_balance` — current reserve held, in stroops (i128).
/// - `max_discount_bps` — maximum discount in BPS.
/// - `max_reserve_target` — denominator / decay limit.
pub fn compute_discount_bps(
    reserve_balance: i128,
    max_discount_bps: u32,
    max_reserve_target: i128,
) -> u32 {
    if reserve_balance <= 0 {
        return max_discount_bps;
    }
    if reserve_balance >= max_reserve_target {
        return 0;
    }

    // decay = max_discount_bps * reserve_balance / max_reserve_target
    // All values fit in i128: max_discount_bps ≤ 5_000, reserve_balance ≤ max_reserve_target.
    let decay =
        (max_discount_bps as i128 * reserve_balance) / max_reserve_target;

    // Saturate at zero
    let discount = max_discount_bps as i128 - decay;
    if discount <= 0 {
        0
    } else {
        discount as u32
    }
}

// ---------------------------------------------------------------------------
// Payout calculation
// ---------------------------------------------------------------------------

/// Compute the native-token payout for a given `reserve_in` amount.
///
/// Formula:
/// ```text
/// payout = reserve_in * (BPS_DENOM + discount_bps) * price_inverse_bps
///          / (BPS_DENOM * BPS_DENOM)
/// ```
///
/// `price_inverse_bps` = `10_000 * (native_token_units_per_reserve_unit)`.
/// E.g. if 1 USDC buys 2 native tokens at par, `price_inverse_bps = 20_000`.
///
/// # Returns
/// `(payout_native_tokens, effective_discount_bps)`
///
/// # Panics
/// Panics in debug builds on overflow — inputs must be reasonably sized.
pub fn compute_payout(
    reserve_in: i128,
    reserve_balance: i128,
    max_discount_bps: u32,
    max_reserve_target: i128,
    price_inverse_bps: i128,
) -> (i128, u32) {
    assert!(reserve_in > 0, "bond: zero reserve_in");
    assert!(price_inverse_bps > 0, "bond: invalid price");

    let discount_bps = compute_discount_bps(reserve_balance, max_discount_bps, max_reserve_target);

    // payout = reserve_in * (10_000 + discount_bps) / 10_000
    //        * price_inverse_bps / 10_000
    //
    // To preserve precision: combine the two divisions into one:
    //   payout = reserve_in * (10_000 + discount_bps) * price_inverse_bps
    //            / (10_000 * 10_000)
    //
    // Maximum intermediate: if reserve_in = 10^18 (1 USDC in stroops? no, USDC has 7 decimals
    // in Soroban so max realistic is ~10^14), price_inverse_bps ≤ 10^9, factor ≤ 15_000:
    // 10^14 * 15_000 * 10^9 = 1.5 × 10^27  — fits in i128 (max ~1.7 × 10^38).
    let numerator = reserve_in
        .checked_mul(BPS_DENOM + discount_bps as i128)
        .expect("bond: overflow in payout numerator step 1")
        .checked_mul(price_inverse_bps)
        .expect("bond: overflow in payout numerator step 2");

    let denominator = BPS_DENOM
        .checked_mul(BPS_DENOM)
        .expect("bond: overflow in payout denominator");

    let payout = numerator / denominator;
    (payout, discount_bps)
}

// ---------------------------------------------------------------------------
// State mutation helpers (called from lib.rs)
// ---------------------------------------------------------------------------

/// Record a completed bond: update reserve balance and total bonded supply.
pub(crate) fn record_bond(env: &Env, reserve_in: i128, native_payout: i128) {
    let res_bal: i128 = env
        .storage()
        .instance()
        .get(&KEY_RES_BAL)
        .unwrap_or(0i128);
    let tot_bond: i128 = env
        .storage()
        .instance()
        .get(&KEY_TOT_BOND)
        .unwrap_or(0i128);

    env.storage()
        .instance()
        .set(&KEY_RES_BAL, &(res_bal + reserve_in));
    env.storage()
        .instance()
        .set(&KEY_TOT_BOND, &(tot_bond + native_payout));
}

/// Return the current [`BondState`] snapshot.
pub(crate) fn get_bond_state(env: &Env) -> BondState {
    let max_discount_bps: u32 = env
        .storage()
        .instance()
        .get(&KEY_MAX_DISC)
        .unwrap_or(0u32);
    let max_reserve_target: i128 = env
        .storage()
        .instance()
        .get(&KEY_MAX_RES)
        .unwrap_or(1i128);
    let reserve_balance: i128 = env
        .storage()
        .instance()
        .get(&KEY_RES_BAL)
        .unwrap_or(0i128);
    let total_bonded: i128 = env
        .storage()
        .instance()
        .get(&KEY_TOT_BOND)
        .unwrap_or(0i128);

    let current_discount_bps =
        compute_discount_bps(reserve_balance, max_discount_bps, max_reserve_target);

    BondState {
        max_discount_bps,
        max_reserve_target,
        reserve_balance,
        total_bonded,
        current_discount_bps,
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Property: discount decays monotonically as reserves grow
    // -----------------------------------------------------------------------
    #[test]
    fn discount_monotone_decay() {
        let max_disc = 3_000u32; // 30 %
        let max_res = 1_000_000i128;

        let mut prev = max_disc;
        // Step through 0 .. max_res in 100 steps
        for step in 0u32..=100 {
            let reserve = max_res * step as i128 / 100;
            let d = compute_discount_bps(reserve, max_disc, max_res);
            assert!(
                d <= prev,
                "discount increased from {} to {} at step {}",
                prev,
                d,
                step
            );
            prev = d;
        }
    }

    // -----------------------------------------------------------------------
    // Property: discount is MAX at zero reserves
    // -----------------------------------------------------------------------
    #[test]
    fn discount_max_at_zero_reserves() {
        assert_eq!(compute_discount_bps(0, 3_000, 1_000_000), 3_000);
        assert_eq!(compute_discount_bps(-1, 3_000, 1_000_000), 3_000);
    }

    // -----------------------------------------------------------------------
    // Property: discount is zero at or beyond target
    // -----------------------------------------------------------------------
    #[test]
    fn discount_zero_at_target() {
        assert_eq!(compute_discount_bps(1_000_000, 3_000, 1_000_000), 0);
        assert_eq!(compute_discount_bps(2_000_000, 3_000, 1_000_000), 0);
    }

    // -----------------------------------------------------------------------
    // Property: payout > reserve_in when discount > 0
    // -----------------------------------------------------------------------
    #[test]
    fn payout_exceeds_par_when_discounted() {
        // price_inverse_bps = 10_000 → 1:1 at par
        let (payout, disc) = compute_payout(10_000_000, 0, 3_000, 1_000_000_000, 10_000);
        assert!(disc > 0);
        // With a 30 % discount, payout should be 13 000 000 (= 10_000_000 * 13_000 / 10_000)
        assert_eq!(payout, 13_000_000);
    }

    // -----------------------------------------------------------------------
    // Property: payout equals par when reserves fill target (discount = 0)
    // -----------------------------------------------------------------------
    #[test]
    fn payout_equals_par_at_full_reserves() {
        // price_inverse_bps = 10_000 → 1:1 at par, discount should be 0
        let (payout, disc) =
            compute_payout(10_000_000, 1_000_000_000, 3_000, 1_000_000_000, 10_000);
        assert_eq!(disc, 0);
        assert_eq!(payout, 10_000_000);
    }

    // -----------------------------------------------------------------------
    // Property: massive reserve inflows → discount deterministically → 0
    // -----------------------------------------------------------------------
    #[test]
    fn massive_inflow_drops_discount_to_zero() {
        let max_disc = 5_000u32; // 50 %
        let max_res = 10_000_000_000i128; // 10 billion stroops
        let target = max_res;

        // Simulate 1 000 sequential bonds of 10 million stroops each
        let mut cumulative_reserve = 0i128;
        let bond_amount = 10_000_000i128; // 10 M stroops each

        let mut last_discount = max_disc;
        for _ in 0..1_000 {
            let d = compute_discount_bps(cumulative_reserve, max_disc, target);
            assert!(
                d <= last_discount,
                "discount must not increase on successive bonds"
            );
            last_discount = d;
            cumulative_reserve += bond_amount;
        }

        // After filling the target, discount must be zero
        let final_discount = compute_discount_bps(target, max_disc, target);
        assert_eq!(final_discount, 0, "discount must be 0 at target reserves");
    }

    // -----------------------------------------------------------------------
    // Property: compute_payout is proportional in reserve_in (linearity)
    // -----------------------------------------------------------------------
    #[test]
    fn payout_linear_in_reserve_in() {
        let reserve_balance = 0i128;
        let max_disc = 2_000u32;
        let max_res = 1_000_000_000i128;
        let price_inv = 10_000i128;

        let (p1, _) = compute_payout(1_000_000, reserve_balance, max_disc, max_res, price_inv);
        let (p2, _) = compute_payout(2_000_000, reserve_balance, max_disc, max_res, price_inv);

        assert_eq!(p2, p1 * 2, "payout must be linear in reserve_in");
    }
}
