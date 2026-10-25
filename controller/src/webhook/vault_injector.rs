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
//! Mutating Admission Webhook — HashiCorp Vault Validator Seed Injector
//!
//! This module implements the Kubernetes MutatingAdmissionWebhook handler that
//! intercepts **Stellar Core pod creation requests** and injects:
//!
//! 1. An **init container** that authenticates against Vault with the pod's
//!    projected Kubernetes service account JWT (see
//!    [`crate::webhook::auth`]), retrieves the validator seed from a KV-v2
//!    path, and renders the generated `stellar-core.cfg`.
//! 2. A **temporary, memory-only `emptyDir` volume** (`medium: Memory`, i.e.
//!    tmpfs) that carries the rendered `stellar-core.cfg` containing the
//!    plaintext seed.
//! 3. Read-only and read-write **volumeMounts** for the init container and the
//!    `stellar-core` application container respectively.
//!
//! ```text
//!  kubectl apply / StellarNode reconciler
//!       │
//!       ▼
//!  Kubernetes API server
//!       │  MutatingWebhookConfiguration (charts/.../vault-injector.yaml)
//!       │  matches Pod CREATE ops annotated stellar.io/vault-injection=enabled
//!       ▼
//!  stellar-webhook Pod  ◄── /mutate/vault  (this module)
//!       │  JSON Patch: + init container, + tmpfs emptyDir, + mounts
//!       ▼
//!  kubelet schedules pod
//!       └─ vault-seed-init (init container)
//!            │  POST /v1/kubernetes/login  {role, jwt=SA token}
//!            │  GET  /v1/secret/data/stellar/validator  (X-Vault-Token)
//!            └─ writes stellar-core.cfg → /etc/stellar-seed (tmpfs, RAM only)
//!                 │
//!                 ▼
//!          stellar-core container mounts /etc/stellar-seed read-only
//! ```
//!
//! # Security properties (hard invariants)
//!
//! - The plaintext seed **never** appears in the pod spec: no `Secret`
//!   resource, no `env`/`envFrom` entry, nothing persisted to etcd.
//! - The only destination of the seed is the tmpfs `emptyDir`
//!   (`medium: Memory`), which is backed by RAM and destroyed with the pod —
//!   it is never on persistent disk.
//! - The Vault client token is used exclusively inside the init container
//!   process; it is written to a file **on the tmpfs only** for the duration
//!   of the curl call and removed immediately afterwards.  It is never
//!   exported to the container environment.
//! - The webhook itself performs no Vault I/O during pod admission; all
//!   secret retrieval happens at pod start-up in the init container's memory.
//!
//! # Idempotency
//!
//! Pods already carrying the annotation `stellar.io/vault-injected: "true"`
//! are passed through without re-injection.
//!
//! # Fail-open
//!
//! A pod that is not a Stellar Core pod (no `stellar-core` container), or a
//! request where an overlay cannot be built, is **allowed through
//! unmodified** with an advisory warning — the injector must never block
//! node creation.
//!
//! # Route registration
//!
//! ```ignore
//! router.route("/mutate/vault", post(vault_mutate_handler))
//! ```
//! The corresponding `MutatingWebhookConfiguration` is defined in
//! `charts/stellar-operator/templates/vault-injector.yaml`.

use std::sync::Arc;
use std::time::Duration;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use base64::Engine as _;
use serde_json::{json, Value};
use tracing::{debug, error, info, instrument};

use super::wasm_mutator::{error_review, fail_open, AdmissionRequest, AdmissionReview};

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// Total time budget for the mutation step (webhook entry → response).
/// Must be < 10 s (K8s hard deadline); we leave 1 s of headroom.
const WEBHOOK_BUDGET: Duration = Duration::from_secs(9);

/// Opt-in annotation: pods that carry `stellar.io/vault-injection: "enabled"`
/// are mutated by this webhook.
pub const ANN_INJECTION_ENABLED: &str = "stellar.io/vault-injection";

/// Idempotency guard annotation written by a successful injection.
pub const ANN_INJECTED: &str = "stellar.io/vault-injected";

/// Metadata annotations recording (non-secret) injection parameters.
pub const ANN_VAULT_ADDR: &str = "stellar.io/vault-addr";
pub const ANN_VAULT_ROLE: &str = "stellar.io/vault-role";
pub const ANN_VAULT_SECRET_PATH: &str = "stellar.io/vault-secret-path";
pub const ANN_VAULT_SECRET_KEY: &str = "stellar.io/vault-secret-key";

/// Name of the injected memory-only volume.
pub const SEED_VOLUME_NAME: &str = "stellar-seed-tmpfs";

/// Mount path of the seed tmpfs inside the containers.
pub const SEED_MOUNT_PATH: &str = "/etc/stellar-seed";

