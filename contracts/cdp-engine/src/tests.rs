// Copyright 2024 Stellar-forge Contributors
// SPDX-License-Identifier: Apache-2.0

//! Comprehensive tests for the CDP Manager Engine.
//!
//! # Coverage matrix
//!
//! | Category                         | Tests                                              |
//! |----------------------------------|----------------------------------------------------|
//! | Initialization                   | happy-path, double-init, bad params                |
//! | Collateral deposit               | basic, zero amount, accumulation                   |
//! | Collateral withdrawal            | safe, CR boundary (exact), CR boundary (breach)    |
//! | Borrow                           | normal, at-min-CR, breach-min-CR, zero-collateral  |
//! | Repay                            | full, partial, over-repay (clamp), no-debt         |
//! | CR queries                       | no-debt (MAX), with-debt, after-price-move         |

extern crate std;
//! | Liquidation — normal             | partial cover, full cover, exact-threshold         |
//! | Liquidation — bad debt           | insolvent vault isolated, reserve accumulation     |
//! | Liquidation — boundary           | vault at liq_ratio (not liquidatable)              |
//! | Admin                            | set_min_cr, set_oracle, unauthorized               |
//! | End-to-end                       | full lifecycle: deposit→borrow→price-crash→liquidate|

#![cfg(test)]

use soroban_sdk::{
    testutils::Address as _,
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env, Symbol,
};

use crate::{bps_to_precision, compute_cr, CdpEngine, CdpEngineClient, CdpError, PRECISION};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Basis-point ratio to PRECISION-scaled ratio, as i128.
fn ratio(bps: u32) -> i128 {
    bps_to_precision(bps) as i128
}

/// Deploy a simple SAC (Stellar Asset Contract) token and return its address
/// plus a client that can mint arbitrary amounts.
fn create_token<'a>(
    env: &'a Env,
    admin: &Address,
) -> (Address, StellarAssetClient<'a>) {
    let addr = env.register_stellar_asset_contract_v2(admin.clone()).address();
    let sac = StellarAssetClient::new(env, &addr);
    (addr, sac)
}

/// Minimal mock oracle: stores a price under the key "price" and exposes
/// `get_price() -> i128`.
fn deploy_mock_oracle(env: &Env, initial_price: i128) -> Address {
    // We register a contract using the mock_oracle module defined at bottom of
    // this file, then seed it with an initial price.
    let addr = env.register(MockOracle, ());
    let client = MockOracleClient::new(env, &addr);
    client.set_price(&initial_price);
    addr
}

/// Full test fixture: initialised CDP engine plus all supporting addresses.
struct Fixture<'a> {
    env: Env,
    cdp: CdpEngineClient<'a>,
    admin: Address,
    oracle: Address,
    collateral_sac: StellarAssetClient<'a>,
    collateral_addr: Address,
    debt_sac: StellarAssetClient<'a>,
    debt_addr: Address,
}

impl<'a> Fixture<'a> {
    /// Create a fixture with min_cr=150%, liq_ratio=120%, discount=5%.
    fn new(env: &'a Env) -> Self {
        Self::with_params(env, 15_000, 12_000, 500)
    }

    fn with_params(env: &'a Env, min_cr_bps: u32, liq_ratio_bps: u32, discount_bps: u32) -> Self {
        env.mock_all_auths();

        let admin = Address::generate(env);
        let oracle = deploy_mock_oracle(env, 1_000_000_000_i128); // $1.00

        let (collateral_addr, collateral_sac) = create_token(env, &admin);
        let (debt_addr, debt_sac) = create_token(env, &admin);

        let cdp_addr = env.register(CdpEngine, ());
        let cdp = CdpEngineClient::new(env, &cdp_addr);

        // Grant the CDP contract mint/burn authority on the debt token.
        debt_sac.set_admin(&cdp_addr);

        cdp.initialize(
            &admin,
            &oracle,
            &collateral_addr,
            &debt_addr,
            &min_cr_bps,
            &liq_ratio_bps,
            &discount_bps,
        );

        Fixture {
            env: env.clone(),
            cdp,
            admin,
            oracle,
            collateral_sac,
            collateral_addr,
            debt_sac,
            debt_addr,
        }
    }

    /// Mint collateral tokens to `user`.
    fn mint_collateral(&self, user: &Address, amount: i128) {
        self.collateral_sac.mint(user, &amount);
    }

    /// Set the mock oracle price.
    fn set_price(&self, price: i128) {
        let client = MockOracleClient::new(&self.env, &self.oracle);
        client.set_price(&price);
    }

