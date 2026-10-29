// Copyright 2024 Stellar-K8s Contributors
// SPDX-License-Identifier: Apache-2.0
//! PodDisruptionBudget auto-tuning controller for validator high availability.
//!
//! Node maintenance (`kubectl drain` for OS patches, autoscaler scale-downs,
//! ...) evicts pods voluntarily. If several validators of one quorum are
//! evicted together — or one is evicted while another is still catching up —
//! the quorum can be lost. A static PodDisruptionBudget cannot express that,
//! because whether a disruption is safe depends on live SCP sync state.
//!
//! This module keeps one `PodDisruptionBudget` per validator group and
//! retunes its `spec.maxUnavailable` continuously:
//!
//! * `0` while any validator in the group is syncing, catching up, unhealthy,
//!   or has not been stable for [`PdbPolicy::stability_window`], and whenever
//!   evicting one validator would break the quorum;
//! * `1` only when every validator is synced and stable **and** the quorum
//!   survives losing any single one.
//!
//! # Layout
//!
//! * [`calculator`] — pure decision logic and the quorum-safety proof. No I/O.
//! * [`observation`] — turns `StellarNode` JSON into observations and groups
//!   validators. No I/O.
//! * [`reconciler`] — manifest generation, the update guard, and
//!   [`PdbController`], the loop that drives a [`PdbCluster`]. Generic over the
//!   cluster so it is testable without Kubernetes.
//! * `kube_backend` (feature `kube-backend`) — the real cluster: watches
//!   `StellarNode` resources and server-side-applies PDBs.
//!
//! See `docs/pdb-auto-tuning.md` for deployment, RBAC and the drain demo.
//!
//! [`PdbPolicy::stability_window`]: calculator::PdbPolicy::stability_window

pub mod calculator;
#[cfg(feature = "kube-backend")]
pub mod kube_backend;
pub mod observation;
pub mod reconciler;

pub use calculator::{
    calculate, default_quorum_threshold, full_mesh_topology, PdbDecision, PdbPolicy, PdbReason,
    SyncState, ValidatorObservation, BLOCKED_MAX_UNAVAILABLE, RELAXED_MAX_UNAVAILABLE,
};
pub use observation::{group_validators, observe_stellar_node, ObservedValidator, ValidatorGroup};
pub use reconciler::{
    build_pdb_manifest, plan_group, GroupPlan, ManagedPdb, PdbCluster, PdbController, PdbError,
    PlanAction, ReconcileReport,
};
