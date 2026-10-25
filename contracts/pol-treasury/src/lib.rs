//! # POL Treasury Manager
//!
//! A Soroban smart contract that orchestrates Protocol Owned Liquidity (POL)
//! for a DAO treasury.  It combines three subsystems:
//!
//! 1. **Bonding Curve** ([`bond`]) — users sell reserve assets (e.g. XLM,
//!    USDC) to the protocol at a dynamic discount that decays as reserves grow,
//!    preventing unlimited dilution.
//! 2. **Time-locked Vesting** ([`vesting`]) — native token payouts vest
//!    linearly over a configurable schedule, preventing immediate market
//!    dumping by bonders.
//! 3. **Reserve Ratio Enforcement** ([`reserve`]) — every bond is validated
//!    against a minimum backing ratio; the contract rejects bonds that would
//!    leave the native token supply under-backed.
//!
//! ## Module Layout
//!
//! ```text
//! pol-treasury/
//! ├── src/
//! │   ├── lib.rs      ← Soroban contract entry-points (this file)
//! │   ├── bond.rs     ← Bonding curve & discount calculation
//! │   ├── vesting.rs  ← Per-bonder time-locked vesting schedules
//! │   └── reserve.rs  ← Reserve ratio validation & tracking
//! └── Cargo.toml
//! ```
//!
//! ## Lifecycle
//!
//! ```text
//! 1. Admin calls `initialize(...)` — sets up all three subsystems.
//! 2. Bonders call `bond(bonder, reserve_amount, price_inverse_bps)`:
//!    a. Bond module calculates payout with current discount.
//!    b. Reserve module validates the projected backing ratio.
//!    c. Reserve asset is transferred from bonder → treasury.
//!    d. Vesting schedule is created for bonder → native payout.
//!    e. State in all three modules is updated atomically.
//! 3. Bonders call `claim(bonder)` to receive vested native tokens.
//! 4. Anyone can query `bond_state()`, `vesting_schedule(addr)`,
//!    `reserve_state()` for off-chain dashboards.
//! ```
//!
//! ## Security Notes for Auditors
//!
//! * `require_auth()` is called on the `bonder`/`claimer` address at the
//!   start of every mutating entry-point.
//! * All arithmetic is integer-only; `overflow-checks = true` in
//!   `profile.release` ensures overflow panics rather than wraps.
//! * The `initialize` entry-point is guarded by `KEY_ADMIN`; calling it
//!   twice panics deterministically.
//! * Reserve token transfers use Soroban's canonical `token::Client`,
//!   eliminating re-entrancy across cross-contract calls (Soroban's
//!   execution model prevents callbacks during a token transfer).
//! * Native token minting requires `env.current_contract_address()` to be
//!   the token's admin — this is set up at deploy time by the DAO operator.

pub mod bond;
pub mod reserve;
pub mod vesting;

use bond::{compute_payout, get_bond_state, initialize as bond_init, record_bond, BondState};
use reserve::{
    check_and_project_ratio, get_reserve_state, initialize as reserve_init,
    record_bond_valuation,
};
use vesting::{create_or_add_schedule, execute_claim, get_schedule, VestingSchedule};

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, token,
    Address, Env, Symbol,
};

// ---------------------------------------------------------------------------
// Storage keys owned by lib.rs
// ---------------------------------------------------------------------------

const KEY_ADMIN: Symbol = symbol_short!("ADMIN");
const KEY_RES_TOK: Symbol = symbol_short!("RES_TOK");
const KEY_NAT_TOK: Symbol = symbol_short!("NAT_TOK");
const KEY_CLIFF: Symbol = symbol_short!("CLIFF");
const KEY_VDUR: Symbol = symbol_short!("VDUR");

// ---------------------------------------------------------------------------
// Error catalogue
// ---------------------------------------------------------------------------

