#![cfg(test)]

use super::reward::*;
use soroban_sdk::testutils::Ledger as _;

/// Pure-math unit tests for reward accounting (no Soroban SDK needed).
/// Rates are reward units per second scaled by REWARD_PRECISION.

const RATE: u128 = 1_000_000 * REWARD_PRECISION;

#[test]
fn test_reward_per_token_zero_staked_returns_stored() {
    // When total_staked is 0, stored value should be returned unchanged.
    let stored = 1_000_000u128;
    let result = compute_reward_per_token(1000, 0, 2000, RATE, 0, stored).unwrap();
    assert_eq!(result, stored, "should return stored when no stake");
}

#[test]
fn test_reward_per_token_past_period_finish_caps() {
    // After period ends, further time should not accrue rewards.
    let total = 1_000_000_000i128; // 1e9 tokens staked
    // Period is 0..1000. Query at t=2000 vs t=3000 — both should return same.
    let r1 = compute_reward_per_token(2000, 500, 1000, RATE, total, 0).unwrap();
    let r2 = compute_reward_per_token(3000, 500, 1000, RATE, total, 0).unwrap();
    assert_eq!(r1, r2, "rewards must not accrue past period_finish");
}

#[test]
fn test_reward_per_token_increases_with_time() {
    let total = 1_000_000i128;
    let r1 = compute_reward_per_token(500, 0, 10_000, RATE, total, 0).unwrap();
    let r2 = compute_reward_per_token(1000, 0, 10_000, RATE, total, 0).unwrap();
    assert!(r2 > r1, "reward_per_token must increase with time");
}

#[test]
fn test_compute_earned_proportional() {
    // Two stakers with equal stake and same duration should earn the same.
    let total = 2_000i128; // two stakers, 1000 each
    let rpt_at_t1000 = compute_reward_per_token(1000, 0, 10_000, RATE, total, 0).unwrap();

    let earned_a = compute_earned(1000, rpt_at_t1000, 0, 0).unwrap();
    let earned_b = compute_earned(1000, rpt_at_t1000, 0, 0).unwrap();
    assert_eq!(earned_a, earned_b, "equal stakers must earn equal rewards");
}

#[test]
fn test_compute_earned_proportional_to_stake() {
    // Staker with 2x stake earns 2x rewards.
    let total = 3_000i128; // staker_a=1000, staker_b=2000
    let rpt = compute_reward_per_token(1000, 0, 10_000, RATE, total, 0).unwrap();
    let earned_a = compute_earned(1000, rpt, 0, 0).unwrap();
    let earned_b = compute_earned(2000, rpt, 0, 0).unwrap();
    assert_eq!(earned_b, earned_a * 2, "rewards must be proportional to stake");
}

#[test]
fn test_last_time_reward_applicable_before_finish() {
    assert_eq!(last_time_reward_applicable(500, 1000), 500);
}

#[test]
fn test_last_time_reward_applicable_after_finish() {
    assert_eq!(last_time_reward_applicable(1500, 1000), 1000);
}

#[test]
fn test_compute_new_reward_rate_fresh_period() {
    // New period with no carry-over.
    let (rate, finish) = compute_new_reward_rate(0, 0, 0, 1_000_000, 1000).unwrap();
    assert_eq!(rate, 1000 * REWARD_PRECISION);
    assert_eq!(finish, 1000u64);
}

#[test]
fn test_compute_new_reward_rate_rollover() {
    // 500s remain at 1000 units/s; the remaining 500_000 plus 1_000_000
    // rewards are spread across the new 1000-second period.
    let (rate, finish) = compute_new_reward_rate(
        0,
        500,
        1000 * REWARD_PRECISION,
        1_000_000,
        1000,
    )
    .unwrap();
    assert_eq!(rate, 1500 * REWARD_PRECISION);
    assert_eq!(finish, 1000u64);
}

#[test]
fn test_total_pool_balance_solvency() {
    // Assert each individual staker's earnings never exceed total rewards emitted in their window.
    let rate = RATE;
    let duration_secs = 1_000u64;
    // total_emitted approx = rate * duration (in reward units, ignoring precision because it cancels)
    let total_emitted = (rate * duration_secs as u128 / REWARD_PRECISION) as i128;

    let scenarios: &[(u64, u64, i128)] = &[
        (0, 1000, 500),
        (200, 800, 1000),
        (500, 1000, 300),
    ];

    for &(enter, exit, stake) in scenarios {
        let rpt_start = compute_reward_per_token(enter, 0, duration_secs, rate, stake, 0).unwrap();
        let rpt_end   = compute_reward_per_token(exit,  0, duration_secs, rate, stake, 0).unwrap();
        let single    = compute_earned(stake, rpt_end, rpt_start, 0).unwrap();
        assert!(
            single <= total_emitted,
            "staker earned {} but total emitted is {}",
            single,
            total_emitted
        );
    }
}

