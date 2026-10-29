// Copyright 2024 Stellar-K8s Contributors
// SPDX-License-Identifier: Apache-2.0
//! Automated storage snapshot and cloud backup controller (issue #84).
//!
//! This module implements an automated persistent-volume snapshot lifecycle
//! across AWS EBS, GCP PD, and local storage providers.  It provides:
//!
//! - [`cloud::SnapshotProvider`] — the pluggable provider abstraction.
//! - [`snapshot::SnapshotController`] — the controller loop that triggers
//!   snapshots on ledger markers, enforces retention, pauses database flushes
//!   for crash consistency, and restores validated snapshots.
//!
//! The design mirrors the provider-trait convention used by the operator's
//! decentralized backup module (`src/backup/providers`), but is intentionally
//! kept dependency-light so the lean `controller` crate remains fast to build.

pub mod cloud;
pub mod snapshot;

pub use cloud::{
    AwsCredentials, AwsEbsConfig, AwsEbsProvider, CloudError, GcpPdConfig, GcpPdProvider,
    ListFilter, LocalSnapshotProvider, ProviderKind, RestoreResult, SnapshotDescriptor,
    SnapshotProvider, SnapshotRequest, SnapshotState,
};
pub use snapshot::{
    FlushCoordinator, RetentionPolicy, SnapshotConfig, SnapshotController, SnapshotReport,
};
