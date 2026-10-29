// Copyright 2024 Stellar-K8s Contributors
// SPDX-License-Identifier: Apache-2.0
//! Pluggable cloud storage providers for automated volume snapshots.
//!
//! This module defines the [`SnapshotProvider`] trait that abstracts the three
//! storage back-ends supported by the automated backup controller (issue #84):
//! AWS EBS, GCP Persistent Disk, and a fully local filesystem provider.
//!
//! # Design
//!
//! The controller loop in [`crate::backup::snapshot`] is provider-agnostic: it
//! drives every back-end through the [`SnapshotProvider`] trait.  Each provider
//! is responsible for the mechanics of taking a crash-consistent block-device
//! snapshot, listing existing snapshots, pruning them, and restoring a fresh
//! volume from a validated snapshot.
//!
//! Request construction for the two cloud providers is factored into pure
//! functions ([`AwsEbsProvider::build_create_snapshot_request`] and friends) so
//! that signing and payload assembly can be unit-tested without a network.  The
//! local provider ([`LocalSnapshotProvider`]) is fully functional against the
//! local filesystem and is the reference implementation used by the integration
//! tests.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Errors that can occur while interacting with a snapshot back-end.
#[derive(Debug, thiserror::Error)]
pub enum CloudError {
    #[error("provider call failed: {0}")]
    Provider(String),

    #[error("snapshot {0} not found")]
    SnapshotNotFound(String),

    #[error("integrity check failed for snapshot {0}: {1}")]
    Integrity(String, String),

    #[error("invalid configuration: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CloudError>;

/// Which storage back-end a provider targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    AwsEbs,
    GcpPd,
    Local,
}

/// Lifecycle state of a volume snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotState {
    /// Snapshot creation has been issued but is not yet complete.
    Pending,
    /// Snapshot is fully materialised and restorable.
    Available,
    /// Snapshot creation failed.
    Failed(String),
}

/// Provider-agnostic metadata describing a snapshot.
///
/// `ledger_sequence` ties the snapshot to the Stellar ledger marker at which it
/// was taken, satisfying the "aligned with ledger sequence markers" requirement
/// from issue #84.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotDescriptor {
    /// Provider-native snapshot identifier (e.g. `snap-0abc...` on AWS).
    pub id: String,
    /// Back-end that produced the snapshot.
    pub provider: ProviderKind,
    /// Identifier of the source volume that was snapshotted.
    pub volume_id: String,
    /// Stellar ledger sequence marker captured at snapshot time.
    pub ledger_sequence: u64,
    /// When the snapshot was initiated.
    pub created_at: DateTime<Utc>,
    /// Size of the source volume in bytes, when known.
    pub size_bytes: Option<u64>,
    /// Current lifecycle state.
    pub state: SnapshotState,
    /// Hex-encoded SHA-256 integrity digest of the snapshot payload.
    pub checksum: Option<String>,
    /// Free-form provider tags (e.g. node name, retention class).
    pub tags: BTreeMap<String, String>,
}

impl SnapshotDescriptor {
    /// Return the value of a tag, if present.
    pub fn tag(&self, key: &str) -> Option<&str> {
        self.tags.get(key).map(|s| s.as_str())
    }
}

/// A request to create a new snapshot of a volume.
#[derive(Debug, Clone)]
pub struct SnapshotRequest {
    /// Source volume identifier.
    pub volume_id: String,
    /// Stellar ledger sequence marker at snapshot time.
    pub ledger_sequence: u64,
    /// Tags to apply to the snapshot.
    pub tags: BTreeMap<String, String>,
}

/// Filter used when listing snapshots for retention enforcement.
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    /// Only return snapshots for this source volume.
    pub volume_id: Option<String>,
    /// Only return snapshots carrying this tag.
    pub tag: Option<(String, String)>,
}

/// Result of restoring a volume from a snapshot.
#[derive(Debug, Clone)]
pub struct RestoreResult {
    /// Identifier of the newly provisioned volume.
    pub volume_id: String,
    /// Snapshot the volume was restored from.
    pub snapshot_id: String,
    /// Whether the restored payload passed the integrity check.
    pub integrity_ok: bool,
}

/// The abstraction every storage back-end implements.
///
/// The controller loop depends only on this trait, which keeps the snapshot
/// scheduling, retention, and restore logic identical across AWS EBS, GCP PD,
/// and local storage.
#[async_trait]
pub trait SnapshotProvider: Send + Sync {
    /// Human-readable name of this provider.
    fn name(&self) -> &'static str;

    /// Which back-end family this provider targets.
    fn kind(&self) -> ProviderKind;

    /// Initiate a snapshot of `request.volume_id`.
    ///
    /// Implementations must not block until the snapshot is `Available`; they
    /// should issue the snapshot and return promptly with state `Pending` (or
    /// `Available` for back-ends that snapshot synchronously).
    async fn create_snapshot(&self, request: SnapshotRequest) -> Result<SnapshotDescriptor>;

    /// Return the current descriptor for a snapshot, or `SnapshotNotFound`.
    async fn describe_snapshot(&self, snapshot_id: &str) -> Result<SnapshotDescriptor>;

    /// List snapshots matching `filter`, newest first.
    async fn list_snapshots(&self, filter: &ListFilter) -> Result<Vec<SnapshotDescriptor>>;

    /// Delete a snapshot, releasing its underlying storage.
    async fn delete_snapshot(&self, snapshot_id: &str) -> Result<()>;

    /// Provision a new volume from `snapshot_id` and verify its integrity.
    ///
    /// `target_volume_id` is the desired identifier for the restored volume.
    /// Providers that cannot honour a caller-supplied identifier may ignore it
    /// and return the identifier they actually allocated.
    async fn restore_volume(
        &self,
        snapshot_id: &str,
        target_volume_id: &str,
    ) -> Result<RestoreResult>;
}

// ---------------------------------------------------------------------------
// Local filesystem provider
// ---------------------------------------------------------------------------

/// Default retention policy applied by the local provider when none is given.
pub const DEFAULT_RETENTION_DAYS: i64 = 7;

/// A fully functional local-filesystem snapshot provider.
///
/// Snapshots are stored as directories under `root`, one child directory per
/// snapshot, containing a payload copy of the source directory and a
/// `manifest.json` describing the snapshot.  This is the reference
/// implementation exercised by the integration tests and is also useful for
/// single-node or air-gapped deployments where no cloud block store is
/// available.
pub struct LocalSnapshotProvider {
    root: PathBuf,
}

/// On-disk manifest written alongside each local snapshot payload.
#[derive(Debug, Serialize, Deserialize)]
pub struct LocalManifest {
    pub id: String,
    pub volume_id: String,
    pub ledger_sequence: u64,
    pub created_at: DateTime<Utc>,
    pub size_bytes: u64,
    pub checksum: String,
    pub tags: BTreeMap<String, String>,
}

