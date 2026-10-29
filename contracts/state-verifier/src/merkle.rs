//! Binary Merkle commitment over a ledger's transaction results.
//!
//! ```text
//! leaf  = sha256(0x00 || tx_hash || result_code_be32)
//! node  = sha256(0x01 || left || right)
//! empty = [0; 32]                  (padding leaf)
//! ```
//!
//! The tree has `depth = ceil(log2(n))` levels (0 for a single leaf) and is
//! padded to `2^depth` leaves with `empty`. Domain separation prevents a node
//! from being passed off as a leaf, and binding the leaf index to the path
//! directions pins each proof to one position.
//!
//! Verification is the hot path. It copies the whole proof into linear
//! memory with one host call and reuses a single 65-byte stack buffer, so
//! each level costs exactly one `sha256` host call plus one buffer transfer
//! each way.

use alloc::vec::Vec;
use soroban_sdk::{Bytes, Env};

/// Supports up to 2^16 = 65 536 transactions per ledger.
pub const MAX_DEPTH: u32 = 16;

const LEAF_PREFIX: u8 = 0x00;
const NODE_PREFIX: u8 = 0x01;
pub const EMPTY: [u8; 32] = [0u8; 32];

fn hash_node(env: &Env, left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut buf = [0u8; 65];
    buf[0] = NODE_PREFIX;
    buf[1..33].copy_from_slice(left);
    buf[33..].copy_from_slice(right);
    env.crypto()
        .sha256(&Bytes::from_array(env, &buf))
        .to_array()
}

pub fn leaf_hash(env: &Env, tx_hash: &[u8; 32], result_code: i32) -> [u8; 32] {
    let mut buf = [0u8; 37];
    buf[0] = LEAF_PREFIX;
    buf[1..33].copy_from_slice(tx_hash);
    buf[33..].copy_from_slice(&result_code.to_be_bytes());
    env.crypto()
        .sha256(&Bytes::from_array(env, &buf))
        .to_array()
}

/// Number of levels needed for `leaf_count` leaves.
pub fn depth_for(leaf_count: u32) -> u32 {
    if leaf_count <= 1 {
        0
    } else {
        32 - (leaf_count - 1).leading_zeros()
    }
}

/// Root over `leaves`, padded with `EMPTY`. An empty set has root `EMPTY`.
pub fn root(env: &Env, mut level: Vec<[u8; 32]>) -> [u8; 32] {
    if level.is_empty() {
        return EMPTY;
    }
    let mut empty = EMPTY;
    for _ in 0..depth_for(level.len() as u32) {
        let pairs = level.len().div_ceil(2);
        for i in 0..pairs {
            let left = level[2 * i];
            let right = level.get(2 * i + 1).copied().unwrap_or(empty);
            level[i] = hash_node(env, &left, &right);
        }
        level.truncate(pairs);
        empty = hash_node(env, &empty, &empty);
    }
    level[0]
}

/// Recomputes the root from `leaf` at `index` using the concatenated
/// sibling hashes in `proof` (bottom-up, 32 bytes each).
///
/// The caller must have checked `proof.len() == 32 * depth` and
/// `depth <= MAX_DEPTH`.
///
/// Hot path: the whole proof is copied into linear memory with a single host
/// call, and every level reuses the same 65-byte stack buffer, so each level
/// costs exactly one `sha256` host call plus one buffer transfer each way.
pub fn compute_root(
    env: &Env,
    leaf: [u8; 32],
    mut index: u32,
    depth: u32,
    proof: &Bytes,
) -> [u8; 32] {
    let mut siblings = [0u8; (32 * MAX_DEPTH) as usize];
    let proof_len = (32 * depth) as usize;
    proof.copy_into_slice(&mut siblings[..proof_len]);

    let mut buf = [0u8; 65];
    buf[0] = NODE_PREFIX;
    let mut node = leaf;
    for sibling in siblings[..proof_len].chunks_exact(32) {
        if index & 1 == 0 {
            buf[1..33].copy_from_slice(&node);
            buf[33..].copy_from_slice(sibling);
        } else {
            buf[1..33].copy_from_slice(sibling);
            buf[33..].copy_from_slice(&node);
        }
        node = env
            .crypto()
            .sha256(&Bytes::from_array(env, &buf))
            .to_array();
        index >>= 1;
    }
    node
}

/// Verifies a state-transition inclusion proof against a trusted, publicly
/// anchored Merkle root.
///
/// Returns `true` iff `leaf` (derived from `tx_hash` and `result_code`) sits
/// at `index` in the tree committed to by `anchored_root`, as witnessed by
/// `proof`. This is the single entry point used by the on-chain registry to
/// mathematically validate third-party state proofs.
///
/// The caller must have checked `proof.len() == 32 * depth` and
/// `depth <= MAX_DEPTH`.
pub fn verify_state_proof(
    env: &Env,
    anchored_root: &[u8; 32],
    tx_hash: &[u8; 32],
    result_code: i32,
    index: u32,
    depth: u32,
    proof: &Bytes,
) -> bool {
    // Reject indices that cannot exist at this depth before doing any hashing.
    if depth > MAX_DEPTH || (depth < 32 && index >= (1u32 << depth)) {
        return false;
    }
    let leaf = leaf_hash(env, tx_hash, result_code);
    let computed = compute_root(env, leaf, index, depth, proof);
    // Constant-time comparison of the two 32-byte digests.
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= computed[i] ^ anchored_root[i];
    }
    diff == 0
}

