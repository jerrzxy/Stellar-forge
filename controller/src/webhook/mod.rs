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
//! Webhook sub-modules for the controller crate.
//!
//! This module exposes three distinct webhook capabilities:
//!
//! ## 1. WASM Bytecode Optimizer (`wasm_mutator`)
//!
//! The [`wasm_mutator`] module implements the `MutatingAdmissionWebhook`
//! handler that intercepts `StellarNode` WASM deployment payloads and routes
//! them through the wasm-opt bytecode optimizer sidecar before they are
//! persisted to etcd.
//!
//! ## 2. WASM Policy Runner (`wasm_runner`)
//!
//! The [`wasm_runner`] module provides a sandboxed Wasmtime execution engine
//! for WASM admission-policy plugins.  Plugins are compiled from raw WASM
//! bytes (loaded from Kubernetes ConfigMaps at runtime) and executed within a
//! per-request `Store` bounded by fuel (CPU) and memory limits.
//!
//! ## 3. Admission Webhook Handler (`admission`)
//!
//! The [`admission`] module wires the WASM runner into a Kubernetes-compatible
//! `AdmissionReview` HTTP server with both validating (`/validate`) and
//! mutating (`/mutate`) endpoints.  Fail-open / fail-closed routing logic
//! is configurable per plugin.
//!
//! # Architecture
//!
//! ```text
//!  Kubernetes API Server
//!       │
//!       │  AdmissionReview (POST /validate or POST /mutate)
//!       ▼
//!  AdmissionServer  ──────────────────────────────────────┐
//!  (admission.rs)                                         │
//!       │                                                 │
//!       │  PolicyInput (JSON)                             │
//!       ▼                                                 │
//!  WasmRunner  ───► Plugin 1 (WASM)                       │
//!  (wasm_runner.rs) Plugin 2 (WASM)  ← ConfigMap loading  │
//!                   Plugin N (WASM)                       │
//!       │                                                 │
//!       │  PolicyOutput { allowed, reason, warnings }     │
//!       ▼                                                 │
//!  AdmissionReviewResponse  ─────────────────────────────┘
//!       │
//!       ▼
//!  Kubernetes API Server
//! ```

pub mod admission;
pub mod wasm_mutator;
pub mod wasm_runner;

pub use admission::{
    AdmissionServer, AdmissionServerState, RegisterPluginRequest, PluginInfo, PluginListResponse,
};
pub use wasm_mutator::{wasm_mutate_handler, WasmMutatorState};
pub use wasm_runner::{CompiledPlugin, PolicyInput, PolicyOutput, WasmRunner, WasmRunnerError, WEBHOOK_TIMEOUT};
