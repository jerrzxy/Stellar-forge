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
//! Integration tests — Vault authentication + seed injection lifecycle (issue #224).
//!
//! These tests spin up an in-process **mock Vault server** (axum) that behaves
//! like a real cluster:
//!
//! - `POST /v1/kubernetes/login` — Kubernetes auth method.  Only the known
//!   role + service-account JWT combination yields a client token; anything
//!   else is rejected with Vault's standard `permission denied` payload.
//! - `GET /v1/secret/data/stellar/validator` — KV-v2 read guarded by the
//!   `X-Vault-Token` header; a missing/incorrect token is a 403.
//!
//! The suite validates the **full lifecycle**:
//!
//! 1. **Authentication**: wrong JWT rejected; correct JWT yields a token; the
//!    token is cached (no duplicate login); an expired lease forces re-login;
//!    a rotated token is denied, the cache is dropped, and the client recovers.
//! 2. **Injection**: the `/mutate/vault` handler receives a Stellar Core pod
//!    creation request and returns a JSON Patch; the decoded patch is verified
//!    op-by-op (init container authenticating via the SA token file, tmpfs
//!    `emptyDir` with `medium: Memory`, read-only mount on `stellar-core`,
//!    idempotency annotation).
//! 3. **Security invariants**: the patch (which becomes etcd state) must not
//!    contain the seed value, the Vault client token, any seed-bearing env
//!    var, or any write to a Kubernetes Secret.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::{Extension, Json, Router};
use base64::Engine as _;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use controller::webhook::auth::{
    extract_secure_string, VaultAuth, VaultAuthConfig, VaultAuthError,
};
use controller::webhook::vault_injector::{
    vault_mutate_handler, VaultInjectorConfig, VaultInjectorState, ANN_INJECTED,
    ANN_INJECTION_ENABLED, ANN_VAULT_ROLE, CORE_CONTAINER_NAME, INIT_CONTAINER_NAME,
    SEED_MOUNT_PATH, SEED_VOLUME_NAME,
};
use controller::webhook::wasm_mutator::{AdmissionRequest, AdmissionReview};

// ─────────────────────────────────────────────────────────────────────────────
// Test constants (a synthetic seed — never a real key)
// ─────────────────────────────────────────────────────────────────────────────

const TEST_JWT: &str = "eyJhbGciOiJSUzI1NiIsImtpZCI6InRlc3QifQ.test-sa-jwt";
const TEST_ROLE: &str = "stellar-validator";
const TEST_CLIENT_TOKEN: &str = "hvs.integration-test-token";
const TEST_SEED: &str = "SBPQUA55J7IX2XWNKRM4LJ3ITLXVMGNQPL6SPHRCVIYGGGRJA57LCCCC";
const TEST_SECRET_PATH: &str = "secret/data/stellar/validator";

// ─────────────────────────────────────────────────────────────────────────────
// Mock Vault server
// ─────────────────────────────────────────────────────────────────────────────

/// Handle over the mock Vault; tests use it to inspect counters and rotate
/// the active client token.
#[derive(Clone)]
struct MockVault {
    login_count: Arc<AtomicUsize>,
    current_token: Arc<Mutex<String>>,
}

impl MockVault {
    async fn rotate_token(&self, new_token: &str) {
        *self.current_token.lock().await = new_token.to_string();
    }

    async fn token(&self) -> String {
        self.current_token.lock().await.clone()
    }
}

fn mock_vault_app() -> (Router, MockVault) {
    let handle = MockVault {
        login_count: Arc::new(AtomicUsize::new(0)),
        current_token: Arc::new(Mutex::new(TEST_CLIENT_TOKEN.to_string())),
    };
    let state = handle.clone();

    let app = Router::new()
        .route(
            "/v1/kubernetes/login",
            axum::routing::post(login_handler).layer(Extension(state.clone())),
        )
        .route(
            "/v1/secret/data/stellar/validator",
            axum::routing::get(read_secret_handler).layer(Extension(state)),
        );
    (app, handle)
}

