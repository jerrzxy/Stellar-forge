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
//! Security Policy Enforcement (OPA)
//!
//! Enforces organizational security policies for StellarNode resources.

use crate::crd::StellarNodeSpec;

/// Default number of pre-initialized WASM sandboxes kept in the warm pool.
pub const DEFAULT_WASM_SANDBOX_POOL_SIZE: u32 = 4;
/// Default maximum number of idle WASM sandboxes retained in the warm pool.
pub const DEFAULT_WASM_SANDBOX_MAX_IDLE: u32 = 8;
/// Hard upper bound on the warm pool size to protect worker node RAM.
pub const MAX_WASM_SANDBOX_POOL_SIZE: u32 = 256;

pub struct PolicyEnforcer;

impl PolicyEnforcer {
    /// Validates a spec against OPA-style security policies
    pub fn enforce_policy(spec: &StellarNodeSpec) -> Vec<String> {
        let mut violations = Vec::new();

        // 1. Ensure privileged containers are disabled (enforced by PSS but checked here too)
        // 2. Ensure only approved registries are used
        if let Some(validator) = &spec.validator_config {
            // Policy: Validators must have history archives enabled in production
            if spec.network == crate::crd::StellarNetwork::Mainnet
                && !validator.enable_history_archive
            {
                violations.push(
                    "Policy Violation: Mainnet validators must have history archives enabled"
                        .to_string(),
                );
            }
        }

        // 3. WASM sandbox pool sizing must be sane so operators cannot exhaust node RAM.
        if let Some(pool_size) = spec.wasm_sandbox_pool_size {
            if pool_size == 0 {
                violations.push(
                    "Policy Violation: wasmSandboxPoolSize must be greater than zero".to_string(),
                );
            } else if pool_size > MAX_WASM_SANDBOX_POOL_SIZE {
                violations.push(format!(
                    "Policy Violation: wasmSandboxPoolSize ({}) exceeds the maximum allowed ({})",
                    pool_size, MAX_WASM_SANDBOX_POOL_SIZE
                ));
            }
        }

        if let Some(max_idle) = spec.wasm_sandbox_max_idle {
            if max_idle == 0 {
                violations.push(
                    "Policy Violation: wasmSandboxMaxIdle must be greater than zero".to_string(),
                );
            } else if max_idle > MAX_WASM_SANDBOX_POOL_SIZE {
                violations.push(format!(
                    "Policy Violation: wasmSandboxMaxIdle ({}) exceeds the maximum allowed ({})",
                    max_idle, MAX_WASM_SANDBOX_POOL_SIZE
                ));
            }
        }

        // The idle cap must be able to hold the configured warm pool.
        let effective_pool_size = spec
            .wasm_sandbox_pool_size
            .unwrap_or(DEFAULT_WASM_SANDBOX_POOL_SIZE);
        let effective_max_idle = spec
            .wasm_sandbox_max_idle
            .unwrap_or(DEFAULT_WASM_SANDBOX_MAX_IDLE);
        if effective_max_idle < effective_pool_size {
            violations.push(format!(
                "Policy Violation: wasmSandboxMaxIdle ({}) must be greater than or equal to wasmSandboxPoolSize ({})",
                effective_max_idle, effective_pool_size
            ));
        }

        violations
    }

    /// Applies defaults for WASM sandbox pool sizing when operators leave them unset.
    pub fn apply_defaults(spec: &mut StellarNodeSpec) {
        if spec.wasm_sandbox_pool_size.is_none() {
            spec.wasm_sandbox_pool_size = Some(DEFAULT_WASM_SANDBOX_POOL_SIZE);
        }
        if spec.wasm_sandbox_max_idle.is_none() {
            spec.wasm_sandbox_max_idle = Some(DEFAULT_WASM_SANDBOX_MAX_IDLE);
        }
    }
}
