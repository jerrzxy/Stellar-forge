// Copyright 2024 Stellar-forge Contributors
// SPDX-License-Identifier: Apache-2.0

//! # CDP Manager Engine
//!
//! A Collateralized Debt Position (CDP) engine for Stellar Soroban.
//!
//! ## Architecture
//!
//! Each *vault* maps one user address to:
//! - `collateral` – raw token units of the locked collateral asset (e.g. XLM).
//! - `debt`       – raw token units of the outstanding synthetic debt (e.g. sUSD).
//!
//! The protocol enforces a **minimum collateralisation ratio (CR)** at borrow
//! and withdrawal time, and a lower **liquidation ratio** below which any
//! keeper may liquidate the vault.
//!
//! ## Collateralisation Ratio
//!
//! ```text
//! CR = (collateral_units * oracle_price * PRECISION)
//!      / (debt_units * PRICE_PRECISION)
//! ```
//!
//! Both `collateral` and `debt` are stored with 7 decimal places (Stellar
//! convention, 1 whole unit = 10_000_000).  `oracle_price` is denominated
//! in [`PRICE_PRECISION`] units (1e9 = 1 USD).
//!
//! A `CR` of `PRECISION` (1e18) equals 100%.  A minimum CR of 150% is
//! represented as `1_500_000_000_000_000_000` in internal units, or
//! `15_000` basis points when passed to `initialize`.
//!
//! ## Module layout
//!
//! | Module       | Purpose                                          |
//! |--------------|--------------------------------------------------|
//! | `lib.rs`     | Contract entry-points, storage, CR arithmetic    |
//! | `liquidate.rs` | Keeper liquidation logic and bad-debt isolation |
//!
//! ## Safety properties
//!
//! * Overflow: all multi-step arithmetic uses `u128`; every narrowing cast is
//!   checked.
//! * Bad debt: when `collateral_value < debt_value` the shortfall is moved to a
//!   protocol reserve (`ReserveBadDebt`) and the vault is zeroed out.

#![no_std]

pub mod liquidate;

#[cfg(test)]
mod tests;

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype,
    token::Client as TokenClient,
    Address, Env, Symbol,
};

// ---------------------------------------------------------------------------
// Numeric constants
// ---------------------------------------------------------------------------

/// Fixed-point precision for ratio arithmetic: `1e18` = 100 %.
pub const PRECISION: u128 = 1_000_000_000_000_000_000;

/// Fixed-point precision for oracle prices: `1e9` = 1 USD.
pub const PRICE_PRECISION: u128 = 1_000_000_000;

/// Stellar 7-decimal token unit: 1 whole token = 10_000_000 stroops.
pub const STROOP: i128 = 10_000_000;

/// Instance-storage TTL bump (~1 year at ~5 s/ledger).
const LEDGER_TTL_BUMP: u32 = 535_000;

/// Basis-point denominator.
pub const BPS_DENOMINATOR: u128 = 10_000;

// ---------------------------------------------------------------------------
// Error codes
// ---------------------------------------------------------------------------

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum CdpError {
    /// Contract has already been initialised.
    AlreadyInitialized = 1,
    /// Contract has not been initialised yet.
    NotInitialized = 2,
    /// Caller is not the authorised admin.
    Unauthorized = 3,
    /// Zero or negative amount supplied.
    InvalidAmount = 4,
    /// Vault does not hold enough collateral for this withdrawal.
    InsufficientCollateral = 5,
    /// Resulting position would fall below the minimum collateralisation ratio.
    BelowMinCollateralRatio = 6,
    /// Vault is above the liquidation threshold; cannot be liquidated.
    VaultNotLiquidatable = 7,
    /// Oracle returned a zero or negative price.
    InvalidOraclePrice = 8,
    /// Arithmetic overflow in internal calculation.
    Overflow = 9,
    /// A configuration parameter was out of range.
    InvalidParameter = 10,
    /// Vault carries no outstanding debt.
    InsufficientDebt = 11,
}

