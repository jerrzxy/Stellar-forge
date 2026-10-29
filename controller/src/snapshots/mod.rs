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

//! Ephemeral Kubernetes Storage Snapshot Controller (Issue #243).
//!
//! This module implements the snapshot lifecycle for Stellar validator PVCs,
//! enabling near-instant horizontal scaling by cloning active Persistent Volumes
//! instead of performing full ledger catch-up from scratch.
//!
//! # Modules
//!
//! - [`volume`] — Kubernetes VolumeSnapshot watcher/reconciler, fsync coordination
//!   with Stellar Core, and PVC auto-generation from snapshots.
//! - [`cloud_api`] — Cloud-provider API adapters for AWS EBS and GCP PD crash-
//!   consistent snapshot creation and lifecycle management.

pub mod cloud_api;
pub mod volume;

pub use cloud_api::{
    CloudProvider, CloudSnapshotError, CloudSnapshotResult, EbsClientConfig, EbsSnapshotClient,
    GcpClientConfig, GcpSnapshotClient, MockSnapshotClient, SnapshotClient,
};
pub use volume::{
    PvcCloneSpec, SnapshotController, SnapshotControllerConfig, SnapshotError, SnapshotPhase,
    SnapshotRequest, SnapshotStatus,
};
