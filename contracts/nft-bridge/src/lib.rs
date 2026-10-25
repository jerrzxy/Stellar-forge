//! # NFT Bridge Vault (ERC-721 ⇄ Soroban)
//!
//! Moves NFTs between Soroban and Ethereum. Native Soroban NFTs are escrowed
//! in this vault while they circulate on Ethereum. Ethereum NFTs are
//! represented on Soroban by synthetic tokens.
//!
//! The bridge provides:
//!
//! * the token registry with an explicit [`vault::Origin`] per token, so
//!   native and synthetic supply can never be confused;
//! * an ERC-721-style read interface (`name`, `symbol`, `owner_of`,
//!   `balance_of`) adapted to Soroban `Address`es;
//! * admin issuance of native NFTs and owner-authorized transfers;
//! * [`NftBridge::lock`], which escrows a native NFT and emits a
//!   [`NativeLocked`] bridge event carrying the destination chain and
//!   Ethereum address;
//! * synthetic minting authorized by a configured relayer multi-sig, with
//!   replay protection, and burning synthetic NFTs to request Ethereum unlock.

#![no_std]

pub mod vault;

#[cfg(test)]
mod test;

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, panic_with_error, Address,
    BytesN, Env, String, U256,
};

const DAY_IN_LEDGERS: u32 = 17_280;
const INSTANCE_BUMP: u32 = 30 * DAY_IN_LEDGERS;
const INSTANCE_THRESHOLD: u32 = INSTANCE_BUMP - DAY_IN_LEDGERS;
const REPLAY_BUMP: u32 = 90 * DAY_IN_LEDGERS;
const REPLAY_THRESHOLD: u32 = REPLAY_BUMP - DAY_IN_LEDGERS;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum BridgeError {
    UnknownToken = 1,
    TokenExists = 2,
    NotOwner = 3,
    /// Synthetic tokens must be burned to return to Ethereum, never locked.
    SyntheticNotLockable = 4,
    AlreadyLocked = 5,
    InvalidDestination = 6,
    Overflow = 7,
    UnauthorizedRelayer = 8,
    InvalidSource = 9,
    DepositAlreadyProcessed = 10,
    SourceAlreadyMinted = 11,
    NotSynthetic = 12,
    TokenLocked = 13,
    SyntheticMetadataImmutable = 14,
    WrongSourceChain = 15,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Relayer,
    Name,
    Symbol,
    BridgeNonce,
    Token(U256),
    Balance(Address),
    SyntheticSource(u64, BytesN<20>, U256),
    ProcessedDeposit(BytesN<32>),
    ProcessedReturn(BytesN<32>),
}

/// Bridge event observed by the relayers: a native NFT is escrowed and
/// should be minted to `dest_address` on EVM chain `dest_chain_id`.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeLocked {
    #[topic]
    pub token_id: U256,
    #[topic]
    pub owner: Address,
    pub dest_chain_id: u64,
    pub dest_address: BytesN<20>,
    /// Unique, increasing per vault; relayers deduplicate on it.
    pub nonce: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeMinted {
    #[topic]
    pub token_id: U256,
    #[topic]
    pub to: Address,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyntheticMinted {
    #[topic]
    pub token_id: U256,
    #[topic]
    pub to: Address,
    pub source_chain_id: u64,
    pub source_contract: BytesN<20>,
    pub source_token_id: U256,
    pub deposit_id: BytesN<32>,
}

/// A consumed synthetic token authorizes relayers to unlock its canonical
/// Ethereum NFT for `dest_address`.
#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyntheticBurned {
    #[topic]
    pub token_id: U256,
    #[topic]
    pub owner: Address,
    pub dest_chain_id: u64,
    pub dest_contract: BytesN<20>,
    pub dest_token_id: U256,
    pub dest_address: BytesN<20>,
    pub nonce: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeUnlocked {
    #[topic]
    pub token_id: U256,
    #[topic]
    pub to: Address,
    pub source_chain_id: u64,
    pub return_id: BytesN<32>,
}

#[contract]
pub struct NftBridge;

fn bump_instance(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_THRESHOLD, INSTANCE_BUMP);
}