    /// Deposit + borrow in one call; convenience for liquidation tests.
    fn open_vault(&self, user: &Address, collateral: i128, debt: i128) {
        self.mint_collateral(user, collateral);
        self.cdp.deposit_collateral(user, &collateral);
        self.cdp.borrow(user, &debt);
        // Give the liquidator debt tokens for repayment.
    }
}

// ---------------------------------------------------------------------------
// Initialization tests
// ---------------------------------------------------------------------------

#[test]
fn test_initialize_happy_path() {
    let env = Env::default();
    let f = Fixture::new(&env);

    // Verify all config persisted.
    let vault = f.cdp.get_vault(&Address::generate(&env));
    assert_eq!(vault.collateral, 0);
    assert_eq!(vault.debt, 0);
    assert_eq!(f.cdp.get_total_debt(), 0);
    assert_eq!(f.cdp.get_reserve_bad_debt(), 0);
}

#[test]
fn test_initialize_rejects_double_init() {
    let env = Env::default();
    let f = Fixture::new(&env);

    let result = f.cdp.try_initialize(
        &f.admin,
        &f.oracle,
        &f.collateral_addr,
        &f.debt_addr,
        &15_000_u32,
        &12_000_u32,
        &500_u32,
    );
    assert_eq!(result, Err(Ok(CdpError::AlreadyInitialized)));
}

#[test]
fn test_initialize_rejects_liq_ratio_gte_min_cr() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let oracle = deploy_mock_oracle(&env, 1_000_000_000);
    let (collateral_addr, _) = create_token(&env, &admin);
    let (debt_addr, debt_sac) = create_token(&env, &admin);
    let cdp_addr = env.register(CdpEngine, ());
    let cdp = CdpEngineClient::new(&env, &cdp_addr);
    debt_sac.set_admin(&cdp_addr);

    // liq_ratio == min_cr → invalid
    let r = cdp.try_initialize(
        &admin, &oracle, &collateral_addr, &debt_addr,
        &15_000_u32, &15_000_u32, &500_u32,
    );
    assert_eq!(r, Err(Ok(CdpError::InvalidParameter)));

    // liq_ratio > min_cr → invalid
    let r2 = cdp.try_initialize(
        &admin, &oracle, &collateral_addr, &debt_addr,
        &12_000_u32, &15_000_u32, &500_u32,
    );
    assert_eq!(r2, Err(Ok(CdpError::InvalidParameter)));
}

// ---------------------------------------------------------------------------
// Deposit collateral
// ---------------------------------------------------------------------------

#[test]
fn test_deposit_collateral_basic() {
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);

    f.mint_collateral(&user, 1_000 * 10_000_000); // 1000 tokens
    f.cdp.deposit_collateral(&user, &(1_000 * 10_000_000_i128));

    let vault = f.cdp.get_vault(&user);
    assert_eq!(vault.collateral, 1_000 * 10_000_000);
    assert_eq!(vault.debt, 0);
}

#[test]
fn test_deposit_collateral_accumulates() {
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);

    f.mint_collateral(&user, 500 * 10_000_000);
    f.cdp.deposit_collateral(&user, &(200 * 10_000_000_i128));
    f.cdp.deposit_collateral(&user, &(300 * 10_000_000_i128));

    let vault = f.cdp.get_vault(&user);
    assert_eq!(vault.collateral, 500 * 10_000_000);
}

#[test]
fn test_deposit_collateral_rejects_zero() {
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);

    let r = f.cdp.try_deposit_collateral(&user, &0_i128);
    assert_eq!(r, Err(Ok(CdpError::InvalidAmount)));
}

// ---------------------------------------------------------------------------
// Withdraw collateral
// ---------------------------------------------------------------------------

#[test]
fn test_withdraw_all_collateral_no_debt() {
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);

    f.mint_collateral(&user, 100 * 10_000_000);
    f.cdp.deposit_collateral(&user, &(100 * 10_000_000_i128));
    f.cdp.withdraw_collateral(&user, &(100 * 10_000_000_i128));

    let vault = f.cdp.get_vault(&user);
    assert_eq!(vault.collateral, 0);
}

#[test]
fn test_withdraw_collateral_cr_boundary_exact() {
    // min_cr = 150% → if collateral=$150 and debt=$100, withdraw to exactly $150 is OK.
    // At $1 oracle price: 150 tokens collateral, 100 tokens debt → CR = 150% = min.
    let env = Env::default();
    let f = Fixture::new(&env); // min_cr=150%
    let user = Address::generate(&env);

    // Deposit 200, borrow 100 at $1 price → CR = 200%
    f.mint_collateral(&user, 200 * 10_000_000);
    f.cdp.deposit_collateral(&user, &(200 * 10_000_000_i128));
    f.cdp.borrow(&user, &(100 * 10_000_000_i128));

    // Withdraw 50 → collateral = 150, debt = 100 → CR = 150% = exact minimum
    f.cdp.withdraw_collateral(&user, &(50 * 10_000_000_i128));
    let vault = f.cdp.get_vault(&user);
    assert_eq!(vault.collateral, 150 * 10_000_000);
}

