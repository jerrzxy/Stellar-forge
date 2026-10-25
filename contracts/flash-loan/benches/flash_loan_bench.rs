use flash_loan::{FlashLoanPool, FlashLoanPoolClient};
use soroban_sdk::{
    contract, contractimpl, contracttype, testutils::Address as _, token, Address, Bytes, Env,
};
use std::time::Instant;

#[contracttype]
enum BenchDataKey {
    Token,
    Pool,
}

#[contract]
struct BenchReceiver;

#[contractimpl]
impl BenchReceiver {
    pub fn initialize(env: Env, pool: Address, token: Address) {
        env.storage().instance().set(&BenchDataKey::Pool, &pool);
        env.storage().instance().set(&BenchDataKey::Token, &token);
    }

    pub fn execute_operation(
        env: Env,
        _token: Address,
        amount: i128,
        fee: i128,
        _user_data: Bytes,
    ) {
        let token: Address = env.storage().instance().get(&BenchDataKey::Token).unwrap();
        let pool: Address = env.storage().instance().get(&BenchDataKey::Pool).unwrap();
        token::Client::new(&env, &token).transfer(
            &env.current_contract_address(),
            &pool,
            &(amount + fee),
        );
    }
}

fn main() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract_v2(admin.clone());
    let token = token_id.address();
    let token_admin = token::StellarAssetClient::new(&env, &token);

    let pool_id = env.register(FlashLoanPool, ());
    let pool = FlashLoanPoolClient::new(&env, &pool_id);
    pool.initialize(&admin, &30);

    let liquidity = 1_000_000;
    token_admin.mint(&admin, &liquidity);
    pool.deposit(&admin, &token, &liquidity);

    let receiver_id = env.register(BenchReceiver, ());
    let receiver = BenchReceiverClient::new(&env, &receiver_id);
    receiver.initialize(&pool_id, &token);

    let amount = 100_000;
    let fee = pool.quote_fee(&token, &amount);
    token_admin.mint(&receiver_id, &(amount + fee));

    let start = Instant::now();
    pool.flash_loan(&token, &amount, &receiver_id, &Bytes::new(&env));
    let elapsed = start.elapsed();
    let budget = env.cost_estimate().budget();

    println!(
        "Flash loan execution: {:.3} ms",
        elapsed.as_secs_f64() * 1_000.0
    );
    println!(
        "Soroban CPU instructions: {}",
        budget.cpu_instruction_cost()
    );
    println!("Soroban memory bytes: {}", budget.memory_bytes_cost());
}
