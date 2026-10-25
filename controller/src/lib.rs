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
//! - [`finalizers`] — Kubernetes finalizer cleanup recovery controller.
//!   Detects `StellarNode` resources stuck in `Terminating` state, verifies
//!   cloud storage detachment via AWS EBS / GCP PD APIs, and safely removes
//!   orphaned finalizers without risking data loss.
//! - [`quorum`] — Quorum-set graph analysis utilities.
//! - [`webhook`] — Admission webhook handlers, including the


pub mod apis;
pub mod crd;
pub mod deployment;

pub mod quorum;
pub mod testnet;
pub mod webhook;

/// Standard test logger initialization macro helper.
///
/// Tests emitting raw tracing logs without initialization can output messy warnings.
/// This macro initializes a test subscriber (or defaults to a silent/noop subscriber)
/// ensuring clean test output. It uses `std::sync::Once` so multiple tests calling it
/// concurrently or sequentially within the same test process only initialize once.
#[macro_export]
macro_rules! init_test_logger {
    () => {{
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            let _ = tracing::subscriber::set_global_default(
                tracing::subscriber::NoSubscriber::default(),
            );
        });
    }};
    ($subscriber:expr) => {{
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            let _ = tracing::subscriber::set_global_default($subscriber);
        });
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_test_logger_macro() {
        init_test_logger!();
        // Emitting tracing events after initialization should succeed cleanly
        tracing::info!("test logger initialized cleanly");
        tracing::debug!("debug message in test");
    }

    #[test]
    fn test_init_test_logger_idempotent() {
        init_test_logger!();
        init_test_logger!();
        tracing::warn!("test multiple initializations");
    }
}