fn has_persistent_key(env: &Env, key: &DataKey) -> bool {
    let storage = env.storage().persistent();
    let exists = storage.has(key);
    if exists {
        storage.extend_ttl(key, REPLAY_THRESHOLD, REPLAY_BUMP);
    }
    exists
}

fn set_replay_marker(env: &Env, key: &DataKey) {
    let storage = env.storage().persistent();
    storage.set(key, &true);
    storage.extend_ttl(key, REPLAY_THRESHOLD, REPLAY_BUMP);
}

#[contractimpl]
impl NftBridge {
    pub fn __constructor(
        env: Env,
        admin: Address,
        relayer: Address,
        name: String,
        symbol: String,
    ) {
        let storage = env.storage().instance();
        storage.set(&DataKey::Admin, &admin);
        storage.set(&DataKey::Relayer, &relayer);
        storage.set(&DataKey::Name, &name);
        storage.set(&DataKey::Symbol, &symbol);
        bump_instance(&env);
    }

    /// Issues a native NFT. Admin only.
    pub fn mint(env: Env, to: Address, token_id: U256) {
        Self::admin(env.clone()).require_auth();
        bump_instance(&env);
        vault::create(&env, &token_id, &to, vault::Origin::Native);
        NativeMinted { token_id, to }.publish(&env);
    }

    /// Mints a Soroban representation of a locked Ethereum NFT. The
    /// configured relayer address is expected to be the bridge's federated
    /// multi-sig; its authorization attests that `deposit_id` was finalized.
    #[allow(clippy::too_many_arguments)]
    pub fn mint_synthetic(
        env: Env,
        to: Address,
        token_id: U256,
        source_chain_id: u64,
        source_contract: BytesN<20>,
        source_token_id: U256,
        deposit_id: BytesN<32>,
        token_uri: String,
    ) {
        Self::relayer(env.clone()).require_auth();
        bump_instance(&env);
        if source_chain_id == 0
            || source_contract.to_array() == [0u8; 20]
            || deposit_id.to_array() == [0u8; 32]
        {
            panic_with_error!(&env, BridgeError::InvalidSource);
        }

        let deposit_key = DataKey::ProcessedDeposit(deposit_id.clone());
        if has_persistent_key(&env, &deposit_key) {
            panic_with_error!(&env, BridgeError::DepositAlreadyProcessed);
        }
        let source_key = DataKey::SyntheticSource(
            source_chain_id,
            source_contract.clone(),
            source_token_id.clone(),
        );
        if has_persistent_key(&env, &source_key) {
            panic_with_error!(&env, BridgeError::SourceAlreadyMinted);
        }

        let source = vault::SourceNft {
            chain_id: source_chain_id,
            contract: source_contract.clone(),
            token_id: source_token_id.clone(),
        };
        vault::create_with_metadata(
            &env,
            &token_id,
            &to,
            vault::Origin::Synthetic,
            &token_uri,
            Some(source),
        );
        let storage = env.storage().persistent();
        storage.set(&source_key, &token_id);
        storage.extend_ttl(&source_key, REPLAY_THRESHOLD, REPLAY_BUMP);
        set_replay_marker(&env, &deposit_key);
        SyntheticMinted {
            token_id,
            to,
            source_chain_id,
            source_contract,
            source_token_id,
            deposit_id,
        }
        .publish(&env);
    }

    /// Escrows the native NFT `token_id` in the vault for transfer to
    /// `dest_address` on EVM chain `dest_chain_id`. Returns the lock nonce.
    pub fn lock(
        env: Env,
        owner: Address,
        token_id: U256,
        dest_chain_id: u64,
        dest_address: BytesN<20>,
    ) -> u64 {
        owner.require_auth();
        bump_instance(&env);
        if dest_chain_id == 0 || dest_address.to_array() == [0u8; 20] {
            panic_with_error!(&env, BridgeError::InvalidDestination);
        }
        let nonce = vault::lock(&env, &owner, &token_id, dest_chain_id);
        NativeLocked {
            token_id,
            owner,
            dest_chain_id,
            dest_address,
            nonce,
        }
        .publish(&env);
        nonce
    }

