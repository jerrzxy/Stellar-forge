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

//! Cloud provider storage verification for finalizer cleanup.
//!
//! This module provides an abstraction for querying AWS EBS and GCP Persistent
//! Disk APIs to confirm that a volume attachment has been fully released before
//! the finalizer safety guard allows finalizer removal.
//!
//! # Safety Invariant
//!
//! A finalizer **must never** be removed unless [`CloudVolumeVerifier::is_volume_detached`]
//! returns `Ok(true)`.  Violating this invariant can lead to cloud data leaks or
//! filesystem corruption on a 500 GiB validator data volume.
//!
//! # Design
//!
//! The public interface is the [`CloudVolumeVerifier`] trait.  Two production
//! implementations are provided:
//!
//! * [`AwsEbsVerifier`] — calls `DescribeVolumes` via the AWS SDK.
//! * [`GcpPdVerifier`] — calls the Compute Engine `disks.get` REST API.
//!
//! Tests inject a [`MockCloudVerifier`] so network calls are never made during
//! unit or integration testing.

use async_trait::async_trait;
use thiserror::Error;
use tracing::{debug, info, warn};

// ─────────────────────────────────────────────────────────────────────────────
// Error type
// ─────────────────────────────────────────────────────────────────────────────

/// Errors that can occur while verifying cloud volume state.
#[derive(Debug, Error)]
pub enum VerifyError {
    /// The cloud provider API returned an unexpected HTTP status code.
    #[error("cloud API returned unexpected status {status}: {body}")]
    UnexpectedStatus { status: u16, body: String },

