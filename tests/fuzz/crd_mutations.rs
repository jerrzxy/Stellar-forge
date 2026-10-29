//! CRD mutation property-based testing harness.
//!
//! Uses `proptest` to generate millions of randomised `StellarNodeSpec` values and
//! multi-step event sequences, then verifies the invariant matrix documented in
//! `docs/development/fuzz-testing.md`.
//!
//! ## Invariants verified
//!
//! | ID  | Invariant                                                                |
//! |-----|--------------------------------------------------------------------------|
//! | I-1 | `spec.validate()` never panics regardless of input                       |
//! | I-2 | `retentionPolicy: Delete` → operator must NOT recreate a deleted PVC     |
//! | I-3 | `Validator` without `validatorConfig` → validation error, not `Ok(())`   |
//! | I-4 | `Horizon` without `horizonConfig`   → validation error, not `Ok(())`    |
//! | I-5 | `SorobanRpc` without `sorobanConfig` → validation error, not `Ok(())`   |
//! | I-6 | `replicas != 1` on Validator → validation error (not panic)              |
//! | I-7 | `network: Custom` without passphrase → validation error                  |
//! | I-8 | Multi-step mutation sequences converge (no infinite loops / panics)      |
//! | I-9 | Validation errors carry a non-empty `how_to_fix` hint                    |
//! | I-10| Swapping `nodeType` mid-stream is safe (error or ok, never panic)        |
//!
//! ## Running
//!
//! ```bash
//! cargo test -p stellar-k8s --features reconciler-fuzz --test crd_mutations
//!
//! # More cases
//! PROPTEST_CASES=10000 cargo test -p stellar-k8s --features reconciler-fuzz --test crd_mutations
//! ```

#![cfg(feature = "reconciler-fuzz")]

use proptest::prelude::*;

use stellar_k8s::crd::{
    HistoryMode, HorizonConfig, NodeType, ResourceRequirements, ResourceSpec, RolloutStrategy,
    SorobanConfig, StellarNetwork, StellarNodeSpec, StorageConfig, ValidatorConfig,
};
// Use the types-module RetentionPolicy (Delete/Retain enum for StorageConfig),
// not the disaster-recovery RetentionPolicy struct.
use stellar_k8s::crd::types::RetentionPolicy;

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn default_resources() -> ResourceRequirements {
    ResourceRequirements {
        requests: ResourceSpec {
            cpu: "500m".to_string(),
            memory: "1Gi".to_string(),
        },
        limits: ResourceSpec {
            cpu: "2".to_string(),
            memory: "4Gi".to_string(),
        },
    }
}

fn storage_with_retention(retention_policy: RetentionPolicy) -> StorageConfig {
    StorageConfig {
        mode: Default::default(),
        storage_class: "standard".to_string(),
        size: "100Gi".to_string(),
        retention_policy,
        annotations: None,
        node_affinity: None,
        snapshot_ref: None,
    }
}

fn default_storage() -> StorageConfig {
    storage_with_retention(RetentionPolicy::Delete)
}

/// A minimal valid Validator spec (all required fields set, replicas=1).
fn valid_validator_spec() -> StellarNodeSpec {
    StellarNodeSpec {
        node_type: NodeType::Validator,
        network: StellarNetwork::Testnet,
        version: "v21.0.0".to_string(),
        history_mode: HistoryMode::default(),
        resources: default_resources(),
        storage: default_storage(),
        validator_config: Some(ValidatorConfig {
            seed_secret_ref: "validator-seed".to_string(),
            seed_secret_source: None,
            quorum_set: None,
            enable_history_archive: false,
            history_archive_urls: vec![],
            catchup_complete: false,
            key_source: Default::default(),
            kms_config: None,
            vl_source: None,
            hsm_config: None,
            external_dns: None,
            known_peers: None,
            quorum_optimization: None,
        }),
        horizon_config: None,
        soroban_config: None,
        replicas: 1,
        suspended: false,
        strategy: RolloutStrategy::default(),
        ..Default::default()
    }
}