// ---------------------------------------------------------------------------
// Storage keys
// ---------------------------------------------------------------------------

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// Contract administrator.
    Admin,
    /// External oracle contract address.
    OracleAddr,
    /// Collateral token contract address (e.g. XLM wrapped token).
    CollateralToken,
    /// Debt / synthetic token contract address (e.g. sUSD).
    DebtToken,
    /// Minimum collateralisation ratio in basis points (e.g. 15000 = 150 %).
    MinCrBps,
    /// Liquidation threshold in basis points (e.g. 12000 = 120 %).
    LiqRatioBps,
    /// Discount given to the liquidator in basis points (e.g. 500 = 5 %).
    LiqDiscountBps,
    /// Cumulative bad debt absorbed by the protocol reserve.
    ReserveBadDebt,
    /// Total outstanding debt across all vaults.
    TotalDebt,
    /// Per-user vault state.
    Vault(Address),
}

// ---------------------------------------------------------------------------
// On-chain types
// ---------------------------------------------------------------------------

/// Per-vault state stored on-chain.
///
/// Both fields use the Stellar 7-decimal convention
/// (`1 whole token = 10_000_000`).
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VaultState {
    /// Amount of collateral locked in this vault (in raw token stroops).
    pub collateral: i128,
    /// Outstanding synthetic debt issued against this vault (in raw token stroops).
    pub debt: i128,
}

// ---------------------------------------------------------------------------
// Internal helpers — storage access
// ---------------------------------------------------------------------------

fn require_initialized(env: &Env) -> Result<(), CdpError> {
    if !env.storage().instance().has(&DataKey::Admin) {
        return Err(CdpError::NotInitialized);
    }
    Ok(())
}

fn bump_ttl(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(LEDGER_TTL_BUMP, LEDGER_TTL_BUMP);
}

fn load_vault(env: &Env, user: &Address) -> VaultState {
    env.storage()
        .persistent()
        .get(&DataKey::Vault(user.clone()))
        .unwrap_or(VaultState {
            collateral: 0,
            debt: 0,
        })
}

fn save_vault(env: &Env, user: &Address, vault: &VaultState) {
    env.storage()
        .persistent()
        .set(&DataKey::Vault(user.clone()), vault);
}

fn load_total_debt(env: &Env) -> i128 {
    env.storage()
        .instance()
        .get(&DataKey::TotalDebt)
        .unwrap_or(0_i128)
}

fn save_total_debt(env: &Env, total: i128) {
    env.storage().instance().set(&DataKey::TotalDebt, &total);
}

// ---------------------------------------------------------------------------
// Internal helpers — oracle
// ---------------------------------------------------------------------------

/// Fetch the current collateral price from the oracle contract.
///
/// Calls `oracle.get_price()` which must return an `i128` scaled by
/// [`PRICE_PRECISION`] (i.e. `1_000_000_000` = $1.00).
pub fn get_oracle_price(env: &Env) -> Result<i128, CdpError> {
    let oracle: Address = env
        .storage()
        .instance()
        .get(&DataKey::OracleAddr)
        .unwrap();

    let price: i128 = env.invoke_contract(
        &oracle,
        &Symbol::new(env, "get_price"),
        soroban_sdk::vec![env],
    );

    if price <= 0 {
        return Err(CdpError::InvalidOraclePrice);
    }
    Ok(price)
}

// ---------------------------------------------------------------------------
// Internal helpers — collateralisation ratio
// ---------------------------------------------------------------------------

/// Compute the collateralisation ratio of a vault.
///
/// Returns the ratio scaled by [`PRECISION`] (1e18 = 100 %).
/// Returns `i128::MAX` when there is no debt (fully safe).
///
/// # Formula
///
/// ```text
/// cr = (collateral * oracle_price * PRECISION)
///      / (debt * PRICE_PRECISION)
/// ```
pub fn compute_cr(collateral: i128, debt: i128, price: i128) -> Result<i128, CdpError> {
    if debt == 0 {
        return Ok(i128::MAX);
    }
    if collateral == 0 {
        return Ok(0);
    }

    // All values must be non-negative at this point.
    let c = collateral as u128;
    let d = debt as u128;
    let p = price as u128;

    // Wide intermediate: max ~1e7 * 1e9 * 1e18 = 1e34 < u128::MAX (~3.4e38).
    let num: u128 = c
        .checked_mul(p)
        .and_then(|v| v.checked_mul(PRECISION))
        .ok_or(CdpError::Overflow)?;

    let den: u128 = d
        .checked_mul(PRICE_PRECISION)
        .ok_or(CdpError::Overflow)?;

    let cr = num / den; // floored — conservative for the borrower

    if cr > i128::MAX as u128 {
        return Ok(i128::MAX);
    }
    Ok(cr as i128)
}