/// Kubernetes auth login: validates role + JWT like Vault verifying the
/// service account against the TokenReview API.
async fn login_handler(
    Extension(vault): Extension<MockVault>,
    Json(body): Json<Value>,
) -> (axum::http::StatusCode, Json<Value>) {
    vault.login_count.fetch_add(1, Ordering::SeqCst);

    let role = body.get("role").and_then(|r| r.as_str()).unwrap_or("");
    let jwt = body.get("jwt").and_then(|j| j.as_str()).unwrap_or("");

    if role != TEST_ROLE || jwt != TEST_JWT {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            Json(json!({ "errors": ["permission denied"] })),
        );
    }

    (
        axum::http::StatusCode::OK,
        Json(json!({
            "auth": {
                "client_token": vault.token().await,
                "lease_duration": 3600,
                "renewable": true
            }
        })),
    )
}

/// KV-v2 secret read: guarded by the `X-Vault-Token` header.
async fn read_secret_handler(
    Extension(vault): Extension<MockVault>,
    headers: axum::http::HeaderMap,
) -> (axum::http::StatusCode, Json<Value>) {
    let presented = headers
        .get("X-Vault-Token")
        .and_then(|t| t.to_str().ok())
        .unwrap_or("");

    if presented != vault.token().await {
        return (
            axum::http::StatusCode::FORBIDDEN,
            Json(json!({ "errors": ["permission denied"] })),
        );
    }

    (
        axum::http::StatusCode::OK,
        Json(json!({
            "request_id": "test",
            "data": {
                "data": { "seed": TEST_SEED },
                "metadata": { "version": 2 }
            }
        })),
    )
}

async fn spawn_mock_vault() -> (String, MockVault) {
    let (app, handle) = mock_vault_app();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), handle)
}

// ─────────────────────────────────────────────────────────────────────────────
// Admission request helpers
// ─────────────────────────────────────────────────────────────────────────────

fn make_pod_review(annotations: Value) -> AdmissionReview {
    AdmissionReview {
        api_version: "admission.k8s.io/v1".to_string(),
        kind: "AdmissionReview".to_string(),
        request: Some(AdmissionRequest {
            uid: "integration-uid-1".to_string(),
            object: Some(json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {
                    "name": "validator-0",
                    "annotations": annotations
                },
                "spec": {
                    "serviceAccountName": "stellar-validator",
                    "containers": [
                        { "name": CORE_CONTAINER_NAME, "image": "stellar/stellar-core:21.0.0" }
                    ]
                }
            })),
            old_object: None,
            operation: "CREATE".to_string(),
            dry_run: Some(false),
        }),
        response: None,
    }
}

fn opt_in_annotations() -> Value {
    json!({
        ANN_INJECTION_ENABLED: "enabled",
        ANN_VAULT_ROLE: TEST_ROLE
    })
}

/// Calls the mutating handler, returns `(http status, decoded JSON Patch)`.
///
/// This exercises the same AdmissionReview contract a real Kubernetes API
/// server uses: the response body is a base64 `JSONPatch` on an allow.
async fn admit(
    state: Arc<VaultInjectorState>,
    review: AdmissionReview,
) -> (axum::http::StatusCode, Vec<Value>) {
    use axum::response::IntoResponse as _;

    let resp = vault_mutate_handler(axum::extract::State(state), Json(review))
        .await
        .into_response();
    let status = resp.status();

    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("response body readable");
    let review: AdmissionReview =
        serde_json::from_slice(&bytes).expect("response body is an AdmissionReview");

    let patch = review
        .response
        .and_then(|r| r.patch)
        .map(|b64| {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .expect("patch must be valid base64");
            serde_json::from_slice::<Vec<Value>>(&raw).expect("patch must decode")
        })
        .unwrap_or_default();

    (status, patch)
}

/// Writes the synthetic JWT to a temp location mirroring the projected
/// service-account token path a real pod has.
fn write_jwt_file(jwt: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, jwt).unwrap();
    (dir, path)
}

