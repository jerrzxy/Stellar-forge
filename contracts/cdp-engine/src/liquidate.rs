// Copyright 2024 Stellar-forge Contributors
// SPDX-License-Identifier: Apache-2.0

//! # Liquidation Module
//!
//! This module provides the public `liquidate` entry-point that allows any
//! keeper (liquidator) to close an undercollateralised vault in exchange for
//! a discounted portion of its collateral.
//!
//! ## Liquidation mechanics
//!
//! 1. **Eligibility check** – the vault CR must be below `liq_ratio_bps`.
//! 2. **Debt repayment** – the keeper supplies at most `max_debt_to_cover`
//!    of the debt token.  The amount is clamped to the vault's outstanding debt.
//! 3. **Collateral seizure** – the keeper receives collateral worth
//!    `debt_covered * (1 + discount)` at the current oracle price.
//! 4. **Bad debt handling** – if the vault's collateral is worth less than its
//!    debt, the shortfall is isolated into `ReserveBadDebt` and the vault is
//!    zeroed out so that it does not propagate systemic risk.
//!
//! ## Discount formula
//!
//! ```text
//! collateral_seized = debt_covered_value * (1 + discount) / oracle_price
//! ```
//!
//! where `debt_covered_value = debt_covered_units * PRICE_PRECISION` and
//! `oracle_price` is the per-unit collateral price (in PRICE_PRECISION units).
//!
//! More precisely, using raw integer arithmetic:
//!
//! ```text
//! // debt_value = debt_covered (stroops) * price (PRICE_PRECISION-scaled)
//! //           = debt_covered * price / PRICE_PRECISION  [in $-equivalent stroops]
//! // bonus factor = (BPS_DENOMINATOR + liq_discount_bps) / BPS_DENOMINATOR
//! // collateral_to_seize = debt_value * bonus_factor / oracle_price
//! //                     = debt_covered * price * (10000 + discount_bps)
//! //                       / (10000 * price)
//! //                     = debt_covered * (10000 + discount_bps) / 10000
//! ```
//!
//! Note: because both debt and collateral are in the same 7-decimal stroop
//! unit and price cancels out in the final step, the formula simplifies to:
//!
//! ```text
//! collateral_seized = debt_covered * (BPS_DENOMINATOR + discount_bps)
//!                     / BPS_DENOMINATOR
//! ```
//!
//! This is equivalent to "give the liquidator back the same number of token
//! units plus the discount bonus" — the price is used only to confirm
//! eligibility, not to compute the seizure amount.  This design is simpler and
//! avoids oracle-price manipulation affecting liquidation profitability.
//!
//! ## Bad-debt isolation
//!
//! When the remaining vault collateral after seizure would be negative
//! (collateral value < debt value), the vault is declared insolvent:
//!
//! - All available collateral is transferred to the keeper (no matter how
//!   small, it is better than nothing).
//! - The uncovered debt shortfall is added to `ReserveBadDebt`.
//! - `TotalDebt` is reduced by the full vault debt.
//! - The vault is deleted (zeroed out).
//!
//! The `ReserveBadDebt` counter acts as an audit trail and can be used by
//! the protocol to trigger recapitalisation from a safety module or fee pool.

use soroban_sdk::{token::Client as TokenClient, Address, Env};

use crate::{
    bps_to_precision, bump_ttl, compute_cr, get_oracle_price, load_total_debt, load_vault,
    save_total_debt, save_vault, BPS_DENOMINATOR, CdpError, DataKey, VaultState,
};