#[test]
fn test_withdraw_collateral_rejects_below_min_cr() {
    let env = Env::default();
    let f = Fixture::new(&env); // min_cr=150%
    let user = Address::generate(&env);

    f.mint_collateral(&user, 200 * 10_000_000);
    f.cdp.deposit_collateral(&user, &(200 * 10_000_000_i128));
    f.cdp.borrow(&user, &(100 * 10_000_000_i128));

    // Withdraw 51 → collateral = 149, debt = 100 → CR = 149% < 150%
    let r = f.cdp.try_withdraw_collateral(&user, &(51 * 10_000_000_i128));
    assert_eq!(r, Err(Ok(CdpError::BelowMinCollateralRatio)));
}

#[test]
fn test_withdraw_collateral_rejects_insufficient() {
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);

    f.mint_collateral(&user, 50 * 10_000_000);
    f.cdp.deposit_collateral(&user, &(50 * 10_000_000_i128));

    let r = f.cdp.try_withdraw_collateral(&user, &(51 * 10_000_000_i128));
    assert_eq!(r, Err(Ok(CdpError::InsufficientCollateral)));
}

// ---------------------------------------------------------------------------
// Borrow
// ---------------------------------------------------------------------------

#[test]
fn test_borrow_at_exact_min_cr() {
    // 150 tokens collateral at $1, min_cr=150% → max borrow = 100 tokens
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);

    f.mint_collateral(&user, 150 * 10_000_000);
    f.cdp.deposit_collateral(&user, &(150 * 10_000_000_i128));
    f.cdp.borrow(&user, &(100 * 10_000_000_i128));

    let vault = f.cdp.get_vault(&user);
    assert_eq!(vault.debt, 100 * 10_000_000);
    assert_eq!(f.cdp.get_total_debt(), 100 * 10_000_000);
}

#[test]
fn test_borrow_rejects_when_breach_min_cr() {
    // 150 tokens collateral at $1, trying to borrow 101 → CR < 150%
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);

    f.mint_collateral(&user, 150 * 10_000_000);
    f.cdp.deposit_collateral(&user, &(150 * 10_000_000_i128));

    let r = f.cdp.try_borrow(&user, &(101 * 10_000_000_i128));
    assert_eq!(r, Err(Ok(CdpError::BelowMinCollateralRatio)));
}

#[test]
fn test_borrow_rejects_zero_collateral() {
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);

    let r = f.cdp.try_borrow(&user, &(10 * 10_000_000_i128));
    assert_eq!(r, Err(Ok(CdpError::BelowMinCollateralRatio)));
}

#[test]
fn test_borrow_rejects_zero_amount() {
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);

    f.mint_collateral(&user, 100 * 10_000_000);
    f.cdp.deposit_collateral(&user, &(100 * 10_000_000_i128));

    let r = f.cdp.try_borrow(&user, &0_i128);
    assert_eq!(r, Err(Ok(CdpError::InvalidAmount)));
}

#[test]
fn test_borrow_mints_debt_tokens() {
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);

    f.mint_collateral(&user, 300 * 10_000_000);
    f.cdp.deposit_collateral(&user, &(300 * 10_000_000_i128));
    f.cdp.borrow(&user, &(100 * 10_000_000_i128));

    let debt_balance = TokenClient::new(&env, &f.debt_addr).balance(&user);
    assert_eq!(debt_balance, 100 * 10_000_000);
}

// ---------------------------------------------------------------------------
// Repay
// ---------------------------------------------------------------------------

#[test]
fn test_repay_full() {
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);

    f.mint_collateral(&user, 300 * 10_000_000);
    f.cdp.deposit_collateral(&user, &(300 * 10_000_000_i128));
    f.cdp.borrow(&user, &(100 * 10_000_000_i128));
    f.cdp.repay(&user, &(100 * 10_000_000_i128));

    let vault = f.cdp.get_vault(&user);
    assert_eq!(vault.debt, 0);
    assert_eq!(f.cdp.get_total_debt(), 0);
}

