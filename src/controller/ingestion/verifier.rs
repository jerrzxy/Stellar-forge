//! Merkle proof verification for ledger state ingestion.
//!
//! This module provides a controller-facing interface for validating state
//! inclusion proofs during fast-sync bootstrap.

use sha2::{Digest, Sha256};

/// A 32-byte SHA-256 hash.
pub type Hash = [u8; 32];

/// A single step in a Merkle proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofStep {
    /// The sibling hash at this level.
    pub hash: Hash,
    /// When true, the sibling is the left child and the current node is the right child.
    /// When false, the sibling is the right child and the current node is the left child.
    pub sibling_is_left: bool,
}

/// A Merkle proof for a leaf node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MerkleProof {
    /// The ordered list of proof steps from leaf to root.
    pub steps: Vec<ProofStep>,
}

/// Computes the SHA-256 hash of data.
pub fn sha256(data: &[u8]) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&result);
    hash
}

/// Computes the hash of a leaf node from its raw serialized bytes.
pub fn hash_leaf(leaf_data: &[u8]) -> Hash {
    sha256(leaf_data)
}

/// Computes the hash of an internal node from its two child hashes.
pub fn hash_node(left: &Hash, right: &Hash) -> Hash {
    let mut data = [0u8; 64];
    data[..32].copy_from_slice(left);
    data[32..].copy_from_slice(right);
    sha256(&data)
}

/// Verifies a Merkle proof against the expected root hash.
pub fn verify_merkle_proof(proof: &MerkleProof, leaf_hash: &Hash, expected_root: &Hash) -> bool {
    if proof.steps.is_empty() {
        return leaf_hash == expected_root;
    }

    let mut current = *leaf_hash;
    for step in &proof.steps {
        current = if step.sibling_is_left {
            hash_node(&step.hash, &current)
        } else {
            hash_node(&current, &step.hash)
        };
    }

    current == *expected_root
}

/// Verifier for Stellar ledger state Merkle proofs.
///
/// Holds the set of trusted validator public keys used to authenticate
/// fast-sync proofs. The key set can be refreshed after a validator
/// identity rotation so ingestion keeps validating without a restart.
#[derive(Debug, Clone, Default)]
pub struct StateVerifier {
    trusted_keys: Vec<Hash>,
}

impl StateVerifier {
    /// Creates a verifier with the given initial trusted validator keys.
    pub fn new(trusted_keys: Vec<Hash>) -> Self {
        Self { trusted_keys }
    }

    /// Returns the currently trusted validator keys.
    pub fn trusted_keys(&self) -> &[Hash] {
        &self.trusted_keys
    }

    /// Returns true if the given key is currently trusted.
    pub fn is_trusted(&self, key: &Hash) -> bool {
        self.trusted_keys.iter().any(|k| k == key)
    }

    /// Replaces the trusted validator key set, e.g. after a key rotation.
    ///
    /// This is intended to be called once the rotated identity has been
    /// confirmed active so that ingestion continues validating proofs
    /// signed by the new key set.
    pub fn refresh_trusted_keys(&mut self, trusted_keys: Vec<Hash>) {
        self.trusted_keys = trusted_keys;
    }

    /// Adds a single rotated validator key to the trusted set.
    ///
    /// Duplicate keys are ignored. Useful for staggered rotations where
    /// nodes migrate one at a time and old keys must remain valid until
    /// the rotation completes.
    pub fn add_trusted_key(&mut self, key: Hash) {
        if !self.is_trusted(&key) {
            self.trusted_keys.push(key);
        }
    }

    /// Removes a validator key from the trusted set.
    ///
    /// Returns true if the key was present and removed.
    pub fn remove_trusted_key(&mut self, key: &Hash) -> bool {
        let before = self.trusted_keys.len();
        self.trusted_keys.retain(|k| k != key);
        self.trusted_keys.len() != before
    }

    /// Verifies a Merkle proof for the given serialized leaf data.
    pub fn verify_state_proof(
        &self,
        proof: &MerkleProof,
        leaf_data: &[u8],
        expected_root: &Hash,
    ) -> bool {
        let leaf_hash = hash_leaf(leaf_data);
        verify_merkle_proof(proof, &leaf_hash, expected_root)
    }

    /// Verifies a Merkle proof using a precomputed leaf hash.
    pub fn verify_state_proof_with_hash(
        &self,
        proof: &MerkleProof,
        leaf_hash: &Hash,
        expected_root: &Hash,
    ) -> bool {
        verify_merkle_proof(proof, leaf_hash, expected_root)
    }
}