#[test]
fn test_zero_reward_for_zero_stake() {
    let rpt = compute_reward_per_token(1000, 0, 2000, RATE, 0, 5000).unwrap();
    assert_eq!(rpt, 5000, "rpt unchanged when no stake");
    let earned = compute_earned(0, rpt, 0, 0).unwrap();
    assert_eq!(earned, 0, "zero stake earns zero");
}

#[test]
fn test_stored_rewards_accumulate_correctly() {
    // Staker with already stored rewards gets more on next checkpoint.
    let already_earned: i128 = 500_000;
    // rpt_delta = REWARD_PRECISION -> pending = 1 * REWARD_PRECISION / REWARD_PRECISION = 1
    let rpt_delta = REWARD_PRECISION;
    let stake = 1i128;
    let earned = compute_earned(stake, rpt_delta, 0, already_earned).unwrap();
    assert_eq!(earned, already_earned + 1);
}

#[test]
fn test_rounding_no_drift_many_checkpoints() {
    // Simulate 100 sequential checkpoints for a single staker to verify no cumulative drift.
    let rate = RATE;
    let total_stake = 1_000_000i128;
    let period_finish = 10_000u64;
    let step = 100u64; // checkpoint every 100s

    let mut rpt_stored = 0u128;
    let mut last_update = 0u64;

    for tick in 1..=100u64 {
        let current_time = tick * step;
        rpt_stored = compute_reward_per_token(
            current_time,
            last_update,
            period_finish,
            rate,
            total_stake,
            rpt_stored,
        )
        .unwrap();
        last_update = current_time.min(period_finish);
    }

    // Final rpt should equal a single direct computation from 0 to period_finish.
    let direct_rpt = compute_reward_per_token(10_000, 0, period_finish, rate, total_stake, 0).unwrap();
    assert_eq!(
        rpt_stored, direct_rpt,
        "incremental checkpoints must produce zero drift vs single computation"
    );
}

#[test]
fn test_remainder_preserves_rewards_across_short_checkpoints() {
    let mut reward_per_token = 0u128;
    let mut remainder = 0u128;
    let mut last_update = 0u64;

    for current_time in 1..=3 {
        (reward_per_token, remainder) = compute_reward_per_token_with_remainder(
            current_time,
            last_update,
            10,
            REWARD_PRECISION,
            3,
            reward_per_token,
            remainder,
        )
        .unwrap();
        last_update = current_time;
    }

    let three_one_unit_stakers = (0..3)
        .map(|_| compute_earned(1, reward_per_token, 0, 0).unwrap())
        .sum::<i128>();
    assert_eq!(three_one_unit_stakers, 3);
}

#[test]
fn test_user_remainder_preserves_fractional_earnings_across_checkpoints() {
    let mut reward_per_token = 0u128;
    let mut pool_remainder = 0u128;
    let mut user_remainder = 0u128;
    let mut last_update = 0u64;
    let mut earned = 0i128;

    for current_time in 1..=3 {
        let (next_rpt, next_pool_remainder) = compute_reward_per_token_with_remainder(
            current_time,
            last_update,
            10,
            REWARD_PRECISION,
            3,
            reward_per_token,
            pool_remainder,
        )
        .unwrap();
        (earned, user_remainder) = compute_earned_with_remainder(
            1,
            next_rpt,
            reward_per_token,
            earned,
            user_remainder,
        )
        .unwrap();
        reward_per_token = next_rpt;
        pool_remainder = next_pool_remainder;
        last_update = current_time;
    }

    assert_eq!(earned, 1);
    assert_eq!(user_remainder, 0);
}

#[test]
fn test_fractional_reward_rate_supports_small_budget_and_short_duration() {
    let (rate, finish) = compute_new_reward_rate(0, 0, 0, 1, 3).unwrap();
    assert_eq!(finish, 3);
    assert_eq!(rate, REWARD_PRECISION / 3);
    assert!(rate > 0, "small budgets must not truncate to a zero rate");
}

#[test]
fn test_reward_rate_rejects_unrepresentable_duration_and_overflow() {
    assert_eq!(
        compute_new_reward_rate(0, 0, 0, 1, u64::MAX),
        Err(crate::Error::RewardRateTooLow),
    );
    assert_eq!(
        compute_new_reward_rate(0, 0, 0, i128::MAX, 1),
        Err(crate::Error::CalculationOverflow),
    );
}