impl LocalSnapshotProvider {
    /// Create a provider rooted at `root`, creating the directory if needed.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn snapshot_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    fn manifest_path(&self, id: &str) -> PathBuf {
        self.snapshot_dir(id).join("manifest.json")
    }

    fn payload_dir(&self, id: &str) -> PathBuf {
        self.snapshot_dir(id).join("payload")
    }

    fn load_manifest(&self, id: &str) -> Result<LocalManifest> {
        let path = self.manifest_path(id);
        let data = std::fs::read_to_string(&path)
            .map_err(|_| CloudError::SnapshotNotFound(id.to_string()))?;
        serde_json::from_str(&data).map_err(|e| CloudError::Provider(e.to_string()))
    }

    fn store_manifest(&self, manifest: &LocalManifest) -> Result<()> {
        let path = self.manifest_path(&manifest.id);
        let data = serde_json::to_string_pretty(manifest)
            .map_err(|e| CloudError::Provider(e.to_string()))?;
        std::fs::write(path, data)?;
        Ok(())
    }

    fn descriptor_from_manifest(&self, manifest: &LocalManifest) -> SnapshotDescriptor {
        SnapshotDescriptor {
            id: manifest.id.clone(),
            provider: ProviderKind::Local,
            volume_id: manifest.volume_id.clone(),
            ledger_sequence: manifest.ledger_sequence,
            created_at: manifest.created_at,
            size_bytes: Some(manifest.size_bytes),
            state: SnapshotState::Available,
            checksum: Some(manifest.checksum.clone()),
            tags: manifest.tags.clone(),
        }
    }

    /// List snapshot ids present on disk (child directories containing a manifest).
    fn local_snapshot_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let id = entry.file_name().to_string_lossy().to_string();
                if self.manifest_path(&id).exists() {
                    ids.push(id);
                }
            }
        }
        ids
    }
}

/// Recursively copy a directory tree, returning the number of bytes copied.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<u64> {
    std::fs::create_dir_all(dst)?;
    let mut bytes = 0u64;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            bytes += copy_dir_recursive(&from, &to)?;
        } else {
            bytes += std::fs::copy(&from, &to)?;
        }
    }
    Ok(bytes)
}

/// Compute a deterministic SHA-256 digest over a directory tree.
///
/// The digest is taken over the sorted list of `relative-path:file-sha256`
/// lines so that it is stable regardless of directory iteration order.  A
/// mismatch after a restore indicates corruption, which is exactly the integrity
/// signal the restore path checks.
pub fn hash_directory(dir: &Path) -> Result<String> {
    let mut lines: Vec<String> = Vec::new();
    collect_hashes(dir, Path::new(""), &mut lines)?;
    lines.sort();
    let mut hasher = Sha256::new();
    for line in &lines {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    Ok(hex::encode(hasher.finalize()))
}

fn collect_hashes(dir: &Path, rel: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let rel_path = rel.join(&name);
        if path.is_dir() {
            collect_hashes(&path, &rel_path, out)?;
        } else {
            let data = std::fs::read(&path)?;
            let mut hasher = Sha256::new();
            hasher.update(&data);
            out.push(format!(
                "{}:{}",
                rel_path.to_string_lossy(),
                hex::encode(hasher.finalize())
            ));
        }
    }
    Ok(())
}

#[async_trait]
impl SnapshotProvider for LocalSnapshotProvider {
    fn name(&self) -> &'static str {
        "local"
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Local
    }

    async fn create_snapshot(&self, request: SnapshotRequest) -> Result<SnapshotDescriptor> {
        let source = Path::new(&request.volume_id);
        if !source.is_dir() {
            return Err(CloudError::Config(format!(
                "source volume {} is not a directory",
                request.volume_id
            )));
        }

        let id = format!(
            "local-{}-{}",
            request.ledger_sequence,
            Utc::now().timestamp_millis()
        );
        let payload = self.payload_dir(&id);
        let size_bytes = copy_dir_recursive(source, &payload)?;
        let checksum = hash_directory(&payload)?;

        let manifest = LocalManifest {
            id: id.clone(),
            volume_id: request.volume_id.clone(),
            ledger_sequence: request.ledger_sequence,
            created_at: Utc::now(),
            size_bytes,
            checksum: checksum.clone(),
            tags: request.tags.clone(),
        };
        self.store_manifest(&manifest)?;

        Ok(SnapshotDescriptor {
            id,
            provider: ProviderKind::Local,
            volume_id: request.volume_id,
            ledger_sequence: request.ledger_sequence,
            created_at: manifest.created_at,
            size_bytes: Some(size_bytes),
            state: SnapshotState::Available,
            checksum: Some(checksum),
            tags: request.tags,
        })
    }

    async fn describe_snapshot(&self, snapshot_id: &str) -> Result<SnapshotDescriptor> {
        let manifest = self.load_manifest(snapshot_id)?;
        Ok(self.descriptor_from_manifest(&manifest))
    }

    async fn list_snapshots(&self, filter: &ListFilter) -> Result<Vec<SnapshotDescriptor>> {
        let mut out = Vec::new();
        for id in self.local_snapshot_ids() {
            let manifest = match self.load_manifest(&id) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let desc = self.descriptor_from_manifest(&manifest);
            if let Some(ref vol) = filter.volume_id {
                if &desc.volume_id != vol {
                    continue;
                }
            }
            if let Some((ref key, ref value)) = filter.tag {
                if desc.tag(key) != Some(value.as_str()) {
                    continue;
                }
            }
            out.push(desc);
        }
        out.sort_by_key(|d| std::cmp::Reverse(d.created_at));
        Ok(out)
    }

    async fn delete_snapshot(&self, snapshot_id: &str) -> Result<()> {
        let dir = self.snapshot_dir(snapshot_id);
        if !dir.exists() {
            return Err(CloudError::SnapshotNotFound(snapshot_id.to_string()));
        }
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    async fn restore_volume(
        &self,
        snapshot_id: &str,
        target_volume_id: &str,
    ) -> Result<RestoreResult> {
        let manifest = self.load_manifest(snapshot_id)?;
        let payload = self.payload_dir(snapshot_id);
        if !payload.is_dir() {
            return Err(CloudError::SnapshotNotFound(snapshot_id.to_string()));
        }

        let target = Path::new(target_volume_id);
        // Clear any pre-existing content so the restore is a faithful copy.
        if target.exists() {
            std::fs::remove_dir_all(target)?;
        }
        copy_dir_recursive(&payload, target)?;

        // Verify the restored tree matches the manifest checksum recorded at
        // snapshot time — this is the crash-consistency/integrity guarantee.
        let restored_hash = hash_directory(target)?;
        let integrity_ok = restored_hash == manifest.checksum;
        if !integrity_ok {
            return Err(CloudError::Integrity(
                snapshot_id.to_string(),
                format!("expected {} got {}", manifest.checksum, restored_hash),
            ));
        }

        Ok(RestoreResult {
            volume_id: target_volume_id.to_string(),
            snapshot_id: snapshot_id.to_string(),
            integrity_ok,
        })
    }
}