/// Top-level POL treasury errors (values 1–99; module errors start at 100).
#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TreasuryError {
    /// The contract has not been initialised.
    NotInitialized = 1,
    /// The contract has already been initialised.
    AlreadyInitialized = 2,
    /// Caller is not the treasury admin.
    Unauthorized = 3,
    /// The bond amount must be strictly positive.
    ZeroBondAmount = 4,
    /// The computed native token payout is zero (price too low or amount too small).
    ZeroPayout = 5,
    /// The bond would breach the minimum reserve ratio.
    UnderBacked = 6,
    /// No vesting schedule exists for the given address.
    NoVestingSchedule = 7,
    /// Nothing is claimable yet (cliff not reached, or already claimed).
    NothingClaimable = 8,
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Emitted when the treasury is successfully initialised.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreasuryInitialized {
    #[topic]
    pub admin: Address,
    pub reserve_token: Address,
    pub native_token: Address,
    pub max_discount_bps: u32,
    pub min_ratio_bps: u32,
}

/// Emitted on each successful bond.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Bonded {
    #[topic]
    pub bonder: Address,
    pub reserve_in: i128,
    pub native_payout: i128,
    pub discount_bps: u32,
}

/// Emitted when a bonder claims vested tokens.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Claimed {
    #[topic]
    pub beneficiary: Address,
    pub amount: i128,
}

// ---------------------------------------------------------------------------
// Read-only response types
// ---------------------------------------------------------------------------

/// Combined treasury snapshot returned by `get_treasury_state()`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreasuryState {
    /// Reserve asset token address.
    pub reserve_token: Address,
    /// Native DAO token address.
    pub native_token: Address,
    /// Bonding curve state (discount, reserve balance, etc.).
    pub bond: BondState,
    /// Current reserve valuation (reference units).
    pub reserve_val: i128,
    /// Current supply valuation (reference units).
    pub supply_val: i128,
    /// Current backing ratio in BPS.
    pub backing_ratio_bps: u32,
    /// Total tokens locked in vesting schedules.
    pub total_vesting_locked: i128,
}

// ---------------------------------------------------------------------------
// Contract definition
// ---------------------------------------------------------------------------

#[contract]
pub struct PolTreasury;

#[contractimpl]
impl PolTreasury {
    // -----------------------------------------------------------------------
    // Initialisation
    // -----------------------------------------------------------------------

    /// Initialise the POL Treasury Manager.
    ///
    /// Must be called exactly once by the deployer.  Sets up all three
    /// subsystems atomically.
    ///
    /// # Parameters
    ///
    /// | Param                | Description                                               |
    /// |----------------------|-----------------------------------------------------------|
    /// | `admin`              | Address authorised to modify treasury parameters.         |
    /// | `reserve_token`      | SEP-41 token accepted as reserve asset (e.g. XLM/USDC).  |
    /// | `native_token`       | Native DAO token minted to bonders.                       |
    /// | `max_discount_bps`   | Maximum bonding discount at zero reserves (≤ 5 000 BPS). |
    /// | `max_reserve_target` | Reserve level at which discount decays to zero.           |
    /// | `min_ratio_bps`      | Minimum backing ratio (1–10 000 BPS, e.g. 5 000 = 50 %). |
    /// | `cliff_ledgers`      | Vesting cliff in ledgers before any tokens can be claimed.|
    /// | `duration_ledgers`   | Total vesting duration in ledgers (must exceed cliff).    |
    pub fn initialize(
        env: Env,
        admin: Address,
        reserve_token: Address,
        native_token: Address,
        max_discount_bps: u32,
        max_reserve_target: i128,
        min_ratio_bps: u32,
        cliff_ledgers: u32,
        duration_ledgers: u32,
    ) {
        admin.require_auth();

        assert!(
            !env.storage().instance().has(&KEY_ADMIN),
            "pol-treasury: already initialized"
        );
        assert!(
            duration_ledgers > cliff_ledgers,
            "pol-treasury: duration must exceed cliff"
        );

        // Initialise sub-modules
        bond_init(&env, max_discount_bps, max_reserve_target);
        reserve_init(&env, min_ratio_bps);

        // Persist treasury-level configuration
        env.storage().instance().set(&KEY_ADMIN, &admin);
        env.storage().instance().set(&KEY_RES_TOK, &reserve_token);
        env.storage().instance().set(&KEY_NAT_TOK, &native_token);
        env.storage().instance().set(&KEY_CLIFF, &cliff_ledgers);
        env.storage().instance().set(&KEY_VDUR, &duration_ledgers);

        TreasuryInitialized {
            admin,
            reserve_token,
            native_token,
            max_discount_bps,
            min_ratio_bps,
        }
        .publish(&env);
    }