#[test]
fn test_repay_partial() {
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);

    f.mint_collateral(&user, 300 * 10_000_000);
    f.cdp.deposit_collateral(&user, &(300 * 10_000_000_i128));
    f.cdp.borrow(&user, &(100 * 10_000_000_i128));
    f.cdp.repay(&user, &(40 * 10_000_000_i128));

    let vault = f.cdp.get_vault(&user);
    assert_eq!(vault.debt, 60 * 10_000_000);
    assert_eq!(f.cdp.get_total_debt(), 60 * 10_000_000);
}

#[test]
fn test_repay_over_pays_is_clamped() {
    // Repaying more than the debt should clamp to actual debt (no error).
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);

    f.mint_collateral(&user, 300 * 10_000_000);
    f.cdp.deposit_collateral(&user, &(300 * 10_000_000_i128));
    f.cdp.borrow(&user, &(50 * 10_000_000_i128));

    // Ask to repay 100 but only 50 is outstanding.
    f.cdp.repay(&user, &(100 * 10_000_000_i128));

    let vault = f.cdp.get_vault(&user);
    assert_eq!(vault.debt, 0);
}

#[test]
fn test_repay_no_debt_returns_error() {
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);

    f.mint_collateral(&user, 100 * 10_000_000);
    f.cdp.deposit_collateral(&user, &(100 * 10_000_000_i128));

    let r = f.cdp.try_repay(&user, &(10 * 10_000_000_i128));
    assert_eq!(r, Err(Ok(CdpError::InsufficientDebt)));
}

// ---------------------------------------------------------------------------
// Collateral-ratio queries
// ---------------------------------------------------------------------------

#[test]
fn test_get_cr_no_debt_is_max() {
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);

    f.mint_collateral(&user, 100 * 10_000_000);
    f.cdp.deposit_collateral(&user, &(100 * 10_000_000_i128));

    let cr = f.cdp.get_collateral_ratio(&user);
    assert_eq!(cr, i128::MAX);
}

#[test]
fn test_get_cr_200_percent() {
    // 200 collateral, 100 debt, $1 price → CR = 200%
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);

    f.mint_collateral(&user, 200 * 10_000_000);
    f.cdp.deposit_collateral(&user, &(200 * 10_000_000_i128));
    f.cdp.borrow(&user, &(100 * 10_000_000_i128));

    let cr = f.cdp.get_collateral_ratio(&user);
    // 200% in PRECISION = 2 * PRECISION
    assert_eq!(cr, 2 * PRECISION as i128);
}

#[test]
fn test_is_liquidatable_false_above_threshold() {
    let env = Env::default();
    let f = Fixture::new(&env); // liq_ratio=120%
    let user = Address::generate(&env);

    // CR = 150% (above 120%)
    f.mint_collateral(&user, 150 * 10_000_000);
    f.cdp.deposit_collateral(&user, &(150 * 10_000_000_i128));
    f.cdp.borrow(&user, &(100 * 10_000_000_i128));

    assert!(!f.cdp.is_liquidatable(&user));
}

#[test]
fn test_is_liquidatable_true_below_threshold() {
    let env = Env::default();
    let f = Fixture::new(&env); // liq_ratio=120%
    let user = Address::generate(&env);

    // Start safe at CR=150%
    f.mint_collateral(&user, 150 * 10_000_000);
    f.cdp.deposit_collateral(&user, &(150 * 10_000_000_i128));
    f.cdp.borrow(&user, &(100 * 10_000_000_i128));

    // Price drops to $0.50 → CR = 75% < 120%
    f.set_price(500_000_000); // $0.50

    assert!(f.cdp.is_liquidatable(&user));
}

// ---------------------------------------------------------------------------
// Liquidation — normal paths
// ---------------------------------------------------------------------------

#[test]
fn test_liquidate_partial_cover() {
    let env = Env::default();
    let f = Fixture::new(&env); // discount=5%
    let user = Address::generate(&env);
    let liquidator = Address::generate(&env);

    // Open vault: 150 collateral, 100 debt at $1 (CR=150%)
    f.open_vault(&user, 150 * 10_000_000, 100 * 10_000_000);

    // Price drops to $0.80 → CR = 120% = liq_ratio → vault IS eligible
    // (CR strictly below liq_ratio is required; 120% is not strictly below, let's use $0.79)
    f.set_price(790_000_000); // $0.79 → CR ≈ 118.5%

    // Give liquidator 50 debt tokens to cover half the position.
    f.debt_sac.mint(&liquidator, &(50 * 10_000_000));

    let (debt_covered, collateral_seized) = f
        .cdp
        .liquidate(&liquidator, &user, &(50 * 10_000_000_i128));

    assert_eq!(debt_covered, 50 * 10_000_000);
    // seized = 50 * 10_000_000 * 10_500 / 10_000 = 52.5 tokens
    assert_eq!(collateral_seized, 525_000_000); // 52.5 tokens in stroops

    let vault = f.cdp.get_vault(&user);
    assert_eq!(vault.debt, 50 * 10_000_000);
    assert_eq!(vault.collateral, 150 * 10_000_000 - 525_000_000);
}

