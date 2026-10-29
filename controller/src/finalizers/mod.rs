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

//! Finalizer cleanup recovery subsystem.
//!
//! This module provides the background garbage-collection reconciler that
//! detects `StellarNode` resources stuck in the `Terminating` state due to
//! interrupted controller processes, verifies underlying cloud storage
//! detachment, and safely removes orphaned finalizers.
//!
//! # Modules
//!
//! * [`cloud_verify`] — [`CloudVolumeVerifier`](cloud_verify::CloudVolumeVerifier)
//!   trait plus AWS EBS and GCP Persistent Disk implementations.
//! * [`cleanup`] — [`FinalizerCleanupReconciler`](cleanup::FinalizerCleanupReconciler)
//!   background GC reconciler and Kubernetes client abstraction.
//!
//! # Safety
//!
//! The finalizer `stellarnode.stellar.org/finalizer` is **never** removed unless
//! the cloud provider API confirms the underlying volume has no active
//! attachments.  See [`cleanup::STELLAR_FINALIZER`] for the exact string.

pub mod cleanup;
pub mod cloud_verify;