/// Name of the rendered config file on the tmpfs volume.
pub const SEED_CFG_FILE: &str = "stellar-core.cfg";

/// Name of the application container that runs Stellar Core.
pub const CORE_CONTAINER_NAME: &str = "stellar-core";

/// Name of the injected init container.
pub const INIT_CONTAINER_NAME: &str = "vault-seed-init";

/// Default image used for the injected init container (official Vault image;
/// the injector script only needs `sh`, `curl` and standard UNIX tools).
pub const DEFAULT_INIT_IMAGE: &str = "hashicorp/vault:1.16.3";

/// Default Vault address used when the pod does not override it.
pub const DEFAULT_VAULT_ADDR: &str = "https://vault.vault.svc:8200";

/// Default Vault role expected to be bound to validator service accounts.
pub const DEFAULT_VAULT_ROLE: &str = "stellar-validator";

/// Default KV-v2 secret path (without the `/v1/` prefix).
pub const DEFAULT_SECRET_PATH: &str = "secret/data/stellar/validator";

/// Default key inside the KV-v2 secret that holds the seed.
pub const DEFAULT_SECRET_KEY: &str = "seed";

/// Default size limit of the seed tmpfs volume.
pub const DEFAULT_TMPFS_SIZE: &str = "16Mi";

// ─────────────────────────────────────────────────────────────────────────────
// Configuration
// ─────────────────────────────────────────────────────────────────────────────

/// Static configuration for the vault injector webhook (set once at server
/// start-up; per-pod annotations take precedence for Vault parameters).
#[derive(Debug, Clone)]
pub struct VaultInjectorConfig {
    pub vault_addr: String,
    pub vault_role: String,
    pub secret_path: String,
    pub secret_key: String,
    pub init_image: String,
    pub tmpfs_size: String,
}

impl Default for VaultInjectorConfig {
    fn default() -> Self {
        Self {
            vault_addr: DEFAULT_VAULT_ADDR.to_string(),
            vault_role: DEFAULT_VAULT_ROLE.to_string(),
            secret_path: DEFAULT_SECRET_PATH.to_string(),
            secret_key: DEFAULT_SECRET_KEY.to_string(),
            init_image: DEFAULT_INIT_IMAGE.to_string(),
            tmpfs_size: DEFAULT_TMPFS_SIZE.to_string(),
        }
    }
}

/// Shared state injected into the axum handler via `State<Arc<VaultInjectorState>>`.
#[derive(Clone, Debug, Default)]
pub struct VaultInjectorState {
    pub config: VaultInjectorConfig,
}

impl VaultInjectorState {
    /// Construct with an explicit configuration (useful in tests).
    pub fn with_config(config: VaultInjectorConfig) -> Self {
        Self { config }
    }