// ─────────────────────────────────────────────────────────────────────────────
// 1. Authentication lifecycle (VaultAuth ↔ mock Vault)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn vault_auth_rejects_wrong_jwt() {
    let (addr, _handle) = spawn_mock_vault().await;
    let (_jwt_file, jwt_path) = write_jwt_file("not-the-real-jwt");

    let mut cfg = VaultAuthConfig::new(&addr, TEST_ROLE);
    cfg.jwt_path = jwt_path;
    let auth = VaultAuth::new(cfg).unwrap();

    match auth.login().await.expect_err("wrong jwt must be rejected") {
        VaultAuthError::Vault { code, .. } => assert_eq!(code, 401),
        other => panic!("expected Vault error, got {other:?}"),
    }
    assert_eq!(auth.login_count(), 1);
    drop(_jwt_file);
}

#[tokio::test]
async fn vault_auth_login_success_and_token_caching() {
    let (addr, handle) = spawn_mock_vault().await;
    let (_jwt_file, jwt_path) = write_jwt_file(TEST_JWT);

    let mut cfg = VaultAuthConfig::new(&addr, TEST_ROLE);
    cfg.jwt_path = jwt_path;
    let auth = VaultAuth::new(cfg).unwrap();

    // 1st token() → login round trip.
    let t1 = auth.token().await.expect("login must succeed");
    assert_eq!(t1.as_str(), handle.token().await);
    assert_eq!(handle.login_count.load(Ordering::SeqCst), 1);

    // 2nd token() → served from the in-memory cache, no extra login.
    let t2 = auth.token().await.expect("cached token");
    assert_eq!(t2.as_str(), handle.token().await);
    assert_eq!(handle.login_count.load(Ordering::SeqCst), 1);
    assert!(auth.has_cached_token().await);

    drop(_jwt_file);
}

#[tokio::test]
async fn vault_auth_relogin_after_cache_invalidation() {
    let (addr, handle) = spawn_mock_vault().await;
    let (_jwt_file, jwt_path) = write_jwt_file(TEST_JWT);

    let mut cfg = VaultAuthConfig::new(&addr, TEST_ROLE);
    cfg.jwt_path = jwt_path;
    let auth = VaultAuth::new(cfg).unwrap();

    let _ = auth.token().await.unwrap();
    assert_eq!(handle.login_count.load(Ordering::SeqCst), 1);

    // Invalidating the cache forces the next token() to re-authenticate —
    // this is exactly what a 403 on a revoked token triggers.
    auth.invalidate_cached().await;
    let _ = auth.token().await.unwrap();
    assert_eq!(handle.login_count.load(Ordering::SeqCst), 2);

    drop(_jwt_file);
}

#[tokio::test]
async fn vault_secret_read_full_roundtrip() {
    let (addr, _handle) = spawn_mock_vault().await;
    let (_jwt_file, jwt_path) = write_jwt_file(TEST_JWT);

    let mut cfg = VaultAuthConfig::new(&addr, TEST_ROLE);
    cfg.jwt_path = jwt_path;
    let auth = VaultAuth::new(cfg).unwrap();

    let doc = auth
        .read_kv_v2(TEST_SECRET_PATH)
        .await
        .expect("KV read must succeed");
    let seed = extract_secure_string(&doc, "seed").expect("seed present");
    assert_eq!(seed, TEST_SEED);

    drop(_jwt_file);
}

