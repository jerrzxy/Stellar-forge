// Copyright 2024 Stellar-K8s Contributors
// Licensed under the Apache License, Version 2.0 (the "License")
//! Finalizer handling and cleanup recovery for StellarNode resources.
//!
//! ## Modules
//!
//! - [`cleanup`] — background GC controller that recovers stuck finalizers
//!   after controller crashes, with cloud storage verification.
//! - [`cloud_verify`] — cloud provider (AWS EBS / GCP PD) volume-attachment
//!   verification to ensure volumes are detached before finalizer removal.
//!
//! ## Safety Architecture
//!
//! ```text
//!  StellarNode (Terminating)
//!       │
//!       ▼
//!  cleanup::scan_and_recover()
//!       │
//!       ├─ find_owned_pvcs()     ← lists PVCs by ownerRef / name pattern
//!       │
//!       ├─ CloudVerifier::verify_volume_detached()  ← for EACH PVC
//!       │       │
//!       │       ├─ AWS EBS: EC2 DescribeVolumes API
//!       │       └─ GCP PD:  Compute Disk GET API
//!       │
//!       └─ strip_finalizer()    ← ONLY if ALL PVCs confirmed detached
//! ```
//!
//! No finalizer is ever removed without cloud API confirmation.

pub mod cleanup;
pub mod cloud_verify;

// Re-export everything from the original flat finalizers module so existing
// call-sites (`super::finalizers::STELLAR_NODE_FINALIZER`, etc.) continue to
// work without modification.
use kube::{
    api::{Api, Patch, PatchParams},
    Client, ResourceExt,
};
use serde_json::json;
use tracing::info;

use crate::crd::StellarNode;
use crate::error::Result;

/// Finalizer name used to protect StellarNode resources.
pub const STELLAR_NODE_FINALIZER: &str = "stellarnode.stellar.org/finalizer";

/// Add our finalizer to a StellarNode if not already present.
pub async fn add_finalizer(client: &Client, node: &StellarNode) -> Result<()> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<StellarNode> = Api::namespaced(client.clone(), &namespace);

    let mut finalizers = node.finalizers().to_vec();
    if !finalizers.contains(&STELLAR_NODE_FINALIZER.to_string()) {
        finalizers.push(STELLAR_NODE_FINALIZER.to_string());
        let patch = json!({ "metadata": { "finalizers": finalizers } });
        api.patch(
            &node.name_any(),
            &PatchParams::apply("stellar-operator"),
            &Patch::Merge(&patch),
        )
        .await?;
        info!("Added finalizer to StellarNode: {}", node.name_any());
    }
    Ok(())
}

/// Remove our finalizer after cleanup is complete.
pub async fn remove_finalizer(client: &Client, node: &StellarNode) -> Result<()> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let api: Api<StellarNode> = Api::namespaced(client.clone(), &namespace);

    let finalizers: Vec<String> = node
        .finalizers()
        .iter()
        .filter(|f| f.as_str() != STELLAR_NODE_FINALIZER)
        .cloned()
        .collect();

    let patch = json!({ "metadata": { "finalizers": finalizers } });
    api.patch(
        &node.name_any(),
        &PatchParams::apply("stellar-operator"),
        &Patch::Merge(&patch),
    )
    .await?;

    info!("Removed finalizer from StellarNode: {}", node.name_any());
    Ok(())
}

/// Returns `true` when the node has a non-zero `deletionTimestamp`.
pub fn is_being_deleted(node: &StellarNode) -> bool {
    node.metadata.deletion_timestamp.is_some()
}

