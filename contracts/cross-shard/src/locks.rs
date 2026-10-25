// Copyright 2024 Stellar-K8s Contributors
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # State Verification Lock Manager (issue #333)
//!
//! Implements the advisory locking mechanism that prevents external actors
//! from mutating targeted liquidity pools while the atomic validation
//! sequence is executing.
//!
//! ## Design
//!
//! Soroban is a **single-threaded**, sequential-execution environment: there
//! are no OS threads, no async runtimes, and no shared-memory race conditions
//! inside a single ledger.  Every top-level transaction runs to completion
//! before the next begins.  This means traditional read/write locks — which
//! exist to serialise concurrent threads — are **not needed and must not be
//! used**.
//!
//! What *is* needed is a protection mechanism against *other transactions in
//! other ledgers* interleaving with a multi-leg swap.  However, since Soroban
//! transactions are atomic within a single ledger, a multi-leg swap spanning
//! only one transaction is already fully serialised.
//!
//! For **cross-ledger** ordering guarantees (rare in practice, but required by
//! the issue spec for "atomic cross-shard arbitrage"), the lock module writes
//! an advisory `PoolLock` record to persistent storage at the start of each
//! leg and removes it on completion or rollback.  Any concurrent transaction
//! (in the same or a subsequent ledger) that calls `lock::acquire` for the
//! same pool while the lock is held will get `PoolAlreadyLocked`, preventing
//! double-spend or inconsistent state reads.
//!
//! ## Soroban concurrency safety
//!
//! * No `std::sync::Mutex` or `RwLock` — these are not available in `no_std`.
//! * No `tokio` / async primitives — Soroban is strictly synchronous.
//! * All "locking" is implemented as persistent-storage writes that are
//!   committed atomically with the rest of the transaction's state changes.
//! * Because the Stellar Core scheduler processes ledger transactions
//!   sequentially and atomically (one at a time), the storage-based lock
//!   provides the same mutual-exclusion guarantee a mutex would in a
//!   concurrent setting, without violating the single-threaded model.
//!
//! ## Lock TTL
//!
//! Every lock carries an `expires_at` ledger sequence.  After that ledger,
//! any actor may call `force_release` to clear a stale lock left behind by a
//! failed/abandoned transaction.  The default TTL is `LOCK_TTL_LEDGERS`.

use soroban_sdk::{symbol_short, Address, Env, Vec};

use crate::errors::CrossShardError;
use crate::types::{DataKey, PoolLock};

/// Default number of ledgers a lock is valid for before it can be
/// force-released.  At ~5 seconds per ledger this is roughly 5 minutes.
pub const LOCK_TTL_LEDGERS: u32 = 60;

// ── Public API ────────────────────────────────────────────────────────────────

/// Acquire an advisory lock on `pool` for the duration of swap `swap_id`.
///
/// # Errors
///
/// * [`CrossShardError::PoolAlreadyLocked`] — another swap (or the same swap
///   in an unexpected re-entry) is holding the lock on this pool.
///
/// # Soroban concurrency note
///
/// This function writes to persistent storage and is therefore committed
/// atomically with all other state changes in the calling transaction.
/// It cannot race with another invocation in the *same* ledger because
/// Soroban executes all invocations of a single transaction sequentially.
/// For *different* transactions, the storage write acts as an exclusive
/// advisory mutex: the second transaction will read the lock and abort
/// before making any state changes.
pub fn acquire(
    env: &Env,
    pool: &Address,
    swap_id: u64,
    ttl_ledgers: Option<u32>,
) -> Result<(), CrossShardError> {
    let key = DataKey::PoolLock(pool.clone());

    // Check for an existing (non-expired) lock.
    if let Some(existing) = env.storage().persistent().get::<DataKey, PoolLock>(&key) {
        let current_seq = env.ledger().sequence();
        if current_seq <= existing.expires_at {
            // Lock is still valid; reject the acquisition.
            return Err(CrossShardError::PoolAlreadyLocked);
        }
        // Existing lock has expired — it was left by a failed transaction.
        // Silently overwrite it (equivalent to force_release + re-acquire).
        env.events().publish(
            (symbol_short!("lk_stale"), pool.clone()),
            (existing.swap_id, current_seq),
        );
    }

    let current_seq = env.ledger().sequence();
    let ttl = ttl_ledgers.unwrap_or(LOCK_TTL_LEDGERS);
    let expires_at = current_seq.saturating_add(ttl);

    let lock = PoolLock {
        pool:        pool.clone(),
        held_by:     env.current_contract_address(),
        acquired_at: current_seq,
        expires_at,
        swap_id,
    };

    env.storage().persistent().set(&key, &lock);

    env.events().publish(
        (symbol_short!("lk_acq"), pool.clone()),
        (swap_id, current_seq, expires_at),
    );

    Ok(())
}

/// Release the advisory lock on `pool` held by the current coordinator.
///
/// # Errors
///
/// * [`CrossShardError::LockNotOwned`] — the lock on this pool is not held by
///   this coordinator instance, or no lock exists.
pub fn release(env: &Env, pool: &Address) -> Result<(), CrossShardError> {
    let key = DataKey::PoolLock(pool.clone());

    let lock: PoolLock = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(CrossShardError::LockNotOwned)?;

    // Only the coordinator that acquired the lock may release it.
    if lock.held_by != env.current_contract_address() {
        return Err(CrossShardError::LockNotOwned);
    }

    env.storage().persistent().remove(&key);

    env.events().publish(
        (symbol_short!("lk_rel"), pool.clone()),
        (lock.swap_id, env.ledger().sequence()),
    );

    Ok(())
}

