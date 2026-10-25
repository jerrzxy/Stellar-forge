#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, Bytes, BytesN, Env, Vec,
};

mod nonce;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum RelayerError {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    InvalidThreshold = 3,
    UnknownRelayer = 4,
    DuplicateSigner = 5,
    InsufficientSignatures = 6,
    InvalidSignature = 7,
    ReplayDetected = 8,
    InvalidPayload = 9,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CrossChainMessage {
    pub source_chain: u32,
    pub destination_chain: u32,
    pub nonce: u64,
    pub target: Address,
    pub payload: Bytes,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attestation {
    pub relayer: Address,
    pub signature: BytesN<64>,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelayerConfig {
    pub threshold: u32,
    pub relayers: Vec<Address>,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DispatchResult {
    pub nonce: u64,
    pub target: Address,
    pub payload: Bytes,
}

const CONFIG_KEY: &str = "config";

#[contract]
pub struct BridgeRelayer;

#[contractimpl]
impl BridgeRelayer {
    /// Initialize the relayer set and signature threshold.
    pub fn initialize(
        env: Env,
        relayers: Vec<Address>,
        threshold: u32,
    ) -> Result<(), RelayerError> {
        if env.storage().instance().has(&CONFIG_KEY) {
            return Err(RelayerError::AlreadyInitialized);
        }
        if threshold == 0 || threshold > relayers.len() {
            return Err(RelayerError::InvalidThreshold);
        }
        let config = RelayerConfig {
            threshold,
            relayers,
        };
        env.storage().instance().set(&CONFIG_KEY, &config);
        nonce::init(&env);
        Ok(())
    }

    /// Verify a threshold of relayer attestations over the message digest.
    pub fn verify_attestations(
        env: Env,
        message: CrossChainMessage,
        attestations: Vec<Attestation>,
    ) -> Result<(), RelayerError> {
        let config: RelayerConfig = env
            .storage()
            .instance()
            .get(&CONFIG_KEY)
            .ok_or(RelayerError::NotInitialized)?;

        let digest = Self::message_digest(&env, &message);
        let mut seen: Vec<Address> = Vec::new(&env);
        let mut valid: u32 = 0;

        for attestation in attestations.iter() {
            if !Self::is_relayer(&config, &attestation.relayer) {
                return Err(RelayerError::UnknownRelayer);
            }
            if seen.contains(&attestation.relayer) {
                return Err(RelayerError::DuplicateSigner);
            }
            seen.push_back(attestation.relayer.clone());

            env.crypto()
                .ed25519_verify(&attestation.signature, &digest, &attestation.relayer);
            valid += 1;
        }

        if valid < config.threshold {
            return Err(RelayerError::InsufficientSignatures);
        }
        Ok(())
    }

    /// Verify attestations, enforce replay protection, and dispatch the payload.
    pub fn execute(
        env: Env,
        message: CrossChainMessage,
        attestations: Vec<Attestation>,
    ) -> Result<DispatchResult, RelayerError> {
        Self::verify_attestations(env.clone(), message.clone(), attestations)?;

        nonce::consume(&env, message.source_chain, message.destination_chain, message.nonce)
            .map_err(|_| RelayerError::ReplayDetected)?;

        Ok(DispatchResult {
            nonce: message.nonce,
            target: message.target,
            payload: message.payload,
        })
    }

    /// Current expected nonce for a given source/destination domain pair.
    pub fn current_nonce(env: Env, source_chain: u32, destination_chain: u32) -> u64 {
        nonce::current(&env, source_chain, destination_chain)
    }

    fn is_relayer(config: &RelayerConfig, relayer: &Address) -> bool {
        config.relayers.contains(relayer)
    }

    fn message_digest(env: &Env, message: &CrossChainMessage) -> BytesN<32> {
        let mut data = Bytes::new(env);
        data.extend_from_array(&message.source_chain.to_be_bytes());
        data.extend_from_array(&message.destination_chain.to_be_bytes());
        data.extend_from_array(&message.nonce.to_be_bytes());
        data.append(&message.payload);
        env.crypto().sha256(&data).into()
    }
}
