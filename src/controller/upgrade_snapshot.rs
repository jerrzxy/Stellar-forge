//! Pre-upgrade CSI VolumeSnapshot gating for Validator StatefulSet rollouts.
//!
//! Snapshot waiting is implemented with controller requeues — never blocking the
//! Tokio runtime on CSI completion.

use kube::api::{Api, Patch, PatchParams};
use kube::{Client, ResourceExt};
use tracing::{info, warn};

use crate::controller::snapshot::VolumeSnapshotReadiness;
use crate::crd::{BackupConfig, StellarNode, StellarNodeSpec};
use crate::error::{Error, Result};

pub const PRE_UPGRADE_SNAPSHOT_NAME_ANNOTATION: &str = "stellar.org/pre-upgrade-snapshot";
pub const PRE_UPGRADE_SNAPSHOT_TARGET_VERSION_ANNOTATION: &str =
    "stellar.org/pre-upgrade-snapshot-target-version";
pub const PRE_UPGRADE_SNAPSHOT_STARTED_AT_ANNOTATION: &str =
    "stellar.org/pre-upgrade-snapshot-started-at";
pub const PRE_UPGRADE_SNAPSHOT_STATUS_ANNOTATION: &str = "stellar.org/pre-upgrade-snapshot-status";

/// Outcome of one non-blocking snapshot-gate evaluation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpgradeSnapshotDecision {
    /// No snapshot required, or snapshot is ReadyToUse — apply the image update.
    Proceed,
    /// Snapshot requested or still pending — pause the StatefulSet and requeue.
    Requeue { reason: String },
    /// CSI failure / timeout — halt the upgrade.
    Halt { reason: String },
}

/// Resolve the snapshot config from `spec.upgradeSafety` (preferred) or `spec.backupConfig`.
pub fn effective_backup_config(spec: &StellarNodeSpec) -> Option<BackupConfig> {
    if let Some(safety) = &spec.upgrade_safety {
        if safety.enabled {
            return Some(safety.to_backup_config());
        }
    }
    spec.backup_config.clone()
}

/// Decide whether a version change may proceed given CSI snapshot readiness.
pub fn evaluate_upgrade_snapshot(
    current_version: Option<&str>,
    desired_version: &str,
    snapshot_name: Option<&str>,
    snapshot_target_version: Option<&str>,
    readiness: Option<&VolumeSnapshotReadiness>,
    elapsed_secs: i64,
    timeout_secs: u64,
) -> UpgradeSnapshotDecision {
    if current_version.is_none() {
        return UpgradeSnapshotDecision::Proceed;
    }
    if current_version == Some(desired_version) {
        return UpgradeSnapshotDecision::Proceed;
    }

    match snapshot_name {
        None => UpgradeSnapshotDecision::Requeue {
            reason: format!("request CSI snapshot before upgrading to {desired_version}"),
        },
        Some(name) if snapshot_target_version != Some(desired_version) => {
            UpgradeSnapshotDecision::Requeue {
                reason: format!("rebuild snapshot {name} for target {desired_version}"),
            }
        }
        Some(name) => match readiness {
            None | Some(VolumeSnapshotReadiness::Pending) => {
                if elapsed_secs >= timeout_secs as i64 {
                    UpgradeSnapshotDecision::Halt {
                        reason: format!(
                            "snapshot {name} did not become ReadyToUse within {timeout_secs}s (CSI timeout)"
                        ),
                    }
                } else {
                    UpgradeSnapshotDecision::Requeue {
                        reason: format!(
                            "waiting for snapshot {name} ReadyToUse (elapsed {elapsed_secs}s, timeout {timeout_secs}s)"
                        ),
                    }
                }
            }
            Some(VolumeSnapshotReadiness::Ready) => UpgradeSnapshotDecision::Proceed,
            Some(VolumeSnapshotReadiness::Failed(reason)) => UpgradeSnapshotDecision::Halt {
                reason: format!("snapshot {name} failed: {reason}. Upgrade halted."),
            },
        },
    }
}

/// Partition value that freezes a StatefulSet rolling update.
pub fn pause_partition(replicas: i32) -> i32 {
    replicas.max(0)
}

/// Patch `spec.updateStrategy.rollingUpdate.partition` to freeze pod replacements.
pub async fn pause_statefulset_rollout(client: &Client, node: &StellarNode) -> Result<()> {
    patch_statefulset_partition(client, node, pause_partition(node.spec.replicas)).await
}

/// Resume rollout by resetting partition to 0.
pub async fn resume_statefulset_rollout(client: &Client, node: &StellarNode) -> Result<()> {
    patch_statefulset_partition(client, node, 0).await
}

