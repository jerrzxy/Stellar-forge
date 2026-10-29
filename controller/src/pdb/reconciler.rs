// Copyright 2024 Stellar-K8s Contributors
// SPDX-License-Identifier: Apache-2.0
//! Reconciles `PodDisruptionBudget`s for validator groups.
//!
//! [`PdbController`] is the "controller thread": it repeatedly asks a
//! [`PdbCluster`] for the current validator groups, runs
//! [`calculator::calculate`](super::calculator::calculate) for each, and
//! creates / updates / garbage-collects the PDBs accordingly.
//!
//! It is generic over [`PdbCluster`] so the logic can be exercised with an
//! in-memory fake; `kube_backend::KubeCluster` (feature `kube-backend`) is the
//! real implementation.
//!
//! # Safety properties
//!
//! * **Fail closed.** If the cluster cannot be observed, nothing is changed (a
//!   stale PDB is better than a deleted one). Validators with unknown health
//!   block disruption.
//! * **Never loosen unverified.** [`plan_group`] refuses any decision that
//!   relaxes `maxUnavailable` without a quorum check
//!   ([`PdbDecision::violates_quorum_invariant`]) and holds the PDB at `0`.
//! * **Tightening is always applied**, immediately and unconditionally.
//! * **Only managed PDBs are touched**: those labelled
//!   `app.kubernetes.io/managed-by=stellar-pdb-controller`.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Display;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use super::calculator::{
    calculate, full_mesh_topology, PdbDecision, PdbPolicy, PdbReason, BLOCKED_MAX_UNAVAILABLE,
};
use super::observation::ValidatorGroup;

/// Server-side-apply field manager used for every write.
pub const FIELD_MANAGER: &str = "stellar-pdb-controller";
/// Label marking PDBs owned by this controller.
pub const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
/// Value of [`MANAGED_BY_LABEL`].
pub const MANAGED_BY_VALUE: &str = "stellar-pdb-controller";
/// Label carrying the group's network key.
pub const GROUP_LABEL: &str = "stellar.org/validator-group";
/// Annotation with the machine-readable reason for the current value.
pub const REASON_ANNOTATION: &str = "stellar.org/pdb-reason";
/// Annotation listing the validators the PDB covers.
pub const MEMBERS_ANNOTATION: &str = "stellar.org/pdb-members";

/// Errors from a [`PdbCluster`].
#[derive(Debug, thiserror::Error)]
pub enum PdbError {
    /// The cluster backend failed (API error, serialization, ...).
    #[error("cluster error: {0}")]
    Cluster(String),
}

impl PdbError {
    /// Wrap any displayable error.
    pub fn cluster(e: impl Display) -> Self {
        PdbError::Cluster(e.to_string())
    }
}

/// A PDB this controller owns, as currently stored in the cluster.
#[derive(Clone, Debug, PartialEq)]
pub struct ManagedPdb {
    /// Namespace.
    pub namespace: String,
    /// Name.
    pub name: String,
    /// The PDB's `spec` as JSON.
    pub spec: Value,
    /// Value of the [`REASON_ANNOTATION`], if present.
    pub reason: Option<String>,
}

/// What the controller needs from a cluster.
///
/// Implementations must only return PDBs carrying
/// `app.kubernetes.io/managed-by=stellar-pdb-controller` from
/// [`list_managed_pdbs`](Self::list_managed_pdbs): everything returned there is
/// eligible for garbage collection.
pub trait PdbCluster: Send + Sync {
    /// Current validator groups, with health observed *now*.
    fn observe_groups(&self) -> impl Future<Output = Result<Vec<ValidatorGroup>, PdbError>> + Send;

    /// PDBs previously created by this controller.
    fn list_managed_pdbs(&self) -> impl Future<Output = Result<Vec<ManagedPdb>, PdbError>> + Send;

    /// Create or update a PDB from `manifest` (idempotent apply).
    fn apply_pdb(&self, manifest: &Value) -> impl Future<Output = Result<(), PdbError>> + Send;

    /// Delete a PDB. Deleting a PDB that does not exist is not an error.
    fn delete_pdb(
        &self,
        namespace: &str,
        name: &str,
    ) -> impl Future<Output = Result<(), PdbError>> + Send;
}