#[test]
fn test_single_unit_stake_accrues_during_short_reward_period() {
    use soroban_sdk::{
        testutils::Address as _, token::StellarAssetClient, Address, Env,
    };

    use crate::{StakingVaultContract, StakingVaultContractClient};

    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let staker = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract_v2(admin.clone());
    let token_admin = StellarAssetClient::new(&env, &token_id.address());
    let vault_id = env.register(StakingVaultContract, ());
    let vault = StakingVaultContractClient::new(&env, &vault_id);

    vault.initialize(&admin, &token_id.address(), &token_id.address()).unwrap();
    token_admin.mint(&admin, &3i128);
    token_admin.mint(&staker, &1i128);
    env.ledger().set_timestamp(0);
    vault.notify_reward_amount(&admin, &3i128, &3u64).unwrap();
    vault.deposit(&staker, &1i128).unwrap();

    env.ledger().set_timestamp(3);
    assert_eq!(vault.earned(&staker).unwrap(), 3);
}

#[test]
fn test_stakers_entering_and_exiting_receive_time_weighted_rewards() {
    use soroban_sdk::{
        testutils::Address as _, token::StellarAssetClient, Address, Env,
    };

    use crate::{StakingVaultContract, StakingVaultContractClient};

    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let alice = Address::generate(&env);
    let bob = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract_v2(admin.clone());
    let token_admin = StellarAssetClient::new(&env, &token_id.address());
    let vault_id = env.register(StakingVaultContract, ());
    let vault = StakingVaultContractClient::new(&env, &vault_id);

    vault.initialize(&admin, &token_id.address(), &token_id.address()).unwrap();
    token_admin.mint(&admin, &1_000i128);
    token_admin.mint(&alice, &100i128);
    token_admin.mint(&bob, &100i128);
    vault.notify_reward_amount(&admin, &1_000i128, &100u64).unwrap();

    env.ledger().set_timestamp(0);
    vault.deposit(&alice, &100i128).unwrap();

    env.ledger().set_timestamp(10);
    vault.deposit(&bob, &100i128).unwrap();
    assert_eq!(vault.earned(&alice).unwrap(), 100);
    assert_eq!(vault.earned(&bob).unwrap(), 0);

    env.ledger().set_timestamp(30);
    vault.withdraw(&alice, &50i128).unwrap();
    assert_eq!(vault.earned(&alice).unwrap(), 200);
    assert_eq!(vault.earned(&bob).unwrap(), 100);

    env.ledger().set_timestamp(40);
    vault.withdraw(&bob, &100i128).unwrap();
    assert_eq!(vault.earned(&alice).unwrap(), 233);
    assert_eq!(vault.earned(&bob).unwrap(), 166);
    assert_eq!(vault.get_total_staked(), 50);

    assert_eq!(vault.claim_reward(&alice).unwrap(), 233);
    assert_eq!(vault.claim_reward(&bob).unwrap(), 166);
}

#[test]
fn test_emergency_withdraw_returns_principal_without_reward_checkpoint() {
    use soroban_sdk::{
        testutils::Address as _, token::StellarAssetClient, Address, Env,
    };

    use crate::{StakingVaultContract, StakingVaultContractClient};

    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let staker = Address::generate(&env);
    let remaining_staker = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract_v2(admin.clone());
    let token_admin = StellarAssetClient::new(&env, &token_id.address());
    let vault_id = env.register(StakingVaultContract, ());
    let vault = StakingVaultContractClient::new(&env, &vault_id);

    vault.initialize(&admin, &token_id.address(), &token_id.address()).unwrap();
    token_admin.mint(&admin, &100i128);
    token_admin.mint(&staker, &1i128);
    token_admin.mint(&remaining_staker, &1i128);
    env.ledger().set_timestamp(0);
    vault.notify_reward_amount(&admin, &100i128, &10u64).unwrap();
    vault.deposit(&staker, &1i128).unwrap();
    vault.deposit(&remaining_staker, &1i128).unwrap();

    env.ledger().set_timestamp(5);
    vault.set_paused(&admin, &true).unwrap();
    assert_eq!(vault.emergency_withdraw(&staker).unwrap(), 1);
    assert_eq!(vault.get_total_staked(), 1);
    assert_eq!(vault.get_user_stake(&staker), 0);
    assert_eq!(vault.earned(&staker).unwrap(), 0);
    env.ledger().set_timestamp(10);
    assert_eq!(vault.earned(&remaining_staker).unwrap(), 50);
}
