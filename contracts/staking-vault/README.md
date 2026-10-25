# Staking Vault Contract

A decentralized staking and continuous yield distribution engine for the Stellar/Soroban ecosystem. Implements the Synthetix/Uniswap `StakingRewards` algorithmic model adapted for Soroban smart contracts.

## Features

- **Continuous reward accrual** using a fixed-point per-token accumulator and carried division remainder across checkpoints with an unchanged stake denominator.
- **Deposit / Withdraw** staked principal with automatic reward checkpoint on every state transition.
- **Claim** accrued rewards at any time.
- **Compound** rewards back into stake (when staking token == reward token).
- **Emergency Withdraw** — bypasses reward calculation when the contract is paused and returns principal. Uncheckpointed rewards for the interval are forfeited so changing total stake cannot retroactively reassign them.
- **Admin pause / unpause** for operational safety.

## Modules

| File | Purpose |
|------|---------|
| `src/lib.rs` | Contract entry-points: initialize, deposit, withdraw, claim_reward, compound, emergency_withdraw, set_paused, view functions |
| `src/reward.rs` | Pure reward math: `compute_reward_per_token`, `compute_earned`, `compute_new_reward_rate` |
| `src/test.rs` | Math and token-backed tests covering precision, reward schedules, emergency exit, and staggered staker entry/exit |

## Algorithm

Reward tracking uses the standard accumulator:

```
reward_per_token += (Δt × scaled_rate) / total_staked
user_earned      += (user_stake × reward_per_token_delta + user_remainder) / PRECISION
```

`REWARD_PRECISION = 1e18` preserves accrual for small stake weights and short
durations. Pool-level division dust is carried across checkpoints while the
pool denominator is unchanged; each user's sub-token numerator is retained
across that user's checkpoints. The emission rate is also stored at this scale,
so budgets smaller than their
duration do not truncate to a zero rate. Integer token payouts can still leave
sub-unit rounding dust.

## Building

```bash
cd contracts/staking-vault
cargo build --target wasm32-unknown-unknown --release
```

## Testing

```bash
cd contracts/staking-vault
cargo test
```
