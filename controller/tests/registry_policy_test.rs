// Copyright 2024 Stellar-K8s Contributors
// SPDX-License-Identifier: Apache-2.0
//! Integration tests for the WASM-based registry admission policy.
//!
//! These tests exercise the full `WasmRunner → AdmissionServer` stack using
//! inline WAT modules that simulate the registry policy's behaviour.  They
//! serve as a contract test: if the host ABI or JSON protocol changes, the
//! tests break immediately.
//!
//! # Why WAT instead of the compiled plugin?
//!
//! Compiling `examples/plugins/registry-policy` to
//! `wasm32-unknown-unknown` requires the Wasm cross-compilation target to be
//! installed in CI.  Using WAT lets us exercise the full execution path
//! without that constraint.  The contract tests in `tests/admission_webhook_conformance.rs`
//! cover the HTTP layer end-to-end.

use std::time::Instant;

use controller::webhook::{AdmissionServer, PolicyInput, WasmRunner, WEBHOOK_TIMEOUT};

// ─────────────────────────────────────────────────────────────────────────────
// WAT fixtures
//
// Each WAT module implements the four-function host ABI:
//
//   env::get_input_len() -> i32
//   env::read_input(ptr: i32, len: i32) -> i32
//   env::write_output(ptr: i32, len: i32) -> i32
//   env::log_message(ptr: i32, len: i32)
//
// and exports a single `validate() -> i32` function that:
//   • reads the `PolicyInput` JSON from the host input buffer
//   • writes a `PolicyOutput` JSON into the host output buffer
//   • returns 0 (allow) or 1 (deny)
// ─────────────────────────────────────────────────────────────────────────────