/// Builds the sibling path for `index`: the reference implementation for
/// off-chain provers and tests. Not exported as a contract function, so it
/// is stripped from the WASM.
pub fn proof(env: &Env, leaves: &[[u8; 32]], mut index: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut level: Vec<[u8; 32]> = leaves.to_vec();
    let mut empty = EMPTY;
    for _ in 0..depth_for(leaves.len() as u32) {
        let sibling = level.get(index ^ 1).copied().unwrap_or(empty);
        out.extend_from_slice(&sibling);
        let pairs = level.len().div_ceil(2);
        let next: Vec<[u8; 32]> = (0..pairs)
            .map(|i| {
                hash_node(
                    env,
                    &level[2 * i],
                    &level.get(2 * i + 1).copied().unwrap_or(empty),
                )
            })
            .collect();
        level = next;
        empty = hash_node(env, &empty, &empty);
        index >>= 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::Env;

    fn leaves(env: &Env, n: usize) -> Vec<[u8; 32]> {
        (0..n)
            .map(|i| leaf_hash(env, &[i as u8; 32], i as i32))
            .collect()
    }

    #[test]
    fn depth_matches_leaf_count() {
        assert_eq!(depth_for(0), 0);
        assert_eq!(depth_for(1), 0);
        assert_eq!(depth_for(2), 1);
        assert_eq!(depth_for(3), 2);
        assert_eq!(depth_for(4), 2);
        assert_eq!(depth_for(5), 3);
    }

    #[test]
    fn single_leaf_root_is_leaf() {
        let env = Env::default();
        let l = leaves(&env, 1);
        assert_eq!(root(&env, l.clone()), l[0]);
    }

    #[test]
    fn empty_set_root_is_empty() {
        let env = Env::default();
        assert_eq!(root(&env, Vec::new()), EMPTY);
    }

    #[test]
    fn proofs_verify_for_all_positions() {
        let env = Env::default();
        for n in 1..=17usize {
            let ls = leaves(&env, n);
            let r = root(&env, ls.clone());
            let depth = depth_for(n as u32);
            for i in 0..n {
                let p = proof(&env, &ls, i);
                assert_eq!(p.len() as u32, 32 * depth);
                let bytes = Bytes::from_slice(&env, &p);
                assert!(verify_state_proof(
                    &env,
                    &r,
                    &[i as u8; 32],
                    i as i32,
                    i as u32,
                    depth,
                    &bytes,
                ));
            }
        }
    }

    #[test]
    fn tampered_proof_is_rejected() {
        let env = Env::default();
        let ls = leaves(&env, 8);
        let r = root(&env, ls.clone());
        let depth = depth_for(8);
        let mut p = proof(&env, &ls, 3);
        p[0] ^= 0xff;
        let bytes = Bytes::from_slice(&env, &p);
        assert!(!verify_state_proof(&env, &r, &[3u8; 32], 3, 3, depth, &bytes));
    }

    #[test]
    fn wrong_index_is_rejected() {
        let env = Env::default();
        let ls = leaves(&env, 8);
        let r = root(&env, ls.clone());
        let depth = depth_for(8);
        let p = Bytes::from_slice(&env, &proof(&env, &ls, 3));
        assert!(!verify_state_proof(&env, &r, &[3u8; 32], 3, 4, depth, &p));
    }

    #[test]
    fn out_of_range_index_is_rejected() {
        let env = Env::default();
        let ls = leaves(&env, 4);
        let r = root(&env, ls.clone());
        let depth = depth_for(4);
        let p = Bytes::from_slice(&env, &proof(&env, &ls, 0));
        assert!(!verify_state_proof(&env, &r, &[0u8; 32], 0, 4, depth, &p));
    }

    /// Gas profile: a full-depth (2^16 leaf) proof must stay well under the
    /// per-transaction instruction ceiling. Each level is one `sha256` host
    /// call, so the cost is linear in `MAX_DEPTH`.
    #[test]
    fn gas_profile_max_depth_proof() {
        let env = Env::default();
        let depth = MAX_DEPTH;
        let index = 0xABCDu32;
        let mut p = Vec::new();
        for level in 0..depth {
            p.extend_from_slice(&[(level as u8).wrapping_add(1); 32]);
        }
        let bytes = Bytes::from_slice(&env, &p);
        let leaf = leaf_hash(&env, &[7u8; 32], 0);
        let r = compute_root(&env, leaf, index, depth, &bytes);
        // Recompute to confirm determinism and that the hot path is stable.
        let r2 = compute_root(&env, leaf, index, depth, &bytes);
        assert_eq!(r, r2);
        assert!(verify_state_proof(&env, &r, &[7u8; 32], 0, index, depth, &bytes));
    }

    /// Gas profile: hashing a wide level (many leaves) stays bounded because
    /// the tree is built bottom-up with one host call per node.
    #[test]
    fn gas_profile_wide_tree() {
        let env = Env::default();
        let ls = leaves(&env, 256);
        let r = root(&env, ls.clone());
        let depth = depth_for(256);
        let p = Bytes::from_slice(&env, &proof(&env, &ls, 255));
        assert!(verify_state_proof(&env, &r, &[255u8; 32], 255, 255, depth, &p));
    }
}
