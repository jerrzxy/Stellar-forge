// Copyright 2024 Stellar-K8s Contributors
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// Copyright 2024 Stellar-K8s Contributors
// SPDX-License-Identifier: Apache-2.0
//! Deterministic Stellar key derivation for ephemeral testnets.
//!
//! Two derivations are implemented:
//!
//! * The **genesis root account** of a standalone network. Stellar Core seeds
//!   the root keypair with `sha256(network passphrase)` (see
//!   `LedgerManagerImpl::startNewLedger` and the `stellar/quickstart` startup
//!   script), so the operator can surface the root account without ever
//!   talking to the node.
//! * **Pre-funded test accounts** declared in `spec.accounts`. Each account
//!   name is hashed together with the passphrase, namespace and resource name
//!   so repeated reconciles always produce the same keypair, and two
//!   testnets never collide.

use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha256};

/// Strkey version byte for ed25519 public keys (`G...`).
const VERSION_BYTE_ACCOUNT_ID: u8 = 6 << 3;
/// Strkey version byte for ed25519 seeds (`S...`).
const VERSION_BYTE_SEED: u8 = 18 << 3;

/// Domain separator for pre-funded account derivation.
const ACCOUNT_DOMAIN: &[u8] = b"stellar-k8s/ephemeral-account/v1";

const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// A Stellar keypair in strkey form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPair {
    /// Public account id (`G...`).
    pub public: String,
    /// Secret seed (`S...`).
    pub secret: String,
}

/// SHA-256 of the UTF-8 network passphrase; this 32-byte value is both the
/// Stellar Core network id and the seed of the genesis root account.
pub fn network_id(passphrase: &str) -> [u8; 32] {
    Sha256::digest(passphrase.as_bytes()).into()
}

/// Derive the genesis root keypair of a standalone network from its
/// passphrase. Matches Stellar Core's `SecretKey::fromSeed(networkId)`.
pub fn root_keypair(passphrase: &str) -> KeyPair {
    keypair_from_seed(&network_id(passphrase))
}

/// Derive the pre-funded keypair for an account declared in
/// `spec.accounts`. Deterministic for a given (passphrase, namespace,
/// testnet name, account name) tuple.
pub fn account_keypair(passphrase: &str, namespace: &str, testnet: &str, account: &str) -> KeyPair {
    let mut hasher = Sha256::new();
    hasher.update(ACCOUNT_DOMAIN);
    for part in [passphrase, namespace, testnet, account] {
        // Length-prefix each component so that ("ab", "c") and ("a", "bc")
        // can never hash to the same seed.
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    let seed: [u8; 32] = hasher.finalize().into();
    keypair_from_seed(&seed)
}

/// Build a keypair from a raw 32-byte ed25519 seed.
pub fn keypair_from_seed(seed: &[u8; 32]) -> KeyPair {
    let signing_key = SigningKey::from_bytes(seed);
    KeyPair {
        public: strkey_encode(
            VERSION_BYTE_ACCOUNT_ID,
            &signing_key.verifying_key().to_bytes(),
        ),
        secret: strkey_encode(VERSION_BYTE_SEED, seed),
    }
}

/// Encode a payload as Stellar strkey: `base32(version || payload || crc16)`.
fn strkey_encode(version: u8, payload: &[u8]) -> String {
    let mut data = Vec::with_capacity(payload.len() + 3);
    data.push(version);
    data.extend_from_slice(payload);
    let checksum = crc16_xmodem(&data);
    data.extend_from_slice(&checksum.to_le_bytes());
    base32_encode(&data)
}

/// RFC 4648 base32 without padding, as used by Stellar strkeys.
fn base32_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity((input.len() * 8).div_ceil(5));
    let mut buffer: u64 = 0;
    let mut bits: u32 = 0;
    for byte in input {
        buffer = (buffer << 8) | u64::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let index = ((buffer >> bits) & 0x1f) as usize;
            out.push(BASE32_ALPHABET[index] as char);
        }
    }
    if bits > 0 {
        let index = ((buffer << (5 - bits)) & 0x1f) as usize;
        out.push(BASE32_ALPHABET[index] as char);
    }
    out
}