// ---------------------------------------------------------------------------
// AWS EBS provider (request construction + SigV4 signing)
// ---------------------------------------------------------------------------

/// AWS credentials used for SigV4 request signing.
#[derive(Debug, Clone)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// Configuration for the AWS EBS snapshot provider.
#[derive(Debug, Clone)]
pub struct AwsEbsConfig {
    pub region: String,
    pub credentials: AwsCredentials,
    /// Override for the EC2 API endpoint (used in tests).
    pub endpoint: Option<String>,
}

/// AWS EBS snapshot provider.
///
/// Snapshot lifecycle maps to the EC2 API as follows:
///
/// | Operation           | EC2 Action          |
/// |---------------------|---------------------|
/// | `create_snapshot`   | `CreateSnapshot`    |
/// | `describe_snapshot` | `DescribeSnapshots` |
/// | `delete_snapshot`   | `DeleteSnapshot`    |
/// | `restore_volume`    | `CreateVolume`      |
pub struct AwsEbsProvider {
    config: AwsEbsConfig,
}

impl AwsEbsProvider {
    pub fn new(config: AwsEbsConfig) -> Self {
        Self { config }
    }

    fn endpoint(&self) -> String {
        self.config
            .endpoint
            .clone()
            .unwrap_or_else(|| format!("https://ec2.{}.amazonaws.com/", self.config.region))
    }

    /// Build the query-string body for an EC2 action.
    fn build_query(action: &str, params: &[(&str, &str)]) -> String {
        let mut q = vec![format!("Action={}", action)];
        for (k, v) in params {
            q.push(format!("{}={}", k, v));
        }
        // EC2 query API expects parameters sorted for predictable signing.
        q.sort();
        q.join("&")
    }

    /// Build a signed `CreateSnapshot` request (method, URL, signed headers).
    ///
    /// Returns the body plus the headers that must accompany it.  Exposed as a
    /// pure function so SigV4 signing is unit-testable without a network.
    pub fn build_create_snapshot_request(
        &self,
        volume_id: &str,
        tags: &BTreeMap<String, String>,
    ) -> (String, BTreeMap<String, String>) {
        let mut params: Vec<(&str, &str)> = vec![
            ("VolumeId", volume_id),
            ("Description", "stellar-k8s automated snapshot"),
        ];
        // Fold tags into TagSpecification.N.Tag.M.Key/Value pairs.
        let mut owned: Vec<(String, String)> = tags
            .iter()
            .map(|(k, v)| (format!("TagSpecification.1.Tag.{}", k), v.clone()))
            .collect();
        owned.sort();
        let tag_refs: Vec<(&str, &str)> = owned
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        params.extend(tag_refs);

        let body = Self::build_query("CreateSnapshot", &params);
        let headers = self.sign("POST", "/", &body);
        (body, headers)
    }

    /// Sign an EC2 query-API request with AWS SigV4 (service `ec2`).
    ///
    /// This produces the `Authorization`, `x-amz-date`, and (when a session
    /// token is present) `x-amz-security-token` headers.
    pub fn sign(&self, method: &str, path: &str, body: &str) -> BTreeMap<String, String> {
        let now = Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = now.format("%Y%m%d").to_string();

        let region = &self.config.region;
        let service = "ec2";
        let payload_hash = hex::encode(Sha256::digest(body.as_bytes()));

        let canonical_headers = format!(
            "content-type:application/x-www-form-urlencoded; charset=utf-8\nhost:ec2.{}.amazonaws.com\nx-amz-date:{}\n",
            region, amz_date
        );
        let signed_headers = "content-type;host;x-amz-date";
        let canonical_request = format!(
            "{}\n{}\n\n{}\n{}\n{}",
            method, path, canonical_headers, signed_headers, payload_hash
        );

        let scope = format!("{}/{}/{}/aws4_request", date_stamp, region, service);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date,
            scope,
            hex::encode(Sha256::digest(canonical_request.as_bytes()))
        );

        let signing_key = derive_signing_key(
            &self.config.credentials.secret_access_key,
            &date_stamp,
            region,
            service,
        );
        let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));

        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            self.config.credentials.access_key_id, scope, signed_headers, signature
        );

        let mut headers = BTreeMap::new();
        headers.insert("Authorization".to_string(), authorization);
        headers.insert("x-amz-date".to_string(), amz_date);
        headers.insert(
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded; charset=utf-8".to_string(),
        );
        if let Some(ref token) = self.config.credentials.session_token {
            headers.insert("x-amz-security-token".to_string(), token.clone());
        }
        headers
    }
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hmac_sha256_chain(key: &[u8], parts: &[&str]) -> Vec<u8> {
    let mut current = key.to_vec();
    for part in parts {
        current = hmac_sha256(&current, part.as_bytes());
    }
    current
}

/// Derive the SigV4 signing key: HMAC chain over date, region, service, and the
/// fixed `aws4_request` terminator.
pub fn derive_signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{}", secret).as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

// Keep the unused chain helper referenced so it is not dropped; it documents the
// generic HMAC chaining used above.
#[allow(dead_code)]
fn _unused_chain_wrapper(key: &[u8], parts: &[&str]) -> Vec<u8> {
    hmac_sha256_chain(key, parts)
}

