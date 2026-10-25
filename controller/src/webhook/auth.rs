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
//! HashiCorp Vault authentication — Kubernetes Service Account auth method.
//!
//! This module implements the Vault client used by the
//! [`vault_injector`](super::vault_injector) admission webhook (and by the
//! integration tests) to authenticate against a Vault server using the
//! Kubernetes auth method.
//!
//! # How Kubernetes auth works
//!
//! Every pod in Kubernetes receives a projected service account JWT at
//! `/var/run/secrets/kubernetes.io/serviceaccount/token`.  The client sends the
//! JWT to the Vault Kubernetes auth mount:
//!
//! ```text
//! POST /v1/<mount>/login  { "role": "<role>", "jwt": "<service-account-jwt>" }
//!       │
//!       ▼
//! Vault verifies the JWT against the TokenReview API and returns a short
//! lived client token (`auth.client_token`, `auth.lease_duration`).
//! ```
//!
//! The returned token is held **only in process memory** — it is never written
//! to disk, never injected into pod environment variables, and is zeroized on
//! drop.  Tokens are cached until 80 % of the lease has elapsed, then
//! transparently renewed with a fresh login.
//!
//! # Security properties
//!
//! - The service account JWT is read from the projected token file on demand;
//!   it is never stored in configuration or passed via environment variables.
//! - Client tokens live in [`SecretToken`], which overwrites the backing
//!   allocation when dropped.
//! - Vault responses are fetched over the address configured in
//!   [`VaultAuthConfig::vault_addr`]; plaintext `http://` addresses produce a
//!   loud warning at construction time because the token would transit in
//!   cleartext.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, instrument, warn};

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// Default Vault Kubernetes auth mount path.
pub const DEFAULT_AUTH_MOUNT: &str = "kubernetes";

/// Projected service account JWT mounted into every pod by the kubelet.
pub const DEFAULT_JWT_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";

/// Fraction of the lease duration to keep a cached token before renewing.
///
/// A margin is required because clocks drift between the webhook pod and the
/// Vault server; refreshing at 80 % guarantees we never send an expired token.
pub const TOKEN_RENEWAL_MARGIN: f64 = 0.8;

/// Default timeout for a single Vault HTTP round trip.  Must be well under the
/// 10 s Kubernetes admission deadline.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

// ─────────────────────────────────────────────────────────────────────────────
// Errors
// ─────────────────────────────────────────────────────────────────────────────

/// Errors that can occur while authenticating with Vault or reading secrets.
#[derive(Debug, Error)]
pub enum VaultAuthError {
    /// The configuration is incomplete or invalid.
    #[error("invalid vault auth configuration: {0}")]
    Config(String),