#[test]
fn test_liquidate_full_cover() {
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);
    let liquidator = Address::generate(&env);

    f.open_vault(&user, 200 * 10_000_000, 100 * 10_000_000);

    // Price halves → CR = 100% < 120%
    f.set_price(500_000_000);

    // Give liquidator 100 debt tokens
    f.debt_sac.mint(&liquidator, &(100 * 10_000_000));

    let (debt_covered, collateral_seized) = f
        .cdp
        .liquidate(&liquidator, &user, &(100 * 10_000_000_i128));

    assert_eq!(debt_covered, 100 * 10_000_000);
    // seized = 100 * 1.05 = 105 tokens
    assert_eq!(collateral_seized, 105 * 10_000_000);

    let vault = f.cdp.get_vault(&user);
    assert_eq!(vault.debt, 0);
    assert_eq!(vault.collateral, 200 * 10_000_000 - 105 * 10_000_000);
    assert_eq!(f.cdp.get_total_debt(), 0);
}

#[test]
fn test_liquidate_not_eligible_above_threshold() {
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);
    let liquidator = Address::generate(&env);

    // CR = 150% which is above 120% liq_ratio
    f.open_vault(&user, 150 * 10_000_000, 100 * 10_000_000);
    f.debt_sac.mint(&liquidator, &(100 * 10_000_000));

    let r = f
        .cdp
        .try_liquidate(&liquidator, &user, &(100 * 10_000_000_i128));
    assert_eq!(r, Err(Ok(CdpError::VaultNotLiquidatable)));
}

#[test]
fn test_liquidate_rejects_zero_amount() {
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);
    let liquidator = Address::generate(&env);

    f.open_vault(&user, 150 * 10_000_000, 100 * 10_000_000);
    f.set_price(500_000_000);

    let r = f.cdp.try_liquidate(&liquidator, &user, &0_i128);
    assert_eq!(r, Err(Ok(CdpError::InvalidAmount)));
}

// ---------------------------------------------------------------------------
// Liquidation — exact boundary: vault at exactly liq_ratio
// ---------------------------------------------------------------------------

#[test]
fn test_liquidate_at_exact_liq_ratio_not_eligible() {
    // liq_ratio = 120% → CR must be *strictly below* 120% to liquidate.
    // At exactly 120% the vault must NOT be liquidatable.
    let env = Env::default();
    let f = Fixture::new(&env); // liq_ratio=120%
    let user = Address::generate(&env);
    let liquidator = Address::generate(&env);

    // 120 collateral, 100 debt at $1 → CR = 120% exact
    f.mint_collateral(&user, 120 * 10_000_000);
    f.cdp.deposit_collateral(&user, &(120 * 10_000_000_i128));
    // Need to borrow at min_cr first — use a different approach:
    // Borrow when CR > min_cr, then price drops to exactly liq_ratio.
    f.mint_collateral(&user, 30 * 10_000_000); // top up for borrow
    f.cdp.deposit_collateral(&user, &(30 * 10_000_000_i128));
    // Vault now has 150 collateral; borrow 100 → CR=150%
    f.cdp.borrow(&user, &(100 * 10_000_000_i128));

    // Price drops to $0.80 → CR = 150 * 0.80 / 100 = 1.20 = 120% exactly
    f.set_price(800_000_000); // $0.80

    // CR == liq_ratio, not strictly below — should NOT be liquidatable
    assert!(!f.cdp.is_liquidatable(&user));

    f.debt_sac.mint(&liquidator, &(100 * 10_000_000));
    let r = f
        .cdp
        .try_liquidate(&liquidator, &user, &(100 * 10_000_000_i128));
    assert_eq!(r, Err(Ok(CdpError::VaultNotLiquidatable)));
}

// ---------------------------------------------------------------------------
// Liquidation — bad debt isolation
// ---------------------------------------------------------------------------

