//! Core flash-loan execution and dynamic fee logic.
//!
//! This module implements:
//!
//! * [`execute_flash_loan`] — the main execution engine that snapshots
//!   balances, sets the reentrancy lock, dispatches the receiver callback,
//!   releases the lock, and verifies the repayment invariant.
//!
//! * [`compute_fee`] — dynamic fee curve that charges more when pool
//!   liquidity is low (higher utilisation risk) and less when liquidity is
//!   abundant.
//!
//! ## Reentrancy guard
//!
//! Soroban does not expose a call-stack to contracts; instead we use a
//! per-asset boolean flag in instance storage (`DataKey::LoanActive`).
//! The flag is set to `true` immediately before the external receiver call
//! and cleared to `false` immediately after.  Any reentrant `flash_loan`
//! invocation for the same token will read the flag and return
//! [`Error::ReentrantCall`] before touching any funds.
//!
//! Note: Soroban's host-level cross-contract re-entry protection independently
//! blocks direct re-entry into the same contract instance.  Our application-
//! level guard provides defence-in-depth and a clear, documented error code.
//!
//! ## Repayment invariant
//!
//! ```text
//! balance_after  ≥  balance_before + fee
//! ```
//!
//! Where `balance_before` is the **actual** on-chain token balance before the
//! loan is sent out.  After we transfer `amount` to the receiver, balance
//! drops to `balance_before - amount`.  The receiver must return at least
//! `amount + fee`, bringing balance back to at least `balance_before + fee`.
//!
//! This uses the real token balance (queried via the token interface) rather
//! than the tracked `PoolBalance` storage value, providing the strongest
//! possible solvency guarantee.
//!
//! ## Dynamic fee curve
//!
//! ```text
//!   base_fee   = amount × base_fee_bps / 10_000
//!   util_bps   = min(amount × 10_000 / pool_balance, 10_000)
//!   multiplier = 10_000 + 2 × util_bps² / 10_000   (in bps)
//!   fee        = base_fee × multiplier / 10_000
//!   fee        = max(fee, 1)
//! ```
//!
//! At 0 % utilisation the multiplier is 1.0×; at 50 % it is ~1.5×;
//! at 100 % it is 3.0×.  All arithmetic uses checked operations and
//! returns [`Error::ArithmeticOverflow`] on any overflow.

use soroban_sdk::{Address, Bytes, Env, Symbol};
use soroban_sdk::token::Client as TokenClient;

use crate::{receiver::FlashLoanReceiverClient, DataKey, Error, FlashLoanPool};

// ---------------------------------------------------------------------------
// Public entry-point
// ---------------------------------------------------------------------------

/// Execute a flash loan of `amount` units of `token` to `receiver`.
///
/// Steps:
///  1. Validate amount > 0 and pool has sufficient liquidity.
///  2. Compute the dynamic fee.
///  3. Assert no reentrant loan is in-flight for this token.
///  4. Snapshot the on-chain token balance.
///  5. **Set reentrancy lock** for `token`.
///  6. Transfer `amount` to `receiver`.
///  7. Call `receiver.execute_operation(token, amount, fee, user_data)`.
///  8. **Clear reentrancy lock** for `token`.
///  9. Read on-chain balance after callback.
/// 10. Assert: `balance_after >= balance_before + fee` (repayment check).
/// 11. Update tracked `PoolBalance`, `TotalBorrowed`, `TotalFeesCollected`.
/// 12. Emit a `flash_executed` event.
///
/// Returns the fee actually collected.
pub(crate) fn execute_flash_loan(
    env: &Env,
    token: Address,
    amount: i128,
    receiver: Address,
    user_data: Bytes,
) -> Result<i128, Error> {
    // 1. Basic validation.
    if amount <= 0 {
        return Err(Error::InvalidAmount);
    }

    let pool_balance = FlashLoanPool::pool_balance(env, &token);
    if pool_balance < amount {
        return Err(Error::InsufficientLiquidity);
    }

    // 2. Compute dynamic fee.
    let base_fee_bps: u32 = env
        .storage()
        .instance()
        .get(&DataKey::BaseFeeBps)
        .unwrap_or(0);
    let fee = compute_fee(amount, pool_balance, base_fee_bps)?;

    // 3. Reentrancy guard: fail fast if lock is already held.
    if env
        .storage()
        .instance()
        .get::<_, bool>(&DataKey::LoanActive(token.clone()))
        .unwrap_or(false)
    {
        return Err(Error::ReentrantCall);
    }

    let tc = TokenClient::new(env, &token);
    let contract_addr = env.current_contract_address();

    // 4. Snapshot real on-chain balance BEFORE the transfer.
    let balance_before: i128 = tc.balance(&contract_addr);

    // Compute repayment floor.
    //
    // Timeline:
    //   balance_before              (snapshot)
    //   → transfer `amount` out → balance drops to (balance_before - amount)
    //   → receiver callback returns `amount + fee` → balance rises to ≥ balance_before + fee
    //
    // So:  repayment_floor = balance_before + fee
    let repayment_floor = balance_before
        .checked_add(fee)
        .ok_or(Error::ArithmeticOverflow)?;

    // 5. Engage reentrancy lock.
    env.storage()
        .instance()
        .set(&DataKey::LoanActive(token.clone()), &true);

    // 6. Transfer funds to receiver.
    tc.transfer(&contract_addr, &receiver, &amount);

    // 7. Invoke the typed receiver callback.
    FlashLoanReceiverClient::new(env, &receiver)
        .execute_operation(&token, &amount, &fee, &user_data);

    // 8. Release reentrancy lock immediately after the external call returns.
    env.storage()
        .instance()
        .set(&DataKey::LoanActive(token.clone()), &false);

    // 9. Read on-chain balance AFTER callback.
    let balance_after: i128 = tc.balance(&contract_addr);

    // 10. Repayment invariant: balance must be at least balance_before + fee.
    if balance_after < repayment_floor {
        return Err(Error::RepaymentDeficit);
    }

    // 11. Actual fee = net gain = balance_after - balance_before.
    //     (receiver returned amount + at_least_fee, we sent amount out, so
    //      net change = at_least_fee)
    let actual_fee = balance_after
        .checked_sub(balance_before)
        .ok_or(Error::ArithmeticOverflow)?;

    // Update tracked pool balance (add the fee gain).
    let new_pool_balance = pool_balance
        .checked_add(actual_fee)
        .ok_or(Error::ArithmeticOverflow)?;
    env.storage()
        .persistent()
        .set(&DataKey::PoolBalance(token.clone()), &new_pool_balance);

    // Update analytics counters.
    let prev_borrowed: i128 = env
        .storage()
        .persistent()
        .get(&DataKey::TotalBorrowed(token.clone()))
        .unwrap_or(0i128);
    env.storage().persistent().set(
        &DataKey::TotalBorrowed(token.clone()),
        &prev_borrowed
            .checked_add(amount)
            .ok_or(Error::ArithmeticOverflow)?,
    );

    let prev_fees: i128 = env
        .storage()
        .persistent()
        .get(&DataKey::TotalFeesCollected(token.clone()))
        .unwrap_or(0i128);
    env.storage().persistent().set(
        &DataKey::TotalFeesCollected(token.clone()),
        &prev_fees
            .checked_add(actual_fee)
            .ok_or(Error::ArithmeticOverflow)?,
    );

    // 12. Emit event.
    env.events().publish(
        (Symbol::new(env, "flash_executed"), token),
        (receiver, amount, actual_fee),
    );

    Ok(actual_fee)
}