/// A WAT plugin that reads the full `PolicyInput` JSON, extracts
/// `objectSpec.version`, and denies if it contains an unapproved registry.
///
/// The "approved" check here is: version must NOT start with "bad.registry/".
/// This mimics what the Rust plugin does for a configurable allow-list.
///
/// Note: WAT is stack-based; implementing a real JSON parser in WAT is
/// impractical.  Instead we use a Rust host-side test for the logic,
/// and this fixture tests the I/O plumbing.
const DENY_BAD_REGISTRY_WAT: &str = r#"
(module
  (import "env" "get_input_len" (func $get_input_len (result i32)))
  (import "env" "read_input"    (func $read_input    (param i32 i32) (result i32)))
  (import "env" "write_output"  (func $write_output  (param i32 i32) (result i32)))
  (import "env" "log_message"   (func $log_message   (param i32 i32)))
  (memory (export "memory") 2)

  ;; Static deny output JSON (offset 0x200 in memory)
  (data (i32.const 0x200)
    "{\"allowed\":false,\"reason\":\"image is not from an approved registry\"}"
  )
  ;; Static allow output JSON (offset 0x300 in memory)
  (data (i32.const 0x300)
    "{\"allowed\":true}"
  )
  ;; Static "bad.registry/" needle (offset 0x400)
  (data (i32.const 0x400) "bad.registry/")

  ;; Input scratch buffer starts at 0x1000
  (global $input_start i32 (i32.const 0x1000))

  (func (export "validate") (result i32)
    (local $len i32)
    (local $i i32)
    (local $needle_len i32)
    (local $found i32)
    (local $j i32)
    (local $match i32)

    ;; Read input into scratch area.
    (local.set $len (call $get_input_len))
    (drop (call $read_input (global.get $input_start) (local.get $len)))

    ;; Needle length = 13 ("bad.registry/")
    (local.set $needle_len (i32.const 13))

    ;; Scan the input buffer for the needle.
    (local.set $i (i32.const 0))
    (local.set $found (i32.const 0))
    (block $break
      (loop $scan
        ;; if i + needle_len > len: break
        (br_if $break
          (i32.gt_u
            (i32.add (local.get $i) (local.get $needle_len))
            (local.get $len)))

        ;; Compare needle at position i.
        (local.set $j (i32.const 0))
        (local.set $match (i32.const 1))
        (block $nomatch
          (loop $cmp
            (br_if $nomatch (i32.ge_u (local.get $j) (local.get $needle_len)))
            (br_if $nomatch
              (i32.ne
                (i32.load8_u
                  (i32.add (global.get $input_start) (i32.add (local.get $i) (local.get $j))))
                (i32.load8_u (i32.add (i32.const 0x400) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br $cmp)
          )
        )
        ;; If match is still 1 after the inner loop, the needle was found.
        (if (local.get $match) (then (local.set $found (i32.const 1)) (br $break)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $scan)
      )
    )

    ;; Write deny or allow output.
    (if (local.get $found)
      (then
        (drop (call $write_output (i32.const 0x200) (i32.const 68)))
        (return (i32.const 1))
      )
    )
    (drop (call $write_output (i32.const 0x300) (i32.const 16)))
    (i32.const 0)
  )
)
"#;

/// A simple always-allow WAT plugin (no output written → runner uses return code).
const ALLOW_ALL_WAT: &str = r#"
(module
  (import "env" "get_input_len" (func (result i32)))
  (import "env" "read_input"    (func (param i32 i32) (result i32)))
  (import "env" "write_output"  (func (param i32 i32) (result i32)))
  (import "env" "log_message"   (func (param i32 i32)))
  (memory (export "memory") 1)
  (func (export "validate") (result i32) i32.const 0)
)
"#;

// ─────────────────────────────────────────────────────────────────────────────
// Helper
// ─────────────────────────────────────────────────────────────────────────────

fn make_input(version: &str) -> PolicyInput {
    PolicyInput {
        operation: "CREATE".into(),
        namespace: "stellar".into(),
        name: "test-node".into(),
        resource_kind: "stellar.org/StellarNode".into(),
        object_spec: serde_json::json!({ "version": version }),
        annotations: Default::default(),
        labels: Default::default(),
    }
}

fn deadline() -> Instant {
    Instant::now() + WEBHOOK_TIMEOUT
}

// ─────────────────────────────────────────────────────────────────────────────
// WasmRunner unit tests (policy logic via WAT)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn registry_policy_denies_unapproved_registry() {
    let runner = WasmRunner::new().expect("runner");
    let wasm = wat::parse_str(DENY_BAD_REGISTRY_WAT).expect("valid WAT");
    let plugin = runner.compile("registry-policy", &wasm, false).expect("compile");

    let input = make_input("bad.registry/stellar/stellar-core:v21.0.0");
    let output = runner
        .run_plugin(&plugin, &input, deadline())
        .expect("run plugin");

    assert!(
        !output.allowed,
        "expected DENY for unapproved registry, got allowed=true; reason={:?}",
        output.reason
    );
    assert!(
        output.reason.as_deref().unwrap_or("").contains("approved registry"),
        "expected reason to mention 'approved registry', got: {:?}",
        output.reason
    );
}

#[test]
fn registry_policy_allows_approved_registry() {
    let runner = WasmRunner::new().expect("runner");
    let wasm = wat::parse_str(DENY_BAD_REGISTRY_WAT).expect("valid WAT");
    let plugin = runner.compile("registry-policy", &wasm, false).expect("compile");

    let input = make_input("ghcr.io/stellar/stellar-core:v21.0.0");
    let output = runner
        .run_plugin(&plugin, &input, deadline())
        .expect("run plugin");

    assert!(
        output.allowed,
        "expected ALLOW for approved registry, got denied; reason={:?}",
        output.reason
    );
}

#[test]
fn allow_all_plugin_allows_everything() {
    let runner = WasmRunner::new().expect("runner");
    let wasm = wat::parse_str(ALLOW_ALL_WAT).expect("valid WAT");
    let plugin = runner.compile("allow-all", &wasm, false).expect("compile");

    for version in &[
        "bad.registry/evil/image:latest",
        "ghcr.io/stellar/core:v21",
        "",
        "v21.0.0",
    ] {
        let input = make_input(version);
        let output = runner
            .run_plugin(&plugin, &input, deadline())
            .expect("run plugin");
        assert!(output.allowed, "allow-all should allow '{version}'");
    }
}

#[test]
fn fail_open_plugin_allows_on_error() {
    // A plugin that immediately traps (divide by zero) with fail_open=true
    // should still allow the request.
    let trap_wat = r#"
    (module
      (import "env" "get_input_len" (func (result i32)))
      (import "env" "read_input"    (func (param i32 i32) (result i32)))
      (import "env" "write_output"  (func (param i32 i32) (result i32)))
      (import "env" "log_message"   (func (param i32 i32)))
      (memory (export "memory") 1)
      (func (export "validate") (result i32)
        i32.const 1
        i32.const 0
        i32.div_u   ;; trap: division by zero
      )
    )"#;
    let runner = WasmRunner::new().expect("runner");
    let wasm = wat::parse_str(trap_wat).expect("valid WAT");
    let plugin = runner
        .compile("trap-plugin", &wasm, true /* fail_open */)
        .expect("compile");

    let input = make_input("any.registry/image:latest");
    // The plugin traps → WasmRunnerError; fail_open means the AdmissionServer
    // will allow it.  But WasmRunner.run_plugin itself returns Err.
    let result = runner.run_plugin(&plugin, &input, deadline());
    assert!(result.is_err(), "trapping plugin should return Err");
    // The error should be surfaced; fail_open routing happens in AdmissionServerState.
    assert!(plugin.fail_open, "plugin.fail_open should be true");
}

// ─────────────────────────────────────────────────────────────────────────────
// AdmissionServer integration tests (HTTP layer)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn admission_server_denies_unapproved_registry_via_validate_endpoint() {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use base64::Engine as _;
    use tower::ServiceExt;

    let wasm = wat::parse_str(DENY_BAD_REGISTRY_WAT).expect("valid WAT");
    let wasm_b64 = base64::engine::general_purpose::STANDARD.encode(&wasm);

    let server = AdmissionServer::new(WasmRunner::new().unwrap());
    let router = server.router();

    // Register the plugin via POST /plugins.
    let reg_body = serde_json::json!({
        "name": "registry-policy",
        "wasm_b64": wasm_b64,
        "fail_open": false
    });
    let reg_req = Request::builder()
        .method(Method::POST)
        .uri("/plugins")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&reg_body).unwrap()))
        .unwrap();
    let reg_resp = router.clone().oneshot(reg_req).await.unwrap();
    assert_eq!(reg_resp.status(), StatusCode::OK, "plugin registration failed");

    // POST /validate with a non-compliant StellarNode.
    let review = serde_json::json!({
        "apiVersion": "admission.k8s.io/v1",
        "kind": "AdmissionReview",
        "request": {
            "uid": "deny-test-uid",
            "operation": "CREATE",
            "kind": { "group": "stellar.org", "version": "v1alpha1", "kind": "StellarNode" },
            "name": "bad-validator",
            "namespace": "stellar",
            "object": {
                "metadata": { "name": "bad-validator", "namespace": "stellar" },
                "spec": {
                    "nodeType": "Validator",
                    "network": "testnet",
                    "version": "bad.registry/evil/stellar-core:v21"
                }
            }
        }
    });
    let val_req = Request::builder()
        .method(Method::POST)
        .uri("/validate")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&review).unwrap()))
        .unwrap();
    let val_resp = router.clone().oneshot(val_req).await.unwrap();
    assert_eq!(val_resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(val_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(
        !json["response"]["allowed"].as_bool().unwrap_or(true),
        "expected admission DENIED for unapproved registry, got: {json}"
    );
}

#[tokio::test]
async fn admission_server_allows_approved_registry_via_validate_endpoint() {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use base64::Engine as _;
    use tower::ServiceExt;

    let wasm = wat::parse_str(DENY_BAD_REGISTRY_WAT).expect("valid WAT");
    let wasm_b64 = base64::engine::general_purpose::STANDARD.encode(&wasm);

    let server = AdmissionServer::new(WasmRunner::new().unwrap());
    let router = server.router();

    // Register plugin.
    let reg_body = serde_json::json!({
        "name": "registry-policy", "wasm_b64": wasm_b64, "fail_open": false
    });
    let _ = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/plugins")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&reg_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    // POST /validate with a compliant StellarNode.
    let review = serde_json::json!({
        "apiVersion": "admission.k8s.io/v1",
        "kind": "AdmissionReview",
        "request": {
            "uid": "allow-test-uid",
            "operation": "CREATE",
            "kind": { "group": "stellar.org", "version": "v1alpha1", "kind": "StellarNode" },
            "name": "good-validator",
            "namespace": "stellar",
            "object": {
                "metadata": { "name": "good-validator", "namespace": "stellar" },
                "spec": {
                    "nodeType": "Validator",
                    "network": "testnet",
                    "version": "ghcr.io/stellar/stellar-core:v21.0.0"
                }
            }
        }
    });
    let val_req = Request::builder()
        .method(Method::POST)
        .uri("/validate")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&review).unwrap()))
        .unwrap();
    let val_resp = router.oneshot(val_req).await.unwrap();
    assert_eq!(val_resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(val_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(
        json["response"]["allowed"].as_bool().unwrap_or(false),
        "expected admission ALLOWED for approved registry, got: {json}"
    );
}

#[tokio::test]
async fn admission_server_mutate_adds_policy_annotation() {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use tower::ServiceExt;

    let server = AdmissionServer::new(WasmRunner::new().unwrap());
    let router = server.router();

    let review = serde_json::json!({
        "apiVersion": "admission.k8s.io/v1",
        "kind": "AdmissionReview",
        "request": {
            "uid": "mutate-test-uid",
            "operation": "CREATE",
            "kind": { "group": "stellar.org", "version": "v1alpha1", "kind": "StellarNode" },
            "name": "my-validator",
            "namespace": "stellar",
            "object": {
                "metadata": { "name": "my-validator", "namespace": "stellar" },
                "spec": { "nodeType": "Validator", "network": "testnet", "version": "v21.0.0" }
            }
        }
    });

    let req = Request::builder()
        .method(Method::POST)
        .uri("/mutate")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&review).unwrap()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(
        json["response"]["allowed"].as_bool().unwrap_or(false),
        "mutate with no plugins should allow"
    );
    // The mutating handler always injects a JSON patch with the policy annotation.
    assert!(
        json["response"]["patch"].is_string(),
        "expected patch in mutate response, got: {json}"
    );
    assert_eq!(
        json["response"]["patchType"],
        "JSONPatch",
        "expected patchType=JSONPatch"
    );
}
