//! Multi-Collateral Stablecoin Debt Ceiling Controller
//!
//! This crate provides a Soroban contract that tracks the total outstanding
//! debt minted per collateral type and enforces per-collateral debt ceilings.
//!
//! The contract exposes administrative functions to configure ceilings and
//! governance functions that are time-locked. Minting engines call into the
//! controller to enforce blocking checks before issuing new debt.

#![no_std]
#![allow(clippy::module_name_repetitions)]

use soroban_sdk::{contract, contractimpl, Address, Env};

mod controller;

#[cfg(test)]
mod test;

pub use controller::{CollateralConfig, DataKey, Error};

/// Public contract interface for the debt ceiling controller.
#[contract]
pub struct DebtCeilingController;

#[contractimpl]
impl DebtCeilingController {
    /// Initialize the controller with an administrator and governor.
    ///
    /// # Arguments
    /// * `admin` - Address allowed to perform immediate administrative actions.
    /// * `governor` - Address allowed to queue time-locked ceiling changes.
    ///
    /// # Errors
    /// Returns [`Error::AlreadyInitialized`] if the contract has already been
    /// initialized.
    pub fn initialize(env: Env, admin: Address, governor: Address) -> Result<(), Error> {
        controller::initialize(&env, &admin, &governor)
    }

    /// Return the current administrator address.
    pub fn admin(env: Env) -> Result<Address, Error> {
        controller::admin(&env)
    }

    /// Return the current governor address.
    pub fn governor(env: Env) -> Result<Address, Error> {
        controller::governor(&env)
    }

    /// Transfer the administrator role to a new address.
    ///
    /// Only the current administrator may call this function.
    pub fn set_admin(env: Env, new_admin: Address) -> Result<(), Error> {
        controller::set_admin(&env, &new_admin)
    }

    /// Transfer the governor role to a new address.
    ///
    /// Only the current administrator may call this function.
    pub fn set_governor(env: Env, new_governor: Address) -> Result<(), Error> {
        controller::set_governor(&env, &new_governor)
    }

    /// Register a collateral type with an initial debt ceiling.
    ///
    /// Only the administrator may call this function.
    pub fn register_collateral(
        env: Env,
        collateral: Address,
        ceiling: i128,
    ) -> Result<(), Error> {
        controller::register_collateral(&env, &collateral, ceiling)
    }

    /// Return the configuration for a collateral type.
    pub fn collateral_config(env: Env, collateral: Address) -> Result<CollateralConfig, Error> {
        controller::collateral_config(&env, &collateral)
    }

    /// Return the total outstanding debt minted against a collateral type.
    pub fn total_debt(env: Env, collateral: Address) -> Result<i128, Error> {
        controller::total_debt(&env, &collateral)
    }

    /// Return the remaining debt capacity for a collateral type.
    pub fn remaining_capacity(env: Env, collateral: Address) -> Result<i128, Error> {
        controller::remaining_capacity(&env, &collateral)
    }

    /// Immediately set a collateral's debt ceiling.
    ///
    /// Only the administrator may call this function. For time-locked changes
    /// use [`Self::queue_ceiling_change`] and [`Self::execute_ceiling_change`].
    pub fn set_ceiling(env: Env, collateral: Address, ceiling: i128) -> Result<(), Error> {
        controller::set_ceiling(&env, &collateral, ceiling)
    }

    /// Queue a time-locked change to a collateral's debt ceiling.
    ///
    /// Only the governor may call this function. The change becomes executable
    /// after the configured delay has elapsed.
    pub fn queue_ceiling_change(
        env: Env,
        collateral: Address,
        ceiling: i128,
    ) -> Result<u64, Error> {
        controller::queue_ceiling_change(&env, &collateral, ceiling)
    }

    /// Execute a previously queued ceiling change once its delay has elapsed.
    ///
    /// Only the governor may call this function.
    pub fn execute_ceiling_change(env: Env, collateral: Address) -> Result<(), Error> {
        controller::execute_ceiling_change(&env, &collateral)
    }

    /// Cancel a queued ceiling change for a collateral type.
    ///
    /// Only the governor may call this function.
    pub fn cancel_ceiling_change(env: Env, collateral: Address) -> Result<(), Error> {
        controller::cancel_ceiling_change(&env, &collateral)
    }

    /// Return the pending (queued) ceiling change for a collateral type, if any.
    pub fn pending_ceiling_change(
        env: Env,
        collateral: Address,
    ) -> Result<Option<controller::PendingCeilingChange>, Error> {
        controller::pending_ceiling_change(&env, &collateral)
    }

    /// Enforce the debt ceiling when minting debt against a collateral type.
    ///
    /// Reverts with [`Error::CeilingExceeded`] if the requested mint would push
    /// the collateral's outstanding debt past its ceiling.
    pub fn enforce_mint(
        env: Env,
        collateral: Address,
        amount: i128,
    ) -> Result<(), Error> {
        controller::enforce_mint(&env, &collateral, amount)
    }

    /// Record debt that has been minted against a collateral type.
    ///
    /// This is the accounting counterpart to [`Self::enforce_mint`] and is
    /// intended to be called by the minting engine after a successful mint.
    pub fn record_mint(env: Env, collateral: Address, amount: i128) -> Result<(), Error> {
        controller::record_mint(&env, &collateral, amount)
    }

    /// Record debt that has been repaid against a collateral type.
    ///
    /// Reduces the collateral's outstanding debt, freeing up ceiling capacity.
    pub fn record_repay(env: Env, collateral: Address, amount: i128) -> Result<(), Error> {
        controller::record_repay(&env, &collateral, amount)
    }
}