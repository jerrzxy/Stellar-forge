# NFT Bridge Vault (ERC-721 ⇄ Soroban)

This contract moves NFTs between Soroban and Ethereum. Native Soroban NFTs
are locked in this vault while they circulate on Ethereum. NFTs locked on
Ethereum are represented on Soroban by synthetic tokens.

## Bridge flows

| Part | Status |
|------|--------|
| Token registry recording each token's origin (`Native` or `Synthetic`) | ✅ [`src/vault.rs`](src/vault.rs) |
| ERC-721-style views: `name`, `symbol`, `owner_of`, `balance_of`, `token` (IDs are `U256`, like `uint256`) | ✅ |
| Admin minting of native NFTs (`mint`) | ✅ |
| `lock`: moves a native NFT into the vault and emits `native_locked` | ✅ |
| Guard: synthetic tokens can't be locked into the native vault | ✅ |
| Guard: a token can't be locked twice | ✅ |
| `mint_synthetic`: relayer-authorized mint with source and deposit replay protection | ✅ |
| `burn_synthetic`: burn and emit canonical Ethereum NFT release data | ✅ |
| `unlock_native`: relayer-authorized release tied to lock nonce and chain | ✅ |
| Owner-authorized `transfer`, metadata views, and native metadata updates | ✅ |
| Round-trip tests for both directions | ✅ |

The constructor requires an `admin` and a `relayer` address. Configure the
relayer as the federated bridge multi-signature contract; synthetic issuance
and native unlock both require that address to authorize the invocation. Admin
authorization is required to rotate the relayer. Synthetic metadata is set at
mint and cannot be changed through this contract.

## Lock event

`lock(owner, token_id, dest_chain_id, dest_address)` requires the owner's
authorisation.

- It rejects:
  - a destination chain ID of `0`;
  - an all-zero Ethereum address;
  - a caller who doesn't own the token;
  - a token that isn't native;
  - a token that is already locked.
- The vault's own address becomes the token's owner.
- It emits this event:

```text
topics: ["native_locked", token_id: U256, owner: Address]
data:   { dest_chain_id: u64, dest_address: BytesN<20>, nonce: u64 }
```

`nonce` is unique for each vault and always increases. Relayers use it to
discard duplicate lock events.

## Return flows

For Ethereum → Soroban, `mint_synthetic` records the source chain, ERC-721
contract and token ID, along with the finalized deposit ID. It rejects a
replayed deposit ID and a second live representation of the same source NFT.
`burn_synthetic(owner, token_id, dest_address)` requires owner authorization,
burns the representation, and emits `synthetic_burned` with the canonical
Ethereum NFT identity and destination address.

For Soroban → Ethereum → Soroban, `unlock_native` requires relayer
authorization, the exact nonce emitted by `native_locked`, and the same source
chain the token was locked to. A return event ID is consumed once, then the
native NFT is returned to its recipient.

Replay markers use Soroban persistent storage with a 90-day TTL and are renewed
when accessed. Production deployments must maintain token and bridge state
before its TTL expires, as with other persistent contract entries.

## Build & test

```sh
cargo test
cargo build --target wasm32v1-none --release
```
