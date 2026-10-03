//! Flash Loan Liquidity Pool
//!
//! A trustless, multi-asset flash loan vault for Soroban.  Arbitrageurs can
//! borrow any pooled asset for the duration of a single transaction with zero
//! collateral.  If the borrowed amount plus the dynamic fee is not returned
//! before the current call-frame exits the entire transaction reverts —
//! guaranteeing that no capital can drain from the pool.
//!
//! # Architecture
//!
//! ```text
//!  ┌─────────────────────────────────────────────────────┐
//!  │                 FlashLoanPool (this)                │
//!  │                                                     │
//!  │  flash_loan(token, amount, receiver, user_data)     │
//!  │    1. snapshot pool balance                         │
//!  │    2. set reentrancy lock                           │
//!  │    3. transfer `amount` → receiver                  │
//!  │    4. invoke receiver.execute_operation(...)        │
//!  │    5. clear reentrancy lock                         │
//!  │    6. assert new balance ≥ snapshot + fee           │
//!  └─────────────────────────────────────────────────────┘
//! ```
//!
//! # Modules
//! - `execution` — core flash-loan execution logic and fee math
//! - `test`      — comprehensive unit tests (cfg(test))

#![no_std]

pub mod execution;
#[cfg(test)]
mod test;

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype,
    token::Client as TokenClient,
    Address, Env, Symbol,
};

// ---------------------------------------------------------------------------
// Error catalogue
// ---------------------------------------------------------------------------

/// All errors that can be returned by the flash-loan pool.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    /// Pool has already been initialised.
    AlreadyInitialized = 1,
    /// A required initialisation step has not been completed.
    NotInitialized = 2,
    /// Caller does not have the required admin role.
    Unauthorized = 3,
    /// The pool does not hold enough of the requested asset.
    InsufficientLiquidity = 4,
    /// The receiver did not return the principal + fee in full.
    RepaymentDeficit = 5,
    /// A reentrant call was detected during the receiver callback phase.
    ReentrantCall = 6,
    /// Requested amount must be strictly positive.
    InvalidAmount = 7,
    /// Arithmetic overflow during fee or balance calculation.
    ArithmeticOverflow = 8,
    /// The liquidity amount specified for deposit is zero or negative.
    InvalidDeposit = 9,
    /// The withdrawal amount exceeds the available pool balance.
    InsufficientBalance = 10,
    /// Fee basis-points value out of the allowed range (0..=10_000).
    InvalidFeeBps = 11,
}

// ---------------------------------------------------------------------------
// Storage key schema
// ---------------------------------------------------------------------------

/// Persistent storage keys used by the pool.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// Privileged account that may adjust pool parameters.
    Admin,
    /// Whether the pool has been initialised.
    Initialized,
    /// Base fee in basis-points (1 bps = 0.01 %).  Stored as u32.
    BaseFeeBps,
    /// Pool liquidity deposited for a specific asset token.
    PoolBalance(Address),
    /// Whether a flash-loan is currently in-flight for a given token.
    /// Used as the per-asset reentrancy lock.
    LoanActive(Address),
    /// Total volume ever borrowed for a specific token (for analytics).
    TotalBorrowed(Address),
    /// Total fees ever collected for a specific token.
    TotalFeesCollected(Address),
}

// ---------------------------------------------------------------------------
// Contract
// ---------------------------------------------------------------------------

#[contract]
pub struct FlashLoanPool;

#[contractimpl]
impl FlashLoanPool {
    // -----------------------------------------------------------------------
    // Lifecycle
    // -----------------------------------------------------------------------

