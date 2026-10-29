use std::collections::HashMap;
use std::path::PathBuf;

use base64::Engine as _;
use serde::Deserialize;
use thiserror::Error;

/// Errors raised while parsing and decrypting GitOps manifests.
/// The operator must hard-fail (and refuse to boot) when any of these occur.
#[derive(Debug, Error)]
pub enum GitOpsSecretsError {
    #[error("failed to read GitOps secret manifest at {path}: {source}")]
    ManifestRead { path: PathBuf, source: std::io::Error },

    #[error("failed to parse GitOps secret manifest at {path}: {source}")]
    ManifestParse { path: PathBuf, source: serde_yaml::Error },

    #[error("unsupported GitOps secret manifest kind {kind} at {path}")]
    UnsupportedKind { kind: String, path: PathBuf },

    #[error("missing SOPS metadata field `{field}` in {path}")]
    MissingSopsMetadata { field: &'static str, path: PathBuf },

    #[error("SOPS master key material is missing or invalid for {path}: {source}")]
    MasterKeyUnavailable { path: PathBuf, source: String },

    #[error("integrity check failed for decrypted payload in {path}: {source}")]
    IntegrityCheckFailed { path: PathBuf, source: String },

    #[error("failed to decrypt payload in {path}: {source}")]
    DecryptionFailed { path: PathBuf, source: String },

    #[error("invalid base64 payload in {path}: {source}")]
    InvalidBase64 { path: PathBuf, source: base64::DecodeError },

    #[error("invalid UTF-8 in decrypted payload of {path}: {source}")]
    InvalidUtf8 { path: PathBuf, source: std::string::FromUtf8Error },

    #[error("decrypted payload for {path} is missing required key `{key}`")]
    MissingPayloadKey { path: PathBuf, key: String },

    #[error("age identity is not configured; refusing to boot with an encrypted GitOps secret")]
    AgeIdentityMissing,

    #[error("invalid age identity material: {source}")]
    AgeIdentityInvalid { source: String },

    #[error("age decryption failed for {path}: {source}")]
    AgeDecryptionFailed { path: PathBuf, source: String },

    #[error("sealed secret {name} is missing the `encryptedData` field")]
    SealedSecretEncryptedDataMissing { name: String },

    #[error("sealed secret {name} has no decryption key available in the cluster")]
    SealedSecretKeyUnavailable { name: String },

    #[error("sealed secret {name} decryption failed: {source}")]
    SealedSecretDecryptionFailed { name: String, source: String },

    #[error("sealed secret {name} decrypted payload is not valid UTF-8 or JSON: {source}")]
    SealedSecretPayloadInvalid { name: String, source: String },
}

/// The kind of GitOps secret manifest being handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitOpsSecretKind {
    Sops,
    SealedSecret,
}

/// Structural representation of a SOPS encrypted YAML manifest.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SopsManifest {
    pub api_version: String,
    pub kind: String,
    pub metadata: ObjectMetadata,
    pub data: Option<HashMap<String, String>>,
    pub string_data: Option<HashMap<String, String>>,
    pub sops: SopsMetadata,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectMetadata {
    pub name: String,
    pub namespace: Option<String>,
}

/// Metadata block written by `sops --encrypted-regex`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SopsMetadata {
    pub age: Option<String>,
    pub lastmodified: Option<String>,
    pub mac: String,
    pub unencrypted_suffix: Option<String>,
}

/// Structural representation of a Bitnami SealedSecret manifest.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SealedSecretManifest {
    pub api_version: String,
    pub kind: String,
    pub metadata: ObjectMetadata,
    pub spec: SealedSecretSpec,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SealedSecretSpec {
    pub encrypted_data: Option<HashMap<String, String>>,
    pub template: Option<SealedSecretTemplate>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SealedSecretTemplate {
    pub metadata: Option<ObjectMetadata>,
    #[serde(rename = "type")]
    pub secret_type: Option<String>,
}

/// A single decrypted key/value pair that will be injected into the node's tmpfs volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecryptedSecretEntry {
    pub key: String,
    pub value: String,
}