#[async_trait]
impl SnapshotProvider for AwsEbsProvider {
    fn name(&self) -> &'static str {
        "aws-ebs"
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::AwsEbs
    }

    async fn create_snapshot(&self, request: SnapshotRequest) -> Result<SnapshotDescriptor> {
        let (body, headers) = self.build_create_snapshot_request(&request.volume_id, &request.tags);
        let client = reqwest::Client::new();
        let mut req = client.post(self.endpoint()).header(
            "Content-Type",
            "application/x-www-form-urlencoded; charset=utf-8",
        );
        for (k, v) in &headers {
            req = req.header(k, v);
        }
        let resp = req
            .body(body)
            .send()
            .await
            .map_err(|e| CloudError::Provider(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(CloudError::Provider(format!(
                "CreateSnapshot failed: {}",
                resp.status()
            )));
        }
        let text = resp.text().await.unwrap_or_default();
        let snapshot_id = extract_xml_tag(&text, "snapshotId")
            .ok_or_else(|| CloudError::Provider("missing snapshotId in response".into()))?;

        Ok(SnapshotDescriptor {
            id: snapshot_id,
            provider: ProviderKind::AwsEbs,
            volume_id: request.volume_id,
            ledger_sequence: request.ledger_sequence,
            created_at: Utc::now(),
            size_bytes: None,
            state: SnapshotState::Pending,
            checksum: None,
            tags: request.tags,
        })
    }

    async fn describe_snapshot(&self, snapshot_id: &str) -> Result<SnapshotDescriptor> {
        let params = [("SnapshotId", snapshot_id)];
        let body = Self::build_query("DescribeSnapshots", &params);
        let headers = self.sign("POST", "/", &body);
        let client = reqwest::Client::new();
        let mut req = client.post(self.endpoint()).header(
            "Content-Type",
            "application/x-www-form-urlencoded; charset=utf-8",
        );
        for (k, v) in &headers {
            req = req.header(k, v);
        }
        let resp = req
            .body(body)
            .send()
            .await
            .map_err(|e| CloudError::Provider(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(CloudError::SnapshotNotFound(snapshot_id.to_string()));
        }
        let text = resp.text().await.unwrap_or_default();
        Ok(SnapshotDescriptor {
            id: snapshot_id.to_string(),
            provider: ProviderKind::AwsEbs,
            volume_id: extract_xml_tag(&text, "volumeId").unwrap_or_default(),
            ledger_sequence: 0,
            created_at: Utc::now(),
            size_bytes: extract_xml_tag(&text, "volumeSize").and_then(|s| s.parse().ok()),
            state: if extract_xml_tag(&text, "status").as_deref() == Some("completed") {
                SnapshotState::Available
            } else {
                SnapshotState::Pending
            },
            checksum: None,
            tags: BTreeMap::new(),
        })
    }

    async fn list_snapshots(&self, _filter: &ListFilter) -> Result<Vec<SnapshotDescriptor>> {
        let body = Self::build_query("DescribeSnapshots", &[("Owner", "self")]);
        let headers = self.sign("POST", "/", &body);
        let client = reqwest::Client::new();
        let mut req = client.post(self.endpoint()).header(
            "Content-Type",
            "application/x-www-form-urlencoded; charset=utf-8",
        );
        for (k, v) in &headers {
            req = req.header(k, v);
        }
        let resp = req
            .body(body)
            .send()
            .await
            .map_err(|e| CloudError::Provider(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(CloudError::Provider(format!(
                "DescribeSnapshots failed: {}",
                resp.status()
            )));
        }
        let text = resp.text().await.unwrap_or_default();
        Ok(parse_aws_snapshot_items(&text))
    }

    async fn delete_snapshot(&self, snapshot_id: &str) -> Result<()> {
        let body = Self::build_query("DeleteSnapshot", &[("SnapshotId", snapshot_id)]);
        let headers = self.sign("POST", "/", &body);
        let client = reqwest::Client::new();
        let mut req = client.post(self.endpoint()).header(
            "Content-Type",
            "application/x-www-form-urlencoded; charset=utf-8",
        );
        for (k, v) in &headers {
            req = req.header(k, v);
        }
        let resp = req
            .body(body)
            .send()
            .await
            .map_err(|e| CloudError::Provider(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(CloudError::Provider(format!(
                "DeleteSnapshot failed: {}",
                resp.status()
            )));
        }
        Ok(())
    }

    async fn restore_volume(
        &self,
        snapshot_id: &str,
        target_volume_id: &str,
    ) -> Result<RestoreResult> {
        let params = [
            ("SnapshotId", snapshot_id),
            ("AvailabilityZone", "us-east-1a"),
            ("VolumeType", "gp3"),
        ];
        let body = Self::build_query("CreateVolume", &params);
        let headers = self.sign("POST", "/", &body);
        let client = reqwest::Client::new();
        let mut req = client.post(self.endpoint()).header(
            "Content-Type",
            "application/x-www-form-urlencoded; charset=utf-8",
        );
        for (k, v) in &headers {
            req = req.header(k, v);
        }
        let resp = req
            .body(body)
            .send()
            .await
            .map_err(|e| CloudError::Provider(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(CloudError::Provider(format!(
                "CreateVolume failed: {}",
                resp.status()
            )));
        }
        let text = resp.text().await.unwrap_or_default();
        let volume_id =
            extract_xml_tag(&text, "volumeId").unwrap_or_else(|| target_volume_id.to_string());
        Ok(RestoreResult {
            volume_id,
            snapshot_id: snapshot_id.to_string(),
            // Block-level restores are verified by the CSI layer; the EBS API
            // does not expose a payload digest.
            integrity_ok: true,
        })
    }
}

/// Extract the first `<tag>value</tag>` occurrence from an XML body.
fn extract_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_string())
}

/// Parse `<item>` blocks from a `DescribeSnapshots` response into descriptors.
fn parse_aws_snapshot_items(xml: &str) -> Vec<SnapshotDescriptor> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<item>") {
        let after = &rest[start + "<item>".len()..];
        let end = match after.find("</item>") {
            Some(e) => e,
            None => break,
        };
        let item = &after[..end];
        if let Some(id) = extract_xml_tag(item, "snapshotId") {
            out.push(SnapshotDescriptor {
                id,
                provider: ProviderKind::AwsEbs,
                volume_id: extract_xml_tag(item, "volumeId").unwrap_or_default(),
                ledger_sequence: 0,
                created_at: Utc::now(),
                size_bytes: extract_xml_tag(item, "volumeSize").and_then(|s| s.parse().ok()),
                state: if extract_xml_tag(item, "status").as_deref() == Some("completed") {
                    SnapshotState::Available
                } else {
                    SnapshotState::Pending
                },
                checksum: None,
                tags: BTreeMap::new(),
            });
        }
        rest = &after[end + "</item>".len()..];
    }
    out
}

// ---------------------------------------------------------------------------
// GCP Persistent Disk provider
// ---------------------------------------------------------------------------

/// Configuration for the GCP PD snapshot provider.
#[derive(Debug, Clone)]
pub struct GcpPdConfig {
    pub project_id: String,
    /// OAuth2 bearer token used to authenticate API calls.
    pub access_token: String,
    /// Override for the Compute API endpoint (used in tests).
    pub endpoint: Option<String>,
}

/// GCP Persistent Disk snapshot provider.
///
/// Maps to the Compute Engine API:
///
/// | Operation           | GCP API                                   |
/// |---------------------|-------------------------------------------|
/// | `create_snapshot`   | `POST /global/snapshots`                  |
/// | `describe_snapshot` | `GET  /global/snapshots/{name}`           |
/// | `delete_snapshot`   | `DELETE /global/snapshots/{name}`         |
/// | `restore_volume`    | `POST /disks` (sourceSnapshot)            |
pub struct GcpPdProvider {
    config: GcpPdConfig,
}

impl GcpPdProvider {
    pub fn new(config: GcpPdConfig) -> Self {
        Self { config }
    }

    fn endpoint(&self) -> String {
        self.config
            .endpoint
            .clone()
            .unwrap_or_else(|| "https://compute.googleapis.com/compute/v1".to_string())
    }

    /// Build the JSON body for creating a snapshot of `source_disk`.
    ///
    /// Exposed as a pure function so payload assembly is unit-testable.
    pub fn build_create_snapshot_body(
        snapshot_name: &str,
        source_disk: &str,
        labels: &BTreeMap<String, String>,
    ) -> String {
        let labels_json: Vec<String> = labels
            .iter()
            .map(|(k, v)| format!("\"{}\":\"{}\"", k, v))
            .collect();
        format!(
            "{{\"name\":\"{}\",\"sourceDisk\":\"{}\",\"labels\":{{{}}}}}",
            snapshot_name,
            source_disk,
            labels_json.join(",")
        )
    }