/// A minimal valid Horizon spec.
fn valid_horizon_spec() -> StellarNodeSpec {
    StellarNodeSpec {
        node_type: NodeType::Horizon,
        network: StellarNetwork::Testnet,
        version: "v21.0.0".to_string(),
        history_mode: HistoryMode::default(),
        resources: default_resources(),
        storage: default_storage(),
        validator_config: None,
        horizon_config: Some(HorizonConfig {
            database_secret_ref: "horizon-db".to_string(),
            enable_ingest: true,
            stellar_core_url: "http://stellar-core:11626".to_string(),
            ingest_workers: 1,
            enable_experimental_ingestion: false,
            auto_migration: false,
        }),
        soroban_config: None,
        replicas: 2,
        suspended: false,
        strategy: RolloutStrategy::default(),
        ..Default::default()
    }
}

/// A minimal valid SorobanRpc spec.
fn valid_soroban_spec() -> StellarNodeSpec {
    StellarNodeSpec {
        node_type: NodeType::SorobanRpc,
        network: StellarNetwork::Testnet,
        version: "v21.0.0".to_string(),
        history_mode: HistoryMode::default(),
        resources: default_resources(),
        storage: default_storage(),
        validator_config: None,
        horizon_config: None,
        soroban_config: Some(SorobanConfig {
            stellar_core_url: "http://stellar-core:11626".to_string(),
            #[allow(deprecated)]
            captive_core_config: None,
            captive_core_structured_config: None,
            enable_preflight: true,
            max_events_per_request: 10_000,
            cache: None,
        }),
        replicas: 2,
        suspended: false,
        strategy: RolloutStrategy::default(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// proptest strategies
// ---------------------------------------------------------------------------

fn arb_node_type() -> impl Strategy<Value = NodeType> {
    prop_oneof![
        Just(NodeType::Validator),
        Just(NodeType::Horizon),
        Just(NodeType::SorobanRpc),
    ]
}

fn arb_network() -> impl Strategy<Value = StellarNetwork> {
    prop_oneof![
        Just(StellarNetwork::Mainnet),
        Just(StellarNetwork::Testnet),
        Just(StellarNetwork::Futurenet),
        // Custom with a valid DNS-1123 name
        "[a-z][a-z0-9]{0,10}".prop_map(StellarNetwork::Custom),
    ]
}

fn arb_retention_policy() -> impl Strategy<Value = RetentionPolicy> {
    prop_oneof![Just(RetentionPolicy::Delete), Just(RetentionPolicy::Retain)]
}

fn arb_version() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("v21.0.0".to_string()),
        Just("v22.0.0".to_string()),
        // Edge cases: empty, missing v-prefix
        Just("".to_string()),
        Just("21.0.0".to_string()),
    ]
}

fn arb_replica_count() -> impl Strategy<Value = i32> {
    prop_oneof![
        // Valid ranges for Horizon/Soroban
        1i32..=10i32,
        // Exactly zero — never valid
        Just(0i32),
        // Negative — never valid
        (-5i32..=-1i32),
        // Large value
        100i32..=1000i32,
    ]
}

/// Generates a Validator spec with randomised fields that may be invalid.
fn arb_validator_spec() -> impl Strategy<Value = StellarNodeSpec> {
    (
        arb_version(),
        // Validators must be replicas=1 per the CRD rules; fuzz over a few
        // values including 1 (valid) and others (invalid) to exercise both paths.
        prop_oneof![Just(1i32), Just(0i32), 2i32..=5i32],
        prop::bool::ANY, // suspended
        arb_retention_policy(),
        arb_network(),
        prop::bool::ANY, // include validator_config or not
    )
        .prop_map(
            |(version, replicas, suspended, retention, network, has_config)| {
                StellarNodeSpec {
                    node_type: NodeType::Validator,
                    network,
                    version,
                    history_mode: HistoryMode::default(),
                    resources: default_resources(),
                    storage: storage_with_retention(retention),
                    validator_config: if has_config {
                        Some(ValidatorConfig {
                            seed_secret_ref: "validator-seed".to_string(),
                            seed_secret_source: None,
                            quorum_set: None,
                            enable_history_archive: false,
                            history_archive_urls: vec![],
                            catchup_complete: false,
                            key_source: Default::default(),
                            kms_config: None,
                            vl_source: None,
                            hsm_config: None,
                            external_dns: None,
                            known_peers: None,
                            quorum_optimization: None,
                        })
                    } else {
                        None
                    },
                    horizon_config: None,
                    soroban_config: None,
                    replicas,
                    suspended,
                    strategy: RolloutStrategy::default(),
                    ..Default::default()
                }
            },
        )
}

/// Generates a Horizon spec with randomised fields.
fn arb_horizon_spec() -> impl Strategy<Value = StellarNodeSpec> {
    (
        arb_version(),
        arb_replica_count(),
        prop::bool::ANY,
        arb_retention_policy(),
        prop::bool::ANY, // include horizon_config or not
    )
        .prop_map(|(version, replicas, suspended, retention, has_config)| {
            StellarNodeSpec {
                node_type: NodeType::Horizon,
                network: StellarNetwork::Testnet,
                version,
                history_mode: HistoryMode::default(),
                resources: default_resources(),
                storage: storage_with_retention(retention),
                validator_config: None,
                horizon_config: if has_config {
                    Some(HorizonConfig {
                        database_secret_ref: "horizon-db".to_string(),
                        enable_ingest: true,
                        stellar_core_url: "http://stellar-core:11626".to_string(),
                        ingest_workers: 1,
                        enable_experimental_ingestion: false,
                        auto_migration: false,
                    })
                } else {
                    None
                },
                soroban_config: None,
                replicas,
                suspended,
                strategy: RolloutStrategy::default(),
                ..Default::default()
            }
        })
}

/// Generates a SorobanRpc spec with randomised fields.
fn arb_soroban_spec() -> impl Strategy<Value = StellarNodeSpec> {
    (
        arb_version(),
        arb_replica_count(),
        prop::bool::ANY,
        arb_retention_policy(),
        prop::bool::ANY, // include soroban_config or not
    )
        .prop_map(|(version, replicas, suspended, retention, has_config)| {
            StellarNodeSpec {
                node_type: NodeType::SorobanRpc,
                network: StellarNetwork::Testnet,
                version,
                history_mode: HistoryMode::default(),
                resources: default_resources(),
                storage: storage_with_retention(retention),
                validator_config: None,
                horizon_config: None,
                soroban_config: if has_config {
                    Some(SorobanConfig {
                        stellar_core_url: "http://stellar-core:11626".to_string(),
                        #[allow(deprecated)]
                        captive_core_config: None,
                        captive_core_structured_config: None,
                        enable_preflight: true,
                        max_events_per_request: 10_000,
                        cache: None,
                    })
                } else {
                    None
                },
                replicas,
                suspended,
                strategy: RolloutStrategy::default(),
                ..Default::default()
            }
        })
}

/// Composite strategy: any of the three node types, with arbitrary valid/invalid fields.
fn arb_any_spec() -> impl Strategy<Value = StellarNodeSpec> {
    prop_oneof![
        arb_validator_spec(),
        arb_horizon_spec(),
        arb_soroban_spec(),
    ]
}

// ---------------------------------------------------------------------------
// Mutation strategy (models user edits to a live resource)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum SpecMutation {
    ChangeReplicas(i32),
    ToggleSuspended,
    ChangeVersion(String),
    ChangeRetentionPolicy(RetentionPolicy),
    DropValidatorConfig,
    DropHorizonConfig,
    DropSorobanConfig,
    ChangeNodeType(NodeType),
}