/// CRC-16/XMODEM (poly 0x1021, init 0, no reflection, no final xor), the
/// checksum Stellar appends to strkeys.
fn crc16_xmodem(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for byte in data {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Documented genesis root account of the standalone network used by
    /// `stellar/quickstart --local`. Verifies the whole pipeline:
    /// sha256(passphrase) -> ed25519 pubkey -> strkey.
    const STANDALONE_ROOT_ACCOUNT: &str =
        "GBZXN7PIRZGNMHGA7MUUUF4GWPY5AYPV6LY4UV2GL6VJGIQRXFDNMADI";

    #[test]
    fn crc16_xmodem_known_vector() {
        // CRC-16/XMODEM("123456789") == 0x31C3
        assert_eq!(crc16_xmodem(b"123456789"), 0x31c3);
    }

    #[test]
    fn standalone_root_account_matches_stellar_core() {
        let root = root_keypair("Standalone Network ; February 2017");
        assert_eq!(root.public, STANDALONE_ROOT_ACCOUNT);
    }

    #[test]
    fn root_keypair_is_deterministic() {
        let a = root_keypair("Standalone Network ; February 2017");
        let b = root_keypair("Standalone Network ; February 2017");
        assert_eq!(a, b);
        let other = root_keypair("Some Other Network ; February 2017");
        assert_ne!(a.public, other.public);
    }

    #[test]
    fn strkeys_have_expected_shape() {
        let root = root_keypair("Standalone Network ; February 2017");
        assert_eq!(root.public.len(), 56);
        assert_eq!(root.secret.len(), 56);
        assert!(root.public.starts_with('G'));
        assert!(root.secret.starts_with('S'));
        assert!(root
            .public
            .chars()
            .all(|c| "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567".contains(c)));
    }

    #[test]
    fn account_keys_are_deterministic_and_unique() {
        let a = account_keypair("net", "ns", "testnet", "deployer");
        let b = account_keypair("net", "ns", "testnet", "deployer");
        assert_eq!(a, b);

        let other_account = account_keypair("net", "ns", "testnet", "signer");
        let other_testnet = account_keypair("net", "ns", "other", "deployer");
        let other_ns = account_keypair("net", "other", "testnet", "deployer");
        for key in [&other_account, &other_testnet, &other_ns] {
            assert_ne!(a.public, key.public);
        }
        assert_ne!(a.public, a.secret);
    }

    #[test]
    fn account_seed_components_are_unambiguous() {
        // Length prefixing must prevent ("ab", "c") == ("a", "bc") collisions.
        let a = account_keypair("ab", "c", "t", "acc");
        let b = account_keypair("a", "c", "t", "acc");
        let mut hasher = Sha256::new();
        hasher.update(ACCOUNT_DOMAIN);
        hasher.update((2u64).to_be_bytes());
        hasher.update(b"ab");
        hasher.update((1u64).to_be_bytes());
        hasher.update(b"c");
        hasher.update((1u64).to_be_bytes());
        hasher.update(b"t");
        hasher.update((3u64).to_be_bytes());
        hasher.update(b"acc");
        let expected: [u8; 32] = hasher.finalize().into();
        assert_eq!(a, keypair_from_seed(&expected));
        assert_ne!(a.public, b.public);
    }

    #[test]
    fn base32_pads_final_group() {
        assert_eq!(base32_encode(b""), "");
        assert_eq!(base32_encode(b"f"), "MY");
        assert_eq!(base32_encode(b"fo"), "MZXQ");
        assert_eq!(base32_encode(b"foo"), "MZXW6");
        assert_eq!(base32_encode(b"foob"), "MZXW6YQ");
        assert_eq!(base32_encode(b"fooba"), "MZXW6YTB");
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
    }
}