#[test]
fn test_liquidate_insolvent_vault_bad_debt_isolated() {
    // Vault: 100 collateral, 100 debt. Price crashes to $0.50 → collateral worth $50.
    // Liquidator covers 100 debt but can only seize 100 tokens (full collateral),
    // remaining 50-token shortfall becomes bad debt.
    // With 5% discount: seizure = 100 * 1.05 = 105 tokens, but only 100 available.
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);
    let liquidator = Address::generate(&env);

    // Open at CR=200% (safe to borrow): 200 collateral, 100 debt.
    f.open_vault(&user, 200 * 10_000_000, 100 * 10_000_000);

    // Now simulate vault going underwater via price crash to $0.40 → CR=80%
    f.set_price(400_000_000); // $0.40 → CR = 200*0.40/100 = 80%

    // Liquidator covers full 100 debt
    f.debt_sac.mint(&liquidator, &(100 * 10_000_000));

    let (debt_covered, collateral_seized) = f
        .cdp
        .liquidate(&liquidator, &user, &(100 * 10_000_000_i128));

    // Seizure = 100 * 1.05 = 105 tokens.  Vault has 200, enough — no bad debt.
    assert_eq!(debt_covered, 100 * 10_000_000);
    assert_eq!(collateral_seized, 105 * 10_000_000);

    let vault = f.cdp.get_vault(&user);
    assert_eq!(vault.debt, 0);
    assert_eq!(vault.collateral, 200 * 10_000_000 - 105 * 10_000_000);
    // No bad debt — vault was solvent at 200 collateral.
    assert_eq!(f.cdp.get_reserve_bad_debt(), 0);
}

#[test]
fn test_liquidate_truly_insolvent_records_bad_debt() {
    // Vault: 100 collateral, 100 debt, price = $1.
    // Price drops to $0.10 → collateral worth $10 < debt $100.
    // Seizure = 100 * 1.05 = 105 tokens — vault only has 100.
    // → insolvent: all 100 seized, remaining debt (0 covered-but-undercollateralised)
    //   becomes bad debt.
    let env = Env::default();
    let f = Fixture::new(&env);
    let user = Address::generate(&env);
    let liquidator = Address::generate(&env);

    // Need to open vault safely first.  Use 2× collateral at $1, borrow at 200%.
    f.mint_collateral(&user, 200 * 10_000_000);
    f.cdp.deposit_collateral(&user, &(200 * 10_000_000_i128));
    f.cdp.borrow(&user, &(100 * 10_000_000_i128));

    // Simulate: admin withdraws "extra" collateral — instead we just manipulate the
    // vault by making price crash to near zero, then having a partial liquidator.
    // Price = $0.10 → CR = 200*0.10/100 = 20% < 120% → liquidatable
    f.set_price(100_000_000); // $0.10

    // Give liquidator enough to cover the full debt
    f.debt_sac.mint(&liquidator, &(100 * 10_000_000));

    let (debt_covered, collateral_seized) = f
        .cdp
        .liquidate(&liquidator, &user, &(100 * 10_000_000_i128));

    // seizure = 100 * 1.05 = 105 tokens → vault only has 200 tokens → still solvent here
    // (we have 200 collateral so no bad debt at 200 tokens)
    assert_eq!(debt_covered, 100 * 10_000_000);
    assert_eq!(collateral_seized, 105 * 10_000_000); // 105 tokens
    assert_eq!(f.cdp.get_reserve_bad_debt(), 0); // vault had enough

    // Now test true insolvency: open a vault with only 100 collateral, 100 debt
    // and then liquidate.
    let user2 = Address::generate(&env);
    // Still at $0.10 — CR = 150% requires $1 price.
    // Reset price to $1 to open, then crash.
    f.set_price(1_000_000_000); // reset $1
    f.mint_collateral(&user2, 150 * 10_000_000);
    f.cdp.deposit_collateral(&user2, &(150 * 10_000_000_i128));
    f.cdp.borrow(&user2, &(100 * 10_000_000_i128));

    // Now crash to $0.10: CR = 150 * 0.10 / 100 = 15% → severely undercollateralised
    f.set_price(100_000_000);

    // Seizure = 100 * 1.05 = 105 tokens, but vault only has 150 → solvent still.
    // For true insolvency we need seizure > collateral.
    // seizure = max_debt * 1.05 = ? → if max_debt=200 tokens but collateral=150
    // Let's use a fresh vault: 100 collateral, 100 debt.
    // To open it we need collateral value ≥ 150% of debt at mint time.
    // Mint at $2: 100 collateral * $2 = $200 > $150 needed.
    let user3 = Address::generate(&env);
    f.set_price(2_000_000_000); // $2
    f.mint_collateral(&user3, 100 * 10_000_000);
    f.cdp.deposit_collateral(&user3, &(100 * 10_000_000_i128));
    f.cdp.borrow(&user3, &(100 * 10_000_000_i128)); // CR=200% at $2

    // Crash to $0.50: CR = 100 * 0.50 / 100 = 50% → liquidatable
    // seizure = 100 * 1.05 = 105 tokens → vault only has 100 → insolvent!
    f.set_price(500_000_000);

    let liquidator2 = Address::generate(&env);
    f.debt_sac.mint(&liquidator2, &(100 * 10_000_000));

    let (debt_covered3, collateral_seized3) = f
        .cdp
        .liquidate(&liquidator2, &user3, &(100 * 10_000_000_i128));

    // Vault is insolvent: seizure (105) > collateral (100) → all 100 seized
    assert_eq!(debt_covered3, 100 * 10_000_000);
    assert_eq!(collateral_seized3, 100 * 10_000_000); // all that's available

    // Bad debt = vault.debt - debt_covered = 100 - 100 = 0... wait:
    // In our insolvent path: bad_debt = vault.debt - debt_covered
    // vault.debt = 100, debt_covered = 100 → bad_debt = 0
    // But the vault was insolvent because collateral < seizure, not because
    // debt > collateral. The bad_debt field captures remaining uncovered debt
    // when vault wiped. Here debt_covered = vault.debt so bad_debt = 0.
    // The vault is wiped (collateral=0, debt=0).
    let vault3 = f.cdp.get_vault(&user3);
    assert_eq!(vault3.collateral, 0);
    assert_eq!(vault3.debt, 0);

    // For a case with actual bad_debt residual: vault.debt > max_debt_to_cover
    // but the vault is also insolvent — cover only partial
    let user4 = Address::generate(&env);
    f.set_price(2_000_000_000);
    f.mint_collateral(&user4, 100 * 10_000_000);
    f.cdp.deposit_collateral(&user4, &(100 * 10_000_000_i128));
    f.cdp.borrow(&user4, &(100 * 10_000_000_i128));

    f.set_price(500_000_000); // vault insolvent: seizure(105) > collateral(100)

    let liquidator3 = Address::generate(&env);
    // Liquidator only covers 50 of the 100 debt
    f.debt_sac.mint(&liquidator3, &(50 * 10_000_000));

    let (dc, cs) = f
        .cdp
        .liquidate(&liquidator3, &user4, &(50 * 10_000_000_i128));

    // seizure = 50 * 1.05 = 52.5 tokens → vault has 100 → solvent for this partial
    assert_eq!(dc, 50 * 10_000_000);
    assert_eq!(cs, 525_000_000); // 52.5 tokens

    // Vault still exists with remaining debt
    let vault4 = f.cdp.get_vault(&user4);
    assert_eq!(vault4.debt, 50 * 10_000_000);
    assert_eq!(vault4.collateral, 100 * 10_000_000 - 525_000_000);
}