fn arb_mutation() -> impl Strategy<Value = SpecMutation> {
    prop_oneof![
        arb_replica_count().prop_map(SpecMutation::ChangeReplicas),
        Just(SpecMutation::ToggleSuspended),
        arb_version().prop_map(SpecMutation::ChangeVersion),
        arb_retention_policy().prop_map(SpecMutation::ChangeRetentionPolicy),
        Just(SpecMutation::DropValidatorConfig),
        Just(SpecMutation::DropHorizonConfig),
        Just(SpecMutation::DropSorobanConfig),
        arb_node_type().prop_map(SpecMutation::ChangeNodeType),
    ]
}

fn apply_mutation(spec: &mut StellarNodeSpec, mutation: &SpecMutation) {
    match mutation {
        SpecMutation::ChangeReplicas(r) => spec.replicas = *r,
        SpecMutation::ToggleSuspended => spec.suspended = !spec.suspended,
        SpecMutation::ChangeVersion(v) => spec.version.clone_from(v),
        SpecMutation::ChangeRetentionPolicy(rp) => spec.storage.retention_policy = rp.clone(),
        SpecMutation::DropValidatorConfig => spec.validator_config = None,
        SpecMutation::DropHorizonConfig => spec.horizon_config = None,
        SpecMutation::DropSorobanConfig => spec.soroban_config = None,
        SpecMutation::ChangeNodeType(nt) => spec.node_type = nt.clone(),
    }
}