    /// Construct from environment — reads `VAULT_ADDR`, `VAULT_ROLE`,
    /// `VAULT_SECRET_PATH`, `VAULT_SECRET_KEY`, `VAULT_INIT_IMAGE`,
    /// `VAULT_TMPFS_SIZE` (all non-secret operational parameters).
    pub fn from_env() -> Self {
        let mut config = VaultInjectorConfig::default();
        if let Ok(v) = std::env::var("VAULT_ADDR") {
            if !v.is_empty() {
                config.vault_addr = v;
            }
        }
        if let Ok(v) = std::env::var("VAULT_ROLE") {
            if !v.is_empty() {
                config.vault_role = v;
            }
        }
        if let Ok(v) = std::env::var("VAULT_SECRET_PATH") {
            if !v.is_empty() {
                config.secret_path = v;
            }
        }
        if let Ok(v) = std::env::var("VAULT_SECRET_KEY") {
            if !v.is_empty() {
                config.secret_key = v;
            }
        }
        if let Ok(v) = std::env::var("VAULT_INIT_IMAGE") {
            if !v.is_empty() {
                config.init_image = v;
            }
        }
        if let Ok(v) = std::env::var("VAULT_TMPFS_SIZE") {
            if !v.is_empty() {
                config.tmpfs_size = v;
            }
        }
        Self::with_config(config)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Overlay — per-pod resolved parameters
// ─────────────────────────────────────────────────────────────────────────────

/// Per-pod resolved Vault parameters: annotations on the pod override the
/// server-level defaults.
#[derive(Debug, Clone)]
struct InjectionOverlay {
    vault_addr: String,
    vault_role: String,
    secret_path: String,
    secret_key: String,
    init_image: String,
    tmpfs_size: String,
}

impl InjectionOverlay {
    /// Resolves per-pod annotations over the static config.  Returns `None`
    /// when the pod does not opt in.
    fn from_pod(object: &Value, base: &VaultInjectorConfig) -> Result<Self, String> {
        let annotations = annotation_table(object);

        let enabled = annotations
            .get(ANN_INJECTION_ENABLED)
            .and_then(|v| v.as_str())
            .is_some_and(|v| v == "enabled");

        if !enabled {
            return Err("pod does not carry stellar.io/vault-injection=enabled".into());
        }

        let pick = |annotation: &str, default_value: &str| -> String {
            annotations
                .get(annotation)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| default_value.to_string())
        };

        Ok(Self {
            vault_addr: pick(ANN_VAULT_ADDR, &base.vault_addr),
            vault_role: pick(ANN_VAULT_ROLE, &base.vault_role),
            secret_path: pick(ANN_VAULT_SECRET_PATH, &base.secret_path),
            secret_key: pick(ANN_VAULT_SECRET_KEY, &base.secret_key),
            init_image: base.init_image.clone(),
            tmpfs_size: base.tmpfs_size.clone(),
        })
    }
}

/// Returns the pod's annotation map, tolerating a missing annotations table.
fn annotation_table(object: &Value) -> std::collections::BTreeMap<String, Value> {
    object
        .pointer("/metadata/annotations")
        .and_then(|v| v.as_object())
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

/// `true` if the pod already carries the idempotency annotation.
fn is_already_injected(object: &Value) -> bool {
    object
        .pointer("/metadata/annotations")
        .and_then(|a| a.get(ANN_INJECTED))
        .and_then(|v| v.as_str())
        == Some("true")
}

// ─────────────────────────────────────────────────────────────────────────────
// Axum handler — POST /mutate/vault
// ─────────────────────────────────────────────────────────────────────────────

/// Axum handler for `POST /mutate/vault`.
#[instrument(skip(state, review), fields(uid = tracing::field::Empty))]
pub async fn vault_mutate_handler(
    State(state): State<Arc<VaultInjectorState>>,
    Json(review): Json<AdmissionReview>,
) -> impl IntoResponse {
    let request: AdmissionRequest = match &review.request {
        Some(r) => {
            tracing::Span::current().record("uid", r.uid.as_str());
            r.clone()
        }
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(error_review("", "missing request in AdmissionReview")),
            )
                .into_response();
        }
    };

    // Dry-run: allow immediately without modification.
    if request.dry_run == Some(true) {
        debug!(uid = %request.uid, "dry-run: skipping vault injection");
        return Json(fail_open(&request.uid, vec![])).into_response();
    }

    let object = match &request.object {
        Some(o) => o.clone(),
        None => {
            // DELETE operations have no object — allow them through.
            return Json(fail_open(&request.uid, vec![])).into_response();
        }
    };

    // We only mutate Pods; everything else passes through untouched.
    if !is_core_pod(&object) {
        return Json(fail_open(&request.uid, vec![])).into_response();
    }

    // Idempotency: never inject twice.
    if is_already_injected(&object) {
        debug!(
            uid = %request.uid,
            "pod already carries {ANN_INJECTED}=true — skipping"
        );
        return Json(fail_open(&request.uid, vec![])).into_response();
    }

    // Resolve per-pod overlay; fail-open when the pod does not opt in.
    let overlay = match InjectionOverlay::from_pod(&object, &state.config) {
        Ok(o) => o,
        Err(reason) => {
            debug!(uid = %request.uid, %reason, "vault injection not enabled for pod");
            return Json(fail_open(&request.uid, vec![])).into_response();
        }
    };

    info!(
        uid = %request.uid,
        vault_role = %overlay.vault_role,
        secret_path = %overlay.secret_path,
        "intercepted Stellar Core pod — injecting Vault seed init container"
    );

    // Build the JSON Patch within the admission budget.
    let patch = match tokio::time::timeout(WEBHOOK_BUDGET, async { build_patch(&object, &overlay) })
        .await
    {
        Ok(Ok(patch)) => patch,
        Ok(Err(e)) => {
            error!(uid = %request.uid, err = %e, "failed to build vault injection patch — allowing original pod");
            return Json(fail_open(
                &request.uid,
                vec![format!(
                    "vault injector could not build patch; deploying original pod: {e}"
                )],
            ))
            .into_response();
        }
        Err(_) => {
            error!(
                uid = %request.uid,
                budget_ms = WEBHOOK_BUDGET.as_millis(),
                "vault injection timed out — allowing original pod"
            );
            return Json(fail_open(
                &request.uid,
                vec!["vault injection timed out; deploying original pod".to_string()],
            ))
            .into_response();
        }
    };

