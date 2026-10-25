//! Comprehensive tests for the Flash Loan Liquidity Pool.
//!
//! Test coverage:
//!  1. Pool initialisation and parameter validation.
//!  2. Deposit and withdraw liquidity.
//!  3. Successful profitable arbitrage flash loan.
//!  4. Unprofitable / under-repayment flash loan reverts.
//!  5. Reentrancy attempt is blocked.
//!  6. Fee computation correctness.
//!  7. Quote-fee view function.
//!  8. Uninitialised pool rejects calls.
//!  9. Zero and negative amount guards.
//! 10. Admin-only operations rejected for non-admin callers.

#![cfg(test)]

extern crate std;

use soroban_sdk::{
    contract, contractimpl, contracttype,
    testutils::Address as _,
    token::{Client as TokenClient, StellarAssetClient},
    Address, Bytes, Env,
};

use crate::{Error, FlashLoanPool, FlashLoanPoolClient};

// ---------------------------------------------------------------------------
// Helper: create a SAC token for testing
// ---------------------------------------------------------------------------

fn create_token<'a>(
    env: &'a Env,
    admin: &Address,
) -> (Address, TokenClient<'a>, StellarAssetClient<'a>) {
    let token_id = env.register_stellar_asset_contract_v2(admin.clone());
    let addr = token_id.address();
    let client = TokenClient::new(env, &addr);
    let sac = StellarAssetClient::new(env, &addr);
    (addr, client, sac)
}

// ---------------------------------------------------------------------------
// Profitable receiver — transfers principal + fee back to the pool.
// The pool address is stored in contract storage during `init`.
// ---------------------------------------------------------------------------

#[contracttype]
enum ProfKey {
    Pool,
}

#[contract]
pub struct ProfitableReceiver;

#[contractimpl]
impl ProfitableReceiver {
    /// Store the pool address so execute_operation can send funds there.
    pub fn init(env: Env, pool: Address) {
        env.storage().instance().set(&ProfKey::Pool, &pool);
    }

    /// Flash-loan callback: repay principal + fee.
    pub fn execute_operation(env: Env, token: Address, amount: i128, fee: i128, _data: Bytes) {
        let pool: Address = env.storage().instance().get(&ProfKey::Pool).unwrap();
        let repay = amount + fee;
        let me = env.current_contract_address();
        TokenClient::new(&env, &token).transfer(&me, &pool, &repay);
    }
}

// ---------------------------------------------------------------------------
// Unprofitable receiver — repays only the principal, skips the fee.
// ---------------------------------------------------------------------------

#[contracttype]
enum UnprofKey {
    Pool,
}

#[contract]
pub struct UnprofitableReceiver;

#[contractimpl]
impl UnprofitableReceiver {
    pub fn init(env: Env, pool: Address) {
        env.storage().instance().set(&UnprofKey::Pool, &pool);
    }

    /// Repays only the principal — no fee → pool revert expected.
    pub fn execute_operation(env: Env, token: Address, amount: i128, _fee: i128, _data: Bytes) {
        let pool: Address = env.storage().instance().get(&UnprofKey::Pool).unwrap();
        let me = env.current_contract_address();
        TokenClient::new(&env, &token).transfer(&me, &pool, &amount);
    }
}

#[contracttype]
enum PartialKey {
    Pool,
}

#[contract]
pub struct PartialReceiver;

#[contractimpl]
impl PartialReceiver {
    pub fn init(env: Env, pool: Address) {
        env.storage().instance().set(&PartialKey::Pool, &pool);
    }

    pub fn execute_operation(env: Env, token: Address, amount: i128, _fee: i128, _data: Bytes) {
        let pool: Address = env.storage().instance().get(&PartialKey::Pool).unwrap();
        let me = env.current_contract_address();
        TokenClient::new(&env, &token).transfer(&me, &pool, &(amount - 1));
    }
}

// ---------------------------------------------------------------------------
// Reentrant receiver — attempts a second flash_loan call mid-execution.
// ---------------------------------------------------------------------------

#[contracttype]
enum ReentKey {
    Pool,
}

#[contract]
pub struct ReentrantReceiver;

#[contractimpl]
impl ReentrantReceiver {
    pub fn init(env: Env, pool: Address) {
        env.storage().instance().set(&ReentKey::Pool, &pool);
    }