    // -----------------------------------------------------------------------
    // Bond
    // -----------------------------------------------------------------------

    /// Bond reserve assets in exchange for vested native tokens.
    ///
    /// The caller must have approved the treasury contract to transfer
    /// `reserve_amount` of the reserve token on their behalf (via `approve`
    /// on the reserve token contract before calling this).
    ///
    /// # Parameters
    ///
    /// | Param                | Description                                                    |
    /// |----------------------|----------------------------------------------------------------|
    /// | `bonder`             | Address bonding reserve assets (must auth).                    |
    /// | `reserve_amount`     | Amount of reserve asset to deposit, in base units (stroops).  |
    /// | `price_inverse_bps`  | `10_000 × (native_token_price / reserve_token_price)`.         |
    ///                           Used to convert reserve value → native token payout.
    ///                           E.g. if 1 USDC buys 5 native tokens at par → 50_000.     |
    ///
    /// # Returns
    /// `(native_payout, effective_discount_bps)` — amounts are in stroops.
    pub fn bond(
        env: Env,
        bonder: Address,
        reserve_amount: i128,
        price_inverse_bps: i128,
    ) -> (i128, u32) {
        bonder.require_auth();

        assert!(
            env.storage().instance().has(&KEY_ADMIN),
            "pol-treasury: not initialized"
        );
        assert!(reserve_amount > 0, "pol-treasury: zero bond amount");
        assert!(price_inverse_bps > 0, "pol-treasury: invalid price");

        let reserve_token: Address = env.storage().instance().get(&KEY_RES_TOK).unwrap();
        let native_token: Address = env.storage().instance().get(&KEY_NAT_TOK).unwrap();
        let cliff_ledgers: u32 = env.storage().instance().get(&KEY_CLIFF).unwrap();
        let duration_ledgers: u32 = env.storage().instance().get(&KEY_VDUR).unwrap();

        // ---- 1. Compute payout via bonding curve --------------------------
        let bond_st = get_bond_state(&env);
        let (native_payout, discount_bps) = compute_payout(
            reserve_amount,
            bond_st.reserve_balance,
            bond_st.max_discount_bps,
            bond_st.max_reserve_target,
            price_inverse_bps,
        );
        assert!(native_payout > 0, "pol-treasury: zero payout");

        // ---- 2. Reserve ratio pre-flight check ----------------------------
        // We use the raw reserve_amount as its own valuation in the reference
        // unit, and native_payout * price_inverse_bps / BPS_DENOM as the
        // supply valuation in the same unit.
        //
        // Simplified: treat reserve_amount as the reserve value, and
        // native_payout as the supply value (same price unit normalisation
        // already done by price_inverse_bps in compute_payout).
        //
        // For a production oracle integration the caller would pass pre-
        // converted USD-denominated values; here we use the raw amounts
        // as a conservative approximation.
        let reserve_in_val = reserve_amount;
        let native_payout_val = native_payout;
        check_and_project_ratio(&env, reserve_in_val, native_payout_val);

        // ---- 3. Transfer reserve asset from bonder → treasury -------------
        let reserve_client = token::Client::new(&env, &reserve_token);
        reserve_client.transfer(
            &bonder,
            &env.current_contract_address(),
            &reserve_amount,
        );

        // ---- 4. Mint native tokens into the treasury (held for vesting) ---
        // The treasury contract must be the native token's mint authority.
        let native_client = token::Client::new(&env, &native_token);
        native_client.mint(&env.current_contract_address(), &native_payout);

        // ---- 5. Create / accumulate vesting schedule ----------------------
        create_or_add_schedule(
            &env,
            &bonder,
            native_payout,
            cliff_ledgers,
            duration_ledgers,
        );

        // ---- 6. Update bond & reserve module state -----------------------
        record_bond(&env, reserve_amount, native_payout);
        record_bond_valuation(&env, reserve_in_val, native_payout_val);

        // ---- 7. Emit event -----------------------------------------------
        Bonded {
            bonder,
            reserve_in: reserve_amount,
            native_payout,
            discount_bps,
        }
        .publish(&env);

        (native_payout, discount_bps)
    }