#[tokio::test]
async fn vault_secret_read_denied_on_rotated_token_then_recovers() {
    let (addr, handle) = spawn_mock_vault().await;
    let (_jwt_file, jwt_path) = write_jwt_file(TEST_JWT);

    let mut cfg = VaultAuthConfig::new(&addr, TEST_ROLE);
    cfg.jwt_path = jwt_path;
    let auth = VaultAuth::new(cfg).unwrap();

    // Initial read works with the cached token.
    let doc = auth.read_kv_v2(TEST_SECRET_PATH).await.unwrap();
    assert_eq!(extract_secure_string(&doc, "seed").unwrap(), TEST_SEED);

    // Rotate the token server-side; the cached one is now unusable.
    handle.rotate_token("hvs.rotated-after-revocation").await;

    match auth.read_kv_v2(TEST_SECRET_PATH).await {
        Err(VaultAuthError::Vault { code, .. }) => assert_eq!(code, 403),
        other => panic!("expected denial, got {other:?}"),
    }

    // The 403 path must have dropped the stale cache: the next read
    // re-authenticates transparently and succeeds again.
    let doc = auth.read_kv_v2(TEST_SECRET_PATH).await.unwrap();
    assert_eq!(extract_secure_string(&doc, "seed").unwrap(), TEST_SEED);
    assert_eq!(handle.token().await, "hvs.rotated-after-revocation");

    drop(_jwt_file);
}

// ─────────────────────────────────────────────────────────────────────────────
// 2+3. Injection lifecycle + security invariants
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn injection_intercepts_stellar_core_pod() {
    let state = Arc::new(VaultInjectorState::with_config(
        VaultInjectorConfig::default(),
    ));

    let (status, patch) = admit(state, make_pod_review(opt_in_annotations())).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(!patch.is_empty(), "injected pods must receive a patch");
    assert!(patch.len() >= 5);
}

#[tokio::test]
async fn injection_patch_contents() {
    let state = Arc::new(VaultInjectorState::with_config(
        VaultInjectorConfig::default(),
    ));

    let (status, patch) = admit(state, make_pod_review(opt_in_annotations())).await;
    assert_eq!(status, axum::http::StatusCode::OK);

    // Idempotency guard annotation is set (JSON-escaped annotation key).
    let injected = patch.iter().find(|op| {
        op.get("path").and_then(|p| p.as_str())
            == Some("/metadata/annotations/stellar.io~1vault-injected")
    });
    assert!(injected.is_some(), "idempotency annotation must be set");
    assert_eq!(injected.unwrap()["value"], "true");

    // tmpfs emptyDir volume with medium=Memory and a capped size.
    let vol_op = patch
        .iter()
        .find(|op| op.get("path").and_then(|p| p.as_str()) == Some("/spec/volumes"))
        .expect("volume op must exist");
    let vol = &vol_op["value"][0];
    assert_eq!(vol["name"], SEED_VOLUME_NAME);
    assert_eq!(vol["emptyDir"]["medium"], "Memory");
    assert!(vol["emptyDir"]["sizeLimit"].as_str().is_some());

    // Init container present, authenticating via the K8s SA token file.
    let init_op = patch
        .iter()
        .find(|op| op.get("path").and_then(|p| p.as_str()) == Some("/spec/initContainers"))
        .expect("init container op must exist");
    let init = &init_op["value"][0];
    assert_eq!(init["name"], INIT_CONTAINER_NAME);

    let env = init["env"].as_array().unwrap();
    // Role and Vault address (non-secret) are present as env config …
    assert!(env.iter().any(|e| e["name"] == "VAULT_K8S_ROLE"));
    assert!(env.iter().any(|e| e["name"] == "VAULT_ADDR"));
    // … but no env var carries the JWT, a client token, or a seed.
    for e in env {
        let v = e["value"].as_str().unwrap_or("");
        assert!(!v.contains("hvs."), "env leaked a vault token: {v}");
        assert!(
            !(v.len() == 56 && v.starts_with('S')),
            "env leaked a seed: {v}"
        );
        assert!(!v.contains("eyJ"), "env leaked the SA JWT");
    }
    // The init script reads the JWT from the projected token file.
    let script = init["args"][0].as_str().unwrap().to_string();
    // The JWT is read from the projected service-account token file through
    // the (non-secret) SA_JWT config env var — never stored in raw form.
    assert!(script.contains("$SA_JWT"));
    assert!(!script.contains(TEST_SEED));

    // Security-context hardening on the injected init container.
    assert_eq!(init["securityContext"]["allowPrivilegeEscalation"], false);
    assert_eq!(init["securityContext"]["runAsNonRoot"], true);

    // Read-only mount delivered into the stellar-core container.
    let mount_op = patch
        .iter()
        .find(|op| {
            op.get("path")
                .and_then(|p| p.as_str())
                .is_some_and(|p| p.starts_with("/spec/containers/0/volumeMounts"))
        })
        .expect("core mount op must exist");
    let mount = &mount_op["value"][0];
    assert_eq!(mount["name"], SEED_VOLUME_NAME);
    assert_eq!(mount["mountPath"], SEED_MOUNT_PATH);
    assert_eq!(mount["readOnly"], true);
}