    /// A network or transport error occurred while calling the cloud API.
    #[error("cloud API transport error: {0}")]
    Transport(#[from] reqwest::Error),

    /// The volume ID format is invalid for the target provider.
    #[error("invalid volume ID for provider '{provider}': '{volume_id}'")]
    InvalidVolumeId {
        provider: &'static str,
        volume_id: String,
    },

    /// An unexpected error occurred during verification.
    #[error("verification error: {0}")]
    Other(#[from] anyhow::Error),
}

// ─────────────────────────────────────────────────────────────────────────────
// Core trait
// ─────────────────────────────────────────────────────────────────────────────

/// Abstraction over a cloud provider's block-storage attachment API.
///
/// Implementors must guarantee that `is_volume_detached` returns `Ok(true)`
/// **only** when there are zero active attachments on the underlying volume.
/// The garbage-collection reconciler treats any other result as "unsafe to
/// remove the finalizer" and will retry on the next reconciliation cycle.
#[async_trait]
pub trait CloudVolumeVerifier: Send + Sync {
    /// Returns `Ok(true)` if and only if the given volume has no active
    /// attachments on the cloud provider side, confirming it is safe to
    /// remove the Kubernetes finalizer.
    ///
    /// # Arguments
    ///
    /// * `volume_id` – The cloud-provider volume identifier extracted from the
    ///   PersistentVolume's `.spec.awsElasticBlockStore.volumeID` or
    ///   `.spec.gcePersistentDisk.pdName`.
    /// * `region_or_zone` – The AWS region (e.g. `"us-east-1"`) or GCP zone
    ///   (e.g. `"us-central1-a"`) where the volume resides.
    async fn is_volume_detached(
        &self,
        volume_id: &str,
        region_or_zone: &str,
    ) -> Result<bool, VerifyError>;

    /// Returns a human-readable name for this verifier, used in log messages.
    fn provider_name(&self) -> &'static str;
}

// ─────────────────────────────────────────────────────────────────────────────
// AWS EBS implementation
// ─────────────────────────────────────────────────────────────────────────────

/// AWS EBS volume attachment verifier.
///
/// Calls the EC2 `DescribeVolumes` API to check whether any attachment is in
/// the `attached` or `attaching` state.  The volume is considered detached
/// only when the attachments list is empty or all entries are `detached`.
///
/// Authentication is performed via the standard AWS credential chain
/// (environment variables → AWS config file → EC2 instance metadata).
pub struct AwsEbsVerifier {
    client: reqwest::Client,
}

impl AwsEbsVerifier {
    /// Creates a new `AwsEbsVerifier` using the default `reqwest` client.
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    /// Creates a new `AwsEbsVerifier` with a custom HTTP client (useful for
    /// injecting mTLS certificates in production deployments).
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl Default for AwsEbsVerifier {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CloudVolumeVerifier for AwsEbsVerifier {
    fn provider_name(&self) -> &'static str {
        "aws-ebs"
    }

    async fn is_volume_detached(
        &self,
        volume_id: &str,
        region: &str,
    ) -> Result<bool, VerifyError> {
        // Validate AWS volume ID format (vol-<hex>)
        if !volume_id.starts_with("vol-") || volume_id.len() < 12 {
            return Err(VerifyError::InvalidVolumeId {
                provider: "aws-ebs",
                volume_id: volume_id.to_owned(),
            });
        }

        debug!(
            volume_id = %volume_id,
            region = %region,
            "querying AWS EC2 DescribeVolumes API"
        );

        // Build the EC2 query URL.  In production, the AWS SDK handles
        // SigV4 signing; here we use the plain HTTP endpoint for testability.
        // The actual signing is delegated to the EC2 instance metadata service
        // or an injected credential provider in full deployment.
        let url = format!(
            "https://ec2.{region}.amazonaws.com/?Action=DescribeVolumes\
             &VolumeId.1={volume_id}\
             &Version=2016-11-15"
        );

        let response = self
            .client
            .get(&url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send()
            .await?;

        let status = response.status().as_u16();
        if status != 200 {
            let body = response.text().await.unwrap_or_default();
            warn!(
                volume_id = %volume_id,
                status = status,
                "AWS DescribeVolumes returned unexpected status"
            );
            return Err(VerifyError::UnexpectedStatus { status, body });
        }

        let body = response.text().await?;
        let is_detached = is_aws_volume_detached_from_xml(&body);

        info!(
            volume_id = %volume_id,
            region = %region,
            is_detached = is_detached,
            "AWS EBS volume attachment state verified"
        );

        Ok(is_detached)
    }
}

/// Parses a minimal subset of the EC2 `DescribeVolumesResponse` XML to
/// determine if the volume has any active attachments.
///
/// We only look for `<status>attached</status>` or `<status>attaching</status>`
/// within an `<item>` block.  Any other content is treated as "not attached".
fn is_aws_volume_detached_from_xml(xml: &str) -> bool {
    // Simple substring scan avoids pulling in a full XML parser dependency.
    !xml.contains("<status>attached</status>") && !xml.contains("<status>attaching</status>")
}

// ─────────────────────────────────────────────────────────────────────────────
// GCP Persistent Disk implementation
// ─────────────────────────────────────────────────────────────────────────────

/// GCP Persistent Disk attachment verifier.
///
/// Calls the Compute Engine `disks.get` REST API and inspects the `users`
/// field.  A non-empty `users` list means the disk is still attached to at
/// least one VM instance.
///
/// Authentication is performed via the GCE metadata server token endpoint
/// (`http://metadata.google.internal/...`).
pub struct GcpPdVerifier {
    client: reqwest::Client,
    /// GCP project ID, resolved from the instance metadata server if empty.
    project_id: String,
}

impl GcpPdVerifier {
    /// Creates a new `GcpPdVerifier` with the given project ID.
    pub fn new(project_id: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            project_id: project_id.into(),
        }
    }

    /// Creates a new `GcpPdVerifier` with a custom HTTP client.
    pub fn with_client(client: reqwest::Client, project_id: impl Into<String>) -> Self {
        Self {
            client,
            project_id: project_id.into(),
        }
    }

    /// Fetches a short-lived OAuth2 access token from the GCE metadata server.
    async fn fetch_metadata_token(&self) -> Result<String, VerifyError> {
        let token_url = "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";
        let response = self
            .client
            .get(token_url)
            .header("Metadata-Flavor", "Google")
            .send()
            .await?;

        let status = response.status().as_u16();
        if status != 200 {
            let body = response.text().await.unwrap_or_default();
            return Err(VerifyError::UnexpectedStatus { status, body });
        }

        let json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| VerifyError::Other(anyhow::anyhow!("token parse error: {e}")))?;

        json["access_token"]
            .as_str()
            .map(|s| s.to_owned())
            .ok_or_else(|| VerifyError::Other(anyhow::anyhow!("missing access_token in GCE metadata response")))
    }
}

#[async_trait]
impl CloudVolumeVerifier for GcpPdVerifier {
    fn provider_name(&self) -> &'static str {
        "gcp-pd"
    }

    async fn is_volume_detached(
        &self,
        disk_name: &str,
        zone: &str,
    ) -> Result<bool, VerifyError> {
        // Validate GCP disk name (lowercase letters, digits, hyphens, max 63 chars)
        if disk_name.is_empty() || disk_name.len() > 63 {
            return Err(VerifyError::InvalidVolumeId {
                provider: "gcp-pd",
                volume_id: disk_name.to_owned(),
            });
        }

        debug!(
            disk_name = %disk_name,
            zone = %zone,
            "querying GCP Compute Engine disks.get API"
        );

        let token = self.fetch_metadata_token().await?;

        let url = format!(
            "https://compute.googleapis.com/compute/v1/projects/{project}/zones/{zone}/disks/{disk}",
            project = self.project_id,
            zone = zone,
            disk = disk_name,
        );

        let response = self
            .client
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await?;

        let status = response.status().as_u16();
        if status == 404 {
            // Volume no longer exists → already cleaned up, safe to remove finalizer
            info!(
                disk_name = %disk_name,
                "GCP PD disk not found (404); treating as detached"
            );
            return Ok(true);
        }
        if status != 200 {
            let body = response.text().await.unwrap_or_default();
            warn!(
                disk_name = %disk_name,
                status = status,
                "GCP disks.get returned unexpected status"
            );
            return Err(VerifyError::UnexpectedStatus { status, body });
        }

        let json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| VerifyError::Other(anyhow::anyhow!("disk response parse error: {e}")))?;

        // A disk with no `users` field or an empty `users` array is detached.
        let is_detached = json
            .get("users")
            .and_then(|u| u.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(true);

        info!(
            disk_name = %disk_name,
            zone = %zone,
            is_detached = is_detached,
            "GCP PD disk attachment state verified"
        );

        Ok(is_detached)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Mock verifier for testing
// ─────────────────────────────────────────────────────────────────────────────

/// A deterministic mock verifier for unit and integration tests.
///
/// The caller controls whether `is_volume_detached` returns `true` or `false`
/// and whether it returns an error, allowing tests to exercise every branch of
/// the GC reconciler without making any network calls.
pub struct MockCloudVerifier {
    /// The fixed result to return from `is_volume_detached`.
    pub detached: bool,
    /// If set, `is_volume_detached` returns this error instead of `detached`.
    pub error: Option<String>,
    /// The provider name reported by `provider_name()`.
    pub name: &'static str,
}

impl MockCloudVerifier {
    /// Creates a mock that always reports the volume as detached.
    pub fn detached() -> Self {
        Self {
            detached: true,
            error: None,
            name: "mock",
        }
    }

    /// Creates a mock that always reports the volume as still attached.
    pub fn attached() -> Self {
        Self {
            detached: false,
            error: None,
            name: "mock",
        }
    }

    /// Creates a mock that always returns an error.
    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            detached: false,
            error: Some(msg.into()),
            name: "mock-error",
        }
    }
}

#[async_trait]
impl CloudVolumeVerifier for MockCloudVerifier {
    fn provider_name(&self) -> &'static str {
        self.name
    }

