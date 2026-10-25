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
//! - [`wasm_mutator`] implements the MutatingAdmissionWebhook handler that
//!   intercepts StellarNode WASM deployment payloads and routes them through
//!   the wasm-opt bytecode optimizer sidecar before they are persisted to
//!   etcd.
//! - [`auth`] implements HashiCorp Vault authentication via the Kubernetes
//!   service account auth method (token exchange + in-memory token cache).
//! - [`vault_injector`] implements the MutatingAdmissionWebhook handler that
//!   intercepts Stellar Core pod creation and injects a Vault-backed seed
//!   init container plus a memory-only tmpfs volume carrying the generated
//!   `stellar-core.cfg` (issue #224).

pub mod auth;
pub mod vault_injector;
pub mod wasm_mutator;

pub use auth::{
    extract_secure_string, CachedToken, SecretToken, VaultAuth, VaultAuthConfig, VaultAuthError,
};
pub use vault_injector::{vault_mutate_handler, VaultInjectorConfig, VaultInjectorState};
pub use wasm_mutator::{
    error_review, fail_open, wasm_mutate_handler, AdmissionRequest, AdmissionResponse,
    AdmissionReview, WasmMutatorState,
};