/// Liquidate an undercollateralised vault.
///
/// # Parameters
///
/// - `liquidator`       – the keeper account; receives discounted collateral.
/// - `vault_owner`      – the owner of the vault to be liquidated.
/// - `max_debt_to_cover` – maximum debt tokens the keeper is willing to burn;
///                        clamped to the vault's actual outstanding debt.
///
/// # Returns
///
/// `(debt_covered, collateral_seized)` — the actual amounts transacted.
///
/// # Errors
///
/// - [`CdpError::NotInitialized`]     – contract not yet set up.
/// - [`CdpError::InvalidAmount`]      – `max_debt_to_cover` is zero or negative.
/// - [`CdpError::VaultNotLiquidatable`] – vault CR is above the liquidation threshold.
/// - [`CdpError::InsufficientDebt`]   – the vault has no outstanding debt.
pub fn liquidate(
    env: &Env,
    liquidator: Address,
    vault_owner: Address,
    max_debt_to_cover: i128,
) -> Result<(i128, i128), CdpError> {
    if max_debt_to_cover <= 0 {
        return Err(CdpError::InvalidAmount);
    }
    liquidator.require_auth();

    let vault = load_vault(env, &vault_owner);

    if vault.debt == 0 {
        return Err(CdpError::InsufficientDebt);
    }
    if vault.collateral < 0 {
        return Err(CdpError::InsufficientCollateral);
    }

    // -------------------------------------------------------------------------
    // 1. Eligibility check — vault must be below the liquidation threshold.
    // -------------------------------------------------------------------------
    let price = get_oracle_price(env)?;
    let liq_ratio_bps: u32 = env
        .storage()
        .instance()
        .get(&DataKey::LiqRatioBps)
        .unwrap();
    let liq_threshold = bps_to_precision(liq_ratio_bps);
    let cr = compute_cr(vault.collateral, vault.debt, price)?;

    if (cr as u128) >= liq_threshold {
        return Err(CdpError::VaultNotLiquidatable);
    }

    // -------------------------------------------------------------------------
    // 2. Clamp the debt the keeper wants to cover.
    // -------------------------------------------------------------------------
    let debt_covered = max_debt_to_cover.min(vault.debt);

    // -------------------------------------------------------------------------
    // 3. Compute collateral to seize.
    //
    //    collateral_seized = debt_covered * (BPS_DENOMINATOR + discount_bps)
    //                        / BPS_DENOMINATOR
    //
    //    Arithmetic in u128 to avoid overflow; result fits in i128 because
    //    debt_covered <= vault.debt which was stored as i128.
    // -------------------------------------------------------------------------
    let discount_bps: u32 = env
        .storage()
        .instance()
        .get(&DataKey::LiqDiscountBps)
        .unwrap();

    let bonus_numerator: u128 = BPS_DENOMINATOR + discount_bps as u128;
    let collateral_to_seize_wide: u128 = (debt_covered as u128)
        .checked_mul(bonus_numerator)
        .ok_or(CdpError::Overflow)?
        / BPS_DENOMINATOR;

    // -------------------------------------------------------------------------
    // 4. Bad-debt handling.
    //
    //    If the vault cannot cover the full seizure, we take all of its
    //    collateral and record the shortfall in ReserveBadDebt.
    // -------------------------------------------------------------------------
    let (actual_collateral_seized, bad_debt) =
        if collateral_to_seize_wide > vault.collateral as u128 {
            // Insolvent vault: seize everything, record the shortfall.
            let collateral_all = vault.collateral;
            let shortfall = vault.debt - debt_covered; // remaining debt not covered
            (collateral_all, shortfall)
        } else {
            let seized = collateral_to_seize_wide as i128;
            (seized, 0_i128)
        };

    // -------------------------------------------------------------------------
    // 5. Update vault state.
    // -------------------------------------------------------------------------
    let new_vault = if bad_debt > 0 {
        // Full liquidation — wipe the vault.
        VaultState {
            collateral: 0,
            debt: 0,
        }
    } else {
        VaultState {
            collateral: vault.collateral - actual_collateral_seized,
            debt: vault.debt - debt_covered,
        }
    };
    save_vault(env, &vault_owner, &new_vault);

    // -------------------------------------------------------------------------
    // 6. Accumulate bad debt into the protocol reserve.
    // -------------------------------------------------------------------------
    if bad_debt > 0 {
        let prev_reserve: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ReserveBadDebt)
            .unwrap_or(0_i128);
        let new_reserve = prev_reserve
            .checked_add(bad_debt)
            .ok_or(CdpError::Overflow)?;
        env.storage()
            .instance()
            .set(&DataKey::ReserveBadDebt, &new_reserve);

        // TotalDebt falls by the full vault debt (including bad portion).
        let full_debt_reduction = vault.debt;
        let new_total = (load_total_debt(env) - full_debt_reduction).max(0);
        save_total_debt(env, new_total);
    } else {
        // Partial or exact liquidation — TotalDebt falls by debt_covered only.
        let new_total = (load_total_debt(env) - debt_covered).max(0);
        save_total_debt(env, new_total);
    }

    // -------------------------------------------------------------------------
    // 7. Token transfers.
    //
    //    a) Burn the keeper's debt tokens.
    //    b) Send the seized collateral to the keeper.
    // -------------------------------------------------------------------------
    let debt_token: Address = env
        .storage()
        .instance()
        .get(&DataKey::DebtToken)
        .unwrap();
    let collateral_token: Address = env
        .storage()
        .instance()
        .get(&DataKey::CollateralToken)
        .unwrap();

    // Burn only the debt_covered portion (in the bad-debt case the vault's
    // remaining debt was already wiped above; we still burn what the keeper
    // supplied, which equals `debt_covered`).
    TokenClient::new(env, &debt_token).burn(&liquidator, &debt_covered);

    // Transfer seized collateral from the contract to the liquidator.
    TokenClient::new(env, &collateral_token).transfer(
        &env.current_contract_address(),
        &liquidator,
        &actual_collateral_seized,
    );

    bump_ttl(env);
    Ok((debt_covered, actual_collateral_seized))
}

// ---------------------------------------------------------------------------
// Public contract entry-point (re-exported via CdpEngine impl in lib.rs)
// ---------------------------------------------------------------------------
//
// The liquidation function is exposed on `CdpEngine` by calling into this
// module.  See `CdpEngine::liquidate` in lib.rs.
