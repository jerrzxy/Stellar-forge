//! Standardized GitOps event schema for the decentralized autonomous GitOps
//! webhook manager.
//!
//! The emitted events are intentionally lightweight: they carry only hashes,
//! commit SHAs / IPFS CIDs and a monotonically increasing nonce. Off-chain
//! listeners (GitHub Actions, GitLab CI, etc.) consume these events and use
//! the referenced artifacts to perform the actual infrastructure mutation.
//!
//! # Topic layout
//!
//! Every GitOps trigger event is published with two topics:
//!
//! ```text
//! topics = ( Symbol("gitops"), Symbol("approved") )
//! ```
//!
//! The first topic is a stable namespace (`gitops`) so listeners can filter
//! all events emitted by this contract. The second topic identifies the event
//! kind (`approved`) so future event kinds can be added without breaking
//! existing listeners.
//!
//! # Payload layout
//!
//! The data payload is a `GitOpsEvent` struct encoded as a Soroban `Val`:
//!
//! ```text
//! GitOpsEvent {
//!     proposal_id: u64,        // on-chain governance proposal identifier
//!     payload_hash: BytesN<32>,// hash of the proposed infrastructure payload
//!     artifact: Bytes,         // Git commit SHA or IPFS CID (UTF-8 bytes)
//!     nonce: u64,              // strictly increasing replay-protection nonce
//! }
//! ```
//!
//! Listeners MUST reject any event whose `nonce` is not strictly greater than
//! the last nonce they observed for this contract. The contract itself also
//! enforces monotonic nonces so that a replayed event can never be produced
//! on-chain.

use soroban_sdk::{contracttype, Bytes, BytesN, Env, Symbol};

/// Topic namespace for all events emitted by the GitOps manager.
pub const TOPIC_NAMESPACE: &str = "gitops";

/// Topic identifying an approved-proposal trigger event.
pub const TOPIC_APPROVED: &str = "approved";

/// Standardized payload emitted when a governance proposal is approved and
/// its infrastructure payload hash has been validated by the required
/// multi-signature threshold.
///
/// The payload is deliberately small to minimize ledger bloat: it references
/// off-chain artifacts (Git commit SHAs or IPFS CIDs) rather than embedding
/// raw YAML manifests.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitOpsEvent {
    /// The on-chain governance proposal identifier that was approved.
    pub proposal_id: u64,
    /// Hash of the proposed infrastructure payload that was signed off.
    pub payload_hash: BytesN<32>,
    /// Reference to the off-chain artifact: either a Git commit SHA or an
    /// IPFS CID, encoded as UTF-8 bytes.
    pub artifact: Bytes,
    /// Strictly increasing nonce used for replay protection.
    pub nonce: u64,
}

/// Returns the topic tuple used for approved-proposal GitOps events.
///
/// Listeners should filter on `(gitops, approved)`.
pub fn approved_topics(env: &Env) -> (Symbol, Symbol) {
    (
        Symbol::new(env, TOPIC_NAMESPACE),
        Symbol::new(env, TOPIC_APPROVED),
    )
}

/// Publishes a standardized GitOps trigger event.
///
/// The caller is responsible for having validated the multi-signature
/// threshold and for supplying a nonce that is strictly greater than any
/// previously emitted nonce (the contract enforces this in `lib.rs`).
pub fn emit_approved(env: &Env, event: &GitOpsEvent) {
    let (ns, kind) = approved_topics(env);
    env.events().publish((ns, kind), event.clone());
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::testutils::Events;
    use soroban_sdk::{vec, Env, IntoVal};

    fn sample_event(env: &Env, nonce: u64) -> GitOpsEvent {
        GitOpsEvent {
            proposal_id: 42,
            payload_hash: BytesN::from_array(env, &[7u8; 32]),
            artifact: Bytes::from_slice(env, b"a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0"),
            nonce,
        }
    }

    #[test]
    fn emits_standardized_topics_and_payload() {
        let env = Env::default();
        let event = sample_event(&env, 1);

        emit_approved(&env, &event);

        let events = env.events().all();
        assert_eq!(events.len(), 1);

        let (contract, topics, data) = events.get(0).unwrap();
        assert_eq!(contract, env.current_contract_address());

        let expected_topics = vec![
            &env,
            Symbol::new(&env, TOPIC_NAMESPACE).into_val(&env),
            Symbol::new(&env, TOPIC_APPROVED).into_val(&env),
        ];
        assert_eq!(topics, expected_topics);

        let decoded: GitOpsEvent = data.into_val(&env);
        assert_eq!(decoded, event);
    }

    #[test]
    fn payload_is_lightweight() {
        let env = Env::default();
        let event = sample_event(&env, 1);
        // Artifact must be a short reference (SHA / CID), not a raw manifest.
        assert!(event.artifact.len() <= 128);
    }

    #[test]
    fn rejects_replayed_nonce() {
        // Simulate an off-chain listener that tracks the last seen nonce.
        let env = Env::default();
        let mut last_nonce: Option<u64> = None;

        let accept = |event: &GitOpsEvent, last: &mut Option<u64>| -> bool {
            match *last {
                Some(prev) if event.nonce <= prev => false,
                _ => {
                    *last = Some(event.nonce);
                    true
                }
            }
        };

        let first = sample_event(&env, 10);
        assert!(accept(&first, &mut last_nonce));

        // Exact replay of the same event must be rejected.
        assert!(!accept(&first, &mut last_nonce));

        // A lower nonce must also be rejected.
        let stale = sample_event(&env, 9);
        assert!(!accept(&stale, &mut last_nonce));

        // A strictly greater nonce is accepted.
        let next = sample_event(&env, 11);
        assert!(accept(&next, &mut last_nonce));
    }

    #[test]
    fn distinct_nonces_produce_distinct_events() {
        let env = Env::default();
        let a = sample_event(&env, 1);
        let b = sample_event(&env, 2);
        assert_ne!(a, b);
    }
}