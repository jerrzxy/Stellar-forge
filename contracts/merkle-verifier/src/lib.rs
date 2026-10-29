//! # merkle-verifier
//!
//! A Soroban-native Merkle Tree state-proof verification library written in
//! pure Rust (no `std` heap beyond `Vec`, no recursion).
//!
//! ## Features
//! * **Single-path verification** – verify that a single leaf belongs to a
//!   Merkle tree given its proof path.
//! * **Multi-proof verification** – verify multiple leaves simultaneously in
//!   one O(k log N) pass.
//! * **SHA-256 hashing** – using the `sha2` crate with `no_std`-compatible
//!   feature flags, compatible with Bitcoin, Ethereum, and Stellar ledger header
//!   Merkle structures.
//! * **Stack-safe** – all traversals are iterative to avoid Wasm stack overflow
//!   even at tree depth 32.
//! * **O(log N) instruction scaling** – CPU cost grows logarithmically with
//!   tree depth, fitting within Soroban instruction budgets.
//!
//! ## Modules
//! * [`proof`] – core data types and verification algorithms.
//!
//! ## Quick Start
//! ```rust
//! use merkle_verifier::{hash_leaf, MerkleProof, ProofNode, Side, verify_proof};
//!
//! // Leaf and its expected root (all-zeros here for illustration only).
//! let leaf  = hash_leaf(b"my data");
//! let root  = [0u8; 32]; // replace with the real Merkle root
//! let proof = MerkleProof { leaf, path: vec![] };
//!
//! // Returns true only when the reconstructed root matches.
//! let _ = verify_proof(&proof, &root);
//! ```

pub mod proof;

// Re-export the public surface so callers can write `merkle_verifier::verify_proof`.
pub use proof::{
    hash_leaf, verify_proof, verify_multi_proof,
    Hash, Side, ProofNode, MerkleProof, MultiLeaf, MultiProof,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a proof path for `leaf` given the sibling hashes along the path
    /// from the leaf level up to the root.
    fn proof_for(leaf: Hash, siblings: &[(Hash, Side)]) -> MerkleProof {
        let mut path = Vec::with_capacity(siblings.len());
        for (hash, side) in siblings.iter() {
            path.push(ProofNode { hash: *hash, side: *side });
        }
        MerkleProof { leaf, path }
    }

    /// Recompute a parent hash from a child and its sibling.
    fn combine(child: Hash, sibling: Hash, side: Side) -> Hash {
        match side {
            Side::Left => {
                // sibling is on the left
                let mut buf = [0u8; 64];
                buf[..32].copy_from_slice(&sibling);
                buf[32..].copy_from_slice(&child);
                hash_leaf(&buf)
            }
            Side::Right => {
                // sibling is on the right
                let mut buf = [0u8; 64];
                buf[..32].copy_from_slice(&child);
                buf[32..].copy_from_slice(&sibling);
                hash_leaf(&buf)
            }
        }
    }

    #[test]
    fn single_leaf_empty_path_matches_root() {
        let leaf = hash_leaf(b"historical state payload");
        let root = leaf;
        let proof = MerkleProof { leaf, path: vec![] };
        assert!(verify_proof(&proof, &root));
    }

    #[test]
    fn single_leaf_rejects_wrong_root() {
        let leaf = hash_leaf(b"historical state payload");
        let proof = MerkleProof { leaf, path: vec![] };
        let mut root = leaf;
        root[0] ^= 0xff;
        assert!(!verify_proof(&proof, &root));
    }

    #[test]
    fn historical_state_inclusion_proof_end_to_end() {
        // Simulate a small historical state tree with four leaves.
        let l0 = hash_leaf(b"tx:0");
        let l1 = hash_leaf(b"tx:1");
        let l2 = hash_leaf(b"tx:2");
        let l3 = hash_leaf(b"tx:3");

        let n01 = combine(l0, l1, Side::Right);
        let n23 = combine(l2, l3, Side::Right);
        let root = combine(n01, n23, Side::Right);

        // Prove inclusion of l2.
        let proof = proof_for(l2, &[(l3, Side::Right), (n01, Side::Left)]);
        assert!(verify_proof(&proof, &root));

        // Tampering with the leaf must fail.
        let tampered = MerkleProof {
            leaf: hash_leaf(b"tx:2-tampered"),
            path: proof.path.clone(),
        };
        assert!(!verify_proof(&tampered, &root));
    }

    #[test]
    fn multi_proof_verifies_multiple_leaves() {
        let l0 = hash_leaf(b"a");
        let l1 = hash_leaf(b"b");
        let l2 = hash_leaf(b"c");
        let l3 = hash_leaf(b"d");

        let n01 = combine(l0, l1, Side::Right);
        let n23 = combine(l2, l3, Side::Right);
        let root = combine(n01, n23, Side::Right);

        let leaves = vec![
            MultiLeaf {
                leaf: l0,
                path: proof_for(l0, &[(l1, Side::Right), (n23, Side::Right)]).path,
            },
            MultiLeaf {
                leaf: l2,
                path: proof_for(l2, &[(l3, Side::Right), (n01, Side::Left)]).path,
            },
        ];
        let multi = MultiProof { leaves };
        assert!(verify_multi_proof(&multi, &root));
    }

    #[test]
    fn multi_proof_rejects_invalid_leaf() {
        let l0 = hash_leaf(b"a");
        let l1 = hash_leaf(b"b");
        let n01 = combine(l0, l1, Side::Right);
        let root = n01;

        let leaves = vec![MultiLeaf {
            leaf: hash_leaf(b"not-a"),
            path: proof_for(l0, &[(l1, Side::Right)]).path,
        }];
        let multi = MultiProof { leaves };
        assert!(!verify_multi_proof(&multi, &root));
    }
}