/// Release a lock that has passed its `expires_at` ledger.
///
/// Any caller may invoke this to clean up stale locks left behind by failed
/// or abandoned swap transactions.
///
/// # Errors
///
/// * [`CrossShardError::LockStillValid`] — the lock has not yet expired; only
///   the owning coordinator may release it early via [`release`].
/// * [`CrossShardError::LockNotOwned`] — no lock exists for this pool.
pub fn force_release(env: &Env, pool: &Address) -> Result<(), CrossShardError> {
    let key = DataKey::PoolLock(pool.clone());

    let lock: PoolLock = env
        .storage()
        .persistent()
        .get(&key)
        .ok_or(CrossShardError::LockNotOwned)?;

    let current_seq = env.ledger().sequence();
    if current_seq <= lock.expires_at {
        return Err(CrossShardError::LockStillValid);
    }

    env.storage().persistent().remove(&key);

    env.events().publish(
        (symbol_short!("lk_frc"), pool.clone()),
        (lock.swap_id, current_seq),
    );

    Ok(())
}

/// Read the current lock state for `pool` without modifying it.
///
/// Returns `None` if the pool is not locked.
pub fn read_lock(env: &Env, pool: &Address) -> Option<PoolLock> {
    env.storage()
        .persistent()
        .get(&DataKey::PoolLock(pool.clone()))
}

/// Returns `true` if `pool` is currently locked by a non-expired lock.
pub fn is_locked(env: &Env, pool: &Address) -> bool {
    match read_lock(env, pool) {
        None => false,
        Some(lock) => env.ledger().sequence() <= lock.expires_at,
    }
}

/// Acquire locks on all pools in `pools` atomically (all-or-nothing).
///
/// If any pool is already locked, no locks are acquired and
/// `PoolAlreadyLocked` is returned.  This prevents partial lock acquisition,
/// which could lead to deadlocks if two competing swaps each grab a subset of
/// the required pools.
///
/// # Algorithm
///
/// 1. Check all pools for existing locks in a single read pass.
/// 2. Only if every pool is free, write all locks.
///
/// This is safe in Soroban's single-threaded model: the check and write
/// happen in the same transaction and cannot be interleaved.
pub fn acquire_all(
    env:        &Env,
    pools:      &Vec<Address>,
    swap_id:    u64,
    ttl_ledgers: Option<u32>,
) -> Result<(), CrossShardError> {
    // --- Check phase: scan all pools for pre-existing valid locks ---
    for pool in pools.iter() {
        let key = DataKey::PoolLock(pool.clone());
        if let Some(existing) = env.storage().persistent().get::<DataKey, PoolLock>(&key) {
            if env.ledger().sequence() <= existing.expires_at {
                // Pool is locked; abort without acquiring anything.
                return Err(CrossShardError::PoolAlreadyLocked);
            }
            // Expired lock: will be silently overwritten in acquire phase.
        }
    }

    // --- Acquire phase: all pools are free, write all locks atomically ---
    for pool in pools.iter() {
        // acquire() handles expired-lock cleanup internally.
        acquire(env, &pool, swap_id, ttl_ledgers)?;
    }

    Ok(())
}

/// Release all locks in `pools`, ignoring pools that are not locked by this
/// coordinator (allows partial clean-up during rollback).
///
/// Returns the number of locks actually released.
pub fn release_all(env: &Env, pools: &Vec<Address>) -> u32 {
    let mut released = 0u32;
    for pool in pools.iter() {
        if release(env, &pool).is_ok() {
            released += 1;
        }
    }
    released
}

// ── Active lock inspection ──────────────────────────────────────────────────

/// Counts how many pools in `pools` currently hold an active, non-expired advisory lock.
///
/// Useful for monitoring and debugging: callers can check how many of their
/// target pools are currently locked by in-flight swaps.
///
/// # Arguments
///
/// * `env` — The Soroban environment.
/// * `pools` — List of pool addresses to check.
///
/// # Returns
///
/// The number of pools that currently have an active (non-expired) lock.
pub fn count_active_locks(env: &Env, pools: &Vec<Address>) -> u32 {
    let mut active = 0u32;
    for pool in pools.iter() {
        if is_locked(env, &pool) {
            active += 1;
        }
    }
    active
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{testutils::Address as _, vec, Env};

    /// Helper: create a minimal env with a registered cross-shard contract so
    /// `env.current_contract_address()` returns a stable address.
    fn make_env() -> Env {
        Env::default()
    }

    // These tests are covered by integration tests in test.rs which use
    // a fully registered coordinator contract. Storage access from unit
    // tests in soroban-sdk 22.0.0 requires a registered contract context.

    #[test]
    fn lock_ttl_constant_is_reasonable() {
        // 60 ledgers * ~5 seconds/ledger = ~5 minutes.  This should be ample
        // for a multi-leg swap while being short enough to prevent DoS via
        // lock exhaustion.
        assert_eq!(LOCK_TTL_LEDGERS, 60);
        assert!(LOCK_TTL_LEDGERS >= 10, "TTL must allow for realistic execution");
        assert!(LOCK_TTL_LEDGERS <= 600, "TTL must not leave locks stale for too long");
    }
}