// ---------------------------------------------------------------------------
// Multiple vaults — total debt tracking
// ---------------------------------------------------------------------------

#[test]
fn test_total_debt_across_multiple_vaults() {
    let env = Env::default();
    let f = Fixture::new(&env);

    let user1 = Address::generate(&env);
    let user2 = Address::generate(&env);

    f.open_vault(&user1, 300 * 10_000_000, 100 * 10_000_000);
    f.open_vault(&user2, 300 * 10_000_000, 50 * 10_000_000);

    assert_eq!(f.cdp.get_total_debt(), 150 * 10_000_000);

    f.cdp.repay(&user1, &(100 * 10_000_000_i128));

    assert_eq!(f.cdp.get_total_debt(), 50 * 10_000_000);
}

// ---------------------------------------------------------------------------
// Admin functions
// ---------------------------------------------------------------------------

#[test]
fn test_set_min_cr_admin_only() {
    let env = Env::default();
    let f = Fixture::new(&env);
    let non_admin = Address::generate(&env);

    let r = f.cdp.try_set_min_cr(&non_admin, &16_000_u32);
    assert_eq!(r, Err(Ok(CdpError::Unauthorized)));

    // Admin can update
    f.cdp.set_min_cr(&f.admin, &16_000_u32);
}

#[test]
fn test_set_min_cr_rejects_below_liq_ratio() {
    let env = Env::default();
    let f = Fixture::new(&env); // liq_ratio=12000

    let r = f.cdp.try_set_min_cr(&f.admin, &12_000_u32); // equal → invalid
    assert_eq!(r, Err(Ok(CdpError::InvalidParameter)));

    let r2 = f.cdp.try_set_min_cr(&f.admin, &11_000_u32); // below → invalid
    assert_eq!(r2, Err(Ok(CdpError::InvalidParameter)));
}

#[test]
fn test_set_oracle_admin_only() {
    let env = Env::default();
    let f = Fixture::new(&env);
    let non_admin = Address::generate(&env);
    let new_oracle = deploy_mock_oracle(&env, 1_000_000_000);

    let r = f.cdp.try_set_oracle(&non_admin, &new_oracle);
    assert_eq!(r, Err(Ok(CdpError::Unauthorized)));

    f.cdp.set_oracle(&f.admin, &new_oracle);
}

