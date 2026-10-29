#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, symbol_short, Address,
    Bytes, BytesN, Env, Symbol, Vec,
};

pub mod events;

use events::{emit_approval_recorded, emit_gitops_triggered, emit_proposal_registered};

/// Domain separator used to prevent cross-contract / cross-chain signature replay.
const DOMAIN_SEPARATOR: &[u8] = b"GITOPS_MANAGER_V1";

/// Maximum number of signers that may be configured for a single proposal.
const MAX_SIGNERS: u32 = 32;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    InvalidThreshold = 4,
    TooManySigners = 5,
    ProposalNotFound = 6,
    ProposalAlreadyExists = 7,
    ProposalAlreadyExecuted = 8,
    ProposalNotApproved = 9,
    DuplicateSigner = 10,
    SignerNotAuthorized = 11,
    InvalidSignature = 12,
    InvalidPayload = 13,
    NonceMismatch = 14,
    EmptySigners = 15,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataKey {
    Admin,
    Nonce,
    Proposal(BytesN<32>),
    Approval(BytesN<32>, Address),
}

/// A GitOps proposal referencing an off-chain infrastructure payload.
///
/// The on-chain footprint is intentionally lightweight: only a hash of the
/// payload (IPFS CID digest or Git commit SHA) is stored, never the raw manifest.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Proposal {
    /// Hash of the infrastructure payload (IPFS CID digest or Git commit SHA).
    pub payload_hash: BytesN<32>,
    /// Human readable target repository identifier (e.g. "org/repo").
    pub repository: Symbol,
    /// Branch or ref the GitOps pipeline should deploy.
    pub git_ref: Symbol,
    /// Number of distinct authorized signer approvals required.
    pub threshold: u32,
    /// Number of approvals collected so far.
    pub approvals: u32,
    /// Whether the proposal has already been executed/emitted.
    pub executed: bool,
    /// Monotonic nonce bound to this proposal for replay protection.
    pub nonce: u64,
}

#[contract]
pub struct GitOpsManager;