/// What to do with one group's PDB.
#[derive(Clone, Debug, PartialEq)]
pub enum PlanAction {
    /// No PDB exists yet; create it from this manifest.
    Create(Value),
    /// A PDB exists but differs; apply this manifest.
    Update(Value),
    /// The PDB already matches.
    Unchanged,
}

/// The plan for one group.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupPlan {
    /// Namespace of the PDB.
    pub namespace: String,
    /// Name of the PDB.
    pub name: String,
    /// The decision that will be applied (after the guard).
    pub decision: PdbDecision,
    /// `true` if the calculator's decision was refused by the safety guard.
    pub held: bool,
    /// What to do.
    pub action: PlanAction,
}

/// Build the `policy/v1` PodDisruptionBudget manifest for `group`.
///
/// The selector matches the operator's validator pods by
/// `app.kubernetes.io/name=stellar-node`, `stellar.org/node-type=Validator`
/// and `app.kubernetes.io/instance In (<member names>)`. Matching by instance
/// avoids re-deriving the operator's network label (custom networks use an
/// unstable hash there). Pods must be owned by a scalable controller
/// (validators are StatefulSet pods) for `maxUnavailable` to be honoured.
pub fn build_pdb_manifest(group: &ValidatorGroup, decision: &PdbDecision) -> Value {
    json!({
        "apiVersion": "policy/v1",
        "kind": "PodDisruptionBudget",
        "metadata": {
            "name": group.pdb_name(),
            "namespace": group.namespace,
            "labels": {
                "app.kubernetes.io/name": "stellar-validator-pdb",
                MANAGED_BY_LABEL: MANAGED_BY_VALUE,
                GROUP_LABEL: group.network_key,
            },
            "annotations": {
                REASON_ANNOTATION: decision.reason.code(),
                MEMBERS_ANNOTATION: group.member_ids().join(","),
            },
        },
        "spec": {
            "maxUnavailable": decision.max_unavailable,
            "selector": {
                "matchLabels": {
                    "app.kubernetes.io/name": "stellar-node",
                    "stellar.org/node-type": "Validator",
                },
                "matchExpressions": [{
                    "key": "app.kubernetes.io/instance",
                    "operator": "In",
                    "values": group.member_ids(),
                }],
            },
        },
    })
}

/// Decide what to do with `group`'s PDB given what currently exists.
///
/// This is where the quorum guard lives: a decision that relaxes
/// `maxUnavailable` without a verified quorum check is refused and the PDB is
/// held at `0` ([`GroupPlan::held`]).
pub fn plan_group(
    group: &ValidatorGroup,
    current: Option<&ManagedPdb>,
    policy: &PdbPolicy,
) -> GroupPlan {
    let topology = full_mesh_topology(&group.member_ids(), group.min_quorum);
    let mut decision = calculate(&group.members, &topology, &[], policy);

    let mut held = false;
    if decision.violates_quorum_invariant() {
        held = true;
        decision = PdbDecision {
            max_unavailable: BLOCKED_MAX_UNAVAILABLE,
            reason: PdbReason::TopologyUnsafe(
                "refused to relax maxUnavailable without a quorum check".to_string(),
            ),
            quorum_verified: false,
        };
    }

    let manifest = build_pdb_manifest(group, &decision);
    let action = match current {
        None => PlanAction::Create(manifest),
        Some(cur) if same_as_desired(cur, &manifest, &decision) => PlanAction::Unchanged,
        Some(_) => PlanAction::Update(manifest),
    };

    GroupPlan {
        namespace: group.namespace.clone(),
        name: group.pdb_name(),
        decision,
        held,
        action,
    }
}

/// Compare only the fields this controller owns, so server-side defaulting of
/// other fields never causes endless re-applies.
fn same_as_desired(cur: &ManagedPdb, manifest: &Value, decision: &PdbDecision) -> bool {
    let want = &manifest["spec"];
    cur.spec.get("maxUnavailable") == want.get("maxUnavailable")
        && cur.spec.get("selector") == want.get("selector")
        && cur.reason.as_deref() == Some(decision.reason.code())
}

/// Outcome of one reconcile pass for a single PDB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupReport {
    /// PDB namespace.
    pub namespace: String,
    /// PDB name.
    pub name: String,
    /// The `maxUnavailable` now in force (or intended, if the write failed).
    pub max_unavailable: u32,
    /// Kebab-case reason code.
    pub reason: &'static str,
    /// `created`, `updated`, `unchanged`, `held` or `error`.
    pub outcome: &'static str,
}

