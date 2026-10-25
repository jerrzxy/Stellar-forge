//! # Reserve Ratio Module
//!
//! Dynamically calculates and enforces a minimum backing ratio between the
//! treasury's reserve assets and the circulating native token supply.
//!
//! ## Backing Ratio
//!
//! ```text
//! backing_ratio_bps = reserve_value_in_usd * BPS_DENOM / native_supply_value_in_usd
//! ```
//!
//! Where values are expressed in a common denomination (e.g. micro-USD,
//! or any consistent oracle-reported unit).  The contract stores:
//!
//! * `RESERVE_VAL` — current reserve valuation (sum of all bonded reserve
//!   assets, converted to the reference unit by the oracle price feed
//!   provided at bond time).
//! * `SUPPLY_VAL`  — current circulating supply value (total native tokens
//!   issued × oracle price per native token, same unit).
//!
//! If `backing_ratio_bps < MIN_RATIO_BPS` the treasury is **under-backed**
//! and the contract rejects new bonds that would worsen the ratio.
//!
//! ## Storage keys (≤ 9 chars)
//!
//! | Key         | Type  | Description                                       |
//! |-------------|-------|---------------------------------------------------|
//! | `MIN_RATIO` | u32   | Minimum backing ratio in BPS (e.g. 5 000 = 50 %) |
//! | `RES_VAL`   | i128  | Cumulative reserve valuation, reference units     |
//! | `SUP_VAL`   | i128  | Cumulative supply valuation, reference units      |
//!
//! ## No floats
//!
//! All calculations use `i128` integer arithmetic.  Ratios are in BPS
//! (1/10 000), so `10_000 BPS = 100 %` backing.

use soroban_sdk::{contracterror, symbol_short, Env, Symbol};

// ---------------------------------------------------------------------------
// Storage keys
// ---------------------------------------------------------------------------

/// Minimum required backing ratio in BPS (default 5 000 = 50 %).
pub const KEY_MIN_RATIO: Symbol = symbol_short!("MIN_RATIO");
/// Cumulative reserve valuation (sum of reserve asset values, same unit).
pub const KEY_RES_VAL: Symbol = symbol_short!("RES_VAL");
/// Cumulative supply valuation (circulating supply × native token price).
pub const KEY_SUP_VAL: Symbol = symbol_short!("SUP_VAL");

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Basis-points denominator (10 000 = 100 %).
pub const BPS_DENOM: i128 = 10_000;
/// Default minimum backing ratio if not explicitly set: 50 %.
pub const DEFAULT_MIN_RATIO_BPS: u32 = 5_000;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Reserve-module specific errors (values 200–299).
#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReserveError {
    /// The bond would push the backing ratio below the minimum.
    UnderBacked = 200,
    /// The price parameters provided are invalid (zero or negative).
    InvalidPrice = 201,
    /// The minimum ratio must be > 0 and ≤ 10 000 BPS.
    InvalidMinRatio = 202,
}

// ---------------------------------------------------------------------------
// Initialisation
// ---------------------------------------------------------------------------

/// Initialise reserve-ratio parameters.  Must be called exactly once.
///
/// # Parameters
/// - `min_ratio_bps` — minimum backing ratio in BPS (1–10 000).
pub(crate) fn initialize(env: &Env, min_ratio_bps: u32) {
    assert!(!env.storage().instance().has(&KEY_MIN_RATIO), "reserve: already initialized");
    assert!(
        min_ratio_bps > 0 && min_ratio_bps <= 10_000,
        "reserve: invalid min ratio"
    );

    env.storage().instance().set(&KEY_MIN_RATIO, &min_ratio_bps);
    env.storage().instance().set(&KEY_RES_VAL, &0i128);
    env.storage().instance().set(&KEY_SUP_VAL, &0i128);
}

// ---------------------------------------------------------------------------
// Ratio calculation
// ---------------------------------------------------------------------------

/// Compute the current backing ratio in basis-points.
///
/// ```text
/// backing_ratio_bps = reserve_val * BPS_DENOM / supply_val
/// ```
///
/// Returns `u32::MAX` (∞) when `supply_val == 0` — treasury has issued no
/// tokens yet, backing is considered perfect.
pub fn compute_ratio_bps(reserve_val: i128, supply_val: i128) -> u32 {
    if supply_val <= 0 {
        return u32::MAX;
    }
    if reserve_val <= 0 {
        return 0;
    }
    let ratio = reserve_val * BPS_DENOM / supply_val;
    if ratio > u32::MAX as i128 {
        u32::MAX
    } else {
        ratio as u32
    }
}

// ---------------------------------------------------------------------------
// Pre-bond check
// ---------------------------------------------------------------------------