async fn patch_statefulset_partition(
    client: &Client,
    node: &StellarNode,
    partition: i32,
) -> Result<()> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let name = node.name_any();
    let api: Api<k8s_openapi::api::apps::v1::StatefulSet> =
        Api::namespaced(client.clone(), &namespace);

    let patch = serde_json::json!({
        "spec": {
            "updateStrategy": {
                "type": "RollingUpdate",
                "rollingUpdate": { "partition": partition }
            }
        }
    });

    match api
        .patch(&name, &PatchParams::apply("stellar-operator"), &Patch::Merge(&patch))
        .await
    {
        Ok(_) => {
            info!(
                "StatefulSet {}/{} rollout partition set to {}",
                namespace, name, partition
            );
            Ok(())
        }
        Err(kube::Error::Api(e)) if e.code == 404 => {
            warn!(
                "StatefulSet {}/{} not found while adjusting rollout partition; skipping",
                namespace, name
            );
            Ok(())
        }
        Err(e) => Err(Error::KubeError(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_when_no_existing_workload() {
        assert_eq!(
            evaluate_upgrade_snapshot(None, "v22", None, None, None, 0, 300),
            UpgradeSnapshotDecision::Proceed
        );
    }

    #[test]
    fn skip_when_versions_match() {
        assert_eq!(
            evaluate_upgrade_snapshot(Some("v21"), "v21", None, None, None, 0, 300),
            UpgradeSnapshotDecision::Proceed
        );
    }

    #[test]
    fn request_snapshot_on_upgrade() {
        match evaluate_upgrade_snapshot(Some("v21"), "v22", None, None, None, 0, 300) {
            UpgradeSnapshotDecision::Requeue { reason } => {
                assert!(reason.contains("request CSI snapshot"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn pause_while_csi_pending_then_resume_when_ready() {
        let pending = VolumeSnapshotReadiness::Pending;
        match evaluate_upgrade_snapshot(
            Some("v21"),
            "v22",
            Some("snap-1"),
            Some("v22"),
            Some(&pending),
            12,
            300,
        ) {
            UpgradeSnapshotDecision::Requeue { reason } => {
                assert!(reason.contains("waiting"));
                assert!(reason.contains("12s"));
            }
            other => panic!("unexpected {other:?}"),
        }

        let delayed = VolumeSnapshotReadiness::Pending;
        match evaluate_upgrade_snapshot(
            Some("v21"),
            "v22",
            Some("snap-1"),
            Some("v22"),
            Some(&delayed),
            90,
            300,
        ) {
            UpgradeSnapshotDecision::Requeue { .. } => {}
            other => panic!("delayed CSI should still requeue, got {other:?}"),
        }

        let ready = VolumeSnapshotReadiness::Ready;
        assert_eq!(
            evaluate_upgrade_snapshot(
                Some("v21"),
                "v22",
                Some("snap-1"),
                Some("v22"),
                Some(&ready),
                91,
                300,
            ),
            UpgradeSnapshotDecision::Proceed
        );
    }

    #[test]
    fn halt_on_snapshot_failure() {
        let failed = VolumeSnapshotReadiness::Failed("CSI driver unavailable".into());
        match evaluate_upgrade_snapshot(
            Some("v21"),
            "v22",
            Some("snap-1"),
            Some("v22"),
            Some(&failed),
            5,
            300,
        ) {
            UpgradeSnapshotDecision::Halt { reason } => {
                assert!(reason.contains("CSI driver unavailable"));
                assert!(reason.contains("Upgrade halted"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn halt_on_csi_timeout() {
        let pending = VolumeSnapshotReadiness::Pending;
        match evaluate_upgrade_snapshot(
            Some("v21"),
            "v22",
            Some("snap-1"),
            Some("v22"),
            Some(&pending),
            300,
            300,
        ) {
            UpgradeSnapshotDecision::Halt { reason } => {
                assert!(reason.contains("ReadyToUse"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn pause_partition_holds_all_replicas() {
        assert_eq!(pause_partition(3), 3);
        assert_eq!(pause_partition(0), 0);
    }

    #[test]
    fn upgrade_safety_maps_to_backup_config() {
        let safety = crate::crd::UpgradeSafetyConfig {
            enabled: true,
            volume_snapshot_class_name: Some("csi-snap".into()),
            flush_before_snapshot: true,
            ready_timeout_seconds: 120,
            encryption_key_ref: None,
            halt_on_csi_unavailable: true,
        };
        let cfg = safety.to_backup_config();
        assert_eq!(cfg.volume_snapshot_class_name.as_deref(), Some("csi-snap"));
        assert!(cfg.flush_before_snapshot);
        assert_eq!(cfg.ready_timeout_seconds, 120);
    }
}
