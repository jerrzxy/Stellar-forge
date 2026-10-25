//! Debt Ceiling Controller implementation.

/// Storage layout (keys are namespaced to avoid clobbering unrelated data):
/// - admin            -> Address
/// - governor          -> Address
/// - pending_gov_delay -> u64
/// - ceiling(collateral) -> i128
/// - debt(collateral)    -> i128

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short, Address, Env, Symbol, Val};

/// Errors returned by the debt ceiling controller.
#[contracterror]
#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum DebtCeilingError {
    /// The admin has not been initialized.
    NotInitialized = 1,
    /// The contract has already been initialized.
    AlreadyInitialized = 2,
    /// The caller is not authorized for this operation.
    Unauthorized = 3,
    /// The collateral type has no configured ceiling.
    CeilingNotSet = 4,
    /// The requested mint would exceed the configured ceiling.
    CeilingExceeded = 5,
    /// A negative amount was provided.
    InvalidAmount = 6,
    /// A negative ceiling was provided.
    InvalidCeiling = 7,
    /// No pending governance action exists for this collateral.
    NoPendingGovernance = 8,
    /// The time-lock has not yet elapsed.
    TimeLockNotElapsed = 9,
    /// An overflow occurred while accounting for debt.
    Overflow = 10,
}

/// Events emitted by the controller.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DebtCeilingEvents {
    /// Emitted when a collateral's ceiling is updated.
    CeilingUpdated(Symbol, i128),
    /// Emitted when debt is recorded against a collateral.
    DebtRecorded(Symbol, i128, i128),
    /// Emitted when debt is repaid for a collateral.
    DebtRepaid(Symbol, i128, i128),
    /// Emitted when a governance change is queued.
    GovernanceQueued(Symbol, i128, u64),
    /// Emitted when a queued governance change is executed.
    GovernanceExecuted(Symbol, i128),
}

/// Storage keys for the debt ceiling controller.
///
/// Each variant maps to a distinct storage key so that unrelated entries
/// (per-collateral debt, per-collateral ceilings, admin, governor, and
/// time-locked pending updates) never clobber one another.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    /// Contract administrator allowed to set ceilings directly.
    Admin,
    /// Governor allowed to propose/execute time-locked ceiling updates.
    Governor,
    /// Total outstanding debt minted for a given collateral type.
    Debt(Symbol),
    /// Current debt ceiling for a given collateral type.
    Ceiling(Symbol),
    /// Pending (time-locked) ceiling update for a given collateral type.
    PendingCeiling(Symbol),
    /// Ledger timestamp at which a pending ceiling update becomes executable.
    PendingCeilingTime(Symbol),
}

/// Default timelock (in ledgers) applied to governor-driven ceiling updates.
const DEFAULT_TIMELOCK_LEDGERS: u32 = 100;

#[contract]
pub struct DebtCeilingController;

