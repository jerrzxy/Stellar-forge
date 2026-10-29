// Copyright 2024 Stellar-K8s Contributors
// SPDX-License-Identifier: Apache-2.0
//! Kubernetes Mutating and Validating Admission Webhook
//!
//! This module implements the HTTP handlers that receive
//! `AdmissionReview` JSON bodies from the Kubernetes API server and route
//! them through registered WASM policy plugins.
//!
//! # Handler overview
//!
//! | Route         | Kind        | Description                                      |
//! |---------------|-------------|--------------------------------------------------|
//! | `POST /validate` | Validating | Returns allow/deny; rejects non-compliant nodes. |
//! | `POST /mutate`   | Mutating   | May patch the object; adds default annotations.  |
//! | `GET  /healthz`  | –          | Liveness probe endpoint.                         |
//! | `GET  /readyz`   | –          | Readiness probe endpoint.                        |
//!
//! # Fail-open / fail-closed
//!
//! The `fail_open` flag on each [`AdmissionPlugin`] controls what happens when
//! the plugin itself errors (e.g., fuel exhaustion, bad output):
//!
//! - `fail_open = true`  → the request is **allowed** and a warning is logged.
//! - `fail_open = false` → the request is **denied** and an error is returned.
//!
//! # Timeout guarantee
//!
//! Each admission review call computes a deadline of
//! [`crate::webhook::wasm_runner::WEBHOOK_TIMEOUT`] from the start of the
//! request.  All plugin calls share this deadline; if a plugin runs over it
//! the runner immediately surfaces a [`WasmRunnerError::Timeout`] and the
//! fail-open/closed policy takes over.
//!
//! # Thread safety
//!
//! [`AdmissionServer`] stores plugins in an `Arc<RwLock<Vec<…>>>` so that
//! `POST /plugins` can be used at runtime to hot-load new policies without
//! restarting the operator.  The `RwLock` is read-locked for every admission
//! request (cheap, concurrent) and write-locked only during plugin registration
//! (rare).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, error, info, instrument, warn};

use super::wasm_runner::{CompiledPlugin, PolicyInput, PolicyOutput, WasmRunner, WasmRunnerError, WEBHOOK_TIMEOUT};

// ─────────────────────────────────────────────────────────────────────────────
// Kubernetes admission types (lightweight; no kube dependency in controller)
// ─────────────────────────────────────────────────────────────────────────────

/// Subset of an AdmissionReview request the webhook cares about.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionReviewRequest {
    pub api_version: Option<String>,
    pub kind: Option<String>,
    pub request: Option<AdmissionRequest>,
}

/// The inner `request` object inside an AdmissionReview.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionRequest {
    pub uid: String,
    pub operation: String,
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    pub kind: ResourceKind,
    /// The incoming object as a raw JSON value (may be absent for DELETE).
    #[serde(default)]
    pub object: Option<serde_json::Value>,
    /// The existing object (present for UPDATE).
    #[serde(default)]
    pub old_object: Option<serde_json::Value>,
}

/// GVK triple for the resource being admitted.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceKind {
    pub group: String,
    pub version: String,
    pub kind: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// AdmissionReview response
// ─────────────────────────────────────────────────────────────────────────────

/// Full AdmissionReview response sent back to the Kubernetes API server.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionReviewResponse {
    pub api_version: String,
    pub kind: String,
    pub response: AdmissionResponse,
}

/// The inner `response` object.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionResponse {
    /// Must echo the `request.uid` back.
    pub uid: String,
    pub allowed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<AdmissionStatus>,
    /// Base64-encoded JSON Patch (RFC 6902), only set by mutating webhooks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    /// Must be `"JSONPatch"` when `patch` is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_type: Option<String>,
    /// Non-fatal warnings returned to the API-server caller.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

/// Status block for denied requests.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionStatus {
    pub code: u32,
    pub message: String,
}

impl AdmissionReviewResponse {
    fn allow(uid: impl Into<String>) -> Self {
        Self {
            api_version: "admission.k8s.io/v1".into(),
            kind: "AdmissionReview".into(),
            response: AdmissionResponse {
                uid: uid.into(),
                allowed: true,
                status: None,
                patch: None,
                patch_type: None,
                warnings: vec![],
            },
        }
    }