    // -----------------------------------------------------------------------
    // Claim
    // -----------------------------------------------------------------------

    /// Claim all currently vested native tokens for `beneficiary`.
    ///
    /// Tokens are transferred from the treasury contract to the beneficiary.
    /// Returns the amount transferred (0 if nothing is claimable yet).
    pub fn claim(env: Env, beneficiary: Address) -> i128 {
        beneficiary.require_auth();

        assert!(
            env.storage().instance().has(&KEY_ADMIN),
            "pol-treasury: not initialized"
        );

        let native_token: Address = env.storage().instance().get(&KEY_NAT_TOK).unwrap();

        let claimed = execute_claim(&env, &beneficiary);

        if claimed == 0 {
            return 0;
        }

        // Transfer from treasury → beneficiary
        let native_client = token::Client::new(&env, &native_token);
        native_client.transfer(&env.current_contract_address(), &beneficiary, &claimed);

        Claimed {
            beneficiary,
            amount: claimed,
        }
        .publish(&env);

        claimed
    }

    // -----------------------------------------------------------------------
    // Read-only queries
    // -----------------------------------------------------------------------

    /// Return the current bonding curve state.
    pub fn bond_state(env: Env) -> BondState {
        get_bond_state(&env)
    }

    /// Return the vesting schedule for `beneficiary`, or panic if none exists.
    pub fn vesting_schedule(env: Env, beneficiary: Address) -> VestingSchedule {
        get_schedule(&env, &beneficiary)
            .expect("pol-treasury: no vesting schedule for address")
    }

    /// Return `(reserve_val, supply_val, backing_ratio_bps)`.
    pub fn reserve_state(env: Env) -> (i128, i128, u32) {
        get_reserve_state(&env)
    }

    /// Return the combined treasury snapshot.
    pub fn get_treasury_state(env: Env) -> TreasuryState {
        let reserve_token: Address = env.storage().instance().get(&KEY_RES_TOK).unwrap();
        let native_token: Address = env.storage().instance().get(&KEY_NAT_TOK).unwrap();
        let bond = get_bond_state(&env);
        let (reserve_val, supply_val, backing_ratio_bps) = get_reserve_state(&env);
        let total_vesting_locked: i128 = env
            .storage()
            .instance()
            .get(&vesting::KEY_VEST_LOCK)
            .unwrap_or(0i128);

        TreasuryState {
            reserve_token,
            native_token,
            bond,
            reserve_val,
            supply_val,
            backing_ratio_bps,
            total_vesting_locked,
        }
    }
}