    /// Tries a reentrant flash_loan, then repays the outer loan.
    pub fn execute_operation(env: Env, token: Address, amount: i128, fee: i128, _data: Bytes) {
        let pool: Address = env.storage().instance().get(&ReentKey::Pool).unwrap();
        let pool_client = FlashLoanPoolClient::new(&env, &pool);

        // This reentrant call MUST fail — either because of our application-level
        // ReentrantCall guard or the Soroban host-level re-entry prevention.
        // Either way it must NOT succeed.
        let result = pool_client.try_flash_loan(
            &token,
            &1_i128,
            &env.current_contract_address(),
            &Bytes::new(&env),
        );
        assert!(result.is_err(), "reentrant call must not succeed");

        // Still repay the outer loan so the outer test can assert lock released.
        let pool2: Address = env.storage().instance().get(&ReentKey::Pool).unwrap();
        let me = env.current_contract_address();
        TokenClient::new(&env, &token).transfer(&me, &pool2, &(amount + fee));
    }
}

// ---------------------------------------------------------------------------
// Test fixture
// ---------------------------------------------------------------------------

struct TestContext {
    env: Env,
    admin: Address,
    pool_addr: Address,
    token_addr: Address,
}

impl TestContext {
    fn setup(base_fee_bps: u32) -> Self {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let (token_addr, _token, _asset) = create_token(&env, &admin);

        let pool_addr = env.register(FlashLoanPool, ());
        let pool = FlashLoanPoolClient::new(&env, &pool_addr);

        pool.initialize(&admin, &base_fee_bps);

        TestContext {
            env,
            admin,
            pool_addr,
            token_addr,
        }
    }

    fn pool(&self) -> FlashLoanPoolClient<'_> {
        FlashLoanPoolClient::new(&self.env, &self.pool_addr)
    }

    fn token(&self) -> TokenClient<'_> {
        TokenClient::new(&self.env, &self.token_addr)
    }

    fn asset(&self) -> StellarAssetClient<'_> {
        StellarAssetClient::new(&self.env, &self.token_addr)
    }

    fn fund_pool(&self, amount: i128) {
        self.asset().mint(&self.admin, &amount);
        self.pool().deposit(&self.admin, &self.token_addr, &amount);
    }
}

// ---------------------------------------------------------------------------
// 1. Initialisation
// ---------------------------------------------------------------------------

#[test]
fn test_initialize_sets_state() {
    let ctx = TestContext::setup(30);
    assert_eq!(ctx.pool().get_base_fee_bps(), 30u32);
    assert_eq!(ctx.pool().get_pool_balance(&ctx.token_addr), 0i128);
}

#[test]
fn test_double_initialize_returns_error() {
    let ctx = TestContext::setup(30);
    let result = ctx.pool().try_initialize(&ctx.admin, &30u32);
    assert_eq!(result, Err(Ok(Error::AlreadyInitialized)));
}

#[test]
fn test_invalid_fee_bps_rejected() {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let pool_addr = env.register(FlashLoanPool, ());
    let pool = FlashLoanPoolClient::new(&env, &pool_addr);
    let result = pool.try_initialize(&admin, &10_001u32);
    assert_eq!(result, Err(Ok(Error::InvalidFeeBps)));
}

// ---------------------------------------------------------------------------
// 2. Deposit and withdraw
// ---------------------------------------------------------------------------

#[test]
fn test_deposit_updates_balance() {
    let ctx = TestContext::setup(30);
    ctx.fund_pool(1_000_000);
    assert_eq!(ctx.pool().get_pool_balance(&ctx.token_addr), 1_000_000i128);
}

#[test]
fn test_withdraw_updates_balance() {
    let ctx = TestContext::setup(30);
    ctx.fund_pool(1_000_000);
    ctx.pool().withdraw(&ctx.admin, &ctx.token_addr, &400_000, &ctx.admin);
    assert_eq!(ctx.pool().get_pool_balance(&ctx.token_addr), 600_000i128);
    assert_eq!(ctx.token().balance(&ctx.admin), 400_000i128);
}

#[test]
fn test_withdraw_over_balance_fails() {
    let ctx = TestContext::setup(30);
    ctx.fund_pool(100_000);
    let result = ctx.pool().try_withdraw(&ctx.admin, &ctx.token_addr, &200_000, &ctx.admin);
    assert_eq!(result, Err(Ok(Error::InsufficientBalance)));
}