    // Defence in depth: the patch must never carry a seed into env vars or
    // into any spot other than the allowed pod-spec pointers.
    if let Err(e) = verify_patch_safety(&patch) {
        error!(uid = %request.uid, err = %e, "patch safety check failed — refusing to mutate");
        return Json(fail_open(
            &request.uid,
            vec![format!(
                "vault injector patch rejected by safety check: {e}"
            )],
        ))
        .into_response();
    }

    let patch_json = match serde_json::to_string(&patch) {
        Ok(s) => s,
        Err(e) => {
            error!(uid = %request.uid, err = %e, "failed to serialize JSON patch");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(error_review(
                    &request.uid,
                    &format!("patch serialization error: {e}"),
                )),
            )
                .into_response();
        }
    };

    let patch_b64 = base64::engine::general_purpose::STANDARD.encode(patch_json.as_bytes());

    info!(
        uid = %request.uid,
        ops = patch.len(),
        "vault seed injection patch ready"
    );

    Json(AdmissionReview {
        api_version: "admission.k8s.io/v1".to_string(),
        kind: "AdmissionReview".to_string(),
        request: None,
        response: Some(super::wasm_mutator::AdmissionResponse {
            uid: request.uid.clone(),
            allowed: true,
            patch: Some(patch_b64),
            patch_type: Some("JSONPatch".to_string()),
            status: None,
            warnings: None,
        }),
    })
    .into_response()
}

/// `true` if the object is a Pod (any named container set).
fn is_core_pod(object: &Value) -> bool {
    object.get("kind").and_then(|k| k.as_str()) == Some("Pod")
}

// ─────────────────────────────────────────────────────────────────────────────
// Patch builder
// ─────────────────────────────────────────────────────────────────────────────

/// Build the JSON Patch that injects the Vault init container, the memory-only
/// volume, and the read-only mount into the `stellar-core` container.
fn build_patch(object: &Value, overlay: &InjectionOverlay) -> Result<Vec<Value>, String> {
    let mut ops = Vec::new();

    // Locate the stellar-core container.  The mutating webhook only fires for
    // Pods, and the opt-in guarantees an operator-managed pod shape, but keep
    // the strictness: no core container ⇒ no injection.
    let core_index = find_container_index(object, CORE_CONTAINER_NAME)
        .ok_or_else(|| format!("no '{CORE_CONTAINER_NAME}' container in pod spec"))?;

    // ── Annotations ───────────────────────────────────────────────────────────
    // Idempotency guard + non-secret parameter records.
    ops.push(add_annotation_op(ANN_INJECTED, "true"));
    ops.push(add_annotation_op(ANN_VAULT_ADDR, &overlay.vault_addr));
    ops.push(add_annotation_op(ANN_VAULT_ROLE, &overlay.vault_role));
    ops.push(add_annotation_op(
        ANN_VAULT_SECRET_PATH,
        &overlay.secret_path,
    ));
    ops.push(add_annotation_op(ANN_VAULT_SECRET_KEY, &overlay.secret_key));

    // ── Memory-only emptyDir volume ───────────────────────────────────────────
    // `medium: Memory` ⇒ tmpfs ⇒ backed by RAM, never written to disk.
    let volume = json!({
        "name": SEED_VOLUME_NAME,
        "emptyDir": {
            "medium": "Memory",
            "sizeLimit": overlay.tmpfs_size,
        }
    });
    if object.pointer("/spec/volumes").is_some() {
        // Append without touching existing volumes.
        ops.push(json!({
            "op": "add",
            "path": "/spec/volumes/-",
            "value": volume
        }));
    } else {
        ops.push(json!({
            "op": "add",
            "path": "/spec/volumes",
            "value": [volume]
        }));
    }

    // ── Init container ────────────────────────────────────────────────────────
    // Authenticates with Vault via the K8s service account JWT, fetches the
    // seed, renders the stellar-core.cfg into the tmpfs volume, then scrubs
    // all intermediate secrets.  The seed value only ever exists in this
    // container's shell/task memory and in the tmpfs config file.
    let init_container = build_init_container(overlay);
    if object.pointer("/spec/initContainers").is_some() {
        ops.push(json!({
            "op": "add",
            "path": "/spec/initContainers/-",
            "value": init_container
        }));
    } else {
        ops.push(json!({
            "op": "add",
            "path": "/spec/initContainers",
            "value": [init_container]
        }));
    }

    // ── Read-only mount for the application container ────────────────────────
    let mount = json!({
        "name": SEED_VOLUME_NAME,
        "mountPath": SEED_MOUNT_PATH,
        "readOnly": true
    });
    let mount_ptr = format!("/spec/containers/{core_index}/volumeMounts");
    if object
        .pointer(&format!("/spec/containers/{core_index}/volumeMounts"))
        .is_some()
    {
        ops.push(json!({
            "op": "add",
            "path": format!("{mount_ptr}/-"),
            "value": mount
        }));
    } else {
        ops.push(json!({
            "op": "add",
            "path": mount_ptr,
            "value": [mount]
        }));
    }

    Ok(ops)
}