/// Convert a basis-point threshold to a [`PRECISION`]-scaled ratio.
///
/// e.g. `15_000` bps → `1_500_000_000_000_000_000` (150 % in PRECISION).
pub fn bps_to_precision(bps: u32) -> u128 {
    (bps as u128) * PRECISION / BPS_DENOMINATOR
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct CdpEngine;

#[contractimpl]
impl CdpEngine {
    // -----------------------------------------------------------------------
    // Initialisation
    // -----------------------------------------------------------------------

    /// Initialise the CDP engine.
    ///
    /// # Parameters
    /// - `admin`            – the privileged address that can update parameters.
    /// - `oracle`           – address of the external price oracle contract.
    /// - `collateral_token` – address of the collateral ERC-20 token (e.g. wXLM).
    /// - `debt_token`       – address of the mintable/burnable debt token (e.g. sUSD).
    /// - `min_cr_bps`       – minimum collateralisation ratio in bps (e.g. 15000 = 150 %).
    /// - `liq_ratio_bps`    – liquidation threshold in bps (e.g. 12000 = 120 %).
    /// - `liq_discount_bps` – discount offered to keepers in bps (e.g. 500 = 5 %).
    pub fn initialize(
        env: Env,
        admin: Address,
        oracle: Address,
        collateral_token: Address,
        debt_token: Address,
        min_cr_bps: u32,
        liq_ratio_bps: u32,
        liq_discount_bps: u32,
    ) -> Result<(), CdpError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(CdpError::AlreadyInitialized);
        }

        // Validate: liquidation ratio must be below the minimum CR, and
        // the discount must leave room for a meaningful incentive.
        if liq_ratio_bps >= min_cr_bps {
            return Err(CdpError::InvalidParameter);
        }
        if liq_discount_bps >= BPS_DENOMINATOR as u32 {
            return Err(CdpError::InvalidParameter);
        }

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::OracleAddr, &oracle);
        env.storage()
            .instance()
            .set(&DataKey::CollateralToken, &collateral_token);
        env.storage()
            .instance()
            .set(&DataKey::DebtToken, &debt_token);
        env.storage()
            .instance()
            .set(&DataKey::MinCrBps, &min_cr_bps);
        env.storage()
            .instance()
            .set(&DataKey::LiqRatioBps, &liq_ratio_bps);
        env.storage()
            .instance()
            .set(&DataKey::LiqDiscountBps, &liq_discount_bps);
        env.storage()
            .instance()
            .set(&DataKey::ReserveBadDebt, &0_i128);
        env.storage()
            .instance()
            .set(&DataKey::TotalDebt, &0_i128);

        bump_ttl(&env);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Collateral management
    // -----------------------------------------------------------------------

    /// Deposit collateral into the caller's vault.
    ///
    /// Transfers `amount` of the collateral token from `user` to the contract
    /// and records it in the vault.  This operation can only increase the CR,
    /// so no ratio check is required.
    pub fn deposit_collateral(env: Env, user: Address, amount: i128) -> Result<(), CdpError> {
        require_initialized(&env)?;
        if amount <= 0 {
            return Err(CdpError::InvalidAmount);
        }
        user.require_auth();

        let collateral_token: Address = env
            .storage()
            .instance()
            .get(&DataKey::CollateralToken)
            .unwrap();

        // Pull collateral from the user into the contract.
        TokenClient::new(&env, &collateral_token).transfer(
            &user,
            &env.current_contract_address(),
            &amount,
        );

        let mut vault = load_vault(&env, &user);
        vault.collateral = vault
            .collateral
            .checked_add(amount)
            .ok_or(CdpError::Overflow)?;
        save_vault(&env, &user, &vault);

        bump_ttl(&env);
        Ok(())
    }

    /// Withdraw collateral from the caller's vault.
    ///
    /// Fails if the resulting vault would fall below the minimum CR.
    /// Allowed with zero debt (full withdrawal).
    pub fn withdraw_collateral(env: Env, user: Address, amount: i128) -> Result<(), CdpError> {
        require_initialized(&env)?;
        if amount <= 0 {
            return Err(CdpError::InvalidAmount);
        }
        user.require_auth();

        let mut vault = load_vault(&env, &user);
        if amount > vault.collateral {
            return Err(CdpError::InsufficientCollateral);
        }

        let new_collateral = vault.collateral - amount;

        // CR check: only required if there is outstanding debt.
        if vault.debt > 0 {
            let price = get_oracle_price(&env)?;
            let min_cr_bps: u32 = env
                .storage()
                .instance()
                .get(&DataKey::MinCrBps)
                .unwrap();
            let min_cr = bps_to_precision(min_cr_bps);
            let new_cr = compute_cr(new_collateral, vault.debt, price)?;
            if (new_cr as u128) < min_cr {
                return Err(CdpError::BelowMinCollateralRatio);
            }
        }

        vault.collateral = new_collateral;
        save_vault(&env, &user, &vault);

        let collateral_token: Address = env
            .storage()
            .instance()
            .get(&DataKey::CollateralToken)
            .unwrap();
        TokenClient::new(&env, &collateral_token).transfer(
            &env.current_contract_address(),
            &user,
            &amount,
        );

        bump_ttl(&env);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Debt management
    // -----------------------------------------------------------------------

    /// Borrow (mint) synthetic debt tokens against locked collateral.
    ///
    /// Checks that the resulting CR is ≥ `min_cr_bps` at the current oracle
    /// price before minting.
    pub fn borrow(env: Env, user: Address, amount: i128) -> Result<(), CdpError> {
        require_initialized(&env)?;
        if amount <= 0 {
            return Err(CdpError::InvalidAmount);
        }
        user.require_auth();

        let mut vault = load_vault(&env, &user);
        let new_debt = vault.debt.checked_add(amount).ok_or(CdpError::Overflow)?;

        let price = get_oracle_price(&env)?;
        let min_cr_bps: u32 = env
            .storage()
            .instance()
            .get(&DataKey::MinCrBps)
            .unwrap();
        let min_cr = bps_to_precision(min_cr_bps);
        let new_cr = compute_cr(vault.collateral, new_debt, price)?;

        if (new_cr as u128) < min_cr {
            return Err(CdpError::BelowMinCollateralRatio);
        }

        vault.debt = new_debt;
        save_vault(&env, &user, &vault);

        // Update global debt counter.
        let total = load_total_debt(&env)
            .checked_add(amount)
            .ok_or(CdpError::Overflow)?;
        save_total_debt(&env, total);

        // Mint debt tokens directly to the borrower.
        let debt_token: Address = env
            .storage()
            .instance()
            .get(&DataKey::DebtToken)
            .unwrap();
        TokenClient::new(&env, &debt_token).mint(&user, &amount);

        bump_ttl(&env);
        Ok(())
    }

    /// Repay (burn) synthetic debt tokens, reducing the vault's debt.
    ///
    /// `amount` is clamped to the vault's actual debt so callers do not need
    /// to calculate the exact balance—overpaying is safe.
    pub fn repay(env: Env, user: Address, amount: i128) -> Result<(), CdpError> {
        require_initialized(&env)?;
        if amount <= 0 {
            return Err(CdpError::InvalidAmount);
        }
        user.require_auth();

        let mut vault = load_vault(&env, &user);
        if vault.debt == 0 {
            return Err(CdpError::InsufficientDebt);
        }

        // Clamp repayment to outstanding debt; safe over-repay UX.
        let actual = amount.min(vault.debt);

        // Burn debt tokens from the user.
        let debt_token: Address = env
            .storage()
            .instance()
            .get(&DataKey::DebtToken)
            .unwrap();
        TokenClient::new(&env, &debt_token).burn(&user, &actual);

        vault.debt -= actual;
        save_vault(&env, &user, &vault);

        // Update global counter.
        let total = (load_total_debt(&env) - actual).max(0);
        save_total_debt(&env, total);

        bump_ttl(&env);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Read-only queries
    // -----------------------------------------------------------------------

    /// Return the raw vault state for a user.
    pub fn get_vault(env: Env, user: Address) -> Result<VaultState, CdpError> {
        require_initialized(&env)?;
        bump_ttl(&env);
        Ok(load_vault(&env, &user))
    }

    /// Return the current collateralisation ratio of a vault.
    ///
    /// Result is scaled by [`PRECISION`] (1e18 = 100 %).
    /// Returns `i128::MAX` when the vault carries no debt.
    pub fn get_collateral_ratio(env: Env, user: Address) -> Result<i128, CdpError> {
        require_initialized(&env)?;
        let vault = load_vault(&env, &user);
        let price = get_oracle_price(&env)?;
        bump_ttl(&env);
        compute_cr(vault.collateral, vault.debt, price)
    }

    /// Return `true` if the vault is currently eligible for liquidation.
    pub fn is_liquidatable(env: Env, user: Address) -> Result<bool, CdpError> {
        require_initialized(&env)?;
        let vault = load_vault(&env, &user);
        if vault.debt == 0 {
            bump_ttl(&env);
            return Ok(false);
        }
        let price = get_oracle_price(&env)?;
        let liq_ratio_bps: u32 = env
            .storage()
            .instance()
            .get(&DataKey::LiqRatioBps)
            .unwrap();
        let liq_threshold = bps_to_precision(liq_ratio_bps);
        let cr = compute_cr(vault.collateral, vault.debt, price)?;
        bump_ttl(&env);
        Ok((cr as u128) < liq_threshold)
    }

    /// Return the total outstanding protocol debt.
    pub fn get_total_debt(env: Env) -> Result<i128, CdpError> {
        require_initialized(&env)?;
        bump_ttl(&env);
        Ok(load_total_debt(&env))
    }

    /// Return the cumulative bad debt held in the protocol reserve.
    pub fn get_reserve_bad_debt(env: Env) -> Result<i128, CdpError> {
        require_initialized(&env)?;
        bump_ttl(&env);
        Ok(env
            .storage()
            .instance()
            .get(&DataKey::ReserveBadDebt)
            .unwrap_or(0_i128))
    }

    // -----------------------------------------------------------------------
    // Admin
    // -----------------------------------------------------------------------

    /// Update the minimum collateralisation ratio.  Admin only.
    pub fn set_min_cr(env: Env, caller: Address, new_min_cr_bps: u32) -> Result<(), CdpError> {
        require_initialized(&env)?;
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if caller != admin {
            return Err(CdpError::Unauthorized);
        }
        caller.require_auth();
        let liq_ratio_bps: u32 = env
            .storage()
            .instance()
            .get(&DataKey::LiqRatioBps)
            .unwrap();
        if new_min_cr_bps <= liq_ratio_bps {
            return Err(CdpError::InvalidParameter);
        }
        env.storage()
            .instance()
            .set(&DataKey::MinCrBps, &new_min_cr_bps);
        bump_ttl(&env);
        Ok(())
    }

    /// Update the oracle contract address.  Admin only.
    pub fn set_oracle(env: Env, caller: Address, new_oracle: Address) -> Result<(), CdpError> {
        require_initialized(&env)?;
        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        if caller != admin {
            return Err(CdpError::Unauthorized);
        }
        caller.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::OracleAddr, &new_oracle);
        bump_ttl(&env);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Liquidation
    // -----------------------------------------------------------------------

    /// Liquidate an undercollateralised vault.
    ///
    /// Any caller (keeper) may call this when a vault's CR is below the
    /// liquidation threshold.  The keeper must hold at least `max_debt_to_cover`
    /// debt tokens; they receive discounted collateral in return.
    ///
    /// Returns `(debt_covered, collateral_seized)`.
    pub fn liquidate(
        env: Env,
        liquidator: Address,
        vault_owner: Address,
        max_debt_to_cover: i128,
    ) -> Result<(i128, i128), CdpError> {
        require_initialized(&env)?;
        liquidate::liquidate(&env, liquidator, vault_owner, max_debt_to_cover)
    }
}