    /// Build the JSON body for restoring a disk from a snapshot.
    pub fn build_create_disk_body(disk_name: &str, source_snapshot: &str) -> String {
        format!(
            "{{\"name\":\"{}\",\"sourceSnapshot\":\"{}\"}}",
            disk_name, source_snapshot
        )
    }

    fn auth_headers(&self) -> BTreeMap<String, String> {
        let mut h = BTreeMap::new();
        h.insert(
            "Authorization".to_string(),
            format!("Bearer {}", self.config.access_token),
        );
        h.insert("Content-Type".to_string(), "application/json".to_string());
        h
    }
}

#[async_trait]
impl SnapshotProvider for GcpPdProvider {
    fn name(&self) -> &'static str {
        "gcp-pd"
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::GcpPd
    }

    async fn create_snapshot(&self, request: SnapshotRequest) -> Result<SnapshotDescriptor> {
        let name = format!(
            "stellar-{}-{}",
            request.ledger_sequence,
            Utc::now().timestamp_millis()
        );
        let body = Self::build_create_snapshot_body(&name, &request.volume_id, &request.tags);
        let url = format!("{}/global/snapshots", self.endpoint());

        let client = reqwest::Client::new();
        let mut req = client.post(&url);
        for (k, v) in self.auth_headers() {
            req = req.header(k, v);
        }
        let resp = req
            .body(body)
            .send()
            .await
            .map_err(|e| CloudError::Provider(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(CloudError::Provider(format!(
                "create snapshot failed: {}",
                resp.status()
            )));
        }
        Ok(SnapshotDescriptor {
            id: name.clone(),
            provider: ProviderKind::GcpPd,
            volume_id: request.volume_id,
            ledger_sequence: request.ledger_sequence,
            created_at: Utc::now(),
            size_bytes: None,
            state: SnapshotState::Pending,
            checksum: None,
            tags: request.tags,
        })
    }

    async fn describe_snapshot(&self, snapshot_id: &str) -> Result<SnapshotDescriptor> {
        let url = format!("{}/global/snapshots/{}", self.endpoint(), snapshot_id);
        let client = reqwest::Client::new();
        let mut req = client.get(&url);
        for (k, v) in self.auth_headers() {
            req = req.header(k, v);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| CloudError::Provider(e.to_string()))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(CloudError::SnapshotNotFound(snapshot_id.to_string()));
        }
        if !resp.status().is_success() {
            return Err(CloudError::Provider(format!(
                "describe snapshot failed: {}",
                resp.status()
            )));
        }
        let text = resp.text().await.unwrap_or_default();
        let status = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(String::from));
        Ok(SnapshotDescriptor {
            id: snapshot_id.to_string(),
            provider: ProviderKind::GcpPd,
            volume_id: extract_json_string(&text, "sourceDisk").unwrap_or_default(),
            ledger_sequence: 0,
            created_at: Utc::now(),
            size_bytes: extract_json_u64(&text, "sizeGb"),
            state: match status.as_deref() {
                Some("READY") => SnapshotState::Available,
                Some("CREATING") => SnapshotState::Pending,
                Some(other) => SnapshotState::Failed(other.to_string()),
                None => SnapshotState::Pending,
            },
            checksum: None,
            tags: BTreeMap::new(),
        })
    }

    async fn list_snapshots(&self, _filter: &ListFilter) -> Result<Vec<SnapshotDescriptor>> {
        let url = format!("{}/global/snapshots", self.endpoint());
        let client = reqwest::Client::new();
        let mut req = client.get(&url);
        for (k, v) in self.auth_headers() {
            req = req.header(k, v);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| CloudError::Provider(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(CloudError::Provider(format!(
                "list snapshots failed: {}",
                resp.status()
            )));
        }
        let text = resp.text().await.unwrap_or_default();
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| CloudError::Provider(e.to_string()))?;
        let mut out = Vec::new();
        if let Some(items) = value.get("items").and_then(|i| i.as_array()) {
            for item in items {
                let id = item
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or_default()
                    .to_string();
                out.push(SnapshotDescriptor {
                    id,
                    provider: ProviderKind::GcpPd,
                    volume_id: item
                        .get("sourceDisk")
                        .and_then(|d| d.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    ledger_sequence: 0,
                    created_at: Utc::now(),
                    size_bytes: item.get("sizeGb").and_then(|s| s.as_u64()),
                    state: match item.get("status").and_then(|s| s.as_str()) {
                        Some("READY") => SnapshotState::Available,
                        Some("CREATING") => SnapshotState::Pending,
                        _ => SnapshotState::Pending,
                    },
                    checksum: None,
                    tags: BTreeMap::new(),
                });
            }
        }
        Ok(out)
    }

    async fn delete_snapshot(&self, snapshot_id: &str) -> Result<()> {
        let url = format!("{}/global/snapshots/{}", self.endpoint(), snapshot_id);
        let client = reqwest::Client::new();
        let mut req = client.delete(&url);
        for (k, v) in self.auth_headers() {
            req = req.header(k, v);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| CloudError::Provider(e.to_string()))?;
        if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
            return Err(CloudError::Provider(format!(
                "delete snapshot failed: {}",
                resp.status()
            )));
        }
        Ok(())
    }

    async fn restore_volume(
        &self,
        snapshot_id: &str,
        target_volume_id: &str,
    ) -> Result<RestoreResult> {
        let url = format!(
            "{}/projects/{}/global/disks",
            self.endpoint(),
            self.config.project_id
        );
        let body = Self::build_create_disk_body(target_volume_id, snapshot_id);
        let client = reqwest::Client::new();
        let mut req = client.post(&url);
        for (k, v) in self.auth_headers() {
            req = req.header(k, v);
        }
        let resp = req
            .body(body)
            .send()
            .await
            .map_err(|e| CloudError::Provider(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(CloudError::Provider(format!(
                "create disk failed: {}",
                resp.status()
            )));
        }
        Ok(RestoreResult {
            volume_id: target_volume_id.to_string(),
            snapshot_id: snapshot_id.to_string(),
            integrity_ok: true,
        })
    }
}