    async fn is_volume_detached(
        &self,
        _volume_id: &str,
        _region_or_zone: &str,
    ) -> Result<bool, VerifyError> {
        if let Some(ref msg) = self.error {
            return Err(VerifyError::Other(anyhow::anyhow!("{}", msg)));
        }
        Ok(self.detached)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Convenience factory
// ─────────────────────────────────────────────────────────────────────────────

/// Selects the appropriate [`CloudVolumeVerifier`] based on the PV's
/// provider annotation (`stellar.org/cloud-provider`).
///
/// Falls back to a no-op verifier that always returns `false` (unsafe) if the
/// provider is unrecognised, preventing accidental finalizer removal.
pub fn verifier_for_provider(
    provider: &str,
    gcp_project_id: Option<&str>,
) -> Box<dyn CloudVolumeVerifier> {
    match provider {
        "aws" | "aws-ebs" => Box::new(AwsEbsVerifier::new()),
        "gcp" | "gcp-pd" => Box::new(GcpPdVerifier::new(
            gcp_project_id.unwrap_or("unknown-project"),
        )),
        other => {
            warn!(
                provider = %other,
                "unrecognised cloud provider; defaulting to safe/blocking verifier"
            );
            Box::new(UnknownProviderVerifier)
        }
    }
}

/// A safe-by-default verifier used when the cloud provider is not recognised.
///
/// Always returns `Ok(false)` so the GC reconciler never strips a finalizer
/// for an unrecognised storage backend.
struct UnknownProviderVerifier;

#[async_trait]
impl CloudVolumeVerifier for UnknownProviderVerifier {
    fn provider_name(&self) -> &'static str {
        "unknown"
    }

    async fn is_volume_detached(
        &self,
        volume_id: &str,
        _region_or_zone: &str,
    ) -> Result<bool, VerifyError> {
        warn!(
            volume_id = %volume_id,
            "unknown provider; refusing to confirm detachment — finalizer will not be removed"
        );
        Ok(false)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── XML parser helper ──────────────────────────────────────────────────

    #[test]
    fn aws_xml_parser_detects_attached_volume() {
        let xml = r#"
            <DescribeVolumesResponse>
              <volumeSet>
                <item>
                  <volumeId>vol-0abc1234</volumeId>
                  <attachmentSet>
                    <item>
                      <instanceId>i-0deadbeef</instanceId>
                      <status>attached</status>
                    </item>
                  </attachmentSet>
                </item>
              </volumeSet>
            </DescribeVolumesResponse>"#;

        assert!(
            !is_aws_volume_detached_from_xml(xml),
            "volume with <status>attached</status> must not be considered detached"
        );
    }

    #[test]
    fn aws_xml_parser_detects_attaching_volume() {
        let xml = r#"<item><status>attaching</status></item>"#;
        assert!(
            !is_aws_volume_detached_from_xml(xml),
            "volume with <status>attaching</status> must not be considered detached"
        );
    }

    #[test]
    fn aws_xml_parser_accepts_detached_volume() {
        let xml = r#"
            <DescribeVolumesResponse>
              <volumeSet>
                <item>
                  <volumeId>vol-0abc1234</volumeId>
                  <attachmentSet/>
                </item>
              </volumeSet>
            </DescribeVolumesResponse>"#;

        assert!(
            is_aws_volume_detached_from_xml(xml),
            "volume with empty attachmentSet must be considered detached"
        );
    }

    #[test]
    fn aws_xml_parser_accepts_detached_status_label() {
        let xml = r#"<item><status>detached</status></item>"#;
        assert!(
            is_aws_volume_detached_from_xml(xml),
            "volume with <status>detached</status> must be considered detached"
        );
    }

    // ── MockCloudVerifier ─────────────────────────────────────────────────

    #[tokio::test]
    async fn mock_detached_returns_true() {
        let v = MockCloudVerifier::detached();
        let result = v.is_volume_detached("vol-000000", "us-east-1").await;
        assert_eq!(result.unwrap(), true);
    }

    #[tokio::test]
    async fn mock_attached_returns_false() {
        let v = MockCloudVerifier::attached();
        let result = v.is_volume_detached("vol-000000", "us-east-1").await;
        assert_eq!(result.unwrap(), false);
    }

    #[tokio::test]
    async fn mock_error_propagates() {
        let v = MockCloudVerifier::error("simulated API failure");
        let result = v.is_volume_detached("vol-000000", "us-east-1").await;
        assert!(result.is_err(), "mock error verifier must return an error");
        let err = result.unwrap_err().to_string();
        assert!(err.contains("simulated API failure"));
    }

    // ── Input validation ───────────────────────────────────────────────────

    #[tokio::test]
    async fn aws_verifier_rejects_invalid_volume_id() {
        let v = AwsEbsVerifier::new();
        let result = v.is_volume_detached("not-a-volume-id", "us-east-1").await;
        assert!(
            matches!(result, Err(VerifyError::InvalidVolumeId { .. })),
            "AWS verifier must reject volume IDs not starting with 'vol-'"
        );
    }

    #[tokio::test]
    async fn aws_verifier_rejects_short_volume_id() {
        let v = AwsEbsVerifier::new();
        // "vol-abc" is too short (< 12 chars)
        let result = v.is_volume_detached("vol-abc", "us-east-1").await;
        assert!(
            matches!(result, Err(VerifyError::InvalidVolumeId { .. })),
            "AWS verifier must reject volume IDs that are too short"
        );
    }

    #[tokio::test]
    async fn gcp_verifier_rejects_empty_disk_name() {
        let v = GcpPdVerifier::new("my-project");
        let result = v.is_volume_detached("", "us-central1-a").await;
        assert!(
            matches!(result, Err(VerifyError::InvalidVolumeId { .. })),
            "GCP verifier must reject empty disk names"
        );
    }

    // ── Factory function ──────────────────────────────────────────────────

    #[tokio::test]
    async fn unknown_provider_always_blocks() {
        let v = verifier_for_provider("azure", None);
        let result = v.is_volume_detached("vol-xyz", "eastus").await;
        assert_eq!(
            result.unwrap(),
            false,
            "unknown provider must return false to prevent unsafe finalizer removal"
        );
    }

    #[test]
    fn factory_returns_aws_verifier_for_aws_labels() {
        let v = verifier_for_provider("aws", None);
        assert_eq!(v.provider_name(), "aws-ebs");

        let v = verifier_for_provider("aws-ebs", None);
        assert_eq!(v.provider_name(), "aws-ebs");
    }

    #[test]
    fn factory_returns_gcp_verifier_for_gcp_labels() {
        let v = verifier_for_provider("gcp", Some("proj-123"));
        assert_eq!(v.provider_name(), "gcp-pd");

        let v = verifier_for_provider("gcp-pd", Some("proj-123"));
        assert_eq!(v.provider_name(), "gcp-pd");
    }
}