// ---------------------------------------------------------------------------
// proptest test cases
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    // -----------------------------------------------------------------------
    // I-1: spec.validate() never panics
    // -----------------------------------------------------------------------

    /// Feeding any generated StellarNodeSpec into validate() must never panic.
    /// A returned Err is acceptable; a panic is the failure condition proptest catches.
    #[test]
    fn prop_spec_validation_never_panics(spec in arb_any_spec()) {
        let _ = spec.validate();
    }

    // -----------------------------------------------------------------------
    // I-2: retentionPolicy: Delete is preserved through validate()
    // -----------------------------------------------------------------------

    /// validate() must not mutate the spec's retention policy.
    /// The downstream reconciler honours this flag to decide whether to delete
    /// a PVC on node removal — if validate() silently changes it, the operator
    /// could either delete data it should keep or keep data it should delete.
    #[test]
    fn prop_delete_retention_preserved_through_validation(
        base in prop_oneof![
            Just(valid_validator_spec()),
            Just(valid_horizon_spec()),
            Just(valid_soroban_spec()),
        ]
    ) {
        let mut spec = base;
        spec.storage.retention_policy = RetentionPolicy::Delete;
        let _ = spec.validate();
        prop_assert_eq!(
            spec.storage.retention_policy,
            RetentionPolicy::Delete,
            "RetentionPolicy::Delete must be preserved identically through validate()"
        );
    }

    // -----------------------------------------------------------------------
    // I-3: Validator without validatorConfig → error
    // -----------------------------------------------------------------------

    /// A Validator spec missing validatorConfig must yield a validation error.
    /// This prevents the reconciler from panicking when it dereferences the
    /// (then None) validatorConfig downstream.
    #[test]
    fn prop_validator_requires_validator_config(suspended in prop::bool::ANY) {
        // Validators must have exactly 1 replica; fix it here so the only
        // error is the missing validatorConfig, not the replica count.
        let spec = StellarNodeSpec {
            node_type: NodeType::Validator,
            validator_config: None,
            replicas: 1,
            suspended,
            ..valid_validator_spec()
        };
        prop_assert!(
            spec.validate().is_err(),
            "Validator spec without validatorConfig must return Err"
        );
    }

    // -----------------------------------------------------------------------
    // I-4: Horizon without horizonConfig → error
    // -----------------------------------------------------------------------

    #[test]
    fn prop_horizon_requires_horizon_config(
        replicas in 1i32..=5i32,
        suspended in prop::bool::ANY,
    ) {
        let spec = StellarNodeSpec {
            node_type: NodeType::Horizon,
            horizon_config: None,
            replicas,
            suspended,
            ..valid_horizon_spec()
        };
        prop_assert!(
            spec.validate().is_err(),
            "Horizon spec without horizonConfig must return Err"
        );
    }

    // -----------------------------------------------------------------------
    // I-5: SorobanRpc without sorobanConfig → error
    // -----------------------------------------------------------------------

    #[test]
    fn prop_soroban_requires_soroban_config(
        replicas in 1i32..=5i32,
        suspended in prop::bool::ANY,
    ) {
        let spec = StellarNodeSpec {
            node_type: NodeType::SorobanRpc,
            soroban_config: None,
            replicas,
            suspended,
            ..valid_soroban_spec()
        };
        prop_assert!(
            spec.validate().is_err(),
            "SorobanRpc spec without sorobanConfig must return Err"
        );
    }

    // -----------------------------------------------------------------------
    // I-6: Validator replicas != 1 → validation error
    // -----------------------------------------------------------------------

    /// Validator nodes must have exactly 1 replica. Any other value must
    /// produce a validation error (not a panic).
    #[test]
    fn prop_invalid_validator_replica_count_is_error(
        // Generate any count except 1
        replicas in prop_oneof![Just(0i32), (-5i32..=-1i32), 2i32..=100i32],
    ) {
        let spec = StellarNodeSpec {
            replicas,
            ..valid_validator_spec()
        };
        prop_assert!(
            spec.validate().is_err(),
            "Validator with replicas={} must return Err", replicas
        );
    }

    // -----------------------------------------------------------------------
    // I-7: network: Custom without passphrase → validation error
    // -----------------------------------------------------------------------

    #[test]
    fn prop_custom_network_requires_passphrase(
        name in "[a-z][a-z0-9]{0,10}",
    ) {
        let spec = StellarNodeSpec {
            network: StellarNetwork::Custom(name),
            custom_network_passphrase: None,
            ..valid_validator_spec()
        };
        prop_assert!(
            spec.validate().is_err(),
            "Custom network without customNetworkPassphrase must return Err"
        );
    }

    // -----------------------------------------------------------------------
    // I-8: Multi-step event sequences converge (no infinite loops / panics)
    // -----------------------------------------------------------------------

    /// Apply a sequence of random mutations to a base spec.
    /// After each step validate() must return in finite time and never panic.
    #[test]
    fn prop_event_sequence_converges(
        base in prop_oneof![
            Just(valid_validator_spec()),
            Just(valid_horizon_spec()),
            Just(valid_soroban_spec()),
        ],
        mutations in prop::collection::vec(arb_mutation(), 0..20),
    ) {
        let mut current = base;
        for mutation in &mutations {
            apply_mutation(&mut current, mutation);
            let _ = current.validate();
        }
        // Final state must also complete without panic
        let _ = current.validate();
    }

    // -----------------------------------------------------------------------
    // I-9: Validation errors carry non-empty how_to_fix hints
    // -----------------------------------------------------------------------

    /// Every SpecValidationError must have a non-empty how_to_fix string so
    /// operators can diagnose issues from logs without reading source code.
    #[test]
    fn prop_validation_errors_have_guidance(spec in arb_any_spec()) {
        if let Err(errors) = spec.validate() {
            for err in &errors {
                prop_assert!(
                    !err.how_to_fix.is_empty(),
                    "Error for field '{}' has empty how_to_fix: {:?}", err.field, err
                );
                prop_assert!(
                    !err.field.is_empty(),
                    "Validation error has empty field name: {:?}", err
                );
                prop_assert!(
                    !err.message.is_empty(),
                    "Validation error has empty message: {:?}", err
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // I-10: Swapping nodeType mid-stream is safe
    // -----------------------------------------------------------------------

    /// Starting from a valid spec, change the nodeType. The result must be
    /// Ok or Err — never a panic — even when the config blocks don't match.
    #[test]
    fn prop_node_type_swap_is_safe(
        base in prop_oneof![
            Just(valid_validator_spec()),
            Just(valid_horizon_spec()),
            Just(valid_soroban_spec()),
        ],
        new_type in arb_node_type(),
    ) {
        let mut spec = base;
        spec.node_type = new_type;
        let _ = spec.validate();
    }
}

// ---------------------------------------------------------------------------
// Deterministic unit tests — fast regression coverage without proptest overhead
// ---------------------------------------------------------------------------

#[test]
#[cfg(feature = "reconciler-fuzz")]
fn validator_missing_config_returns_error_with_field_name() {
    let spec = StellarNodeSpec {
        node_type: NodeType::Validator,
        validator_config: None,
        replicas: 1,
        ..valid_validator_spec()
    };
    let errors = spec.validate().expect_err("Expected validation errors");
    let fields: Vec<&str> = errors.iter().map(|e| e.field.as_str()).collect();
    assert!(
        fields.iter().any(|f| f.to_lowercase().contains("validatorconfig")),
        "Expected a validatorConfig field error, got: {fields:?}"
    );
}

#[test]
#[cfg(feature = "reconciler-fuzz")]
fn horizon_missing_config_returns_error_with_field_name() {
    let spec = StellarNodeSpec {
        node_type: NodeType::Horizon,
        horizon_config: None,
        replicas: 2,
        ..valid_horizon_spec()
    };
    let errors = spec.validate().expect_err("Expected validation errors");
    let fields: Vec<&str> = errors.iter().map(|e| e.field.as_str()).collect();
    assert!(
        fields.iter().any(|f| f.to_lowercase().contains("horizonconfig")),
        "Expected a horizonConfig field error, got: {fields:?}"
    );
}

#[test]
#[cfg(feature = "reconciler-fuzz")]
fn soroban_missing_config_returns_error_with_field_name() {
    let spec = StellarNodeSpec {
        node_type: NodeType::SorobanRpc,
        soroban_config: None,
        replicas: 2,
        ..valid_soroban_spec()
    };
    let errors = spec.validate().expect_err("Expected validation errors");
    let fields: Vec<&str> = errors.iter().map(|e| e.field.as_str()).collect();
    assert!(
        fields.iter().any(|f| f.to_lowercase().contains("sorobanconfig")),
        "Expected a sorobanConfig field error, got: {fields:?}"
    );
}

#[test]
#[cfg(feature = "reconciler-fuzz")]
fn validator_with_two_replicas_returns_error() {
    let spec = StellarNodeSpec {
        replicas: 2,
        ..valid_validator_spec()
    };
    assert!(
        spec.validate().is_err(),
        "Validator with replicas=2 must be a validation error"
    );
}

#[test]
#[cfg(feature = "reconciler-fuzz")]
fn validator_with_zero_replicas_returns_error() {
    let spec = StellarNodeSpec {
        replicas: 0,
        ..valid_validator_spec()
    };
    assert!(
        spec.validate().is_err(),
        "Validator with replicas=0 must be a validation error"
    );
}

#[test]
#[cfg(feature = "reconciler-fuzz")]
fn retention_policy_delete_preserved_by_validate() {
    let spec = StellarNodeSpec {
        storage: storage_with_retention(RetentionPolicy::Delete),
        ..valid_validator_spec()
    };
    assert!(
        spec.validate().is_ok(),
        "retentionPolicy: Delete on a fully valid Validator spec must be Ok"
    );
    // Verify the field wasn't mutated
    assert_eq!(spec.storage.retention_policy, RetentionPolicy::Delete);
}

#[test]
#[cfg(feature = "reconciler-fuzz")]
fn retention_policy_retain_is_valid() {
    let spec = StellarNodeSpec {
        storage: storage_with_retention(RetentionPolicy::Retain),
        ..valid_validator_spec()
    };
    assert!(
        spec.validate().is_ok(),
        "retentionPolicy: Retain on a fully valid Validator spec must be Ok"
    );
}

#[test]
#[cfg(feature = "reconciler-fuzz")]
fn custom_network_without_passphrase_is_error() {
    let spec = StellarNodeSpec {
        network: StellarNetwork::Custom("my-testnet".to_string()),
        custom_network_passphrase: None,
        ..valid_validator_spec()
    };
    assert!(
        spec.validate().is_err(),
        "Custom network without passphrase must be a validation error"
    );
}

#[test]
#[cfg(feature = "reconciler-fuzz")]
fn custom_network_with_passphrase_is_ok() {
    let spec = StellarNodeSpec {
        network: StellarNetwork::Custom("my-testnet".to_string()),
        custom_network_passphrase: Some("My Private Network ; 2024".to_string()),
        ..valid_validator_spec()
    };
    assert!(
        spec.validate().is_ok(),
        "Custom network with passphrase on a valid spec must be Ok"
    );
}

#[test]
#[cfg(feature = "reconciler-fuzz")]
fn all_validation_errors_have_guidance() {
    // Deliberately broken spec: multiple violations at once
    let spec = StellarNodeSpec {
        node_type: NodeType::Validator,
        validator_config: None, // missing
        replicas: 5,            // must be 1 for validator
        ..valid_validator_spec()
    };
    let errors = spec.validate().expect_err("Expected validation errors");
    for err in &errors {
        assert!(!err.how_to_fix.is_empty(), "Empty how_to_fix: {err:?}");
        assert!(!err.field.is_empty(), "Empty field: {err:?}");
        assert!(!err.message.is_empty(), "Empty message: {err:?}");
    }
}