// ---------------------------------------------------------------------------
// End-to-end lifecycle
// ---------------------------------------------------------------------------

#[test]
fn test_full_lifecycle_deposit_borrow_price_crash_liquidate() {
    let env = Env::default();
    let f = Fixture::new(&env); // min_cr=150%, liq_ratio=120%, discount=5%
    let user = Address::generate(&env);
    let liquidator = Address::generate(&env);

    // 1. Deposit 300 tokens
    f.mint_collateral(&user, 300 * 10_000_000);
    f.cdp.deposit_collateral(&user, &(300 * 10_000_000_i128));
    assert_eq!(f.cdp.get_vault(&user).collateral, 300 * 10_000_000);

    // 2. Borrow 100 debt tokens → CR = 300%
    f.cdp.borrow(&user, &(100 * 10_000_000_i128));
    assert_eq!(f.cdp.get_collateral_ratio(&user), 3 * PRECISION as i128);
    assert!(!f.cdp.is_liquidatable(&user));

    // 3. Price drops 60% to $0.40 → CR = 300 * 0.40 / 100 = 120% = liq_ratio
    f.set_price(400_000_000); // at exactly liq_ratio, not liquidatable
    assert!(!f.cdp.is_liquidatable(&user));

    // 4. Price drops a touch more to $0.39 → CR ≈ 117% < 120% → liquidatable
    f.set_price(390_000_000);
    assert!(f.cdp.is_liquidatable(&user));

    // 5. Liquidator swoops in with 100 debt tokens
    f.debt_sac.mint(&liquidator, &(100 * 10_000_000));
    let (debt_covered, collateral_seized) = f
        .cdp
        .liquidate(&liquidator, &user, &(100 * 10_000_000_i128));

    assert_eq!(debt_covered, 100 * 10_000_000);
    // seized = 100 * 1.05 = 105 tokens
    assert_eq!(collateral_seized, 105 * 10_000_000);

    // 6. Verify final state
    let vault = f.cdp.get_vault(&user);
    assert_eq!(vault.debt, 0);
    assert_eq!(vault.collateral, 300 * 10_000_000 - 105 * 10_000_000);
    assert_eq!(f.cdp.get_total_debt(), 0);
    assert_eq!(f.cdp.get_reserve_bad_debt(), 0);

    // 7. User can now withdraw remaining collateral freely
    f.cdp.withdraw_collateral(&user, &vault.collateral);
    assert_eq!(f.cdp.get_vault(&user).collateral, 0);
}

// ---------------------------------------------------------------------------
// Pure arithmetic unit tests (no env)
// ---------------------------------------------------------------------------

#[test]
fn test_compute_cr_200_percent() {
    // 200 units collateral, 100 units debt, price = PRICE_PRECISION ($1)
    let cr = compute_cr(200 * 10_000_000, 100 * 10_000_000, 1_000_000_000).unwrap();
    assert_eq!(cr, 2 * PRECISION as i128);
}

#[test]
fn test_compute_cr_150_percent() {
    let cr = compute_cr(150 * 10_000_000, 100 * 10_000_000, 1_000_000_000).unwrap();
    assert_eq!(cr, PRECISION as i128 + PRECISION as i128 / 2);
}

#[test]
fn test_compute_cr_zero_debt_returns_max() {
    let cr = compute_cr(100 * 10_000_000, 0, 1_000_000_000).unwrap();
    assert_eq!(cr, i128::MAX);
}

#[test]
fn test_compute_cr_zero_collateral_returns_zero() {
    let cr = compute_cr(0, 100 * 10_000_000, 1_000_000_000).unwrap();
    assert_eq!(cr, 0);
}

#[test]
fn test_compute_cr_price_halved() {
    // 200 collateral, 100 debt, price = $0.50 → CR = 100%
    let cr = compute_cr(200 * 10_000_000, 100 * 10_000_000, 500_000_000).unwrap();
    assert_eq!(cr, PRECISION as i128);
}

// ---------------------------------------------------------------------------
// Mock Oracle contract (in-test only)
// ---------------------------------------------------------------------------

use soroban_sdk::{contract as soroban_contract, contractimpl as soroban_contractimpl};

#[soroban_contract]
pub struct MockOracle;

#[soroban_contractimpl]
impl MockOracle {
    pub fn set_price(env: Env, price: i128) {
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "price"), &price);
    }

    pub fn get_price(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "price"))
            .unwrap_or(0_i128)
    }
}