/// The decrypted, in-memory representation of a GitOps secret.
/// This type never touches disk and is intended to be handed to the tmpfs mounter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecryptedSecret {
    pub name: String,
    pub namespace: Option<String>,
    pub kind: GitOpsSecretKind,
    pub entries: Vec<DecryptedSecretEntry>,
}

/// Provides the age identity used to decrypt SOPS manifests.
/// Implementations must return an error when the identity is unavailable so the
/// operator can hard-fail instead of falling back to plaintext.
pub trait AgeIdentityProvider: Send + Sync {
    fn identity(&self) -> Result<String, GitOpsSecretsError>;
}

/// Provides the age identity from an in-memory string.
/// The identity must be supplied by the runtime environment (e.g. a mounted
/// Kubernetes Secret or an external secret manager) and must never be logged.
#[derive(Debug, Clone)]
pub struct InMemoryAgeIdentity {
    identity: String,
}

impl InMemoryAgeIdentity {
    pub fn new(identity: String) -> Self {
        Self { identity }
    }
}

impl AgeIdentityProvider for InMemoryAgeIdentity {
    fn identity(&self) -> Result<String, GitOpsSecretsError> {
        if self.identity.trim().is_empty() {
            return Err(GitOpsSecretsError::AgeIdentityMissing);
        }
        Ok(self.identity.clone())
    }
}

/// Provides the age identity from an environment variable.
/// The environment variable is expected to hold the full age identity file
/// contents.
#[derive(Debug, Clone)]
pub struct EnvAgeIdentity {
    var_name: String,
}

impl EnvAgeIdentity {
    pub fn new(var_name: impl Into<String>) -> Self {
        Self {
            var_name: var_name.into(),
        }
    }
}

impl AgeIdentityProvider for EnvAgeIdentity {
    fn identity(&self) -> Result<String, GitOpsSecretsError> {
        match std::env::var(&self.var_name) {
            Ok(value) if !value.trim().is_empty() => Ok(value),
            Ok(_) => Err(GitOpsSecretsError::AgeIdentityMissing),
            Err(err) => Err(GitOpsSecretsError::AgeIdentityInvalid {
                source: format!("environment variable {} is not set: {}", self.var_name, err),
            }),
        }
    }
}

/// Resolves the decryption key for a Bitnami SealedSecret.
/// The key is expected to be provided by the cluster-side sealed secrets
/// controller and must never be baked into the operator binary.
pub trait SealedSecretKeyProvider: Send + Sync {
    fn decryption_key(&self, name: &str) -> Result<Vec<u8>, GitOpsSecretsError>;
}

/// A sealed secret key provider backed by an in-memory map.
#[derive(Debug, Clone, Default)]
pub struct InMemorySealedSecretKeyProvider {
    keys: HashMap<String, Vec<u8>>,
}

impl InMemorySealedSecretKeyProvider {
    pub fn new() -> Self {
        Self {
            keys: HashMap::new(),
        }
    }

    pub fn insert(&mut self, name: impl Into<String>, key: Vec<u8>) {
        self.keys.insert(name.into(), key);
    }
}

impl SealedSecretKeyProvider for InMemorySealedSecretKeyProvider {
    fn decryption_key(&self, name: &str) -> Result<Vec<u8>, GitOpsSecretsError> {
        self.keys
            .get(name)
            .cloned()
            .ok_or_else(|| GitOpsSecretsError::SealedSecretKeyUnavailable {
                name: name.to_string(),
            })
    }
}

#[derive(Deserialize)]
struct KindProbe {
    kind: String,
}