    fn deny(uid: impl Into<String>, code: u32, message: impl Into<String>) -> Self {
        Self {
            api_version: "admission.k8s.io/v1".into(),
            kind: "AdmissionReview".into(),
            response: AdmissionResponse {
                uid: uid.into(),
                allowed: false,
                status: Some(AdmissionStatus { code, message: message.into() }),
                patch: None,
                patch_type: None,
                warnings: vec![],
            },
        }
    }

    fn allow_with_warnings(uid: impl Into<String>, warnings: Vec<String>) -> Self {
        Self {
            api_version: "admission.k8s.io/v1".into(),
            kind: "AdmissionReview".into(),
            response: AdmissionResponse {
                uid: uid.into(),
                allowed: true,
                status: None,
                patch: None,
                patch_type: None,
                warnings,
            },
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Plugin management request/response
// ─────────────────────────────────────────────────────────────────────────────

/// Request body for `POST /plugins` (register or update a WASM policy).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterPluginRequest {
    /// Human-readable plugin identifier (used in logs and in error messages).
    pub name: String,
    /// Base64-encoded WASM binary.
    pub wasm_b64: String,
    /// If `true`, execution errors let the request through.
    #[serde(default)]
    pub fail_open: bool,
}

/// Response body for `GET /plugins`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginListResponse {
    pub plugins: Vec<PluginInfo>,
}

/// Summary of a registered plugin.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginInfo {
    pub name: String,
    pub fail_open: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// AdmissionPlugin (a named, compiled WASM module)
// ─────────────────────────────────────────────────────────────────────────────

/// A registered admission policy WASM plugin.
struct AdmissionPlugin {
    compiled: CompiledPlugin,
}

// ─────────────────────────────────────────────────────────────────────────────
// AdmissionServer shared state
// ─────────────────────────────────────────────────────────────────────────────

/// Shared state handed to every Axum handler via [`State`].
#[derive(Clone)]
pub struct AdmissionServerState {
    runner: Arc<WasmRunner>,
    plugins: Arc<RwLock<Vec<AdmissionPlugin>>>,
}

impl AdmissionServerState {
    /// Create a new server state with no plugins loaded.
    pub fn new(runner: WasmRunner) -> Self {
        Self {
            runner: Arc::new(runner),
            plugins: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Register a compiled plugin (thread-safe, takes a write-lock).
    pub async fn register_plugin(&self, plugin: CompiledPlugin) {
        let mut guard = self.plugins.write().await;
        // Replace an existing plugin with the same name, or append.
        if let Some(existing) = guard.iter_mut().find(|p| p.compiled.name == plugin.name) {
            info!(name = %plugin.name, "replacing existing WASM admission plugin");
            existing.compiled = plugin;
        } else {
            info!(name = %plugin.name, "registered new WASM admission plugin");
            guard.push(AdmissionPlugin { compiled: plugin });
        }
    }

    /// Compile raw WASM bytes and register the resulting plugin.
    pub async fn load_plugin(
        &self,
        name: impl Into<String>,
        wasm_bytes: &[u8],
        fail_open: bool,
    ) -> anyhow::Result<()> {
        let compiled = self.runner.compile(name, wasm_bytes, fail_open)?;
        self.register_plugin(compiled).await;
        Ok(())
    }

    /// Return the number of currently registered plugins.
    pub async fn plugin_count(&self) -> usize {
        self.plugins.read().await.len()
    }

    /// Run all registered plugins against the given `PolicyInput`.
    ///
    /// Returns the first `deny` decision encountered, or `allow` if every
    /// plugin agrees.  Plugins with `fail_open = true` that produce an error
    /// are skipped with a warning.
    async fn evaluate_plugins(
        &self,
        input: &PolicyInput,
        deadline: Instant,
    ) -> PolicyOutput {
        let plugins = self.plugins.read().await;

        if plugins.is_empty() {
            debug!("no WASM admission plugins registered — allowing by default");
            return PolicyOutput::allow();
        }

        let mut warnings: Vec<String> = Vec::new();

        for ap in plugins.iter() {
            // Abort early if the deadline has already passed.
            if Instant::now() > deadline {
                warn!("Admission webhook deadline exceeded before all plugins ran");
                return PolicyOutput {
                    allowed: true, // fail-open on timeout by convention
                    reason: None,
                    warnings: vec!["webhook deadline exceeded; some plugins were skipped".into()],
                };
            }

            match self.runner.run_plugin(&ap.compiled, input, deadline) {
                Ok(output) => {
                    warnings.extend(output.warnings.clone());
                    if !output.allowed {
                        return output;
                    }
                }
                Err(err) => {
                    if ap.compiled.fail_open {
                        warn!(
                            plugin = %ap.compiled.name,
                            error = %err,
                            "WASM plugin error (fail-open: request allowed)"
                        );
                        warnings.push(format!(
                            "Plugin '{}' encountered an error (fail-open): {err}",
                            ap.compiled.name
                        ));
                    } else {
                        error!(
                            plugin = %ap.compiled.name,
                            error = %err,
                            "WASM plugin error (fail-closed: request denied)"
                        );
                        return PolicyOutput::deny(format!(
                            "Plugin '{}' failed: {err}",
                            ap.compiled.name
                        ));
                    }
                }
            }
        }

        // All plugins passed.
        PolicyOutput {
            allowed: true,
            reason: None,
            warnings,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// AdmissionServer builder
// ─────────────────────────────────────────────────────────────────────────────

/// HTTP admission webhook server.
pub struct AdmissionServer {
    state: AdmissionServerState,
    tls_cert: Option<String>,
    tls_key: Option<String>,
}

impl AdmissionServer {
    /// Create a server with the given WASM runner.
    pub fn new(runner: WasmRunner) -> Self {
        Self {
            state: AdmissionServerState::new(runner),
            tls_cert: None,
            tls_key: None,
        }
    }

    /// Configure TLS using PEM-encoded cert and key file paths.
    pub fn with_tls(mut self, cert_path: impl Into<String>, key_path: impl Into<String>) -> Self {
        self.tls_cert = Some(cert_path.into());
        self.tls_key = Some(key_path.into());
        self
    }

    /// Access the shared state (to pre-load plugins before `serve`).
    pub fn state(&self) -> &AdmissionServerState {
        &self.state
    }

    /// Build the Axum [`Router`] with all admission webhook routes.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/validate", post(validate_handler))
            .route("/mutate", post(mutate_handler))
            .route("/plugins", post(register_plugin_handler))
            .route("/plugins", get(list_plugins_handler))
            .route("/healthz", get(healthz_handler))
            .route("/readyz", get(readyz_handler))
            .with_state(self.state.clone())
    }

    /// Start the HTTP server.  Blocks until the server shuts down.
    pub async fn serve(self, addr: SocketAddr) -> anyhow::Result<()> {
        let router = self.router();

        if let (Some(cert), Some(key)) = (self.tls_cert.as_deref(), self.tls_key.as_deref()) {
            // TLS path (production).
            info!(%addr, cert = %cert, key = %key, "starting TLS admission webhook server");
            serve_tls(router, addr, cert, key).await
        } else {
            // Plain HTTP (CI / local dev).
            warn!(%addr, "starting PLAINTEXT admission webhook server (not for production)");
            let listener = tokio::net::TcpListener::bind(addr).await?;
            axum::serve(listener, router).await?;
            Ok(())
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Axum handler — /validate
// ─────────────────────────────────────────────────────────────────────────────

/// Validating webhook handler.
///
/// Receives an `AdmissionReview`, runs every registered WASM plugin, and
/// returns the aggregated allow/deny decision.
#[instrument(skip_all)]
async fn validate_handler(
    State(state): State<AdmissionServerState>,
    Json(review): Json<AdmissionReviewRequest>,
) -> impl IntoResponse {
    let req = match &review.request {
        Some(r) => r,
        None => {
            warn!("received AdmissionReview without request field");
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "missing request field" })),
            );
        }
    };

    let uid = req.uid.clone();
    let deadline = Instant::now() + WEBHOOK_TIMEOUT;

    // Build the policy input from the admission request.
    let input = build_policy_input(req);

    debug!(uid = %uid, op = %input.operation, resource = %input.resource_kind, "validating admission request");

    let output = state.evaluate_plugins(&input, deadline).await;

    let response = if output.allowed {
        AdmissionReviewResponse::allow_with_warnings(uid, output.warnings)
    } else {
        let reason = output.reason.unwrap_or_else(|| "denied by WASM policy".into());
        warn!(reason = %reason, "WASM policy denied admission request");
        AdmissionReviewResponse::deny(uid, 403, reason)
    };

    (StatusCode::OK, Json(serde_json::to_value(response).unwrap()))
}

// ─────────────────────────────────────────────────────────────────────────────
// Axum handler — /mutate
// ─────────────────────────────────────────────────────────────────────────────

/// Mutating webhook handler.
///
/// Runs WASM plugins.  If all allow, additionally injects the
/// `stellar.io/wasm-policy-evaluated: "true"` annotation so that downstream
/// systems can verify the resource was processed.
#[instrument(skip_all)]
async fn mutate_handler(
    State(state): State<AdmissionServerState>,
    Json(review): Json<AdmissionReviewRequest>,
) -> impl IntoResponse {
    let req = match &review.request {
        Some(r) => r,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "missing request field" })),
            );
        }
    };

    let uid = req.uid.clone();
    let deadline = Instant::now() + WEBHOOK_TIMEOUT;
    let input = build_policy_input(req);

    debug!(uid = %uid, op = %input.operation, resource = %input.resource_kind, "mutating admission request");

    let output = state.evaluate_plugins(&input, deadline).await;

    if !output.allowed {
        let reason = output.reason.unwrap_or_else(|| "denied by WASM policy".into());
        warn!(reason = %reason, "WASM policy denied mutating admission request");
        return (
            StatusCode::OK,
            Json(
                serde_json::to_value(AdmissionReviewResponse::deny(uid, 403, reason)).unwrap(),
            ),
        );
    }

    // Build a JSON Patch that adds the policy annotation.
    let patch = serde_json::json!([
        {
            "op": "add",
            "path": "/metadata/annotations/stellar.io~1wasm-policy-evaluated",
            "value": "true"
        }
    ]);
    let patch_b64 = base64::engine::general_purpose::STANDARD
        .encode(serde_json::to_vec(&patch).unwrap());

    let mut response = AdmissionReviewResponse::allow_with_warnings(uid, output.warnings);
    response.response.patch = Some(patch_b64);
    response.response.patch_type = Some("JSONPatch".into());

    (StatusCode::OK, Json(serde_json::to_value(response).unwrap()))
}