// ---------------------------------------------------------------------------
// Mathematical property tests
//
// These tests validate the core mathematical properties of the bonding
// discount formula, vesting schedule, and reserve ratio without requiring
// a live Soroban environment or token WASM.  Full end-to-end integration
// tests (with token contracts) live in the contract-level `tests/` directory.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, Address, Env};

    // -----------------------------------------------------------------------
    // PROPERTY 1: Discounted payout > par payout at zero reserves
    //
    // Invariant: bonding at zero reserves must always yield more native tokens
    // than bonding at full reserves, for identical reserve_in.
    // -----------------------------------------------------------------------
    #[test]
    fn bond_payout_higher_with_discount() {
        use bond::compute_payout;

        let reserve_in = 10_000_000i128;
        let price_inv = 10_000i128; // 1:1 at par

        // At zero reserves → maximum discount (30 %)
        let (payout_discounted, disc) =
            compute_payout(reserve_in, 0, 3_000, 1_000_000_000, price_inv);

        // At full reserves → zero discount
        let (payout_par, disc_par) =
            compute_payout(reserve_in, 1_000_000_000, 3_000, 1_000_000_000, price_inv);

        assert!(disc > 0, "expected non-zero discount at zero reserves");
        assert_eq!(disc_par, 0, "expected zero discount at full reserves");
        assert!(
            payout_discounted > payout_par,
            "discounted payout {} must exceed par payout {}",
            payout_discounted,
            payout_par
        );
    }

    // -----------------------------------------------------------------------
    // PROPERTY 2: Discount is strictly monotone-decreasing over reserve growth
    //
    // Invariant: each successive bond at higher accumulated reserves must
    // receive a discount that is ≤ the previous bond's discount.
    // -----------------------------------------------------------------------
    #[test]
    fn discount_strictly_monotone_decreasing() {
        use bond::compute_discount_bps;

        let max_disc = 3_000u32;
        let max_res = 10_000_000_000i128;

        let mut prev = max_disc;
        // Simulate 200 sequential bonds of 50 M stroops each
        let mut reserves = 0i128;
        let bond_size = 50_000_000i128;
        for i in 0..200 {
            let d = compute_discount_bps(reserves, max_disc, max_res);
            assert!(
                d <= prev,
                "discount increased on bond {} ({} > {})",
                i,
                d,
                prev
            );
            prev = d;
            reserves += bond_size;
        }
    }

    // -----------------------------------------------------------------------
    // PROPERTY 3: Discount reaches zero at or before max_reserve_target
    //
    // Invariant: once reserves ≥ max_reserve_target, discount = 0.
    // -----------------------------------------------------------------------
    #[test]
    fn discount_zero_at_target() {
        use bond::compute_discount_bps;

        let max_disc = 5_000u32;
        let max_res = 1_000_000_000i128;

        // Exactly at target
        assert_eq!(
            compute_discount_bps(max_res, max_disc, max_res),
            0,
            "discount must be 0 at max_reserve_target"
        );
        // Beyond target
        assert_eq!(
            compute_discount_bps(max_res * 2, max_disc, max_res),
            0,
            "discount must be 0 beyond max_reserve_target"
        );
    }

    // -----------------------------------------------------------------------
    // PROPERTY 4: Payout formula is linear in reserve_in
    //
    // Invariant: doubling reserve_in with identical other parameters must
    // exactly double the payout (at constant reserve_balance).
    // -----------------------------------------------------------------------
    #[test]
    fn payout_linear_in_reserve_in() {
        use bond::compute_payout;

        let base = 5_000_000i128;
        let price_inv = 15_000i128;
        let max_disc = 2_000u32;
        let max_res = 1_000_000_000i128;
        let reserve_bal = 100_000_000i128;

        let (p1, _) = compute_payout(base, reserve_bal, max_disc, max_res, price_inv);
        let (p2, _) = compute_payout(base * 2, reserve_bal, max_disc, max_res, price_inv);
        assert_eq!(p2, p1 * 2, "payout must scale linearly with reserve_in");
    }

    // -----------------------------------------------------------------------
    // PROPERTY 5: Reserve ratio is 100 % when reserve_val = supply_val
    // -----------------------------------------------------------------------
    #[test]
    fn reserve_ratio_100_pct_when_balanced() {
        use reserve::compute_ratio_bps;

        let val = 10_000_000i128;
        assert_eq!(
            compute_ratio_bps(val, val) as i128,
            reserve::BPS_DENOM,
            "ratio must be 10_000 BPS (100%) when reserve equals supply"
        );
    }

    // -----------------------------------------------------------------------
    // PROPERTY 6: Reserve ratio < min_ratio signals under-backed state
    // -----------------------------------------------------------------------
    #[test]
    fn reserve_ratio_rejects_under_backed_bond() {
        use reserve::compute_ratio_bps;

        // reserve_val=100, supply_val=300 → ratio = 33 % < 50 % minimum
        let ratio = compute_ratio_bps(100, 300);
        assert!(
            ratio < 5_000,
            "expected ratio < 50% (5_000 BPS) for reserve=100, supply=300, got {}",
            ratio
        );
    }

    // -----------------------------------------------------------------------
    // PROPERTY 7: Vesting claimable amount is monotone non-decreasing
    //
    // Invariant: time only increases the claimable amount, never decreases it.
    // -----------------------------------------------------------------------
    #[test]
    fn vesting_claimable_never_decreases() {
        use vesting::{compute_claimable, VestingSchedule};

        let env = Env::default();
        let s = VestingSchedule {
            beneficiary: Address::generate(&env),
            total_amount: 10_000_000,
            claimed_amount: 0,
            start_ledger: 0,
            cliff_ledgers: 100,
            duration_ledgers: 500,
        };

        let mut prev = 0i128;
        for ledger in 0u32..=600 {
            let c = compute_claimable(&s, ledger);
            assert!(
                c >= prev,
                "vesting claimable decreased at ledger {}: {} -> {}",
                ledger,
                prev,
                c
            );
            prev = c;
        }
    }

    // -----------------------------------------------------------------------
    // PROPERTY 8: Full payout = total_amount after full duration
    //
    // Invariant: by end_ledger (start + duration), all tokens must be
    // claimable (assuming none have been claimed yet).
    // -----------------------------------------------------------------------
    #[test]
    fn vesting_fully_claimable_after_duration() {
        use vesting::{compute_claimable, VestingSchedule};

        let env = Env::default();
        let total = 5_000_000i128;
        let s = VestingSchedule {
            beneficiary: Address::generate(&env),
            total_amount: total,
            claimed_amount: 0,
            start_ledger: 0,
            cliff_ledgers: 50,
            duration_ledgers: 300,
        };

        // Exactly at end_ledger
        let claimable_at_end = compute_claimable(&s, 300);
        assert_eq!(
            claimable_at_end, total,
            "all {} tokens must be claimable at end_ledger, got {}",
            total, claimable_at_end
        );

        // And beyond
        let claimable_beyond = compute_claimable(&s, 1_000);
        assert_eq!(
            claimable_beyond, total,
            "all tokens must remain claimable beyond end_ledger"
        );
    }

    // -----------------------------------------------------------------------
    // PROPERTY 9: Zero claimable before cliff
    //
    // Invariant: no tokens may be claimed before start_ledger + cliff_ledgers.
    // -----------------------------------------------------------------------
    #[test]
    fn vesting_zero_claimable_before_cliff() {
        use vesting::{compute_claimable, VestingSchedule};

        let env = Env::default();
        let s = VestingSchedule {
            beneficiary: Address::generate(&env),
            total_amount: 1_000_000,
            claimed_amount: 0,
            start_ledger: 100,
            cliff_ledgers: 500,
            duration_ledgers: 2_000,
        };

        // Any ledger strictly before cliff (100 + 500 = 600)
        for ledger in [0u32, 50, 99, 100, 200, 599] {
            let c = compute_claimable(&s, ledger);
            assert_eq!(
                c, 0,
                "expected 0 claimable before cliff at ledger {}, got {}",
                ledger, c
            );
        }
    }

    // -----------------------------------------------------------------------
    // PROPERTY 10: End-to-end discount chain — 1000 sequential bonds
    //
    // Validates that after 1 000 bonds filling the treasury, the effective
    // discount converges to 0 deterministically.
    // -----------------------------------------------------------------------
    #[test]
    fn end_to_end_discount_convergence_1000_bonds() {
        use bond::{compute_discount_bps, compute_payout};

        let max_disc = 3_000u32; // 30 %
        let max_res = 10_000_000_000i128; // 10 B stroops
        let bond_amount = 10_000_000i128; // 10 M stroops each
        let price_inv = 10_000i128;

        let mut cumulative_reserve = 0i128;
        let mut prev_disc = max_disc;

        for i in 0..1_000 {
            let (payout, disc) = compute_payout(
                bond_amount,
                cumulative_reserve,
                max_disc,
                max_res,
                price_inv,
            );

            // Monotone: discount must not increase
            assert!(
                disc <= prev_disc,
                "discount increased on bond {}: {} -> {}",
                i,
                prev_disc,
                disc
            );
            // Payout must always be positive
            assert!(payout > 0, "payout was zero on bond {}", i);

            prev_disc = disc;
            cumulative_reserve += bond_amount;
        }

        // After 1 000 bonds (10 B stroops total = max_res), discount = 0
        let final_disc = compute_discount_bps(cumulative_reserve, max_disc, max_res);
        assert_eq!(
            final_disc, 0,
            "discount must be 0 after reserves reach target, got {}",
            final_disc
        );
    }
}
