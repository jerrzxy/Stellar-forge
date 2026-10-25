#![cfg(test)]

extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    Address, Bytes, BytesN, Env, Vec,
};

fn make_env() -> Env {
    let env = Env::default();
    env.mock_all_auths();
    env
}

fn signer_set(env: &Env, n: u32) -> Vec<Address> {
    let mut signers = Vec::new(env);
    for _ in 0..n {
        signers.push_back(Address::generate(env));
    }
    signers
}

fn payload(env: &Env, seed: u8) -> Bytes {
    let mut b = Bytes::new(env);
    for i in 0..32u8 {
        b.push_back(seed.wrapping_add(i));
    }
    b
}

fn digest(env: &Env, domain: &BytesN<32>, nonce: u64, data: &Bytes) -> BytesN<32> {
    let mut buf = Bytes::new(env);
    for i in 0..32u32 {
        buf.push_back(domain.get(i).unwrap());
    }
    for i in 0..8u32 {
        buf.push_back(((nonce >> (8 * (7 - i))) & 0xff) as u8);
    }
    for i in 0..data.len() {
        buf.push_back(data.get(i).unwrap());
    }
    env.crypto().sha256(&buf).into()
}

fn attestations(env: &Env, signers: &Vec<Address>, msg: &BytesN<32>, count: u32) -> Vec<BytesN<32>> {
    let mut sigs = Vec::new(env);
    for i in 0..count {
        let signer = signers.get(i).unwrap();
        let mut raw = Bytes::new(env);
        for j in 0..32u32 {
            raw.push_back(msg.get(j).unwrap() ^ (i as u8));
        }
        let _ = signer;
        sigs.push_back(env.crypto().sha256(&raw).into());
    }
    sigs
}

fn setup(env: &Env, threshold: u32, total: u32) -> (Address, Vec<Address>, BytesN<32>) {
    let contract_id = env.register_contract(None, BridgeRelayer);
    let client = BridgeRelayerClient::new(env, &contract_id);
    let signers = signer_set(env, total);
    let domain = BytesN::from_array(env, &[7u8; 32]);
    client.initialize(&signers, &threshold, &domain);
    (contract_id, signers, domain)
}

#[test]
fn test_initialize_sets_config() {
    let env = make_env();
    let (contract_id, signers, domain) = setup(&env, 2, 3);
    let client = BridgeRelayerClient::new(&env, &contract_id);
    assert_eq!(client.threshold(), 2);
    assert_eq!(client.domain_separator(), domain);
    assert_eq!(client.signer_count(), signers.len());
}

#[test]
fn test_valid_dispatch_succeeds() {
    let env = make_env();
    let (contract_id, signers, domain) = setup(&env, 2, 3);
    let client = BridgeRelayerClient::new(&env, &contract_id);

    let data = payload(&env, 1);
    let nonce = 0u64;
    let msg = digest(&env, &domain, nonce, &data);
    let sigs = attestations(&env, &signers, &msg, 2);

    let target = Address::generate(&env);
    let result = client.try_dispatch(&nonce, &data, &sigs, &target);
    assert!(result.is_ok());
    assert_eq!(client.last_nonce(), nonce);
}

#[test]
fn test_signature_threshold_failure() {
    let env = make_env();
    let (contract_id, signers, domain) = setup(&env, 3, 4);
    let client = BridgeRelayerClient::new(&env, &contract_id);

    let data = payload(&env, 2);
    let nonce = 0u64;
    let msg = digest(&env, &domain, nonce, &data);
    let sigs = attestations(&env, &signers, &msg, 2);

    let target = Address::generate(&env);
    let result = client.try_dispatch(&nonce, &data, &sigs, &target);
    assert_eq!(result, Err(Ok(Error::ThresholdNotMet)));
}

#[test]
fn test_replay_attack_rejected() {
    let env = make_env();
    let (contract_id, signers, domain) = setup(&env, 2, 3);
    let client = BridgeRelayerClient::new(&env, &contract_id);

    let data = payload(&env, 3);
    let nonce = 0u64;
    let msg = digest(&env, &domain, nonce, &data);
    let sigs = attestations(&env, &signers, &msg, 2);
    let target = Address::generate(&env);

    assert!(client.try_dispatch(&nonce, &data, &sigs, &target).is_ok());
    let replay = client.try_dispatch(&nonce, &data, &sigs, &target);
    assert_eq!(replay, Err(Ok(Error::NonceAlreadyUsed)));
}

#[test]
fn test_out_of_order_nonce_rejected() {
    let env = make_env();
    let (contract_id, signers, domain) = setup(&env, 2, 3);
    let client = BridgeRelayerClient::new(&env, &contract_id);

    let data = payload(&env, 4);
    let nonce = 5u64;
    let msg = digest(&env, &domain, nonce, &data);
    let sigs = attestations(&env, &signers, &msg, 2);
    let target = Address::generate(&env);

    let result = client.try_dispatch(&nonce, &data, &sigs, &target);
    assert_eq!(result, Err(Ok(Error::InvalidNonce)));
}

#[test]
fn test_domain_separator_mismatch_rejected() {
    let env = make_env();
    let (contract_id, signers, _domain) = setup(&env, 2, 3);
    let client = BridgeRelayerClient::new(&env, &contract_id);

    let wrong_domain = BytesN::from_array(&env, &[9u8; 32]);
    let data = payload(&env, 5);
    let nonce = 0u64;
    let msg = digest(&env, &wrong_domain, nonce, &data);
    let sigs = attestations(&env, &signers, &msg, 2);
    let target = Address::generate(&env);

    let result = client.try_dispatch(&nonce, &data, &sigs, &target);
    assert_eq!(result, Err(Ok(Error::InvalidAttestation)));
}

#[test]
fn test_sequential_nonces_advance() {
    let env = make_env();
    let (contract_id, signers, domain) = setup(&env, 2, 3);
    let client = BridgeRelayerClient::new(&env, &contract_id);
    let target = Address::generate(&env);

    for nonce in 0u64..3u64 {
        let data = payload(&env, nonce as u8);
        let msg = digest(&env, &domain, nonce, &data);
        let sigs = attestations(&env, &signers, &msg, 2);
        assert!(client.try_dispatch(&nonce, &data, &sigs, &target).is_ok());
        assert_eq!(client.last_nonce(), nonce);
    }
}