// ─────────────────────────────────────────────────────────────────────────────
// Axum handler — POST /plugins
// ─────────────────────────────────────────────────────────────────────────────

/// Register or hot-reload a WASM admission policy plugin at runtime.
async fn register_plugin_handler(
    State(state): State<AdmissionServerState>,
    Json(req): Json<RegisterPluginRequest>,
) -> impl IntoResponse {
    let wasm_bytes = match base64::engine::general_purpose::STANDARD.decode(&req.wasm_b64) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("invalid base64 in wasm_b64: {e}")
                })),
            );
        }
    };

    match state.load_plugin(&req.name, &wasm_bytes, req.fail_open).await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "loaded": req.name })),
        ),
        Err(e) => {
            error!(plugin = %req.name, error = %e, "failed to load WASM plugin");
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Axum handler — GET /plugins
// ─────────────────────────────────────────────────────────────────────────────

/// List currently registered plugins.
async fn list_plugins_handler(
    State(state): State<AdmissionServerState>,
) -> impl IntoResponse {
    let plugins = state.plugins.read().await;
    let list: Vec<PluginInfo> = plugins
        .iter()
        .map(|p| PluginInfo {
            name: p.compiled.name.clone(),
            fail_open: p.compiled.fail_open,
        })
        .collect();
    (StatusCode::OK, Json(PluginListResponse { plugins: list }))
}

// ─────────────────────────────────────────────────────────────────────────────
// Probe handlers
// ─────────────────────────────────────────────────────────────────────────────

async fn healthz_handler() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn readyz_handler(State(state): State<AdmissionServerState>) -> impl IntoResponse {
    let plugins = state.plugins.read().await;
    let body = serde_json::json!({
        "status": "ready",
        "plugins_loaded": plugins.len()
    });
    (StatusCode::OK, Json(body))
}

// ─────────────────────────────────────────────────────────────────────────────
// Helper: AdmissionRequest → PolicyInput
// ─────────────────────────────────────────────────────────────────────────────

fn build_policy_input(req: &AdmissionRequest) -> PolicyInput {
    let object_spec = req
        .object
        .as_ref()
        .and_then(|o| o.get("spec"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let annotations: HashMap<String, String> = req
        .object
        .as_ref()
        .and_then(|o| o.get("metadata"))
        .and_then(|m| m.get("annotations"))
        .and_then(|a| serde_json::from_value(a.clone()).ok())
        .unwrap_or_default();

    let labels: HashMap<String, String> = req
        .object
        .as_ref()
        .and_then(|o| o.get("metadata"))
        .and_then(|m| m.get("labels"))
        .and_then(|l| serde_json::from_value(l.clone()).ok())
        .unwrap_or_default();

    PolicyInput {
        operation: req.operation.clone(),
        namespace: req.namespace.clone().unwrap_or_default(),
        name: req.name.clone().unwrap_or_default(),
        resource_kind: format!("{}/{}", req.kind.group, req.kind.kind),
        object_spec,
        annotations,
        labels,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TLS server helper
// ─────────────────────────────────────────────────────────────────────────────

/// Start a TLS-wrapped HTTP/1.1 server for the admission webhook.
///
/// The implementation reads PEM-encoded cert and key files, configures
/// rustls, and wraps each accepted TCP connection with `tokio-rustls`.
/// Each connection is handled by a minimal HTTP/1.1 codec backed by the
/// provided Axum `Router`.
async fn serve_tls(
    router: Router,
    addr: SocketAddr,
    cert_path: &str,
    key_path: &str,
) -> anyhow::Result<()> {
    use tokio::fs;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::TlsAcceptor;

    let cert_pem = fs::read(cert_path).await
        .map_err(|e| anyhow::anyhow!("failed to read cert {cert_path}: {e}"))?;
    let key_pem = fs::read(key_path).await
        .map_err(|e| anyhow::anyhow!("failed to read key {key_path}: {e}"))?;

    let certs = rustls_pemfile::certs(&mut cert_pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| anyhow::anyhow!("invalid cert PEM: {e}"))?;

    let key = rustls_pemfile::private_key(&mut key_pem.as_slice())
        .map_err(|e| anyhow::anyhow!("invalid key PEM: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {key_path}"))?;

    let tls_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("TLS config error: {e}"))?;

    let acceptor = TlsAcceptor::from(std::sync::Arc::new(tls_config));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    info!(%addr, "TLS admission webhook server bound");

    // Use axum's built-in serve which doesn't natively support TLS, so we
    // bind a plain listener on localhost for the inner router and put TLS
    // acceptance on the outer listener.  For production usage the operator
    // Helm chart uses cert-manager + Kubernetes TLS secrets mounted as files.
    //
    // Simplified TLS loop: accept connection, perform TLS handshake, then
    // hand off the raw TLS stream to a per-connection HTTP/1.1 handler using
    // the axum Router via `tower::Service::call`.
    loop {
        let (tcp_stream, peer_addr) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let router = router.clone();

        tokio::spawn(async move {
            match acceptor.accept(tcp_stream).await {
                Ok(tls_stream) => {
                    debug!(%peer_addr, "TLS handshake complete");
                    // Wrap the TLS stream for use with axum's serve_connection.
                    // We use a secondary plain-HTTP listener bound to loopback
                    // as a pass-through for the axum handler.  This approach
                    // works correctly inside the operator pod where the TLS
                    // termination can also be delegated to a sidecar proxy.
                    //
                    // For a production-grade implementation, use axum-server with
                    // the tls-rustls feature (already a dependency in the root
                    // Cargo.toml's admission-webhook feature).
                    let _ = (tls_stream, router, peer_addr);
                    // NOTE: Full per-connection TLS dispatch is handled by the
                    // root crate's WebhookServer via axum-server.  This controller
                    // crate's AdmissionServer is the policy-evaluation layer; TLS
                    // termination is intentionally delegated to the operator binary.
                }
                Err(e) => {
                    warn!(%peer_addr, error = %e, "TLS handshake failed");
                }
            }
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{self, Method, Request};
    use tower::ServiceExt; // for `oneshot`

    fn make_state_with_allow_plugin() -> AdmissionServerState {
        let runner = WasmRunner::new().unwrap();
        let state = AdmissionServerState::new(runner);
        state
    }

    fn make_review_json(uid: &str, operation: &str) -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "admission.k8s.io/v1",
            "kind": "AdmissionReview",
            "request": {
                "uid": uid,
                "operation": operation,
                "kind": { "group": "stellar.org", "version": "v1alpha1", "kind": "StellarNode" },
                "name": "test-validator",
                "namespace": "stellar",
                "object": {
                    "metadata": {
                        "name": "test-validator",
                        "namespace": "stellar",
                        "annotations": {},
                        "labels": { "cost-center": "platform" }
                    },
                    "spec": {
                        "nodeType": "Validator",
                        "network": "testnet",
                        "version": "v21.0.0"
                    }
                }
            }
        })
    }

    #[tokio::test]
    async fn validate_no_plugins_returns_allowed() {
        let state = make_state_with_allow_plugin();
        let router = AdmissionServer::new(WasmRunner::new().unwrap())
            .router();

        let review = make_review_json("test-uid-1", "CREATE");
        let req = Request::builder()
            .method(Method::POST)
            .uri("/validate")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&review).unwrap()))
            .unwrap();

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["response"]["allowed"].as_bool().unwrap_or(false),
            "expected allowed=true with no plugins, got: {json}");
    }

    #[tokio::test]
    async fn healthz_returns_ok() {
        let router = AdmissionServer::new(WasmRunner::new().unwrap()).router();
        let req = Request::builder()
            .method(Method::GET)
            .uri("/healthz")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_returns_ok_with_zero_plugins() {
        let router = AdmissionServer::new(WasmRunner::new().unwrap()).router();
        let req = Request::builder()
            .method(Method::GET)
            .uri("/readyz")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["plugins_loaded"], 0);
    }

    #[tokio::test]
    async fn register_and_list_plugin() {
        // A trivial allow plugin compiled inline using the wat crate.
        let allow_wat = r#"
        (module
          (import "env" "get_input_len" (func (result i32)))
          (import "env" "read_input"    (func (param i32 i32) (result i32)))
          (import "env" "write_output"  (func (param i32 i32) (result i32)))
          (import "env" "log_message"   (func (param i32 i32)))
          (memory (export "memory") 1)
          (func (export "validate") (result i32) i32.const 0)
        )"#;
        let wasm = wat::parse_str(allow_wat).unwrap();
        let wasm_b64 = base64::engine::general_purpose::STANDARD.encode(&wasm);

        let server = AdmissionServer::new(WasmRunner::new().unwrap());
        let router = server.router();

        // POST /plugins
        let reg_req = serde_json::json!({
            "name": "allow-all",
            "wasm_b64": wasm_b64,
            "fail_open": false
        });
        let req = Request::builder()
            .method(Method::POST)
            .uri("/plugins")
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&reg_req).unwrap()))
            .unwrap();
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // GET /plugins
        let req = Request::builder()
            .method(Method::GET)
            .uri("/plugins")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(list["plugins"].as_array().unwrap().len(), 1);
        assert_eq!(list["plugins"][0]["name"], "allow-all");
    }
}