    /// The HTTP transport failed (connection refused, TLS, timeout, …).
    #[error("vault connection error: {0}")]
    Http(#[from] reqwest::Error),

    /// Vault rejected the request with a non-success status.
    #[error("vault returned HTTP {code}: {message}")]
    Vault { code: u16, message: String },

    /// The login flow did not yield a usable client token.
    #[error("vault did not return a client token")]
    NoToken,

    /// The request/response payload could not be (de)serialized.
    #[error("vault payload error: {0}")]
    Payload(String),

    /// The secret response is malformed or missing the requested form.
    #[error("malformed secret payload: {0}")]
    MalformedSecret(String),
}

// ─────────────────────────────────────────────────────────────────────────────
// Configuration
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration for [`VaultAuth`].
#[derive(Debug, Clone)]
pub struct VaultAuthConfig {
    /// Base address of the Vault cluster, e.g. `https://vault.vault.svc:8200`.
    pub vault_addr: String,
    /// Kubernetes auth mount path (without a leading slash).
    pub auth_mount: String,
    /// Vault role bound to the validator service account.
    pub role: String,
    /// Absolute path of the projected service account JWT.
    pub jwt_path: PathBuf,
    /// Per-request HTTP timeout.
    pub request_timeout: Duration,
}

impl VaultAuthConfig {
    /// Build a config from explicit values (used in tests and by the injector
    /// overlay logic).
    pub fn new(vault_addr: impl Into<String>, role: impl Into<String>) -> Self {
        Self {
            vault_addr: vault_addr.into(),
            auth_mount: DEFAULT_AUTH_MOUNT.to_string(),
            role: role.into(),
            jwt_path: PathBuf::from(DEFAULT_JWT_PATH),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }

    fn validate(&self) -> Result<(), VaultAuthError> {
        if self.vault_addr.is_empty() {
            return Err(VaultAuthError::Config("vault_addr is empty".into()));
        }
        if !self.vault_addr.starts_with("http://") && !self.vault_addr.starts_with("https://") {
            return Err(VaultAuthError::Config(format!(
                "vault_addr must start with http:// or https://, got {}",
                self.vault_addr
            )));
        }
        if self.role.is_empty() {
            return Err(VaultAuthError::Config("role is empty".into()));
        }
        if !self.jwt_path.is_absolute() {
            return Err(VaultAuthError::Config(
                "jwt_path must be an absolute path".into(),
            ));
        }
        if self.auth_mount.trim() != self.auth_mount || self.auth_mount.starts_with('/') {
            return Err(VaultAuthError::Config(format!(
                "auth_mount must not contain leading/trailing whitespace or a leading slash, got {:?}",
                self.auth_mount
            )));
        }
        if self.vault_addr.starts_with("http://") {
            warn!(
                addr = %self.vault_addr,
                "vault address uses plaintext http — tokens transit in cleartext; \
                 use https:// in production"
            );
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Secrets in memory
// ─────────────────────────────────────────────────────────────────────────────

/// A sensitive string that is zeroized when dropped.
///
/// Tokens and other credentials must live as short as possible in process
/// memory; this wrapper overwrites the backing allocation on drop instead of
/// relying on the allocator to have never reclaimed the bytes.
#[derive(Clone)]
pub struct SecretToken(String);

impl SecretToken {
    /// Wraps the given value.
    ///
    /// Note: the value transiently exists unzeroized in the caller — that is
    /// the smallest window achievable without `const`-time zeroization.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Exposes the token for the duration of a use site.  Prefer borrowing
    /// [`Self::as_str`] over cloning the value.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Drop for SecretToken {
    fn drop(&mut self) {
        zeroize(&mut self.0);
    }
}

impl std::fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never leak the value into logs/panics.
        write!(f, "SecretToken(***)")
    }
}

/// Overwrites a string with zeroes (best-effort: the allocation may be
/// observed by a debugger between the stores and the free, which is the
/// smallest exposure window available in safe Rust).
fn zeroize(buf: &mut String) {
    for byte in unsafe { buf.as_bytes_mut() } {
        *byte = 0;
    }
    buf.clear();
}

// ─────────────────────────────────────────────────────────────────────────────
// Wire types (minimal subset of the Vault API)
// ─────────────────────────────────────────────────────────────────────────────

/// `POST /v1/<auth_mount>/login` request body.
/// Vault expects plain `role` + `jwt` fields (no name munging).
#[derive(Debug, Clone, Serialize)]
pub struct VaultLoginRequest {
    pub role: String,
    pub jwt: String,
}

/// `POST /v1/<auth_mount>/login` success response (subset).
/// Vault serialises these fields in snake_case (`client_token`,
/// `lease_duration`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VaultLoginResponse {
    pub auth: VaultLoginAuth,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VaultLoginAuth {
    pub client_token: String,
    /// Lease TTL in seconds.
    #[serde(default)]
    pub lease_duration: u64,
    #[serde(default)]
    pub renewable: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Cached token
// ─────────────────────────────────────────────────────────────────────────────

/// A Vault client token cached together with its expiry instant.
#[derive(Debug)]
pub struct CachedToken {
    pub token: SecretToken,
    pub expires_at: Instant,
}

impl CachedToken {
    /// `true` while the token is still usable (with the renewal margin).
    pub fn is_fresh(&self) -> bool {
        Instant::now() < self.expires_at
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Client
// ─────────────────────────────────────────────────────────────────────────────

/// Vault client authenticating via the Kubernetes auth method.
///
/// Tokens are cached in memory and renewed at 80 % of the lease.  The client
/// keeps no secrets on disk; see the module docs for the full security model.
#[derive(Clone)]
pub struct VaultAuth {
    config: VaultAuthConfig,
    http: reqwest::Client,
    cached: Arc<RwLock<Option<CachedToken>>>,
    login_count: Arc<AtomicU64>,
}

impl std::fmt::Debug for VaultAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultAuth")
            .field("vault_addr", &self.config.vault_addr)
            .field("auth_mount", &self.config.auth_mount)
            .field("role", &self.config.role)
            .finish_non_exhaustive()
    }
}

impl VaultAuth {
    /// Creates a client.  Fails on invalid configuration; `http://` addresses
    /// are allowed (useful for tests/dev) but warn loudly.
    pub fn new(config: VaultAuthConfig) -> Result<Self, VaultAuthError> {
        config.validate()?;
        let http = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .map_err(VaultAuthError::Http)?;
        Ok(Self {
            config,
            http,
            cached: Arc::new(RwLock::new(None)),
            login_count: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Read-only access to the configuration (does not expose secrets).
    pub fn config(&self) -> &VaultAuthConfig {
        &self.config
    }

    /// Number of cached tokens kept — always 0 or 1.  Exposed for tests.
    pub async fn has_cached_token(&self) -> bool {
        self.cached
            .read()
            .await
            .as_ref()
            .is_some_and(|t| t.is_fresh())
    }

    /// Number of logins performed.  Exposed for tests and metrics.
    pub fn login_count(&self) -> u64 {
        self.login_count.load(Ordering::SeqCst)
    }

    /// Login endpoint URL: `<addr>/v1/<mount>/login`.
    fn login_url(&self) -> String {
        format!(
            "{}/v1/{}/login",
            self.config.vault_addr.trim_end_matches('/'),
            self.config.auth_mount
        )
    }

    /// Reads the projected service account JWT from disk.
    ///
    /// The JWT is read on demand and never cached: the kubelet rotates the
    /// projected token, so a cached copy would go stale.
    fn read_jwt(&self) -> Result<String, VaultAuthError> {
        let jwt: String = std::fs::read_to_string(&self.config.jwt_path)
            .map_err(|e| {
                VaultAuthError::Config(format!(
                    "cannot read JWT {}: {e}",
                    self.config.jwt_path.display()
                ))
            })?
            .trim()
            .to_string();
        if jwt.is_empty() {
            return Err(VaultAuthError::Config(format!(
                "service account token file {} is empty",
                self.config.jwt_path.display()
            )));
        }
        Ok(jwt)
    }

    /// Performs one login round trip: `POST /v1/<mount>/login`.
    #[instrument(skip(self), fields(addr = %self.config.vault_addr, role = %self.config.role))]
    pub async fn login(&self) -> Result<(SecretToken, Duration), VaultAuthError> {
        let jwt = self.read_jwt()?;
        self.login_count.fetch_add(1, Ordering::SeqCst);
        let url = self.login_url();

        let resp = self
            .http
            .post(&url)
            .json(&VaultLoginRequest {
                role: self.config.role.clone(),
                jwt,
            })
            .send()
            .await?;

        let code = resp.status();
        if !code.is_success() {
            let message = resp
                .text()
                .await
                .unwrap_or_else(|_| "<unparseable body>".to_string());
            return Err(VaultAuthError::Vault {
                code: code.as_u16(),
                message,
            });
        }

        let body: VaultLoginResponse = resp
            .json()
            .await
            .map_err(|e| VaultAuthError::Payload(format!("login response: {e}")))?;

        if body.auth.client_token.is_empty() {
            return Err(VaultAuthError::NoToken);
        }

        let lease = Duration::from_secs(body.auth.lease_duration);
        debug!(
            lease_secs = body.auth.lease_duration,
            "vault login succeeded"
        );
        Ok((SecretToken::new(body.auth.client_token), lease))
    }

    /// Returns a usable client token: renews the cached one at 80 % of its
    /// lease and refreshes the cache after a successful login.
    #[instrument(skip(self))]
    pub async fn token(&self) -> Result<SecretToken, VaultAuthError> {
        {
            let cached = self.cached.read().await;
            if let Some(t) = cached.as_ref() {
                if t.is_fresh() {
                    return Ok(t.token.clone());
                }
            }
        }

        let (token, lease) = self.login().await?;
        let expires_in = lease.mul_f64(TOKEN_RENEWAL_MARGIN);
        *self.cached.write().await = Some(CachedToken {
            token: token.clone(),
            expires_at: Instant::now() + expires_in,
        });
        Ok(token)
    }

    /// Forgets any cached token (used after a 403 on an otherwise-valid call).
    #[instrument(skip(self))]
    pub async fn invalidate_cached(&self) {
        *self.cached.write().await = None;
    }

    /// GETs a KV-v2 secret at `path` (e.g. `secret/data/stellar/validator`)
    /// using a freshly obtained token.
    #[instrument(skip(self), fields(path))]
    pub async fn read_kv_v2(&self, path: &str) -> Result<Value, VaultAuthError> {
        let path = path.trim_matches('/');
        if path.is_empty() {
            return Err(VaultAuthError::Config("secret path is empty".into()));
        }

        let token = self.token().await?;
        let url = format!(
            "{}/v1/{}",
            self.config.vault_addr.trim_end_matches('/'),
            path
        );

        let resp = self
            .http
            .get(&url)
            .header("X-Vault-Token", token.as_str())
            .send()
            .await?;

        let code = resp.status();
        if !code.is_success() {
            // 403 from an already-issued token means the cache cannot retry usefully
            // on its own — drop it so the next call re-authenticates.
            if code.as_u16() == 403 {
                self.invalidate_cached().await;
            }
            let message = resp
                .text()
                .await
                .unwrap_or_else(|_| "<unparseable body>".to_string());
            return Err(VaultAuthError::Vault {
                code: code.as_u16(),
                message,
            });
        }

        resp.json()
            .await
            .map_err(|e| VaultAuthError::Payload(format!("secret response: {e}")))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Extracts a key from a KV-v2 secret document.
///
/// Supports both the modern `{ "data": { "data": { "key": … } } }` shape and
/// the legacy (non-wrap-ttl) `{ "data": { "key": … } }` shape.
pub fn extract_secure_field<'a>(doc: &'a Value, key: &str) -> Result<&'a Value, VaultAuthError> {
    if key.is_empty() {
        return Err(VaultAuthError::MalformedSecret(
            "secret key is empty".into(),
        ));
    }

    // Modern KV v2: data.data.<key>
    if let Some(v) = doc.pointer("/data/data").and_then(|d| d.get(key)) {
        return Ok(v);
    }
    // Legacy KV v1 / KV v2 without wrapping: data.<key>
    if let Some(v) = doc.pointer("/data").and_then(|d| d.get(key)) {
        return Ok(v);
    }

    Err(VaultAuthError::MalformedSecret(format!(
        "key {key:?} not found in secret payload"
    )))
}

/// Extracts a string field from a KV-v2 document (convenience over
/// [`extract_secure_field`]).
pub fn extract_secure_string(doc: &Value, key: &str) -> Result<String, VaultAuthError> {
    let v = extract_secure_field(doc, key)?;
    v.as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| VaultAuthError::MalformedSecret(format!("key {key:?} is not a string")))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    fn valid_config() -> VaultAuthConfig {
        VaultAuthConfig::new("https://vault.vault.svc:8200", "stellar-validator")
    }

    #[test]
    fn test_config_rejects_empty_addr() {
        let cfg = VaultAuthConfig::new("", "role");
        assert!(matches!(cfg.validate(), Err(VaultAuthError::Config(_))));
    }

    #[test]
    fn test_config_rejects_non_url_scheme() {
        let cfg = VaultAuthConfig::new("vault.vault.svc:8200", "role");
        assert!(matches!(cfg.validate(), Err(VaultAuthError::Config(_))));
    }

    #[test]
    fn test_config_rejects_empty_role() {
        let cfg = VaultAuthConfig::new("https://vault:8200", "");
        assert!(matches!(cfg.validate(), Err(VaultAuthError::Config(_))));
    }

    #[test]
    fn test_config_rejects_relative_jwt_path() {
        let cfg = VaultAuthConfig::new("https://vault:8200", "role");
        let mut cfg = cfg;
        cfg.jwt_path = PathBuf::from("relative/token");
        assert!(matches!(cfg.validate(), Err(VaultAuthError::Config(_))));
    }

    #[test]
    fn test_config_accepts_https() {
        valid_config().validate().expect("valid config must pass");
    }

    #[test]
    fn test_config_accepts_http_with_warning() {
        VaultAuthConfig::new("http://localhost:8200", "role")
            .validate()
            .expect("http config must pass (warns)");
    }

    #[test]
    fn test_auth_mount_rejects_leading_slash() {
        let mut cfg = valid_config();
        cfg.auth_mount = "/kubernetes".to_string();
        assert!(matches!(cfg.validate(), Err(VaultAuthError::Config(_))));
    }

    #[test]
    fn test_secret_token_redacts_debug() {
        let t = SecretToken::new("hvs.supersecret");
        assert!(!format!("{t:?}").contains("hvs.supersecret"));
    }

    #[test]
    fn test_login_url() {
        let auth = VaultAuth::new(valid_config()).unwrap();
        assert_eq!(
            auth.login_url(),
            "https://vault.vault.svc:8200/v1/kubernetes/login"
        );
    }

    #[test]
    fn test_extract_secure_field_modern_kv_v2() {
        let doc = json!({
            "data": {
                "data": { "seed": "SBPQUA55J7IX2XWNKRM4LJ3ITLXVMGNQPL6SPHRCVIYGGGRJA57LCCCC" }
            }
        });
        assert_eq!(
            extract_secure_string(&doc, "seed").unwrap(),
            "SBPQUA55J7IX2XWNKRM4LJ3ITLXVMGNQPL6SPHRCVIYGGGRJA57LCCCC"
        );
    }

    #[test]
    fn test_extract_secure_field_legacy_kv() {
        let doc = json!({
            "data": { "seed": "SBPQUA55J7IX2XWNKRM4LJ3ITLXVMGNQPL6SPHRCVIYGGGRJA57LCCCC" }
        });
        assert!(extract_secure_string(&doc, "seed").is_ok());
    }

    #[test]
    fn test_extract_secure_field_missing_key() {
        let doc = json!({"data": {"data": {"other": "x"}}});
        assert!(matches!(
            extract_secure_string(&doc, "seed"),
            Err(VaultAuthError::MalformedSecret(_))
        ));
    }

    #[test]
    fn test_extract_secure_field_empty_key() {
        let doc = json!({"data": {"data": {"seed": "x"}}});
        assert!(matches!(
            extract_secure_string(&doc, ""),
            Err(VaultAuthError::MalformedSecret(_))
        ));
    }

    #[test]
    fn test_extract_secure_field_non_string() {
        let doc = json!({"data": {"data": {"seed": 42}}});
        assert!(extract_secure_string(&doc, "seed").is_err());
    }

    #[test]
    fn test_cached_token_expiry() {
        let t = CachedToken {
            token: SecretToken::new("hvs.x"),
            expires_at: Instant::now() + Duration::from_secs(60),
        };
        assert!(t.is_fresh());
        let expired = CachedToken {
            token: SecretToken::new("hvs.x"),
            expires_at: Instant::now() - Duration::from_secs(1),
        };
        assert!(!expired.is_fresh());
    }

    #[tokio::test]
    async fn test_vault_auth_new_rejects_bad_config() {
        let cfg = VaultAuthConfig::new("not-a-url", "role");
        assert!(VaultAuth::new(cfg).is_err());
    }

    #[test]
    fn test_debug_does_not_leak_config_role_secrets() {
        let auth = VaultAuth::new(valid_config()).unwrap();
        let dbg = format!("{auth:?}");
        assert!(dbg.contains("stellar-validator"));
        assert!(!dbg.contains("client_token"));
    }

    /// Guard: reading the JWT from a non-existent path must produce a Config
    /// error, not a panic.
    #[test]
    fn test_read_jwt_missing_file() {
        let cfg = valid_config();
        let auth = VaultAuth::new(cfg).unwrap();
        assert!(matches!(auth.read_jwt(), Err(VaultAuthError::Config(_))));
    }

    #[test]
    fn test_zeroize_clears_buffer() {
        let mut buf = "supersecret".to_string();
        zeroize(&mut buf);
        assert!(buf.is_empty());
    }

    /// Sanity check that Path is used in tests too.
    #[test]
    fn test_jwt_path_type() {
        let p: &Path = Path::new(DEFAULT_JWT_PATH);
        assert!(p.is_absolute());
    }
}