#[test]
fn test_non_admin_withdraw_fails() {
    let ctx = TestContext::setup(30);
    ctx.fund_pool(100_000);
    let rando = Address::generate(&ctx.env);
    let result = ctx.pool().try_withdraw(&rando, &ctx.token_addr, &1_000, &rando);
    assert_eq!(result, Err(Ok(Error::Unauthorized)));
}

// ---------------------------------------------------------------------------
// 3. Profitable flash loan — succeeds
// ---------------------------------------------------------------------------

#[test]
fn test_profitable_flash_loan_succeeds() {
    let ctx = TestContext::setup(30); // 0.30% base fee
    ctx.fund_pool(10_000_000);        // 10 M units

    let receiver_addr = ctx.env.register(ProfitableReceiver, ());
    let receiver_client = ProfitableReceiverClient::new(&ctx.env, &receiver_addr);
    receiver_client.init(&ctx.pool_addr);

    let borrow_amount: i128 = 1_000_000;
    let expected_fee = ctx.pool().quote_fee(&ctx.token_addr, &borrow_amount);
    assert!(expected_fee >= 1, "fee must be at least 1");

    // Mint enough for the receiver to repay principal + fee.
    ctx.asset().mint(&receiver_addr, &(borrow_amount + expected_fee));

    let actual_fee = ctx.pool().flash_loan(
        &ctx.token_addr,
        &borrow_amount,
        &receiver_addr,
        &Bytes::new(&ctx.env),
    );

    assert_eq!(actual_fee, expected_fee);
    assert_eq!(
        ctx.pool().get_pool_balance(&ctx.token_addr),
        10_000_000 + expected_fee
    );
    assert_eq!(ctx.pool().get_total_borrowed(&ctx.token_addr), borrow_amount);
    assert_eq!(ctx.pool().get_total_fees_collected(&ctx.token_addr), expected_fee);
}

// ---------------------------------------------------------------------------
// 4. Unprofitable flash loan — reverts
// ---------------------------------------------------------------------------

#[test]
fn test_unprofitable_flash_loan_reverts() {
    let ctx = TestContext::setup(30);
    ctx.fund_pool(10_000_000);

    let receiver_addr = ctx.env.register(UnprofitableReceiver, ());
    let receiver_client = UnprofitableReceiverClient::new(&ctx.env, &receiver_addr);
    receiver_client.init(&ctx.pool_addr);

    let borrow_amount: i128 = 500_000;
    // Give only the principal — receiver won't have fee funds.
    ctx.asset().mint(&receiver_addr, &borrow_amount);

    let result = ctx.pool().try_flash_loan(
        &ctx.token_addr,
        &borrow_amount,
        &receiver_addr,
        &Bytes::new(&ctx.env),
    );
    assert!(result.is_err(), "unprofitable loan must revert");
    assert_eq!(ctx.pool().get_pool_balance(&ctx.token_addr), 10_000_000);
    assert_eq!(ctx.token().balance(&ctx.pool_addr), 10_000_000);
    assert_eq!(ctx.pool().get_total_borrowed(&ctx.token_addr), 0);
    assert_eq!(ctx.pool().get_total_fees_collected(&ctx.token_addr), 0);
    assert!(!ctx.pool().is_loan_active(&ctx.token_addr));
}

#[test]
fn test_partial_principal_repayment_reverts_without_pool_loss() {
    let ctx = TestContext::setup(30);
    ctx.fund_pool(10_000_000);

    let receiver_addr = ctx.env.register(PartialReceiver, ());
    let receiver_client = PartialReceiverClient::new(&ctx.env, &receiver_addr);
    receiver_client.init(&ctx.pool_addr);

    let borrow_amount = 500_000;
    ctx.asset().mint(&receiver_addr, &borrow_amount);
    let result = ctx.pool().try_flash_loan(
        &ctx.token_addr,
        &borrow_amount,
        &receiver_addr,
        &Bytes::new(&ctx.env),
    );

    assert_eq!(result, Err(Ok(Error::RepaymentDeficit)));
    assert_eq!(ctx.pool().get_pool_balance(&ctx.token_addr), 10_000_000);
    assert_eq!(ctx.token().balance(&ctx.pool_addr), 10_000_000);
    assert_eq!(ctx.pool().get_total_borrowed(&ctx.token_addr), 0);
    assert_eq!(ctx.pool().get_total_fees_collected(&ctx.token_addr), 0);
    assert!(!ctx.pool().is_loan_active(&ctx.token_addr));
}