    /// Burns a synthetic NFT and emits the instruction to release the
    /// canonical Ethereum NFT to `dest_address`.
    pub fn burn_synthetic(
        env: Env,
        owner: Address,
        token_id: U256,
        dest_address: BytesN<20>,
    ) -> u64 {
        owner.require_auth();
        bump_instance(&env);
        if dest_address.to_array() == [0u8; 20] {
            panic_with_error!(&env, BridgeError::InvalidDestination);
        }
        let source = vault::burn_synthetic(&env, &owner, &token_id);
        let nonce = vault::next_bridge_nonce(&env);
        SyntheticBurned {
            token_id,
            owner,
            dest_chain_id: source.chain_id,
            dest_contract: source.contract,
            dest_token_id: source.token_id,
            dest_address,
            nonce,
        }
        .publish(&env);
        nonce
    }

    /// Releases a native NFT after the Ethereum representation has been
    /// burned. The relayer attests to the finalized return event and its ID.
    pub fn unlock_native(
        env: Env,
        token_id: U256,
        lock_nonce: u64,
        source_chain_id: u64,
        return_id: BytesN<32>,
        to: Address,
    ) {
        Self::relayer(env.clone()).require_auth();
        bump_instance(&env);
        if source_chain_id == 0 || return_id.to_array() == [0u8; 32] {
            panic_with_error!(&env, BridgeError::InvalidSource);
        }
        let return_key = DataKey::ProcessedReturn(return_id.clone());
        if has_persistent_key(&env, &return_key) {
            panic_with_error!(&env, BridgeError::DepositAlreadyProcessed);
        }
        vault::unlock_native(&env, &token_id, lock_nonce, source_chain_id, &to);
        set_replay_marker(&env, &return_key);
        NativeUnlocked {
            token_id,
            to,
            source_chain_id,
            return_id,
        }
        .publish(&env);
    }

    /// Transfers an active NFT using Soroban owner authorization.
    pub fn transfer(env: Env, from: Address, to: Address, token_id: U256) {
        from.require_auth();
        bump_instance(&env);
        vault::transfer(&env, &from, &to, &token_id);
    }

    /// Updates metadata for a native token. Synthetic metadata is imported
    /// from Ethereum and immutable through this contract.
    pub fn set_token_uri(env: Env, token_id: U256, token_uri: String) {
        Self::admin(env.clone()).require_auth();
        bump_instance(&env);
        vault::set_token_uri(&env, &token_id, &token_uri);
    }

    /// Changes the federated relayer multi-sig. Admin authorization is
    /// required, and the new address becomes the sole synthetic minter.
    pub fn set_relayer(env: Env, relayer: Address) {
        Self::admin(env.clone()).require_auth();
        bump_instance(&env);
        env.storage().instance().set(&DataKey::Relayer, &relayer);
    }

    // -----------------------------------------------------------------------
    // ERC-721-style views
    // -----------------------------------------------------------------------

    pub fn name(env: Env) -> String {
        env.storage().instance().get(&DataKey::Name).unwrap()
    }

    pub fn symbol(env: Env) -> String {
        env.storage().instance().get(&DataKey::Symbol).unwrap()
    }

    pub fn token_uri(env: Env, token_id: U256) -> String {
        vault::existing_token(&env, &token_id).token_uri
    }

    /// Current owner; the vault's own address while the token is locked.
    pub fn owner_of(env: Env, token_id: U256) -> Address {
        vault::existing_token(&env, &token_id).owner
    }

    pub fn balance_of(env: Env, owner: Address) -> u64 {
        vault::balance(&env, &owner)
    }

    pub fn token(env: Env, token_id: U256) -> Option<vault::TokenRecord> {
        vault::token(&env, &token_id)
    }

    pub fn admin(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Admin).unwrap()
    }

    pub fn relayer(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Relayer).unwrap()
    }
}