/// Validate that accepting a bond will not push the ratio below the minimum.
///
/// Adds the incoming reserve and payout valuation hypothetically and checks
/// the resulting ratio.
///
/// # Parameters
/// - `reserve_in_val` — value of the incoming reserve asset (reference units).
/// - `native_payout_val` — value of native tokens to be issued (same unit).
///
/// Returns `Ok(new_ratio_bps)` or panics with a descriptive message on
/// failure (Soroban contract panics terminate with an error).
pub(crate) fn check_and_project_ratio(
    env: &Env,
    reserve_in_val: i128,
    native_payout_val: i128,
) -> u32 {
    let min_ratio: u32 = env
        .storage()
        .instance()
        .get(&KEY_MIN_RATIO)
        .unwrap_or(DEFAULT_MIN_RATIO_BPS);
    let res_val: i128 = env
        .storage()
        .instance()
        .get(&KEY_RES_VAL)
        .unwrap_or(0i128);
    let sup_val: i128 = env
        .storage()
        .instance()
        .get(&KEY_SUP_VAL)
        .unwrap_or(0i128);

    let new_res_val = res_val + reserve_in_val;
    let new_sup_val = sup_val + native_payout_val;

    let new_ratio = compute_ratio_bps(new_res_val, new_sup_val);

    assert!(
        new_ratio >= min_ratio,
        "reserve: bond would push backing ratio below minimum"
    );

    new_ratio
}

// ---------------------------------------------------------------------------
// State update
// ---------------------------------------------------------------------------

/// Record a completed bond's contribution to reserve and supply valuations.
pub(crate) fn record_bond_valuation(env: &Env, reserve_in_val: i128, native_payout_val: i128) {
    let res_val: i128 = env
        .storage()
        .instance()
        .get(&KEY_RES_VAL)
        .unwrap_or(0i128);
    let sup_val: i128 = env
        .storage()
        .instance()
        .get(&KEY_SUP_VAL)
        .unwrap_or(0i128);

    env.storage()
        .instance()
        .set(&KEY_RES_VAL, &(res_val + reserve_in_val));
    env.storage()
        .instance()
        .set(&KEY_SUP_VAL, &(sup_val + native_payout_val));
}

// ---------------------------------------------------------------------------
// Query helpers
// ---------------------------------------------------------------------------

/// Return `(reserve_val, supply_val, current_ratio_bps)`.
pub(crate) fn get_reserve_state(env: &Env) -> (i128, i128, u32) {
    let res_val: i128 = env
        .storage()
        .instance()
        .get(&KEY_RES_VAL)
        .unwrap_or(0i128);
    let sup_val: i128 = env
        .storage()
        .instance()
        .get(&KEY_SUP_VAL)
        .unwrap_or(0i128);

    let ratio = compute_ratio_bps(res_val, sup_val);
    (res_val, sup_val, ratio)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Property: ratio = BPS_DENOM (100 %) when reserve = supply value
    // -----------------------------------------------------------------------
    #[test]
    fn ratio_100_pct_when_equal() {
        let ratio = compute_ratio_bps(10_000, 10_000);
        assert_eq!(ratio as i128, BPS_DENOM);
    }

    // -----------------------------------------------------------------------
    // Property: ratio is MAX when supply = 0 (no tokens issued)
    // -----------------------------------------------------------------------
    #[test]
    fn ratio_infinite_when_no_supply() {
        assert_eq!(compute_ratio_bps(0, 0), u32::MAX);
        assert_eq!(compute_ratio_bps(1_000_000, 0), u32::MAX);
    }

    // -----------------------------------------------------------------------
    // Property: ratio = 0 when reserve = 0 and supply > 0
    // -----------------------------------------------------------------------
    #[test]
    fn ratio_zero_when_no_reserve() {
        assert_eq!(compute_ratio_bps(0, 1_000_000), 0);
    }

    // -----------------------------------------------------------------------
    // Property: ratio is proportional (50% backing)
    // -----------------------------------------------------------------------
    #[test]
    fn ratio_50_pct() {
        // reserve_val = 5_000, supply_val = 10_000  → ratio = 5_000 BPS = 50%
        let ratio = compute_ratio_bps(5_000, 10_000);
        assert_eq!(ratio, 5_000);
    }

    // -----------------------------------------------------------------------
    // Property: 200 % over-collateralised
    // -----------------------------------------------------------------------
    #[test]
    fn ratio_200_pct() {
        let ratio = compute_ratio_bps(20_000, 10_000);
        assert_eq!(ratio, 20_000);
    }

    // -----------------------------------------------------------------------
    // Property: massive reserve inflow keeps ratio ≥ min throughout
    // -----------------------------------------------------------------------
    #[test]
    fn massive_inflow_keeps_ratio_above_minimum() {
        let min_ratio: u32 = 5_000; // 50 %
        let bond_reserve_val = 1_000_000i128;
        let bond_supply_val = 1_500_000i128; // 66 % backing per bond

        let mut cum_res = 0i128;
        let mut cum_sup = 0i128;

        for _ in 0..1_000 {
            cum_res += bond_reserve_val;
            cum_sup += bond_supply_val;

            let ratio = compute_ratio_bps(cum_res, cum_sup);
            assert!(
                ratio >= min_ratio,
                "ratio {} dropped below min {} after accumulating reserves",
                ratio,
                min_ratio
            );
        }
    }

    // -----------------------------------------------------------------------
    // Property: ratio decreases when supply grows faster than reserve
    // -----------------------------------------------------------------------
    #[test]
    fn ratio_decreases_on_excess_supply() {
        let initial_ratio = compute_ratio_bps(10_000, 10_000);

        // Add more supply than reserve
        let new_ratio = compute_ratio_bps(10_000, 20_000);

        assert!(
            new_ratio < initial_ratio,
            "ratio should decrease when supply grows faster"
        );
        assert_eq!(new_ratio, 5_000); // 50 %
    }
}