    /// Initialise the pool.
    ///
    /// - `admin`        — address that owns administrative actions.
    /// - `base_fee_bps` — starting fee charged as basis-points of the loan
    ///                    amount (e.g. 9 = 0.09 %).  Must be ≤ 10 000.
    ///
    /// Can only be called once.
    pub fn initialize(env: Env, admin: Address, base_fee_bps: u32) -> Result<(), Error> {
        if env
            .storage()
            .instance()
            .get::<_, bool>(&DataKey::Initialized)
            .unwrap_or(false)
        {
            return Err(Error::AlreadyInitialized);
        }
        if base_fee_bps > 10_000 {
            return Err(Error::InvalidFeeBps);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::Initialized, &true);
        env.storage()
            .instance()
            .set(&DataKey::BaseFeeBps, &base_fee_bps);
        env.events().publish(
            (Symbol::new(&env, "initialized"),),
            (admin, base_fee_bps),
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Liquidity management
    // -----------------------------------------------------------------------

    /// Deposit `amount` of `token` into the pool.
    ///
    /// Anyone may deposit; this increases available liquidity and reduces the
    /// dynamic fee charged to future borrowers.
    pub fn deposit(env: Env, depositor: Address, token: Address, amount: i128) -> Result<(), Error> {
        Self::require_initialized(&env)?;
        if amount <= 0 {
            return Err(Error::InvalidDeposit);
        }
        depositor.require_auth();

        // Transfer from depositor into this contract.
        TokenClient::new(&env, &token).transfer(
            &depositor,
            env.current_contract_address(),
            &amount,
        );

        // Update tracked pool balance.
        let prev = Self::pool_balance(&env, &token);
        let next = prev
            .checked_add(amount)
            .ok_or(Error::ArithmeticOverflow)?;
        env.storage()
            .persistent()
            .set(&DataKey::PoolBalance(token.clone()), &next);

        env.events().publish(
            (Symbol::new(&env, "deposited"), token),
            (depositor, amount),
        );
        Ok(())
    }

    /// Withdraw `amount` of `token` from the pool.
    ///
    /// Only the admin may withdraw to prevent capital races against in-flight
    /// loans.  A proper LP-share scheme would sit on top of this primitive.
    pub fn withdraw(
        env: Env,
        caller: Address,
        token: Address,
        amount: i128,
        recipient: Address,
    ) -> Result<(), Error> {
        Self::require_admin(&env, &caller)?;
        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let balance = Self::pool_balance(&env, &token);
        if balance < amount {
            return Err(Error::InsufficientBalance);
        }

        let new_balance = balance
            .checked_sub(amount)
            .ok_or(Error::ArithmeticOverflow)?;
        env.storage()
            .persistent()
            .set(&DataKey::PoolBalance(token.clone()), &new_balance);

        TokenClient::new(&env, &token).transfer(
            &env.current_contract_address(),
            &recipient,
            &amount,
        );

        env.events().publish(
            (Symbol::new(&env, "withdrawn"), token),
            (recipient, amount),
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Flash Loan
    // -----------------------------------------------------------------------

    /// Execute a flash loan.
    ///
    /// Transfers `amount` of `token` to `receiver`, invokes
    /// `receiver.execute_operation(token, amount, fee, user_data)`, then
    /// asserts that the pool balance has increased by at least `fee`.
    ///
    /// The fee is dynamic: lower pool utilisation → lower fee.  See
    /// [`execution::compute_fee`] for the curve.
    ///
    /// # Reentrancy
    /// A per-token lock is set before the external call and cleared
    /// immediately after.  Any attempt by the receiver to re-enter
    /// `flash_loan` for the same token while the lock is active will
    /// return [`Error::ReentrantCall`].
    pub fn flash_loan(
        env: Env,
        token: Address,
        amount: i128,
        receiver: Address,
        user_data: soroban_sdk::Bytes,
    ) -> Result<i128, Error> {
        Self::require_initialized(&env)?;
        execution::execute_flash_loan(&env, token, amount, receiver, user_data)
    }

    // -----------------------------------------------------------------------
    // Admin
    // -----------------------------------------------------------------------

    /// Update the base fee in basis-points.
    pub fn set_base_fee(env: Env, caller: Address, new_fee_bps: u32) -> Result<(), Error> {
        Self::require_admin(&env, &caller)?;
        if new_fee_bps > 10_000 {
            return Err(Error::InvalidFeeBps);
        }
        env.storage()
            .instance()
            .set(&DataKey::BaseFeeBps, &new_fee_bps);
        env.events()
            .publish((Symbol::new(&env, "fee_updated"),), new_fee_bps);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // View / query
    // -----------------------------------------------------------------------

    /// Return the tracked pool balance for `token`.
    pub fn get_pool_balance(env: Env, token: Address) -> i128 {
        Self::pool_balance(&env, &token)
    }

    /// Return the current base fee in basis-points.
    pub fn get_base_fee_bps(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::BaseFeeBps)
            .unwrap_or(0)
    }

    /// Compute and return the fee that would be charged for `amount` of
    /// `token` right now (without executing a loan).
    pub fn quote_fee(env: Env, token: Address, amount: i128) -> Result<i128, Error> {
        let pool_balance = Self::pool_balance(&env, &token);
        let base_fee_bps: u32 = env
            .storage()
            .instance()
            .get(&DataKey::BaseFeeBps)
            .unwrap_or(0);
        execution::compute_fee(amount, pool_balance, base_fee_bps)
    }

    /// Return total volume borrowed for `token` across all flash loans.
    pub fn get_total_borrowed(env: Env, token: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::TotalBorrowed(token))
            .unwrap_or(0i128)
    }

    /// Return total fees collected for `token`.
    pub fn get_total_fees_collected(env: Env, token: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::TotalFeesCollected(token))
            .unwrap_or(0i128)
    }

    /// Check whether the per-asset reentrancy lock is currently set.
    pub fn is_loan_active(env: Env, token: Address) -> bool {
        env.storage()
            .instance()
            .get::<_, bool>(&DataKey::LoanActive(token))
            .unwrap_or(false)
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    pub(crate) fn pool_balance(env: &Env, token: &Address) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::PoolBalance(token.clone()))
            .unwrap_or(0i128)
    }

    pub(crate) fn require_initialized(env: &Env) -> Result<(), Error> {
        if !env
            .storage()
            .instance()
            .get::<_, bool>(&DataKey::Initialized)
            .unwrap_or(false)
        {
            return Err(Error::NotInitialized);
        }
        Ok(())
    }

    pub(crate) fn require_admin(env: &Env, caller: &Address) -> Result<(), Error> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)?;
        if caller != &admin {
            return Err(Error::Unauthorized);
        }
        caller.require_auth();
        Ok(())
    }
}
