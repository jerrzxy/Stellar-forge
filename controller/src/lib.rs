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
//! Stellar-K8s controller crate.
//!
//! This crate provides the deployment pipeline and admission webhook modules
//! used by the Stellar-K8s operator.  It is intentionally kept lean so it can
//! be compiled quickly and linked into the operator binary without pulling in
//! heavy optional dependencies.
//!
//! # Modules
//!
//! - [`apis`] — Custom resource validation and API schema utilities.
//! - [`deployment`] — StellarNode deployment pipeline, including the
//!   [`deployment::optimizer`] WASM bytecode optimizer.
//! - [`pdb`] — PodDisruptionBudget auto-tuning controller that keeps
//!   `maxUnavailable` at 0 while validators sync and only relaxes it to 1
//!   when the quorum can survive losing one validator.
//! - [`finalizers`] — Kubernetes finalizer cleanup recovery controller.
//!   Detects `StellarNode` resources stuck in `Terminating` state, verifies
//!   cloud storage detachment via AWS EBS / GCP PD APIs, and safely removes
//!   orphaned finalizers without risking data loss.
//! - [`quorum`] — Quorum-set graph analysis utilities.


pub mod quorum;