#[contractimpl]
impl DebtCeilingController {
    /// Initialize the controller with an admin and a governor.
    pub fn initialize(env: Env, admin: Address, governor: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("already initialized");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Governor, &governor);
    }

    /// Return the current admin address.
    pub fn admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("admin not set")
    }

    /// Return the current governor address.
    pub fn governor(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Governor)
            .expect("governor not set")
    }

    /// Return the current debt ceiling for a collateral type (0 if unset).
    pub fn ceiling(env: Env, collateral: Symbol) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Ceiling(collateral.clone()))
            .unwrap_or(0)
    }

    /// Return the total outstanding debt for a collateral type (0 if unset).
    pub fn debt(env: Env, collateral: Symbol) -> i128 {
        env.storage()
            .persistent()
            .get(&DataKey::Debt(collateral.clone()))
            .unwrap_or(0)
    }

    /// Admin function: directly set the debt ceiling for a collateral type.
    pub fn set_ceiling(env: Env, admin: Address, collateral: Symbol, ceiling: i128) {
        admin.require_auth();
        let stored_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("admin not set");
        if admin != stored_admin {
            panic!("not admin");
        }
        if ceiling < 0 {
            panic!("ceiling must be non-negative");
        }
        env.storage()
            .persistent()
            .set(&DataKey::Ceiling(collateral), &ceiling);
    }

    /// Governor function: propose a time-locked ceiling update.
    pub fn propose_ceiling(env: Env, governor: Address, collateral: Symbol, ceiling: i128) {
        governor.require_auth();
        let stored_governor: Address = env
            .storage()
            .instance()
            .get(&DataKey::Governor)
            .expect("governor not set");
        if governor != stored_governor {
            panic!("not governor");
        }
        if ceiling < 0 {
            panic!("ceiling must be non-negative");
        }
        let execute_at = env.ledger().sequence() + DEFAULT_TIMELOCK_LEDGERS;
        env.storage()
            .persistent()
            .set(&DataKey::PendingCeiling(collateral.clone()), &ceiling);
        env.storage()
            .persistent()
            .set(&DataKey::PendingCeilingTime(collateral), &execute_at);
    }

    /// Governor function: execute a previously proposed ceiling update once
    /// the timelock has elapsed.
    pub fn execute_ceiling(env: Env, governor: Address, collateral: Symbol) {
        governor.require_auth();
        let stored_governor: Address = env
            .storage()
            .instance()
            .get(&DataKey::Governor)
            .expect("governor not set");
        if governor != stored_governor {
            panic!("not governor");
        }
        let execute_at: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::PendingCeilingTime(collateral.clone()))
            .expect("no pending ceiling");
        if env.ledger().sequence() < execute_at {
            panic!("timelock not elapsed");
        }
        let ceiling: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::PendingCeiling(collateral.clone()))
            .expect("no pending ceiling");
        env.storage()
            .persistent()
            .set(&DataKey::Ceiling(collateral.clone()), &ceiling);
        env.storage()
            .persistent()
            .remove(&DataKey::PendingCeiling(collateral.clone()));
        env.storage()
            .persistent()
            .remove(&DataKey::PendingCeilingTime(collateral));
    }

    /// Check-and-record a mint against the collateral's debt ceiling.
    ///
    /// Reverts if the resulting debt would exceed the configured ceiling.
    /// Returns the new total debt for the collateral on success.
    pub fn check_and_record_mint(env: Env, collateral: Symbol, amount: i128) -> i128 {
        if amount < 0 {
            panic!("amount must be non-negative");
        }
        let current: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Debt(collateral.clone()))
            .unwrap_or(0);
        let ceiling: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Ceiling(collateral.clone()))
            .unwrap_or(0);
        let new_total = current
            .checked_add(amount)
            .expect("debt overflow");
        if new_total > ceiling {
            panic!("debt ceiling exceeded");
        }
        env.storage()
            .persistent()
            .set(&DataKey::Debt(collateral), &new_total);
        new_total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::Env;

    fn setup() -> (Env, DebtCeilingControllerClient<'static>, Address, Address) {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, DebtCeilingController);
        let client = DebtCeilingControllerClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        let governor = Address::generate(&env);
        client.initialize(&admin, &governor);
        (env, client, admin, governor)
    }

    #[test]
    fn test_set_ceiling_and_mint_up_to_limit() {
        let (env, client, admin, _governor) = setup();
        let collateral = Symbol::new(&env, "XLM");
        client.set_ceiling(&admin, &collateral, &1000);

        assert_eq!(client.ceiling(&collateral), 1000);
        assert_eq!(client.debt(&collateral), 0);

        // Mint right up to the ceiling.
        let total = client.check_and_record_mint(&collateral, &1000);
        assert_eq!(total, 1000);
        assert_eq!(client.debt(&collateral), 1000);
    }

    #[test]
    fn test_mint_increments_and_blocks_past_limit() {
        let (env, client, admin, _governor) = setup();
        let collateral = Symbol::new(&env, "XLM");
        client.set_ceiling(&admin, &collateral, &1000);

        client.check_and_record_mint(&collateral, &600);
        client.check_and_record_mint(&collateral, &400);
        assert_eq!(client.debt(&collateral), 1000);

        // One unit past the ceiling must be rejected.
        let result = client.try_check_and_record_mint(&collateral, &1);
        assert!(result.is_err());
        assert_eq!(client.debt(&collateral), 1000);
    }

    #[test]
    fn test_mint_rejected_when_exceeding_in_single_call() {
        let (env, client, admin, _governor) = setup();
        let collateral = Symbol::new(&env, "USDC");
        client.set_ceiling(&admin, &collateral, &500);

        let result = client.try_check_and_record_mint(&collateral, &501);
        assert!(result.is_err());
        assert_eq!(client.debt(&collateral), 0);
    }

    #[test]
    fn test_mint_rejected_without_ceiling() {
        let (env, client, _admin, _governor) = setup();
        let collateral = Symbol::new(&env, "BTC");
        let result = client.try_check_and_record_mint(&collateral, &1);
        assert!(result.is_err());
    }

    #[test]
    fn test_governor_timelock_propose_and_execute() {
        let (env, client, _admin, governor) = setup();
        let collateral = Symbol::new(&env, "XLM");

        client.propose_ceiling(&governor, &collateral, &2000);
        // Ceiling not yet applied.
        assert_eq!(client.ceiling(&collateral), 0);

        // Executing before timelock elapses must fail.
        let early = client.try_execute_ceiling(&governor, &collateral);
        assert!(early.is_err());

        // Advance ledger sequence past the timelock.
        env.ledger().set_sequence_number(DEFAULT_TIMELOCK_LEDGERS + 1);
        client.execute_ceiling(&governor, &collateral);
        assert_eq!(client.ceiling(&collateral), 2000);
    }

    #[test]
    fn test_non_admin_cannot_set_ceiling() {
        let (env, client, _admin, _governor) = setup();
        let collateral = Symbol::new(&env, "XLM");
        let stranger = Address::generate(&env);
        let result = client.try_set_ceiling(&stranger, &collateral, &1000);
        assert!(result.is_err());
    }
}