/// Outcome of one [`PdbController::reconcile_once`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// One entry per validator group.
    pub groups: Vec<GroupReport>,
    /// PDBs created.
    pub created: usize,
    /// PDBs updated.
    pub updated: usize,
    /// PDBs already correct.
    pub unchanged: usize,
    /// Orphaned managed PDBs deleted.
    pub deleted: usize,
    /// Decisions refused by the safety guard.
    pub held: usize,
    /// Per-PDB failures. A failure on one group does not stop the others.
    pub errors: Vec<String>,
}

/// Drives a [`PdbCluster`]: the PDB auto-tuning controller.
pub struct PdbController<C: PdbCluster> {
    cluster: C,
    policy: PdbPolicy,
    trigger: Arc<Notify>,
}

impl<C: PdbCluster> PdbController<C> {
    /// Create a controller.
    pub fn new(cluster: C, policy: PdbPolicy) -> Self {
        Self {
            cluster,
            policy,
            trigger: Arc::new(Notify::new()),
        }
    }

    /// Handle that makes [`run`](Self::run) reconcile immediately. A cluster
    /// backend calls `notify_one()` on it when a `StellarNode` changes, so
    /// tightening happens in real time rather than at the next tick.
    pub fn trigger(&self) -> Arc<Notify> {
        Arc::clone(&self.trigger)
    }

    /// The cluster this controller drives.
    pub fn cluster(&self) -> &C {
        &self.cluster
    }

    /// One full reconcile pass.
    ///
    /// Returns `Err` only if the cluster could not be observed, in which case
    /// nothing was changed. Failures writing individual PDBs are collected in
    /// [`ReconcileReport::errors`].
    pub async fn reconcile_once(&self) -> Result<ReconcileReport, PdbError> {
        let groups = self.cluster.observe_groups().await?;
        let managed = self.cluster.list_managed_pdbs().await?;

        let mut existing: HashMap<(String, String), ManagedPdb> = managed
            .into_iter()
            .map(|p| ((p.namespace.clone(), p.name.clone()), p))
            .collect();

        let mut report = ReconcileReport::default();

        for group in &groups {
            let current = existing.remove(&(group.namespace.clone(), group.pdb_name()));
            let plan = plan_group(group, current.as_ref(), &self.policy);
            let reason = plan.decision.reason.code();
            let max_unavailable = plan.decision.max_unavailable;

            if plan.held {
                report.held += 1;
                warn!(
                    pdb = %plan.name, namespace = %plan.namespace,
                    "safety guard refused to relax maxUnavailable; holding at 0"
                );
            }

            let (outcome, write) = match plan.action {
                PlanAction::Unchanged => {
                    report.unchanged += 1;
                    ("unchanged", None)
                }
                PlanAction::Create(m) => ("created", Some(m)),
                PlanAction::Update(m) => ("updated", Some(m)),
            };

            let mut outcome = outcome;
            if let Some(manifest) = write {
                match self.cluster.apply_pdb(&manifest).await {
                    Ok(()) => {
                        if outcome == "created" {
                            report.created += 1;
                        } else {
                            report.updated += 1;
                        }
                        info!(
                            pdb = %plan.name, namespace = %plan.namespace,
                            max_unavailable, reason = %plan.decision.reason,
                            "{outcome} PodDisruptionBudget"
                        );
                    }
                    Err(e) => {
                        outcome = "error";
                        error!(pdb = %plan.name, namespace = %plan.namespace, "failed to apply PDB: {e}");
                        report
                            .errors
                            .push(format!("{}/{}: {e}", plan.namespace, plan.name));
                    }
                }
            } else {
                debug!(pdb = %plan.name, namespace = %plan.namespace, max_unavailable, "PDB unchanged");
            }

            report.groups.push(GroupReport {
                namespace: plan.namespace,
                name: plan.name,
                max_unavailable,
                reason,
                outcome: if plan.held && outcome != "error" {
                    "held"
                } else {
                    outcome
                },
            });
        }

        // Managed PDBs whose group no longer exists.
        let mut orphans: BTreeMap<(String, String), ManagedPdb> = existing.into_iter().collect();
        while let Some(((namespace, name), _)) = orphans.pop_first() {
            match self.cluster.delete_pdb(&namespace, &name).await {
                Ok(()) => {
                    report.deleted += 1;
                    info!(pdb = %name, namespace = %namespace, "deleted orphaned PodDisruptionBudget");
                }
                Err(e) => {
                    error!(pdb = %name, namespace = %namespace, "failed to delete orphaned PDB: {e}");
                    report.errors.push(format!("{namespace}/{name}: {e}"));
                }
            }
        }

        Ok(report)
    }

