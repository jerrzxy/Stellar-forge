//! Execution engine for the governance queue.
//!
//! This module is responsible for:
//!
//! 1. **Delay enforcement** — verifying that `current_ledger >= earliest_execution_ledger`
//!    before dispatching any proposal.
//! 2. **Payload dispatch** — calling the target contract via `invoke_contract` inside
//!    a try-catch so that a failing call never corrupts the queue state.
//! 3. **Cancellation** — collecting guardian signatures and cancelling a queued proposal
//!    once the threshold is met.

use soroban_sdk::{Address, Env, IntoVal, Symbol, Val, Vec};

use crate::types::{DataKey, GovError, Proposal, ProposalStatus};

const DAY_IN_LEDGERS: u32 = 17_280;
const GUARDIAN_SIG_BUMP: u32 = 90 * DAY_IN_LEDGERS;
const GUARDIAN_SIG_THRESHOLD: u32 = GUARDIAN_SIG_BUMP - DAY_IN_LEDGERS;

// ---------------------------------------------------------------------------
// Delay enforcement
// ---------------------------------------------------------------------------

/// Returns the current execution delay (in ledgers) stored in the contract.
pub fn get_execution_delay(env: &Env) -> u32 {
    env.storage()
        .persistent()
        .get::<DataKey, u32>(&DataKey::ExecutionDelay)
        .unwrap_or(0)
}

/// Compute the ledger at which a newly-queued proposal becomes executable.
///
/// `queued_ledger` — the ledger when the proposal enters `Queued` state.
/// Returns `queued_ledger + execution_delay`.
pub fn compute_earliest_execution_ledger(env: &Env, queued_ledger: u32) -> u32 {
    let delay = get_execution_delay(env);
    queued_ledger.saturating_add(delay)
}

/// Assert that the execution delay for `proposal` has elapsed.
///
/// Returns `GovError::DelayNotElapsed` if the current ledger sequence is still
/// before `proposal.earliest_execution_ledger`.
pub fn assert_delay_elapsed(env: &Env, proposal: &Proposal) -> Result<(), GovError> {
    let current = env.ledger().sequence();
    if current < proposal.earliest_execution_ledger {
        return Err(GovError::DelayNotElapsed);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Payload dispatch
// ---------------------------------------------------------------------------

/// Attempt to invoke the proposal's target contract.
///
/// # Safety / error isolation
///
/// The call is executed via `try_invoke_contract`, which returns invocation
/// errors instead of unwinding the top-level transaction. This allows the
/// caller to persist `Failed` without rolling back unrelated queue state.
///
/// Returns `Ok(())` on success or `Err(GovError::ExecutionFailed)` if the
/// target contract returns an error or panics.
pub fn dispatch_proposal(env: &Env, proposal: &Proposal) -> Result<(), GovError> {
    // This registry encodes the target payload as one Bytes argument.
    let args: Vec<Val> = soroban_sdk::vec![env, proposal.calldata.clone().into_val(env)];

    match env.try_invoke_contract::<Val, soroban_sdk::Error>(
        &proposal.target,
        &proposal.function,
        args,
    ) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(_)) | Err(Ok(_)) | Err(Err(_)) => Err(GovError::ExecutionFailed),
    }
}

// ---------------------------------------------------------------------------
// Guardian cancellation
// ---------------------------------------------------------------------------

/// Record a guardian's cancellation signature for `proposal_id`.
///
/// * Verifies `caller` is in the guardian set.
/// * Verifies `caller` has not already signed.
/// * Appends the signature.
/// * If accumulated signatures >= threshold, returns `true` (caller should
///   proceed with cancellation).
/// * Otherwise returns `false` (more signatures needed).
pub fn record_guardian_sig(
    env: &Env,
    proposal_id: u64,
    caller: &Address,
) -> Result<bool, GovError> {
    // Verify caller is a guardian.
    let guardians: Vec<Address> = env
        .storage()
        .persistent()
        .get::<DataKey, Vec<Address>>(&DataKey::Guardians)
        .unwrap_or_else(|| Vec::new(env));

    let is_guardian = guardians.iter().any(|g| g == *caller);
    if !is_guardian {
        return Err(GovError::NotGuardian);
    }

    // Load existing signatures for this proposal.
    let sig_key = DataKey::CancelSigs(proposal_id);
    let mut sigs: Vec<Address> = env
        .storage()
        .temporary()
        .get::<DataKey, Vec<Address>>(&sig_key)
        .unwrap_or_else(|| Vec::new(env));

    // Verify caller hasn't already signed.
    if sigs.iter().any(|s| s == *caller) {
        return Err(GovError::AlreadySigned);
    }

    sigs.push_back(caller.clone());

    let storage = env.storage().temporary();
    storage.set(&sig_key, &sigs);
    storage.extend_ttl(&sig_key, GUARDIAN_SIG_THRESHOLD, GUARDIAN_SIG_BUMP);

    let threshold: u32 = env
        .storage()
        .persistent()
        .get::<DataKey, u32>(&DataKey::GuardianThreshold)
        .unwrap_or(1);

    Ok(sigs.len() >= threshold)
}

/// Clear the collected guardian signatures for a proposal (called after
/// a successful cancellation so storage is not left dirty).
pub fn clear_guardian_sigs(env: &Env, proposal_id: u64) {
    env.storage()
        .temporary()
        .remove(&DataKey::CancelSigs(proposal_id));
}

// ---------------------------------------------------------------------------
// Proposal persistence helpers (used by lib.rs)
// ---------------------------------------------------------------------------

/// Load a proposal from persistent storage.
/// Returns `GovError::ProposalNotFound` if it does not exist.
pub fn load_proposal(env: &Env, id: u64) -> Result<Proposal, GovError> {
    env.storage()
        .persistent()
        .get::<DataKey, Proposal>(&DataKey::Proposal(id))
        .ok_or(GovError::ProposalNotFound)
}

/// Persist a proposal to storage.
pub fn save_proposal(env: &Env, proposal: &Proposal) {
    env.storage()
        .persistent()
        .set(&DataKey::Proposal(proposal.id), proposal);
}

/// Atomically transition a proposal to `Queued` state, setting the
/// `queued_ledger` and `earliest_execution_ledger` fields.
///
/// The caller must have already verified the proposal is in `Created` state
/// and that quorum has been reached.
pub fn transition_to_queued(env: &Env, proposal: &mut Proposal) {
    let current_ledger = env.ledger().sequence();
    proposal.queued_ledger = current_ledger;
    proposal.earliest_execution_ledger =
        compute_earliest_execution_ledger(env, current_ledger);
    proposal.status = ProposalStatus::Queued;
    save_proposal(env, proposal);
}

/// Mark a proposal as `Executed` and persist.
pub fn transition_to_executed(env: &Env, proposal: &mut Proposal) {
    proposal.status = ProposalStatus::Executed;
    save_proposal(env, proposal);
}

/// Mark a proposal as `Failed` (target call error) and persist.
pub fn transition_to_failed(env: &Env, proposal: &mut Proposal) {
    proposal.status = ProposalStatus::Failed;
    save_proposal(env, proposal);
}

/// Mark a proposal as `Cancelled` and persist.
pub fn transition_to_cancelled(env: &Env, proposal: &mut Proposal) {
    proposal.status = ProposalStatus::Cancelled;
    save_proposal(env, proposal);
}