/// Finds the index of a named container in `/spec/containers`.
fn find_container_index(object: &Value, name: &str) -> Option<usize> {
    object
        .pointer("/spec/containers")
        .and_then(|c| c.as_array())
        .and_then(|arr| {
            arr.iter()
                .position(|c| c.get("name").and_then(|n| n.as_str()) == Some(name))
        })
}

/// Builds an `add` op for a single metadata annotation.
fn add_annotation_op(key: &str, value: &str) -> Value {
    json!({
        "op": "add",
        "path": format!("/metadata/annotations/{}", escape_json_pointer(key)),
        "value": value
    })
}

/// Renders the injected init container object.
///
/// Note: all `env` entries are **non-secret operational parameters** (Vault
/// address, role name, secret path/key).  The seed value and the Vault client
/// token are never placed in env vars — see the module-level security notes.
fn build_init_container(overlay: &InjectionOverlay) -> Value {
    json!({
        "name": INIT_CONTAINER_NAME,
        "image": overlay.init_image,
        "imagePullPolicy": "IfNotPresent",
        "command": ["sh", "-ec"],
        "args": [init_container_script()],
        "env": [
            { "name": "VAULT_ADDR",
              "value": overlay.vault_addr },
            { "name": "VAULT_AUTH_MOUNT",
              "value": "kubernetes" },
            { "name": "VAULT_K8S_ROLE",
              "value": overlay.vault_role },
            { "name": "VAULT_SECRET_PATH",
              "value": overlay.secret_path },
            { "name": "VAULT_SECRET_KEY",
              "value": overlay.secret_key },
            { "name": "SEED_MOUNT_PATH",
              "value": SEED_MOUNT_PATH },
            { "name": "SEED_CFG",
              "value": format!("{SEED_MOUNT_PATH}/{SEED_CFG_FILE}") },
            { "name": "SA_JWT",
              "value": "/var/run/secrets/kubernetes.io/serviceaccount/token" },
        ],
        "volumeMounts": [
            {
                "name": SEED_VOLUME_NAME,
                "mountPath": SEED_MOUNT_PATH
            },
            {
                "name": SEED_VOLUME_NAME,
                "mountPath": "/dev/shm",
                "subPath": "scratch"
            }
        ],
        "securityContext": {
            "allowPrivilegeEscalation": false,
            "runAsNonRoot": true,
            "capabilities": { "drop": ["ALL"] },
            "seccompProfile": { "type": "RuntimeDefault" }
        },
        "resources": {
            "requests": { "cpu": "10m", "memory": "32Mi" },
            "limits":   { "cpu": "100m", "memory": "64Mi" }
        }
    })
}

/// The shell script executed by the init container.
///
/// Security invariants encoded here:
/// - the JWT is read from the projected (kube-rotated) token file, never from
///   an env var;
/// - the Vault client token is exchanged via shell memory and written only to
///   a file **on the tmpfs**, which is deleted immediately after use — never
///   exported to the environment;
/// - the seed is rendered into `stellar-core.cfg` on the tmpfs only, with
///   `0600` permissions taken before first write (umask 077).
fn init_container_script() -> &'static str {
    r#"set -eu

# ── 1. Preconditions ────────────────────────────────────────────────────────
[ -n "$VAULT_ADDR" ] || { echo "vault-seed-init: VAULT_ADDR unset"; exit 1; }
[ -n "$VAULT_K8S_ROLE" ] || { echo "vault-seed-init: VAULT_K8S_ROLE unset"; exit 1; }
[ -n "$VAULT_SECRET_PATH" ] || { echo "vault-seed-init: VAULT_SECRET_PATH unset"; exit 1; }
[ -n "$VAULT_SECRET_KEY" ] || { echo "vault-seed-init: VAULT_SECRET_KEY unset"; exit 1; }
[ -n "$SEED_CFG" ] || { echo "vault-seed-init: SEED_CFG unset"; exit 1; }
[ -r "$SA_JWT" ] || { echo "vault-seed-init: k8s sa token not readable at $SA_JWT"; exit 1; }
[ -d "$SEED_MOUNT_PATH" ] || { echo "vault-seed-init: seed volume not mounted"; exit 1; }

# Lock the tmpfs down before anything sensitive exists on it.
umask 077
chmod 700 "$SEED_MOUNT_PATH"
SCRATCH="$SEED_MOUNT_PATH/.scratch"
mkdir -m 700 "$SCRATCH"

