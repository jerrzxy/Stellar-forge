//! Flash Loan Receiver Trait
//!
//! This module defines [`FlashLoanReceiver`], the interface that every
//! contract which wants to borrow from the [`FlashLoanPool`] must implement.
//!
//! # How it works
//!
//! ```text
//!  Caller
//!    │  flash_loan(token, amount, receiver, user_data)
//!    ▼
//!  FlashLoanPool
//!    │  1. transfer `amount` of `token` → receiver
//!    │  2. invoke receiver.execute_operation(token, amount, fee, user_data)
//!    │  3. assert balance increased by at least `fee`
//!    ▼
//!  FlashLoanReceiver (your contract)
//!    │  receives funds, performs arbitrary logic, repays amount + fee
//!    ▼
//!  FlashLoanPool (repayment verified)
//! ```
//!
//! # Implementation contract
//!
//! A receiver **must** return `amount + fee` units of `token` to the pool
//! contract within the same Soroban transaction, otherwise the pool's
//! repayment check will fail and the entire transaction will be rolled back.
//!
//! # Example
//!
//! ```ignore
//! use soroban_sdk::{contract, contractimpl, Address, Bytes, Env};
//! use flash_loan::receiver::FlashLoanReceiver;
//!
//! #[contract]
//! pub struct ArbitrageBot;
//!
//! #[contractimpl]
//! impl FlashLoanReceiver for ArbitrageBot {
//!     fn execute_operation(
//!         env: Env,
//!         token: Address,
//!         amount: i128,
//!         fee: i128,
//!         user_data: Bytes,
//!     ) {
//!         // 1. Execute profitable trade with the borrowed `amount`.
//!         // 2. Repay `amount + fee` to the pool contract.
//!         let pool = get_pool_address(&env);
//!         token::Client::new(&env, &token)
//!             .transfer(&env.current_contract_address(), &pool, &(amount + fee));
//!     }
//! }
//! ```
//!
//! [`FlashLoanPool`]: crate::FlashLoanPool

use soroban_sdk::{contractclient, Address, Bytes, Env};

// ---------------------------------------------------------------------------
// Receiver trait
// ---------------------------------------------------------------------------

/// Callback interface that flash-loan receiver contracts must implement.
///
/// The [`FlashLoanPool`] invokes `execute_operation` after transferring the
/// requested funds.  The receiver is responsible for:
///
/// 1. Using the borrowed `amount` for any purpose within the transaction.
/// 2. Repaying `amount + fee` of `token` to the pool **before this function
///    returns**.  The pool verifies repayment by comparing its on-chain token
///    balance before and after the callback.
///
/// # Repayment invariant
///
/// ```text
/// pool_balance_after  ≥  pool_balance_before + fee
/// ```
///
/// If the invariant is not met the pool returns [`Error::RepaymentDeficit`]
/// and the whole transaction reverts — no funds leave the pool.
///
/// # Reentrancy
///
/// Calling [`FlashLoanPool::flash_loan`] for the **same token** from within
/// `execute_operation` is blocked by the per-asset reentrancy guard.  Loans
/// of a **different** token are allowed, provided that token's pool has
/// sufficient liquidity.
///
/// [`FlashLoanPool`]: crate::FlashLoanPool
/// [`FlashLoanPool::flash_loan`]: crate::FlashLoanPool::flash_loan
/// [`Error::RepaymentDeficit`]: crate::Error::RepaymentDeficit
#[contractclient(name = "FlashLoanReceiverClient")]
pub trait FlashLoanReceiver {
    /// Called by the [`FlashLoanPool`] immediately after `amount` of `token`
    /// has been transferred to this contract.
    ///
    /// # Parameters
    ///
    /// - `env`       — the Soroban execution environment.
    /// - `token`     — address of the borrowed token's contract.
    /// - `amount`    — number of token units that were transferred to this
    ///                 contract (always `> 0`).
    /// - `fee`       — number of additional token units the pool expects back
    ///                 on top of `amount`.  This is the value returned by
    ///                 [`FlashLoanPool::quote_fee`] at the moment the loan was
    ///                 executed (always `>= 1`).
    /// - `user_data` — arbitrary byte payload forwarded from the original
    ///                 `flash_loan` call; can be empty.
    ///
    /// Returning successfully completes the callback, but does not waive the
    /// pool's repayment check. A callback failure aborts the invocation and
    /// reverts the transaction.
    ///
    /// # Panics
    ///
    /// An unhandled panic inside `execute_operation` causes the Soroban host
    /// to abort the transaction, which also reverts the fund transfer.
    ///
    /// # Examples
    ///
    /// ## Minimal pass-through (repay immediately, no trade)
    ///
    /// ```ignore
    /// fn execute_operation(
    ///     env: Env,
    ///     token: Address,
    ///     amount: i128,
    ///     fee: i128,
    ///     _user_data: Bytes,
    /// ) {
    ///     let pool = get_pool_address(&env);          // stored during initialisation
    ///     let repayment = amount.checked_add(fee).expect("overflow");
    ///     token::Client::new(&env, &token)
    ///         .transfer(&env.current_contract_address(), &pool, &repayment);
    /// }
    /// ```
    ///
    /// ## Arbitrage example (simplified)
    ///
    /// ```ignore
    /// fn execute_operation(
    ///     env: Env,
    ///     token: Address,
    ///     amount: i128,
    ///     fee: i128,
    ///     user_data: Bytes,
    /// ) {
    ///     // 1. Decode trade parameters from user_data.
    ///     let params: TradeParams = decode_params(&user_data);
    ///
    ///     // 2. Execute the trade on an external DEX.
    ///     let profit = execute_trade(&env, &token, amount, &params);
    ///
    ///     // 3. Repay the pool (amount + fee).  The profit stays with us.
    ///     let repayment = amount.checked_add(fee).expect("overflow");
    ///     token::Client::new(&env, &token)
    ///         .transfer(&env.current_contract_address(), &params.pool, &repayment);
    ///
    /// }
    /// ```
    ///
    /// [`FlashLoanPool`]: crate::FlashLoanPool
    /// [`FlashLoanPool::quote_fee`]: crate::FlashLoanPool::quote_fee
    fn execute_operation(env: Env, token: Address, amount: i128, fee: i128, user_data: Bytes);
}