// ---------------------------------------------------------------------------
// 5. Reentrancy — lock behavior
// ---------------------------------------------------------------------------

#[test]
fn test_reentrancy_lock_is_cleared_after_successful_loan() {
    let ctx = TestContext::setup(30);
    ctx.fund_pool(10_000_000);

    let receiver_addr = ctx.env.register(ProfitableReceiver, ());
    let receiver_client = ProfitableReceiverClient::new(&ctx.env, &receiver_addr);
    receiver_client.init(&ctx.pool_addr);

    let borrow_amount: i128 = 100_000;
    let expected_fee = ctx.pool().quote_fee(&ctx.token_addr, &borrow_amount);
    ctx.asset().mint(&receiver_addr, &(borrow_amount + expected_fee));

    // Lock must not be set before the loan.
    assert!(!ctx.pool().is_loan_active(&ctx.token_addr));

    ctx.pool().flash_loan(
        &ctx.token_addr,
        &borrow_amount,
        &receiver_addr,
        &Bytes::new(&ctx.env),
    );

    // Lock must be cleared after successful loan.
    assert!(
        !ctx.pool().is_loan_active(&ctx.token_addr),
        "reentrancy lock must be released after successful loan"
    );
}

#[test]
fn test_malicious_receiver_cannot_reenter_flash_loan() {
    let ctx = TestContext::setup(30);
    ctx.fund_pool(10_000_000);

    let receiver_addr = ctx.env.register(ReentrantReceiver, ());
    let receiver_client = ReentrantReceiverClient::new(&ctx.env, &receiver_addr);
    receiver_client.init(&ctx.pool_addr);

    let borrow_amount = 100_000;
    let fee = ctx.pool().quote_fee(&ctx.token_addr, &borrow_amount);
    ctx.asset().mint(&receiver_addr, &(borrow_amount + fee));

    assert_eq!(
        ctx.pool().flash_loan(
            &ctx.token_addr,
            &borrow_amount,
            &receiver_addr,
            &Bytes::new(&ctx.env),
        ),
        fee
    );
    assert_eq!(
        ctx.pool().get_pool_balance(&ctx.token_addr),
        10_000_000 + fee
    );
    assert!(!ctx.pool().is_loan_active(&ctx.token_addr));
}

#[test]
fn test_reentrancy_guard_blocks_concurrent_loan_on_same_token() {
    // Directly test the guard: set the lock manually and verify flash_loan
    // returns ReentrantCall.
    let ctx = TestContext::setup(30);
    ctx.fund_pool(10_000_000);

    // Simulate the lock being held (as would happen during a callback).
    ctx.env.as_contract(&ctx.pool_addr, || {
        ctx.env
            .storage()
            .instance()
            .set(&crate::DataKey::LoanActive(ctx.token_addr.clone()), &true);
    });

    let receiver = Address::generate(&ctx.env);
    let result = ctx.pool().try_flash_loan(
        &ctx.token_addr,
        &1_000,
        &receiver,
        &Bytes::new(&ctx.env),
    );
    assert_eq!(result, Err(Ok(Error::ReentrantCall)));

    // Clean up the lock.
    ctx.env.as_contract(&ctx.pool_addr, || {
        ctx.env
            .storage()
            .instance()
            .set(&crate::DataKey::LoanActive(ctx.token_addr.clone()), &false);
    });
}

// ---------------------------------------------------------------------------
// 6. Fee computation unit tests
// ---------------------------------------------------------------------------

#[test]
fn test_fee_minimum_is_one() {
    // Even with 0 bps fee, at least 1 unit is charged.
    let fee = crate::execution::compute_fee(1_000, 1_000_000, 0).unwrap();
    assert_eq!(fee, 1);
}

#[test]
fn test_fee_scales_with_utilisation() {
    let fee_low = crate::execution::compute_fee(10_000, 1_000_000, 30).unwrap();
    let fee_high = crate::execution::compute_fee(500_000, 1_000_000, 30).unwrap();
    assert!(
        fee_high > fee_low,
        "higher utilisation must yield higher fee"
    );
}

#[test]
fn test_fee_zero_pool_balance_returns_error() {
    let result = crate::execution::compute_fee(1_000, 0, 30);
    assert_eq!(result, Err(Error::InsufficientLiquidity));
}

#[test]
fn test_fee_negative_amount_returns_error() {
    let result = crate::execution::compute_fee(-1, 1_000_000, 30);
    assert_eq!(result, Err(Error::InvalidAmount));
}