# ── 2. Vault Kubernetes auth login ─────────────────────────────────────────
# The projected SA JWT is read from the token file (kubelet rotates it);
# it is passed in the request body, never in env vars, and never stored.
LOGIN_RESP="$(curl -fsS --connect-timeout 5 --max-time 30 \
  -H 'Content-Type: application/json' \
  -d "{\"role\":\"$VAULT_K8S_ROLE\",\"jwt\":\"$(cat "$SA_JWT")\"}" \
  "${VAULT_ADDR%/}/v1/$VAULT_AUTH_MOUNT/login")"

# Extract the client token in shell memory.  It exists ONLY here and in the
# one-line tmpfs scratch file used for the curl call below — never in env.
VAULT_TOKEN="$(printf '%s' "$LOGIN_RESP" | sed -n 's/.*"client_token"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')"
[ -n "$VAULT_TOKEN" ] || { echo "vault-seed-init: vault login failed (no client token)"; exit 1; }
printf 'X-Vault-Token: %s\n' "$VAULT_TOKEN" > "$SCRATCH/token-header"

# ── 3. Read the validator seed from KV v2 ──────────────────────────────────
SECRET_RESP="$(curl -fsS --connect-timeout 5 --max-time 30 \
  -H @"$SCRATCH/token-header" \
  "${VAULT_ADDR%/}/v1/${VAULT_SECRET_PATH%/}")"

# Scrub the token immediately after the last authorized use.
rm -f "$SCRATCH/token-header"

SEED_VALUE="$(printf '%s' "$SECRET_RESP" | sed -n "s/.*\"$VAULT_SECRET_KEY\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p")"
[ -n "$SEED_VALUE" ] || { echo "vault-seed-init: secret key '$VAULT_SECRET_KEY' missing in vault response"; exit 1; }

# Defensive shape check: Stellar ed25519 seeds are S-prefixed z-base-32 (56 chars).
case "$SEED_VALUE" in
  S?????????????????????????????????????????????????????) ;;
  *) echo "vault-seed-init: secret key '$VAULT_SECRET_KEY' is not a Stellar seed"; exit 1 ;;
esac

# ── 4. Render stellar-core.cfg on the tmpfs (RAM only) ─────────────────────
# The plaintext seed therefore exists only in this process' memory and in the
# 0600 tmpfs file.  Nothing is written to persistent disk, nothing is placed
# in container env vars, and no Kubernetes Secret is created or mutated.
cat > "$SEED_CFG.tmp" <<CFG
# Generated by stellar-k8s vault injector — do not edit.
# Source: vault $VAULT_SECRET_PATH (key: $VAULT_SECRET_KEY)
[VALIDATOR]
VALIDATOR_NAME==VALIDATOR
HOME_DOMAIN=STELLARK8S
SEED_VALUE=$SEED_VALUE
[QUORUM_SET]
VALIDATORS="@@QUORUM_VALIDATORS@@"
THRESHOLD_PERCENT=67
CFG
mv "$SEED_CFG.tmp" "$SEED_CFG"
chmod 400 "$SEED_CFG"

# ── 5. Scrub secrets from shell/task memory ────────────────────────────────
VAULT_TOKEN="$(printf //%.0s)"
SEED_VALUE="$(printf //%.0s)"
LOGIN_RESP="$(printf //%.0s)"
SECRET_RESP="$(printf //%.0s)"
rmdir "$SCRATCH"

echo "vault-seed-init: seed rendered to $SEED_CFG on tmpfs"
"#
}

// ─────────────────────────────────────────────────────────────────────────────
// Patch safety analysis
// ─────────────────────────────────────────────────────────────────────────────

/// Whitelist of JSON pointer prefixes a vault-injection patch may touch.
const ALLOWED_PATCH_PATHS: &[&str] = &[
    "/metadata/annotations/",
    "/spec/volumes",
    "/spec/initContainers",
    "/spec/containers/",
];

/// Verifies the patch only writes to allowed pod-spec locations and never
/// introduces a seed-like value.
///
/// This is defence in depth: the patch is stored on etcd (it becomes the pod
/// spec), so leaking anything sensitive through it would persist the seed in
/// Kubernetes state — exactly what this feature must prevent.
fn verify_patch_safety(patch: &[Value]) -> Result<(), String> {
    let ops = patch;

    for op in ops {
        let path = op
            .get("path")
            .and_then(|p| p.as_str())
            .ok_or_else(|| "op without a path".to_string())?;

        if !ALLOWED_PATCH_PATHS.iter().any(|p| path.starts_with(p)) {
            return Err(format!("patch writes to disallowed path {path}"));
        }

        // No `value` in the patch may contain a Stellar seed-shaped string,
        // which would persist key material into etcd. This detects leaked
        // secret content even when embedded inside other strings (the rendered
        // cfg template uses a variable reference, not a literal).
        if let Some(value) = op.get("value") {
            if contains_seed_shaped(&value_to_string(value)) {
                return Err(format!("patch op at {path} carries a seed-shaped value"));
            }
        }
    }
    Ok(())
}