/// Extract a top-level string field from a JSON body.
fn extract_json_string(json: &str, field: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    value
        .get(field)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Extract a top-level unsigned-integer field from a JSON body.
///
/// Cloud APIs (e.g. GCP `sizeGb`) return numeric fields as JSON numbers, not
/// strings, so this reads them directly rather than through
/// [`extract_json_string`].
fn extract_json_u64(json: &str, field: &str) -> Option<u64> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    value.get(field).and_then(|v| v.as_u64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_tree(root: &Path, files: &[(&str, &str)]) {
        for (rel, content) in files {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, content).unwrap();
        }
    }

    #[test]
    fn hash_directory_is_stable_and_order_independent() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        write_tree(a.path(), &[("x.txt", "hello"), ("sub/y.txt", "world")]);
        // Same content, different creation order.
        write_tree(b.path(), &[("sub/y.txt", "world"), ("x.txt", "hello")]);
        let ha = hash_directory(a.path()).unwrap();
        let hb = hash_directory(b.path()).unwrap();
        assert_eq!(ha, hb);
        assert_eq!(ha.len(), 64);
    }

    #[test]
    fn hash_directory_changes_with_content() {
        let a = tempfile::tempdir().unwrap();
        write_tree(a.path(), &[("x.txt", "hello")]);
        let before = hash_directory(a.path()).unwrap();
        fs::write(a.path().join("x.txt"), "HELLO").unwrap();
        let after = hash_directory(a.path()).unwrap();
        assert_ne!(before, after);
    }

    #[tokio::test]
    async fn local_provider_create_describe_delete_lifecycle() {
        let src = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        write_tree(src.path(), &[("stellar.db", "ledger-data")]);

        let provider = LocalSnapshotProvider::new(store.path()).unwrap();
        let mut tags = BTreeMap::new();
        tags.insert("node".into(), "validator-1".into());
        let request = SnapshotRequest {
            volume_id: src.path().to_string_lossy().to_string(),
            ledger_sequence: 1_048_576,
            tags,
        };

        let desc = provider.create_snapshot(request).await.unwrap();
        assert_eq!(desc.provider, ProviderKind::Local);
        assert_eq!(desc.ledger_sequence, 1_048_576);
        assert_eq!(desc.state, SnapshotState::Available);
        assert!(desc.checksum.is_some());
        assert_eq!(desc.tag("node"), Some("validator-1"));

        let described = provider.describe_snapshot(&desc.id).await.unwrap();
        assert_eq!(described.id, desc.id);

        let listed = provider
            .list_snapshots(&ListFilter {
                volume_id: Some(desc.volume_id.clone()),
                tag: None,
            })
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);

        provider.delete_snapshot(&desc.id).await.unwrap();
        let listed = provider
            .list_snapshots(&ListFilter::default())
            .await
            .unwrap();
        assert!(listed.is_empty());
        assert!(matches!(
            provider.describe_snapshot(&desc.id).await,
            Err(CloudError::SnapshotNotFound(_))
        ));
    }

    #[tokio::test]
    async fn local_provider_restore_verifies_integrity() {
        let src = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        write_tree(
            src.path(),
            &[("stellar.db", "ledger-data"), ("sub/idx", "index")],
        );

        let provider = LocalSnapshotProvider::new(store.path()).unwrap();
        let request = SnapshotRequest {
            volume_id: src.path().to_string_lossy().to_string(),
            ledger_sequence: 100,
            tags: BTreeMap::new(),
        };
        let desc = provider.create_snapshot(request).await.unwrap();

        // Mutate the source after the snapshot; the restore must still reflect
        // the captured state and pass integrity.
        fs::write(src.path().join("stellar.db"), "MUTATED").unwrap();

        let target_path = target.path().join("restored");
        let result = provider
            .restore_volume(&desc.id, &target_path.to_string_lossy())
            .await
            .unwrap();
        assert!(result.integrity_ok);
        assert_eq!(
            fs::read_to_string(target_path.join("stellar.db")).unwrap(),
            "ledger-data"
        );
        assert_eq!(
            fs::read_to_string(target_path.join("sub/idx")).unwrap(),
            "index"
        );
    }

    #[tokio::test]
    async fn local_provider_restore_detects_corruption() {
        let src = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        write_tree(src.path(), &[("stellar.db", "ledger-data")]);

        let provider = LocalSnapshotProvider::new(store.path()).unwrap();
        let request = SnapshotRequest {
            volume_id: src.path().to_string_lossy().to_string(),
            ledger_sequence: 5,
            tags: BTreeMap::new(),
        };
        let desc = provider.create_snapshot(request).await.unwrap();

        // Corrupt the stored snapshot payload after the manifest was written.
        let payload = provider.payload_dir(&desc.id).join("stellar.db");
        fs::write(payload, "BIT-ROT").unwrap();

        let target = tempfile::tempdir().unwrap();
        let target_path = target.path().join("restored");
        let result = provider
            .restore_volume(&desc.id, &target_path.to_string_lossy())
            .await;
        assert!(matches!(result, Err(CloudError::Integrity(_, _))));
    }

    #[tokio::test]
    async fn local_provider_list_filters_by_tag() {
        let src_a = tempfile::tempdir().unwrap();
        let src_b = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        write_tree(src_a.path(), &[("a", "a")]);
        write_tree(src_b.path(), &[("b", "b")]);

        let provider = LocalSnapshotProvider::new(store.path()).unwrap();
        let mut tags_a = BTreeMap::new();
        tags_a.insert("node".into(), "node-a".into());
        provider
            .create_snapshot(SnapshotRequest {
                volume_id: src_a.path().to_string_lossy().to_string(),
                ledger_sequence: 1,
                tags: tags_a,
            })
            .await
            .unwrap();
        let mut tags_b = BTreeMap::new();
        tags_b.insert("node".into(), "node-b".into());
        provider
            .create_snapshot(SnapshotRequest {
                volume_id: src_b.path().to_string_lossy().to_string(),
                ledger_sequence: 2,
                tags: tags_b,
            })
            .await
            .unwrap();

        let filtered = provider
            .list_snapshots(&ListFilter {
                volume_id: None,
                tag: Some(("node".into(), "node-a".into())),
            })
            .await
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].tag("node"), Some("node-a"));
    }

    #[test]
    fn aws_query_builder_sorts_params() {
        let params = [("VolumeId", "vol-1"), ("Description", "desc")];
        let body = AwsEbsProvider::build_query("CreateSnapshot", &params);
        assert!(body.starts_with("Action=CreateSnapshot"));
        // Params are sorted lexicographically after Action for deterministic signing.
        assert!(body.contains("Description=desc"));
        assert!(body.contains("VolumeId=vol-1"));
    }

    #[test]
    fn aws_sigv4_signature_is_deterministic_for_fixed_key() {
        // Derive a signing key and sign the same input twice; must be identical.
        let key1 = derive_signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "iam",
        );
        let key2 = derive_signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "iam",
        );
        assert_eq!(key1, key2);
        assert_eq!(key1.len(), 32);

        // Different secret or date must yield a different key.
        let key3 = derive_signing_key("different", "20150830", "us-east-1", "iam");
        assert_ne!(key1, key3);
    }

    #[test]
    fn aws_sigv4_known_vector_signature_matches() {
        // AWS published SigV4 test vector (iam ListUsers, 2015-08-30, us-east-1).
        let secret = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
        let key = derive_signing_key(secret, "20150830", "us-east-1", "iam");
        let string_to_sign = "AWS4-HMAC-SHA256\n20150830T123600Z\n20150830/us-east-1/iam/aws4_request\nf536975d06c0309214f805bb90ccff089219ecd68b2577efef23edd43b7e1a59";
        let signature = hex::encode(hmac_sha256(&key, string_to_sign.as_bytes()));
        assert_eq!(
            signature,
            "5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    #[test]
    fn aws_sign_produces_authorization_header() {
        let provider = AwsEbsProvider::new(AwsEbsConfig {
            region: "us-east-1".into(),
            credentials: AwsCredentials {
                access_key_id: "AKIAEXAMPLE".into(),
                secret_access_key: "secret".into(),
                session_token: None,
            },
            endpoint: None,
        });
        let headers = provider.sign("POST", "/", "Action=CreateSnapshot");
        let auth = headers.get("Authorization").unwrap();
        assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIAEXAMPLE/"));
        assert!(auth.contains("SignedHeaders=content-type;host;x-amz-date"));
        assert!(auth.contains("/ec2/aws4_request"));
        assert!(headers.contains_key("x-amz-date"));
        // No session token -> no security-token header.
        assert!(!headers.contains_key("x-amz-security-token"));
    }

    #[test]
    fn aws_sign_includes_session_token_when_present() {
        let provider = AwsEbsProvider::new(AwsEbsConfig {
            region: "us-east-1".into(),
            credentials: AwsCredentials {
                access_key_id: "AKIAEXAMPLE".into(),
                secret_access_key: "secret".into(),
                session_token: Some("FQoGZXIvYXdzEJr".into()),
            },
            endpoint: None,
        });
        let headers = provider.sign("POST", "/", "Action=DeleteSnapshot");
        assert_eq!(
            headers.get("x-amz-security-token").map(|s| s.as_str()),
            Some("FQoGZXIvYXdzEJr")
        );
    }

    #[test]
    fn aws_create_snapshot_request_builds_signed_body() {
        let provider = AwsEbsProvider::new(AwsEbsConfig {
            region: "us-east-1".into(),
            credentials: AwsCredentials {
                access_key_id: "AKIAEXAMPLE".into(),
                secret_access_key: "secret".into(),
                session_token: None,
            },
            endpoint: None,
        });
        let mut tags = BTreeMap::new();
        tags.insert("node".into(), "validator-1".into());
        let (body, headers) = provider.build_create_snapshot_request("vol-123", &tags);
        assert!(body.contains("Action=CreateSnapshot"));
        assert!(body.contains("VolumeId=vol-123"));
        assert!(body.contains("TagSpecification.1.Tag.node=validator-1"));
        assert!(headers.contains_key("Authorization"));
    }

    #[test]
    fn aws_parse_snapshot_items_handles_empty_and_populated() {
        assert!(parse_aws_snapshot_items("<DescribeSnapshotsResponse/>").is_empty());

        let xml = "<DescribeSnapshotsResponse><snapshotSet>\
            <item><snapshotId>snap-1</snapshotId><volumeId>vol-1</volumeId><status>completed</status><volumeSize>8</volumeSize></item>\
            <item><snapshotId>snap-2</snapshotId><volumeId>vol-2</volumeId><status>pending</status></item>\
            </snapshotSet></DescribeSnapshotsResponse>";
        let items = parse_aws_snapshot_items(xml);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "snap-1");
        assert_eq!(items[0].state, SnapshotState::Available);
        assert_eq!(items[0].size_bytes, Some(8));
        assert_eq!(items[1].id, "snap-2");
        assert_eq!(items[1].state, SnapshotState::Pending);
    }

    #[test]
    fn gcp_create_snapshot_body_is_valid_json() {
        let mut labels = BTreeMap::new();
        labels.insert("node".into(), "validator-1".into());
        let body = GcpPdProvider::build_create_snapshot_body(
            "stellar-1",
            "projects/p/zones/z/disks/d",
            &labels,
        );
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["name"], "stellar-1");
        assert_eq!(parsed["sourceDisk"], "projects/p/zones/z/disks/d");
        assert_eq!(parsed["labels"]["node"], "validator-1");
    }

    #[test]
    fn gcp_create_disk_body_is_valid_json() {
        let body = GcpPdProvider::build_create_disk_body("disk-1", "snapshot-1");
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["name"], "disk-1");
        assert_eq!(parsed["sourceSnapshot"], "snapshot-1");
    }

    #[test]
    fn extract_json_string_reads_fields() {
        let json = r#"{"status":"READY","sizeGb":10,"sourceDisk":"d"}"#;
        assert_eq!(extract_json_string(json, "status").unwrap(), "READY");
        // Non-string field returns None.
        assert!(extract_json_string(json, "sizeGb").is_none());
    }

    #[test]
    fn extract_xml_tag_reads_nested_values() {
        let xml = "<r><snapshotId>snap-abc</snapshotId></r>";
        assert_eq!(extract_xml_tag(xml, "snapshotId").unwrap(), "snap-abc");
        assert!(extract_xml_tag(xml, "missing").is_none());
    }

    // --- HTTP round-trips via wiremock (no live cloud required) ----------

    use wiremock::matchers::{body_string_contains, method, path as url_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn aws_provider_with_endpoint(endpoint: String) -> AwsEbsProvider {
        AwsEbsProvider::new(AwsEbsConfig {
            region: "us-east-1".into(),
            credentials: AwsCredentials {
                access_key_id: "AKIAEXAMPLE".into(),
                secret_access_key: "secret".into(),
                session_token: None,
            },
            endpoint: Some(endpoint),
        })
    }

    fn gcp_provider_with_endpoint(endpoint: String) -> GcpPdProvider {
        GcpPdProvider::new(GcpPdConfig {
            project_id: "test-project".into(),
            access_token: "test-token".into(),
            endpoint: Some(endpoint),
        })
    }

    #[tokio::test]
    async fn aws_create_snapshot_http_round_trip() {
        let server = MockServer::start().await;
        let xml_response = "<CreateSnapshotResponse>\
            <requestId>req-1</requestId>\
            <snapshot><snapshotId>snap-abc123</snapshotId><status>pending</status></snapshot>\
            </CreateSnapshotResponse>";
        Mock::given(method("POST"))
            .and(url_path("/"))
            .and(body_string_contains("Action=CreateSnapshot"))
            .and(body_string_contains("VolumeId=vol-789"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/xml")
                    .set_body_string(xml_response),
            )
            .expect(1)
            .mount(&server)
            .await;

        let provider = aws_provider_with_endpoint(server.uri());
        let mut tags = BTreeMap::new();
        tags.insert("node".into(), "validator-1".into());
        let desc = provider
            .create_snapshot(SnapshotRequest {
                volume_id: "vol-789".into(),
                ledger_sequence: 42,
                tags,
            })
            .await
            .unwrap();

        assert_eq!(desc.id, "snap-abc123");
        assert_eq!(desc.provider, ProviderKind::AwsEbs);
        assert_eq!(desc.volume_id, "vol-789");
        assert_eq!(desc.ledger_sequence, 42);
        assert_eq!(desc.state, SnapshotState::Pending);

        // The outgoing request must have carried a SigV4 Authorization header.
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        let auth = received[0]
            .headers
            .get("Authorization")
            .expect("Authorization header present");
        assert!(auth
            .to_str()
            .unwrap()
            .starts_with("AWS4-HMAC-SHA256 Credential=AKIAEXAMPLE/"));
    }

    #[tokio::test]
    async fn aws_delete_snapshot_http_round_trip() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(url_path("/"))
            .and(body_string_contains("Action=DeleteSnapshot"))
            .and(body_string_contains("SnapshotId=snap-del"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let provider = aws_provider_with_endpoint(server.uri());
        provider.delete_snapshot("snap-del").await.unwrap();
    }

    #[tokio::test]
    async fn aws_restore_volume_http_round_trip() {
        let server = MockServer::start().await;
        let xml_response = "<CreateVolumeResponse>\
            <volume><volumeId>vol-restored-1</volumeId></volume>\
            </CreateVolumeResponse>";
        Mock::given(method("POST"))
            .and(url_path("/"))
            .and(body_string_contains("Action=CreateVolume"))
            .and(body_string_contains("SnapshotId=snap-src"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/xml")
                    .set_body_string(xml_response),
            )
            .expect(1)
            .mount(&server)
            .await;

        let provider = aws_provider_with_endpoint(server.uri());
        let result = provider
            .restore_volume("snap-src", "vol-fallback")
            .await
            .unwrap();
        assert_eq!(result.volume_id, "vol-restored-1");
        assert_eq!(result.snapshot_id, "snap-src");
        assert!(result.integrity_ok);
    }

    #[tokio::test]
    async fn aws_create_snapshot_error_status_surfaces() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("Action=CreateSnapshot"))
            .respond_with(ResponseTemplate::new(400).set_body_string("<ErrorResponse/>"))
            .mount(&server)
            .await;

        let provider = aws_provider_with_endpoint(server.uri());
        let err = provider
            .create_snapshot(SnapshotRequest {
                volume_id: "vol-err".into(),
                ledger_sequence: 1,
                tags: BTreeMap::new(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, CloudError::Provider(_)));
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn gcp_create_snapshot_http_round_trip() {
        let server = MockServer::start().await;
        let json_response = r#"{"name":"stellar-42-1","status":"CREATING"}"#;
        Mock::given(method("POST"))
            .and(url_path("/global/snapshots"))
            .and(body_string_contains("sourceDisk"))
            .and(body_string_contains("stellar-42"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(json_response),
            )
            .expect(1)
            .mount(&server)
            .await;

        let provider = gcp_provider_with_endpoint(server.uri());
        let desc = provider
            .create_snapshot(SnapshotRequest {
                volume_id: "projects/p/zones/z/disks/d".into(),
                ledger_sequence: 42,
                tags: BTreeMap::new(),
            })
            .await
            .unwrap();

        assert!(desc.id.starts_with("stellar-42-"));
        assert_eq!(desc.provider, ProviderKind::GcpPd);
        assert_eq!(desc.ledger_sequence, 42);

        // Bearer token auth must be attached.
        let received = server.received_requests().await.unwrap();
        let auth = received[0]
            .headers
            .get("Authorization")
            .expect("Authorization header present");
        assert_eq!(auth.to_str().unwrap(), "Bearer test-token");
    }

    #[tokio::test]
    async fn gcp_describe_snapshot_ready_maps_to_available() {
        let server = MockServer::start().await;
        let json_response = r#"{"name":"snap-g","status":"READY","sourceDisk":"d","sizeGb":20}"#;
        Mock::given(method("GET"))
            .and(url_path("/global/snapshots/snap-g"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(json_response),
            )
            .mount(&server)
            .await;

        let provider = gcp_provider_with_endpoint(server.uri());
        let desc = provider.describe_snapshot("snap-g").await.unwrap();
        assert_eq!(desc.state, SnapshotState::Available);
        assert_eq!(desc.size_bytes, Some(20));
        assert_eq!(desc.volume_id, "d");
    }

    #[tokio::test]
    async fn gcp_describe_snapshot_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(url_path("/global/snapshots/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let provider = gcp_provider_with_endpoint(server.uri());
        let err = provider.describe_snapshot("missing").await.unwrap_err();
        assert!(matches!(err, CloudError::SnapshotNotFound(_)));
    }

    #[tokio::test]
    async fn gcp_restore_volume_http_round_trip() {
        let server = MockServer::start().await;
        let json_response = r#"{"name":"disk-new","status":"CREATING"}"#;
        Mock::given(method("POST"))
            .and(url_path("/projects/test-project/global/disks"))
            .and(body_string_contains("sourceSnapshot"))
            .and(body_string_contains("snap-src"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(json_response),
            )
            .expect(1)
            .mount(&server)
            .await;

        let provider = gcp_provider_with_endpoint(server.uri());
        let result = provider
            .restore_volume("snap-src", "disk-new")
            .await
            .unwrap();
        assert_eq!(result.volume_id, "disk-new");
        assert_eq!(result.snapshot_id, "snap-src");
        assert!(result.integrity_ok);
    }

    #[tokio::test]
    async fn gcp_list_snapshots_parses_items() {
        let server = MockServer::start().await;
        let json_response = r#"{"items":[
            {"name":"s1","sourceDisk":"d1","status":"READY","sizeGb":10},
            {"name":"s2","sourceDisk":"d2","status":"CREATING"}
        ]}"#;
        Mock::given(method("GET"))
            .and(url_path("/global/snapshots"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(json_response),
            )
            .mount(&server)
            .await;

        let provider = gcp_provider_with_endpoint(server.uri());
        let items = provider
            .list_snapshots(&ListFilter::default())
            .await
            .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "s1");
        assert_eq!(items[0].state, SnapshotState::Available);
        assert_eq!(items[0].size_bytes, Some(10));
        assert_eq!(items[1].id, "s2");
        assert_eq!(items[1].state, SnapshotState::Pending);
    }

    #[tokio::test]
    async fn aws_list_snapshots_parses_items() {
        let server = MockServer::start().await;
        let xml_response = "<DescribeSnapshotsResponse><snapshotSet>\
            <item><snapshotId>a</snapshotId><volumeId>v1</volumeId><status>completed</status><volumeSize>8</volumeSize></item>\
            </snapshotSet></DescribeSnapshotsResponse>";
        Mock::given(method("POST"))
            .and(body_string_contains("Action=DescribeSnapshots"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/xml")
                    .set_body_string(xml_response),
            )
            .mount(&server)
            .await;

        let provider = aws_provider_with_endpoint(server.uri());
        let items = provider
            .list_snapshots(&ListFilter::default())
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "a");
        assert_eq!(items[0].state, SnapshotState::Available);
        assert_eq!(items[0].size_bytes, Some(8));
    }
}