#[tokio::test]
async fn injection_does_not_persist_secret_into_pod_spec() {
    let state = Arc::new(VaultInjectorState::with_config(
        VaultInjectorConfig::default(),
    ));

    let (_status, patch) = admit(state, make_pod_review(opt_in_annotations())).await;
    let serialized = serde_json::to_string(&patch).unwrap();

    // The plaintext seed must never enter the pod spec (etcd state).
    assert!(
        !serialized.contains(TEST_SEED),
        "patch must not carry the plaintext seed"
    );
    // The Vault client token must never enter the pod spec either.
    assert!(
        !serialized.contains(TEST_CLIENT_TOKEN),
        "patch must not carry the vault client token"
    );
    // No generic seed env var may be defined through the patch.
    assert!(
        !serialized.contains("\"name\":\"STELLAR_SEED\""),
        "patch must not define a seed env var"
    );
    // No Secret resource may be referenced or touched.
    assert!(!serialized.contains("\"kind\":\"Secret\""));
}

#[tokio::test]
async fn injection_is_idempotent() {
    let state = Arc::new(VaultInjectorState::with_config(
        VaultInjectorConfig::default(),
    ));

    let mut annotations = opt_in_annotations();
    annotations[ANN_INJECTED] = json!("true");
    let (_status, patch) = admit(state, make_pod_review(annotations)).await;
    assert!(patch.is_empty(), "already-injected pods get no patch");
}

#[tokio::test]
async fn injection_skips_pods_without_opt_in() {
    let state = Arc::new(VaultInjectorState::with_config(
        VaultInjectorConfig::default(),
    ));

    let (_status, patch) = admit(state, make_pod_review(json!({}))).await;
    assert!(patch.is_empty(), "non-opted pods pass through untouched");
}

#[tokio::test]
async fn injection_fail_open_without_core_container() {
    let state = Arc::new(VaultInjectorState::with_config(
        VaultInjectorConfig::default(),
    ));

    let mut review = make_pod_review(opt_in_annotations());
    review.request.as_mut().unwrap().object.as_mut().unwrap()["spec"]["containers"][0]["name"] =
        json!("horizon");
    let (_status, patch) = admit(state, review).await;
    assert!(patch.is_empty(), "fail-open: no patch for foreign pods");
}

#[tokio::test]
async fn injection_dry_run_not_modified() {
    let state = Arc::new(VaultInjectorState::with_config(
        VaultInjectorConfig::default(),
    ));

    let mut review = make_pod_review(opt_in_annotations());
    review.request.as_mut().unwrap().dry_run = Some(true);
    let (status, patch) = admit(state, review).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(patch.is_empty());
}

#[tokio::test]
async fn non_pod_objects_pass_through() {
    let state = Arc::new(VaultInjectorState::with_config(
        VaultInjectorConfig::default(),
    ));

    let review = AdmissionReview {
        api_version: "admission.k8s.io/v1".to_string(),
        kind: "AdmissionReview".to_string(),
        request: Some(AdmissionRequest {
            uid: "uid-non-pod".to_string(),
            object: Some(json!({
                "apiVersion": "stellar.org/v1alpha1",
                "kind": "StellarNode",
                "spec": {}
            })),
            old_object: None,
            operation: "CREATE".to_string(),
            dry_run: Some(false),
        }),
        response: None,
    };

    let (_status, patch) = admit(state, review).await;
    assert!(patch.is_empty());
}