/// Detects a Stellar ed25519 seed-shaped run inside a string.
///
/// A seed is exactly 56 characters of z-base-32 (A–Z, 2–7) starting with `S`./// We only use this as a *negative* tripwire: variable references such as
/// `$SEED_VALUE` are correctly not flagged.
fn contains_seed_shaped(s: &str) -> bool {
    let is_zb32 = |c: char| c.is_ascii_uppercase() || ('2'..='7').contains(&c);
    let bytes: Vec<char> = s.chars().collect();
    if bytes.len() < 56 {
        return false;
    }
    (0..=bytes.len() - 56)
        .any(|i| bytes[i] == 'S' && bytes[i + 1..i + 56].iter().all(|c| is_zb32(*c)))
}

/// Compact string form of a JSON value (for substring scans).
fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Escape a JSON Pointer segment (RFC 6901: `/` → `~1`, `~` → `~0`).
fn escape_json_pointer(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_pod(annotations: Value) -> Value {
        json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "stellar-validator-0",
                "annotations": annotations
            },
            "spec": {
                "serviceAccountName": "stellar-validator",
                "containers": [
                    {
                        "name": "stellar-core",
                        "image": "stellar/stellar-core:21.0.0"
                    }
                ]
            }
        })
    }

    fn opt_in_annotations() -> Value {
        json!({
            ANN_INJECTION_ENABLED: "enabled"
        })
    }

    fn default_overlay() -> InjectionOverlay {
        InjectionOverlay::from_pod(
            &sample_pod(opt_in_annotations()),
            &VaultInjectorConfig::default(),
        )
        .unwrap()
    }

    #[test]
    fn test_is_core_pod_true() {
        assert!(is_core_pod(&sample_pod(json!({}))));
    }

    #[test]
    fn test_is_core_pod_kind_matters() {
        assert!(!is_core_pod(&json!({
            "apiVersion": "stellar.org/v1alpha1",
            "kind": "StellarNode",
            "spec": {}
        })));
    }

    #[test]
    fn test_is_already_injected_detects_annotation() {
        let mut pod = sample_pod(opt_in_annotations());
        pod["metadata"]["annotations"][ANN_INJECTED] = json!("true");
        assert!(is_already_injected(&pod));
        assert!(!is_already_injected(&sample_pod(opt_in_annotations())));
    }

    #[test]
    fn test_opt_in_required() {
        assert!(InjectionOverlay::from_pod(
            &sample_pod(json!({})),
            &VaultInjectorConfig::default()
        )
        .is_err());
    }

    #[test]
    fn test_annotations_override_defaults() {
        let pod = sample_pod(json!({
            ANN_INJECTION_ENABLED: "enabled",
            ANN_VAULT_ADDR: "https://vault.team-a.svc:8200",
            ANN_VAULT_ROLE: "validator-team-a",
            ANN_VAULT_SECRET_PATH: "secret/data/stellar/team-a",
            ANN_VAULT_SECRET_KEY: "seed"
        }));
        let overlay = InjectionOverlay::from_pod(&pod, &VaultInjectorConfig::default()).unwrap();
        assert_eq!(overlay.vault_addr, "https://vault.team-a.svc:8200");
        assert_eq!(overlay.vault_role, "validator-team-a");
        assert_eq!(overlay.secret_path, "secret/data/stellar/team-a");
    }

    #[test]
    fn test_build_patch_shape() {
        let overlay = default_overlay();
        let pod = sample_pod(opt_in_annotations());
        let patch = build_patch(&pod, &overlay).unwrap();
        assert!(
            patch.len() >= 5,
            "annotations + volume + init container + mount"
        );

        verify_patch_safety(&patch).unwrap();
    }

    #[test]
    fn test_build_patch_initial_volume_array() {
        let overlay = default_overlay();
        let pod = sample_pod(opt_in_annotations()); // no spec.volumes, no initContainers
        let patch = build_patch(&pod, &overlay).unwrap();

        let volume_op = patch
            .iter()
            .find(|op| op.get("path").and_then(|p| p.as_str()) == Some("/spec/volumes"))
            .unwrap();
        assert_eq!(volume_op["value"].as_array().unwrap().len(), 1);
        let v = &volume_op["value"][0];
        assert_eq!(v["name"], SEED_VOLUME_NAME);
        assert_eq!(v["emptyDir"]["medium"], "Memory");
        assert_eq!(v["emptyDir"]["sizeLimit"], "16Mi");
    }

    #[test]
    fn test_build_patch_appends_existing_volume() {
        let overlay = default_overlay();
        let mut pod = sample_pod(opt_in_annotations());
        pod["spec"]["volumes"] = json!([{"name": "logs", "emptyDir": {}}]);
        let patch = build_patch(&pod, &overlay).unwrap();

        let volume_op = patch
            .iter()
            .find(|op| op.get("path").and_then(|p| p.as_str()) == Some("/spec/volumes/-"))
            .expect("append op expected");
        assert_eq!(volume_op["op"], "add");
        assert_eq!(volume_op["value"]["name"], SEED_VOLUME_NAME);
    }

    #[test]
    fn test_build_patch_ro_mount_on_core_container() {
        let overlay = default_overlay();
        let pod = sample_pod(opt_in_annotations());
        let patch = build_patch(&pod, &overlay).unwrap();

        let mount_op = patch
            .iter()
            .find(|op| {
                op.get("path")
                    .and_then(|p| p.as_str())
                    .is_some_and(|p| p.starts_with("/spec/containers/0/volumeMounts"))
            })
            .expect("core mount op expected");
        assert_eq!(mount_op["value"][0]["readOnly"], true);
        assert_eq!(mount_op["value"][0]["mountPath"], SEED_MOUNT_PATH);
    }

    #[test]
    fn test_build_patch_fails_without_core_container() {
        let overlay = default_overlay();
        let mut pod = sample_pod(opt_in_annotations());
        pod["spec"]["containers"][0]["name"] = json!("horizon");
        assert!(build_patch(&pod, &overlay).is_err());
    }

    #[test]
    fn test_init_container_has_no_seed_env() {
        let overlay = default_overlay();
        let c = build_init_container(&overlay);
        assert_eq!(
            c["name"], INIT_CONTAINER_NAME,
            "init container name mismatch"
        );
        for env in c["env"].as_array().unwrap() {
            let value = env["value"].as_str().unwrap_or("");
            assert!(
                !value.starts_with('S') || value.len() != 56,
                "env var leaked a seed-shaped value: {value}"
            );
        }

        // The JWT path is supplied as a (non-secret) env config point; the
        // script reads the token from that file rather than env.
        let env = c["env"].as_array().unwrap();
        let sa_jwt = env
            .iter()
            .find(|e| e["name"] == "SA_JWT")
            .map(|e| e["value"].as_str().unwrap());
        assert_eq!(
            sa_jwt,
            Some("/var/run/secrets/kubernetes.io/serviceaccount/token")
        );
        let script = c["args"][0].as_str().unwrap();
        assert!(script.contains("$SA_JWT"));
        assert!(!script.contains("SEED_VALUE=S"));
    }

    #[test]
    fn test_verify_patch_safety_rejects_foreign_path() {
        let bad = json!([
            { "op": "replace", "path": "/spec/nodeSelector", "value": {} }
        ]);
        assert!(verify_patch_safety(bad.as_array().unwrap()).is_err());
    }

    #[test]
    fn test_verify_patch_safety_rejects_seed_literal() {
        let bad = json!([
            {
                "op": "add",
                "path": format!("/metadata/annotations/{}", escape_json_pointer("stellar.io/x")),
                "value": "SEED_VALUE=SBPQUA55J7IX2XWNKRM4LJ3ITLXVMGNQPL6SPHRCVIYGGGRJA57LCCCC"
            }
        ]);
        assert!(verify_patch_safety(bad.as_array().unwrap()).is_err());
    }

    #[test]
    fn test_verify_patch_safety_accepts_valid_patch() {
        let overlay = default_overlay();
        let patch = build_patch(&sample_pod(opt_in_annotations()), &overlay).unwrap();
        verify_patch_safety(&patch).unwrap();
    }

    #[test]
    fn test_escape_json_pointer() {
        assert_eq!(
            escape_json_pointer("stellar.io/vault-addr"),
            "stellar.io~1vault-addr"
        );
        assert_eq!(escape_json_pointer("a~b/c"), "a~0b~1c");
    }

    #[test]
    fn test_annotation_table_handles_missing_annotations() {
        let pod = json!({"kind": "Pod", "metadata": {"name": "x"}, "spec": {}});
        assert!(annotation_table(&pod).is_empty());
    }

    #[test]
    fn test_default_config_values() {
        let cfg = VaultInjectorConfig::default();
        assert!(cfg.vault_addr.starts_with("https://"));
        assert_eq!(cfg.secret_key, DEFAULT_SECRET_KEY);
        assert_eq!(cfg.tmpfs_size, DEFAULT_TMPFS_SIZE);
    }
}