// ---------------------------------------------------------------------------
// Dynamic fee computation
// ---------------------------------------------------------------------------

/// Compute the dynamic fee for a flash loan.
///
/// # Fee model
///
/// ```text
///   base_fee   = amount × base_fee_bps / 10_000
///   util_bps   = min(amount × 10_000 / pool_balance, 10_000)
///   multiplier = 10_000 + 2 × util_bps² / 10_000   (expressed in bps)
///   fee        = base_fee × multiplier / 10_000
///   fee        = max(fee, 1)
/// ```
///
/// Utilisation multiplier at key points:
/// - 0 % → 1.0× base fee
/// - 50 % → ~1.5× base fee
/// - 100 % → 3.0× base fee
///
/// When `pool_balance ≤ 0` the function returns `InsufficientLiquidity`.
/// When `amount ≤ 0` the function returns `InvalidAmount`.
/// The minimum returned fee is always 1 unit.
pub fn compute_fee(amount: i128, pool_balance: i128, base_fee_bps: u32) -> Result<i128, Error> {
    if pool_balance <= 0 {
        return Err(Error::InsufficientLiquidity);
    }
    if amount <= 0 {
        return Err(Error::InvalidAmount);
    }

    // Base fee component.
    let base_fee: i128 = amount
        .checked_mul(base_fee_bps as i128)
        .ok_or(Error::ArithmeticOverflow)?
        .checked_div(10_000)
        .ok_or(Error::ArithmeticOverflow)?;

    // Utilisation in basis-points (0..=10_000).
    let util_bps: i128 = amount
        .checked_mul(10_000)
        .ok_or(Error::ArithmeticOverflow)?
        .checked_div(pool_balance)
        .ok_or(Error::ArithmeticOverflow)?
        .min(10_000);

    // Dynamic multiplier in basis-points:
    //   10_000 + 2 × util_bps² / 10_000
    let util_sq: i128 = util_bps
        .checked_mul(util_bps)
        .ok_or(Error::ArithmeticOverflow)?;
    let dynamic_component: i128 = util_sq
        .checked_mul(2)
        .ok_or(Error::ArithmeticOverflow)?
        .checked_div(10_000)
        .ok_or(Error::ArithmeticOverflow)?;
    let multiplier_bps: i128 = 10_000i128
        .checked_add(dynamic_component)
        .ok_or(Error::ArithmeticOverflow)?;

    // Apply multiplier to base fee.
    let fee: i128 = base_fee
        .checked_mul(multiplier_bps)
        .ok_or(Error::ArithmeticOverflow)?
        .checked_div(10_000)
        .ok_or(Error::ArithmeticOverflow)?;

    // Guarantee at least 1 unit so the pool always captures some gain.
    Ok(fee.max(1))
}