/// Returns `true` when the node carries our finalizer.
pub fn has_finalizer(node: &StellarNode) -> bool {
    node.finalizers()
        .iter()
        .any(|f| f == STELLAR_NODE_FINALIZER)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{
        NodeType, ResourceRequirements, ResourceSpec, StellarNetwork, StellarNodeSpec,
        StorageConfig,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    use kube::api::ObjectMeta;

    fn create_test_spec() -> StellarNodeSpec {
        StellarNodeSpec {
            node_type: NodeType::Validator,
            network: StellarNetwork::Testnet,
            version: "v21.0.0".to_string(),
            resources: ResourceRequirements {
                requests: ResourceSpec {
                    cpu: "500m".to_string(),
                    memory: "1Gi".to_string(),
                },
                limits: ResourceSpec {
                    cpu: "2".to_string(),
                    memory: "4Gi".to_string(),
                },
            },
            storage: StorageConfig {
                storage_class: "standard".to_string(),
                size: "100Gi".to_string(),
                ..Default::default()
            },
            replicas: 1,
            ..Default::default()
        }
    }

    #[test]
    fn test_finalizer_name() {
        assert_eq!(STELLAR_NODE_FINALIZER, "stellarnode.stellar.org/finalizer");
    }

    #[test]
    fn test_has_finalizer_returns_true_when_present() {
        let node = StellarNode {
            metadata: ObjectMeta {
                name: Some("test-node".to_string()),
                namespace: Some("default".to_string()),
                finalizers: Some(vec![STELLAR_NODE_FINALIZER.to_string()]),
                ..Default::default()
            },
            spec: create_test_spec(),
            status: None,
        };
        assert!(has_finalizer(&node));
    }

    #[test]
    fn test_has_finalizer_returns_false_when_absent() {
        let node = StellarNode {
            metadata: ObjectMeta {
                name: Some("test-node".to_string()),
                namespace: Some("default".to_string()),
                finalizers: Some(vec!["other.finalizer/test".to_string()]),
                ..Default::default()
            },
            spec: create_test_spec(),
            status: None,
        };
        assert!(!has_finalizer(&node));
    }

    #[test]
    fn test_is_being_deleted_returns_true_when_deletion_timestamp_set() {
        let node = StellarNode {
            metadata: ObjectMeta {
                name: Some("test-node".to_string()),
                namespace: Some("default".to_string()),
                deletion_timestamp: Some(Time(chrono::Utc::now())),
                finalizers: Some(vec![STELLAR_NODE_FINALIZER.to_string()]),
                ..Default::default()
            },
            spec: create_test_spec(),
            status: None,
        };
        assert!(is_being_deleted(&node));
    }

    #[test]
    fn test_is_being_deleted_returns_false_when_no_deletion_timestamp() {
        let node = StellarNode {
            metadata: ObjectMeta {
                name: Some("test-node".to_string()),
                namespace: Some("default".to_string()),
                deletion_timestamp: None,
                finalizers: Some(vec![STELLAR_NODE_FINALIZER.to_string()]),
                ..Default::default()
            },
            spec: create_test_spec(),
            status: None,
        };
        assert!(!is_being_deleted(&node));
    }

    fn spec_with_retention(policy: crate::crd::types::RetentionPolicy) -> StellarNodeSpec {
        let mut spec = create_test_spec();
        spec.storage.retention_policy = policy;
        spec
    }

    #[test]
    fn test_should_delete_pvc_when_policy_is_delete() {
        let spec = spec_with_retention(crate::crd::types::RetentionPolicy::Delete);
        assert!(spec.should_delete_pvc());
    }

    #[test]
    fn test_should_not_delete_pvc_when_policy_is_retain() {
        let spec = spec_with_retention(crate::crd::types::RetentionPolicy::Retain);
        assert!(!spec.should_delete_pvc());
    }

    #[test]
    fn test_default_retention_policy_is_delete() {
        assert!(create_test_spec().should_delete_pvc());
    }

    #[test]
    fn test_finalizer_present_on_node_with_delete_policy() {
        let node = StellarNode {
            metadata: ObjectMeta {
                name: Some("validator-delete".to_string()),
                namespace: Some("default".to_string()),
                finalizers: Some(vec![STELLAR_NODE_FINALIZER.to_string()]),
                ..Default::default()
            },
            spec: spec_with_retention(crate::crd::types::RetentionPolicy::Delete),
            status: None,
        };
        assert!(has_finalizer(&node));
        assert!(node.spec.should_delete_pvc());
    }

    #[test]
    fn test_finalizer_present_on_node_with_retain_policy() {
        let node = StellarNode {
            metadata: ObjectMeta {
                name: Some("validator-retain".to_string()),
                namespace: Some("default".to_string()),
                finalizers: Some(vec![STELLAR_NODE_FINALIZER.to_string()]),
                ..Default::default()
            },
            spec: spec_with_retention(crate::crd::types::RetentionPolicy::Retain),
            status: None,
        };
        assert!(has_finalizer(&node));
        assert!(!node.spec.should_delete_pvc());
    }

    #[test]
    fn test_retention_policy_roundtrip_delete() {
        let p = crate::crd::types::RetentionPolicy::Delete;
        let json = serde_json::to_string(&p).unwrap();
        let r: crate::crd::types::RetentionPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(p, r);
    }

    #[test]
    fn test_retention_policy_roundtrip_retain() {
        let p = crate::crd::types::RetentionPolicy::Retain;
        let json = serde_json::to_string(&p).unwrap();
        let r: crate::crd::types::RetentionPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(p, r);
    }
}