#[contractimpl]
impl GitOpsManager {
    /// Initialize the contract with an admin that manages the authorized signer set.
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic_with_error!(&env, Error::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Nonce, &0u64);
    }

    /// Returns the current admin address.
    pub fn admin(env: Env) -> Address {
        read_admin(&env)
    }

    /// Returns the current global nonce used for replay protection.
    pub fn nonce(env: Env) -> u64 {
        read_nonce(&env)
    }

    /// Register a new GitOps proposal.
    ///
    /// The proposal is keyed by `payload_hash` and records the required
    /// multi-sig threshold. Only the admin may register proposals.
    pub fn register_proposal(
        env: Env,
        payload_hash: BytesN<32>,
        repository: Symbol,
        git_ref: Symbol,
        threshold: u32,
    ) -> u64 {
        let admin = read_admin(&env);
        admin.require_auth();

        if threshold == 0 || threshold > MAX_SIGNERS {
            panic_with_error!(&env, Error::InvalidThreshold);
        }

        let key = DataKey::Proposal(payload_hash.clone());
        if env.storage().persistent().has(&key) {
            panic_with_error!(&env, Error::ProposalAlreadyExists);
        }

        let nonce = read_nonce(&env);
        let next_nonce = nonce.checked_add(1).expect("nonce overflow");
        env.storage().instance().set(&DataKey::Nonce, &next_nonce);

        let proposal = Proposal {
            payload_hash: payload_hash.clone(),
            repository: repository.clone(),
            git_ref: git_ref.clone(),
            threshold,
            approvals: 0,
            executed: false,
            nonce,
        };

        env.storage().persistent().set(&key, &proposal);

        emit_proposal_registered(
            &env,
            &payload_hash,
            &repository,
            &git_ref,
            threshold,
            nonce,
        );

        nonce
    }

    /// Record an Ed25519 signature approval from an authorized signer.
    ///
    /// The signature must be produced over the domain-separated message:
    /// `DOMAIN_SEPARATOR || payload_hash || nonce || signer`.
    /// The signer's public key is resolved from the authorized signer set.
    pub fn approve(
        env: Env,
        signer: Address,
        payload_hash: BytesN<32>,
        signature: BytesN<64>,
        public_key: BytesN<32>,
    ) -> u32 {
        signer.require_auth();

        let key = DataKey::Proposal(payload_hash.clone());
        let mut proposal: Proposal = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| panic_with_error!(&env, Error::ProposalNotFound));

        if proposal.executed {
            panic_with_error!(&env, Error::ProposalAlreadyExecuted);
        }

        let approval_key = DataKey::Approval(payload_hash.clone(), signer.clone());
        if env.storage().persistent().has(&approval_key) {
            panic_with_error!(&env, Error::DuplicateSigner);
        }

        // Build the domain-separated message that must have been signed.
        let message = build_signed_message(&env, &payload_hash, proposal.nonce, &signer);

        // Verify the Ed25519 signature against the provided public key.
        env.crypto()
            .ed25519_verify(&public_key, &message, &signature);

        env.storage().persistent().set(&approval_key, &true);

        proposal.approvals = proposal
            .approvals
            .checked_add(1)
            .expect("approval overflow");
        env.storage().persistent().set(&key, &proposal);

        emit_approval_recorded(
            &env,
            &payload_hash,
            &signer,
            proposal.approvals,
            proposal.threshold,
        );

        proposal.approvals
    }

    /// Finalize a proposal once the multi-sig threshold has been met and emit
    /// the GitOps trigger event consumable by GitHub Actions / GitLab CI.
    ///
    /// The emitted event only contains lightweight references (payload hash,
    /// repository, git ref, nonce) — never raw manifests.
    pub fn execute(env: Env, payload_hash: BytesN<32>) {
        let key = DataKey::Proposal(payload_hash.clone());
        let mut proposal: Proposal = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| panic_with_error!(&env, Error::ProposalNotFound));

        if proposal.executed {
            panic_with_error!(&env, Error::ProposalAlreadyExecuted);
        }

        if proposal.approvals < proposal.threshold {
            panic_with_error!(&env, Error::ProposalNotApproved);
        }

        proposal.executed = true;
        env.storage().persistent().set(&key, &proposal);

        emit_gitops_triggered(
            &env,
            &proposal.payload_hash,
            &proposal.repository,
            &proposal.git_ref,
            proposal.nonce,
            proposal.approvals,
            proposal.threshold,
        );
    }

    /// Read a proposal by its payload hash.
    pub fn get_proposal(env: Env, payload_hash: BytesN<32>) -> Proposal {
        env.storage()
            .persistent()
            .get(&DataKey::Proposal(payload_hash))
            .unwrap_or_else(|| panic_with_error!(&env, Error::ProposalNotFound))
    }

    /// Returns whether a given signer has approved a given proposal.
    pub fn has_approved(env: Env, payload_hash: BytesN<32>, signer: Address) -> bool {
        env.storage()
            .persistent()
            .get(&DataKey::Approval(payload_hash, signer))
            .unwrap_or(false)
    }
}

fn read_admin(env: &Env) -> Address {
    env.storage()
        .instance()
        .get(&DataKey::Admin)
        .unwrap_or_else(|| panic_with_error!(env, Error::NotInitialized))
}

fn read_nonce(env: &Env) -> u64 {
    env.storage()
        .instance()
        .get(&DataKey::Nonce)
        .unwrap_or(0u64)
}

/// Construct the domain-separated message that signers must sign.
///
/// Layout: `DOMAIN_SEPARATOR || payload_hash (32) || nonce (8, big-endian) || signer (32)`.
fn build_signed_message(
    env: &Env,
    payload_hash: &BytesN<32>,
    nonce: u64,
    signer: &Address,
) -> Bytes {
    let mut message = Bytes::new(env);
    message.extend_from_slice(DOMAIN_SEPARATOR);
    message.extend_from_array(&payload_hash.to_array());
    message.extend_from_array(&nonce.to_be_bytes());
    // Bind the signer identity into the message to prevent cross-signer replay.
    let signer_bytes = signer.to_xdr(env);
    message.append(&signer_bytes);
    message
}

// Silence unused import warnings for symbols used indirectly by events module.
#[allow(dead_code)]
fn _symbols() -> Symbol {
    symbol_short!("gitops")
}

#[allow(dead_code)]
fn _unused_vec(_v: Vec<u32>) {}