    /// Reconcile every `interval`, and immediately whenever
    /// [`trigger`](Self::trigger) fires, until `shutdown` flips to `true` (or
    /// its sender is dropped). The first pass runs immediately.
    pub async fn run(&self, interval: Duration, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = self.trigger.notified() => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        info!("PDB controller shutting down");
                        return;
                    }
                    continue;
                }
            }

            match self.reconcile_once().await {
                Ok(report) if !report.errors.is_empty() => {
                    warn!(
                        errors = report.errors.len(),
                        "PDB reconcile finished with errors"
                    );
                }
                Ok(report) => debug!(
                    created = report.created,
                    updated = report.updated,
                    unchanged = report.unchanged,
                    deleted = report.deleted,
                    "PDB reconcile complete"
                ),
                Err(e) => warn!("PDB reconcile skipped, cluster not observable: {e}"),
            }
        }
    }
}

/// Handle to a controller running on its own task; see [`PdbController::spawn`].
pub struct PdbControllerHandle {
    shutdown: watch::Sender<bool>,
    join: JoinHandle<()>,
}

impl PdbControllerHandle {
    /// Ask the controller to stop and wait for it to finish.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.join.await;
    }
}

impl<C: PdbCluster + 'static> PdbController<C> {
    /// Run the controller on a dedicated Tokio task (the "controller thread").
    pub fn spawn(self, interval: Duration) -> PdbControllerHandle {
        let (tx, rx) = watch::channel(false);
        let join = tokio::spawn(async move { self.run(interval, rx).await });
        PdbControllerHandle { shutdown: tx, join }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdb::calculator::{SyncState, ValidatorObservation};

    fn group(members: Vec<ValidatorObservation>, min_quorum: Option<usize>) -> ValidatorGroup {
        ValidatorGroup {
            namespace: "stellar".to_string(),
            network_key: "testnet".to_string(),
            members,
            min_quorum,
        }
    }

    fn stable(id: &str) -> ValidatorObservation {
        ValidatorObservation::new(id, SyncState::Synced, Duration::from_secs(3600))
    }

    fn four(synced: bool) -> ValidatorGroup {
        let mut members: Vec<_> = ["v1", "v2", "v3", "v4"].iter().map(|i| stable(i)).collect();
        if !synced {
            members[0] = ValidatorObservation::new("v1", SyncState::CatchingUp, Duration::ZERO);
        }
        group(members, None)
    }

    fn managed(from: &Value) -> ManagedPdb {
        ManagedPdb {
            namespace: "stellar".to_string(),
            name: "stellar-validators-testnet-pdb".to_string(),
            spec: from["spec"].clone(),
            reason: from["metadata"]["annotations"][REASON_ANNOTATION]
                .as_str()
                .map(str::to_string),
        }
    }

    #[test]
    fn manifest_has_expected_shape() {
        let g = four(true);
        let decision = calculate(
            &g.members,
            &full_mesh_topology(&g.member_ids(), None),
            &[],
            &PdbPolicy::default(),
        );
        let m = build_pdb_manifest(&g, &decision);
        assert_eq!(m["apiVersion"], "policy/v1");
        assert_eq!(m["kind"], "PodDisruptionBudget");
        assert_eq!(m["metadata"]["name"], "stellar-validators-testnet-pdb");
        assert_eq!(m["metadata"]["namespace"], "stellar");
        assert_eq!(m["metadata"]["labels"][MANAGED_BY_LABEL], MANAGED_BY_VALUE);
        assert_eq!(m["metadata"]["labels"][GROUP_LABEL], "testnet");
        assert_eq!(
            m["metadata"]["annotations"][REASON_ANNOTATION],
            "synced-and-stable"
        );
        assert_eq!(
            m["metadata"]["annotations"][MEMBERS_ANNOTATION],
            "v1,v2,v3,v4"
        );
        assert_eq!(m["spec"]["maxUnavailable"], 1);
        let sel = &m["spec"]["selector"];
        assert_eq!(sel["matchLabels"]["stellar.org/node-type"], "Validator");
        assert_eq!(
            sel["matchExpressions"][0]["key"],
            "app.kubernetes.io/instance"
        );
        assert_eq!(sel["matchExpressions"][0]["operator"], "In");
        assert_eq!(
            sel["matchExpressions"][0]["values"],
            json!(["v1", "v2", "v3", "v4"])
        );
    }

    #[test]
    fn syncing_group_creates_a_blocking_pdb() {
        let plan = plan_group(&four(false), None, &PdbPolicy::default());
        assert!(!plan.held);
        assert_eq!(plan.decision.max_unavailable, 0);
        match plan.action {
            PlanAction::Create(m) => {
                assert_eq!(m["spec"]["maxUnavailable"], 0);
                assert_eq!(
                    m["metadata"]["annotations"][REASON_ANNOTATION],
                    "catching-up"
                );
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn synced_group_relaxes_an_existing_blocking_pdb() {
        let blocked = plan_group(&four(false), None, &PdbPolicy::default());
        let PlanAction::Create(m) = blocked.action else {
            panic!("expected create");
        };
        let plan = plan_group(&four(true), Some(&managed(&m)), &PdbPolicy::default());
        assert_eq!(plan.decision.max_unavailable, 1);
        assert!(matches!(plan.action, PlanAction::Update(_)));
    }

    #[test]
    fn identical_state_is_unchanged() {
        let first = plan_group(&four(true), None, &PdbPolicy::default());
        let PlanAction::Create(m) = first.action else {
            panic!("expected create");
        };
        let again = plan_group(&four(true), Some(&managed(&m)), &PdbPolicy::default());
        assert_eq!(again.action, PlanAction::Unchanged);
    }

    #[test]
    fn server_defaulted_extra_spec_fields_do_not_cause_updates() {
        let first = plan_group(&four(true), None, &PdbPolicy::default());
        let PlanAction::Create(m) = first.action else {
            panic!("expected create");
        };
        let mut cur = managed(&m);
        cur.spec["unhealthyPodEvictionPolicy"] = json!("IfHealthyBudget");
        let again = plan_group(&four(true), Some(&cur), &PdbPolicy::default());
        assert_eq!(again.action, PlanAction::Unchanged);
    }

    #[test]
    fn membership_change_updates_the_selector_even_if_value_is_same() {
        let first = plan_group(&four(true), None, &PdbPolicy::default());
        let PlanAction::Create(m) = first.action else {
            panic!("expected create");
        };
        let mut five = four(true);
        five.members.push(stable("v5"));
        let plan = plan_group(&five, Some(&managed(&m)), &PdbPolicy::default());
        assert_eq!(plan.decision.max_unavailable, 1);
        assert!(matches!(plan.action, PlanAction::Update(_)));
    }

    #[test]
    fn group_at_the_quorum_floor_is_never_relaxed() {
        let three = group(vec![stable("a"), stable("b"), stable("c")], None);
        let plan = plan_group(&three, None, &PdbPolicy::default());
        assert_eq!(plan.decision.max_unavailable, 0);
        assert_eq!(plan.decision.reason.code(), "quorum-floor");
        assert!(
            !plan.held,
            "calculator already blocked it; the guard was not needed"
        );
    }

    #[test]
    fn tightening_replaces_a_relaxed_pdb() {
        let relaxed = plan_group(&four(true), None, &PdbPolicy::default());
        let PlanAction::Create(m) = relaxed.action else {
            panic!("expected create");
        };
        let plan = plan_group(&four(false), Some(&managed(&m)), &PdbPolicy::default());
        assert_eq!(plan.decision.max_unavailable, 0);
        match plan.action {
            PlanAction::Update(m) => assert_eq!(m["spec"]["maxUnavailable"], 0),
            other => panic!("expected Update, got {other:?}"),
        }
    }

    #[test]
    fn empty_group_is_blocked() {
        let plan = plan_group(&group(vec![], None), None, &PdbPolicy::default());
        assert_eq!(plan.decision.max_unavailable, 0);
        assert_eq!(plan.decision.reason, PdbReason::EmptyGroup);
    }

    #[test]
    fn min_quorum_override_flows_through_the_plan() {
        let three = group(vec![stable("a"), stable("b"), stable("c")], Some(2));
        let plan = plan_group(&three, None, &PdbPolicy::default());
        assert_eq!(plan.decision.max_unavailable, 1);
    }
}