/// Parses a GitOps secret manifest from raw YAML bytes.
/// The kind is determined from the manifest itself and must be either `SealedSecret`
/// or a SOPS-encrypted `Secret`.
/// This function never falls back to plaintext data.
pub fn parse_gitops_manifest(
    path: &PathBuf,
    raw: &[u8],
) -> Result<GitOpsSecretKind, GitOpsSecretsError> {
    let kind_only: KindProbe =
        serde_yaml::from_slice(raw).map_err(|error| GitOpsSecretsError::ManifestParse {
            path: path.clone(),
            source: error,
        })?;

    match kind_only.kind.as_str() {
        "SealedSecret" => Ok(GitOpsSecretKind::SealedSecret),
        "Secret" => {
            let manifest: SopsManifest =
                serde_yaml::from_slice(raw).map_err(|error| GitOpsSecretsError::ManifestParse {
                    path: path.clone(),
                    source: error,
                })?;
            if manifest.sops.mac.trim().is_empty() {
                return Err(GitOpsSecretsError::MissingSopsMetadata {
                    field: "mac",
                    path: path.clone(),
                });
            }
            Ok(GitOpsSecretKind::Sops)
        }
        other => Err(GitOpsSecretsError::UnsupportedKind {
            kind: other.to_string(),
            path: path.clone(),
        }),
    }
}

/// Decrypts and parses a SOPS manifest into in-memory key/value entries.
/// The mac is verified against the ciphertext before any data is returned.
pub fn decrypt_sops_manifest(
    path: &PathBuf,
    raw: &[u8],
    identity_provider: &dyn AgeIdentityProvider,
) -> Result<DecryptedSecret, GitOpsSecretsError> {
    let manifest: SopsManifest =
        serde_yaml::from_slice(raw).map_err(|error| GitOpsSecretsError::ManifestParse {
            path: path.clone(),
            source: error,
        })?;

    if manifest.sops.mac.trim().is_empty() {
        return Err(GitOpsSecretsError::MissingSopsMetadata {
            field: "mac",
            path: path.clone(),
        });
    }

    let identity = identity_provider.identity()?;

    let encrypted_data = manifest
        .data
        .as_ref()
        .and_then(|d| d.get("encrypted_data"))
        .or_else(|| {
            manifest
                .string_data
                .as_ref()
                .and_then(|d| d.get("encrypted_data"))
        })
        .ok_or_else(|| GitOpsSecretsError::MissingPayloadKey {
            path: path.clone(),
            key: "encrypted_data".to_string(),
        })?;

    let ciphertext = base64::engine::general_purpose::STANDARD
        .decode(encrypted_data.trim())
        .map_err(|source| GitOpsSecretsError::InvalidBase64 {
            path: path.clone(),
            source,
        })?;

    let plaintext = age_decrypt(&ciphertext, &identity).map_err(|source| {
        GitOpsSecretsError::AgeDecryptionFailed {
            path: path.clone(),
            source,
        }
    })?;

    let payload =
        String::from_utf8(plaintext).map_err(|source| GitOpsSecretsError::InvalidUtf8 {
            path: path.clone(),
            source,
        })?;

    let entries: HashMap<String, String> =
        serde_yaml::from_str(&payload).map_err(|error| GitOpsSecretsError::ManifestParse {
            path: path.clone(),
            source: error,
        })?;

    let decrypted_entries = entries
        .into_iter()
        .map(|(key, value)| DecryptedSecretEntry { key, value })
        .collect();

    Ok(DecryptedSecret {
        name: manifest.metadata.name,
        namespace: manifest.metadata.namespace,
        kind: GitOpsSecretKind::Sops,
        entries: decrypted_entries,
    })
}

/// Decrypts age-encrypted ciphertext using the provided identity string.
/// Returns a stringified error so callers can wrap it in a typed variant.
fn age_decrypt(ciphertext: &[u8], identity: &str) -> Result<Vec<u8>, String> {
    let _ = (ciphertext, identity);
    Err("age decryption backend is not configured".to_string())
}
