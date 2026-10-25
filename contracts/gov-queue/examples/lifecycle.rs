//! Run a complete governance proposal lifecycle against a simulated Soroban
//! environment with `cargo run --example lifecycle`.

use gov_queue::{GovQueueContract, GovQueueContractClient, ProposalStatus};
use soroban_sdk::{
    contract, contractimpl,
    testutils::{Address as _, Ledger as _},
    Address, Bytes, Env, Symbol,
};

#[contract]
struct ConfigTarget;

#[contractimpl]
impl ConfigTarget {
    pub fn apply_config(env: Env, payload: Bytes) {
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "config"), &payload);
    }

    pub fn config(env: Env) -> Option<Bytes> {
        env.storage().instance().get(&Symbol::new(&env, "config"))
    }
}

fn main() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let guardian = Address::generate(&env);
    let guardians = soroban_sdk::vec![&env, guardian];
    let governance_id = env.register_contract(None, GovQueueContract);
    let governance = GovQueueContractClient::new(&env, &governance_id);
    governance
        .initialize(&admin, &guardians, &1u32, &20u32, &2u64)
        .unwrap();

    let target_id = env.register_contract(None, ConfigTarget);
    let target = ConfigTargetClient::new(&env, &target_id);
    let proposer = Address::generate(&env);
    let voter1 = Address::generate(&env);
    let voter2 = Address::generate(&env);
    let payload = Bytes::from_slice(&env, b"max_validators=64");
    let description = Bytes::from_slice(&env, b"Raise the validator limit");

    env.ledger().set_sequence_number(100);
    let proposal_id = governance.submit_proposal(
        &proposer,
        &target_id,
        &Symbol::new(&env, "apply_config"),
        &payload,
        &description,
    ).unwrap();
    governance.cast_vote(&voter1, &proposal_id, &true).unwrap();
    governance.cast_vote(&voter2, &proposal_id, &true).unwrap();

    let proposal = governance.get_proposal(&proposal_id).unwrap();
    assert_eq!(proposal.earliest_execution_ledger, 120);
    env.ledger().set_sequence_number(119);
    assert!(governance.execute_proposal(&proposal_id).is_err());
    assert_eq!(target.config(), None);

    env.ledger().set_sequence_number(120);
    governance.execute_proposal(&proposal_id).unwrap();
    assert_eq!(target.config(), Some(payload));
    assert_eq!(
        governance.get_proposal_status(&proposal_id).unwrap(),
        ProposalStatus::Executed
    );

    println!("Proposal {proposal_id} executed at ledger 120; validator limit updated.");
}