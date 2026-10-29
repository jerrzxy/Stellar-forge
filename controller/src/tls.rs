// Copyright 2024 Stellar-K8s Contributors
// SPDX-License-Identifier: Apache-2.0
//! TLS Certificate Generation Helper
//!
//! Generates self-signed TLS certificates for the admission webhook server
//! using the `rcgen` crate.  In production, certificates are typically
//! provisioned by cert-manager via a `Certificate` resource and mounted as
//! a Kubernetes Secret.  This helper is used for local development and
//! integration testing where cert-manager is not available.
//!
//! # Usage
//!
//! ```rust,no_run
//! use controller::tls::{generate_self_signed, write_cert_files};
//! use std::net::IpAddr;
//!
//! # async fn example() -> anyhow::Result<()> {
//! let cert = generate_self_signed(
//!     "stellar-admission-webhook.stellar-system.svc",
//!     &["stellar-admission-webhook.stellar-system.svc.cluster.local"],
//!     Some(vec!["127.0.0.1".parse::<IpAddr>().unwrap()]),
//! )?;
//! write_cert_files(&cert, "/tmp/tls.crt", "/tmp/tls.key")?;
//! # Ok(())
//! # }
//! ```

use std::path::Path;

use anyhow::Context;
use tracing::info;

// ─────────────────────────────────────────────────────────────────────────────
// Public API
// ─────────────────────────────────────────────────────────────────────────────

/// A PEM-encoded self-signed certificate and private key.
#[derive(Debug, Clone)]
pub struct SelfSignedCert {
    /// PEM-encoded X.509 certificate.
    pub cert_pem: String,
    /// PEM-encoded private key (PKCS#8 or EC).
    pub key_pem: String,
}

/// Generate a self-signed TLS certificate suitable for the admission webhook.
///
/// # Arguments
///
/// * `cn`         – Common Name / primary DNS SAN (e.g.
///   `"stellar-webhook.stellar-system.svc"`).
/// * `extra_sans` – Additional DNS SANs (e.g. the full cluster-local FQDN).
/// * `ip_sans`    – Optional IP SANs (e.g. `127.0.0.1` for local testing).
///
/// The certificate is valid for 10 years, which is intentionally long for
/// development / CI use cases.  Production deployments should use cert-manager
/// with short-lived, automatically-rotated certificates.
pub fn generate_self_signed(
    cn: impl AsRef<str>,
    extra_sans: &[impl AsRef<str>],
    ip_sans: Option<Vec<std::net::IpAddr>>,
) -> anyhow::Result<SelfSignedCert> {
    use rcgen::{CertificateParams, DistinguishedName, DnType, IpAddr, KeyPair, SanType};

    let cn = cn.as_ref();
    let key_pair = KeyPair::generate().context("failed to generate key pair")?;

    let mut params = CertificateParams::default();
    params.not_before = time::OffsetDateTime::now_utc();
    params.not_after = time::OffsetDateTime::now_utc()
        + time::Duration::days(365 * 10); // 10 years

    // Distinguished name
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, cn);
    dn.push(DnType::OrganizationName, "Stellar-K8s");
    params.distinguished_name = dn;

    // Subject Alternative Names
    let mut sans: Vec<SanType> = vec![SanType::DnsName(cn.to_string().try_into()
        .map_err(|e| anyhow::anyhow!("invalid DNS name '{cn}': {e:?}"))?)];

    for san in extra_sans {
        let s = san.as_ref();
        sans.push(SanType::DnsName(s.to_string().try_into()
            .map_err(|e| anyhow::anyhow!("invalid DNS SAN '{s}': {e:?}"))?));
    }

    if let Some(ips) = ip_sans {
        for ip in ips {
            let rcgen_ip = match ip {
                std::net::IpAddr::V4(v4) => IpAddr::V4(v4.into()),
                std::net::IpAddr::V6(v6) => IpAddr::V6(v6.into()),
            };
            sans.push(SanType::IpAddress(rcgen_ip));
        }
    }

    params.subject_alt_names = sans;

    // Self-signed (no CA)
    params.is_ca = rcgen::IsCa::SelfSignedOnly;

    let cert = params
        .self_signed(&key_pair)
        .context("failed to self-sign certificate")?;

    info!(
        cn = %cn,
        extra_sans = ?extra_sans.iter().map(|s| s.as_ref()).collect::<Vec<_>>(),
        "generated self-signed TLS certificate for admission webhook"
    );

    Ok(SelfSignedCert {
        cert_pem: cert.pem(),
        key_pem: key_pair.serialize_pem(),
    })
}

/// Write PEM-encoded cert and key to files at the given paths.
///
/// Creates parent directories if they don't exist.
pub fn write_cert_files(
    cert: &SelfSignedCert,
    cert_path: impl AsRef<Path>,
    key_path: impl AsRef<Path>,
) -> anyhow::Result<()> {
    let cert_path = cert_path.as_ref();
    let key_path = key_path.as_ref();

    if let Some(parent) = cert_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    if let Some(parent) = key_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }

    std::fs::write(cert_path, cert.cert_pem.as_bytes())
        .with_context(|| format!("failed to write cert to {}", cert_path.display()))?;
    std::fs::write(key_path, cert.key_pem.as_bytes())
        .with_context(|| format!("failed to write key to {}", key_path.display()))?;

    info!(
        cert = %cert_path.display(),
        key = %key_path.display(),
        "wrote TLS certificate and key files"
    );

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    #[test]
    fn generates_valid_cert_pem() {
        let cert = generate_self_signed(
            "webhook.stellar-system.svc",
            &["webhook.stellar-system.svc.cluster.local"],
            Some(vec!["127.0.0.1".parse::<IpAddr>().unwrap()]),
        )
        .expect("cert generation should succeed");

        assert!(cert.cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(cert.key_pem.contains("PRIVATE KEY"));
    }

    #[test]
    fn generates_cert_with_no_extra_sans() {
        let cert = generate_self_signed::<&str>(
            "minimal.svc",
            &[],
            None,
        )
        .expect("cert generation with minimal SANs should succeed");

        assert!(!cert.cert_pem.is_empty());
        assert!(!cert.key_pem.is_empty());
    }

    #[test]
    fn writes_cert_files_to_temp_dir() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cert_path = dir.path().join("tls.crt");
        let key_path = dir.path().join("tls.key");

        let cert = generate_self_signed::<&str>("test.svc", &[], None).unwrap();
        write_cert_files(&cert, &cert_path, &key_path).expect("write should succeed");

        assert!(cert_path.exists(), "cert file should exist");
        assert!(key_path.exists(), "key file should exist");

        let read_cert = std::fs::read_to_string(&cert_path).unwrap();
        assert_eq!(read_cert, cert.cert_pem);
    }
}