#[test]
fn test_fee_100pct_utilisation_is_3x_base() {
    // At 100 % utilisation multiplier = 3.0×.
    // base_fee = 1_000_000 × 30 / 10_000 = 3_000
    // fee = 3_000 × 30_000 / 10_000 = 9_000
    let fee = crate::execution::compute_fee(1_000_000, 1_000_000, 30).unwrap();
    assert_eq!(fee, 9_000i128);
}

// ---------------------------------------------------------------------------
// 7. Quote fee view
// ---------------------------------------------------------------------------

#[test]
fn test_quote_fee_matches_actual() {
    let ctx = TestContext::setup(30);
    ctx.fund_pool(10_000_000);

    let receiver_addr = ctx.env.register(ProfitableReceiver, ());
    let receiver_client = ProfitableReceiverClient::new(&ctx.env, &receiver_addr);
    receiver_client.init(&ctx.pool_addr);

    let borrow_amount: i128 = 2_000_000;
    let quoted_fee = ctx.pool().quote_fee(&ctx.token_addr, &borrow_amount);
    ctx.asset().mint(&receiver_addr, &(borrow_amount + quoted_fee));

    let actual_fee = ctx.pool().flash_loan(
        &ctx.token_addr,
        &borrow_amount,
        &receiver_addr,
        &Bytes::new(&ctx.env),
    );
    assert_eq!(actual_fee, quoted_fee);
}

// ---------------------------------------------------------------------------
// 8. Uninitialised pool rejects calls
// ---------------------------------------------------------------------------

#[test]
fn test_uninitialised_pool_rejects_flash_loan() {
    let env = Env::default();
    env.mock_all_auths();
    let pool_addr = env.register(FlashLoanPool, ());
    let pool = FlashLoanPoolClient::new(&env, &pool_addr);

    let token = Address::generate(&env);
    let receiver = Address::generate(&env);

    let result = pool.try_flash_loan(&token, &1_000, &receiver, &Bytes::new(&env));
    assert_eq!(result, Err(Ok(Error::NotInitialized)));
}

// ---------------------------------------------------------------------------
// 9. Zero / negative amount guards
// ---------------------------------------------------------------------------

#[test]
fn test_flash_loan_zero_amount_rejected() {
    let ctx = TestContext::setup(30);
    ctx.fund_pool(1_000_000);
    let receiver = Address::generate(&ctx.env);
    let result = ctx.pool().try_flash_loan(
        &ctx.token_addr,
        &0,
        &receiver,
        &Bytes::new(&ctx.env),
    );
    assert_eq!(result, Err(Ok(Error::InvalidAmount)));
}

#[test]
fn test_flash_loan_amount_exceeds_pool_fails() {
    let ctx = TestContext::setup(30);
    ctx.fund_pool(100_000);
    let receiver = Address::generate(&ctx.env);
    let result = ctx.pool().try_flash_loan(
        &ctx.token_addr,
        &200_000,
        &receiver,
        &Bytes::new(&ctx.env),
    );
    assert_eq!(result, Err(Ok(Error::InsufficientLiquidity)));
}

#[test]
fn test_deposit_zero_amount_rejected() {
    let ctx = TestContext::setup(30);
    let result = ctx.pool().try_deposit(&ctx.admin, &ctx.token_addr, &0);
    assert_eq!(result, Err(Ok(Error::InvalidDeposit)));
}

// ---------------------------------------------------------------------------
// 10. Admin-only operations
// ---------------------------------------------------------------------------

#[test]
fn test_set_base_fee_non_admin_rejected() {
    let ctx = TestContext::setup(30);
    let rando = Address::generate(&ctx.env);
    let result = ctx.pool().try_set_base_fee(&rando, &50u32);
    assert_eq!(result, Err(Ok(Error::Unauthorized)));
}

#[test]
fn test_set_base_fee_updates_correctly() {
    let ctx = TestContext::setup(30);
    ctx.pool().set_base_fee(&ctx.admin, &100u32);
    assert_eq!(ctx.pool().get_base_fee_bps(), 100u32);
}

#[test]
fn test_set_base_fee_over_10000_rejected() {
    let ctx = TestContext::setup(30);
    let result = ctx.pool().try_set_base_fee(&ctx.admin, &10_001u32);
    assert_eq!(result, Err(Ok(Error::InvalidFeeBps)));
}
