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

// Copyright 2024 Stellar-K8s Contributors
// SPDX-License-Identifier: Apache-2.0
//! HTTP helpers for talking to a provisioned ephemeral testnet.
//!
//! The controller reaches the network through the Service it creates in the
//! same namespace, so every check (RPC health, friendbot funding) exercises
//! the same paths CI jobs will use.

use std::time::Duration;

use serde_json::json;

/// Default HTTP port exposed by the `stellar/quickstart` stack.
pub const HTTP_PORT: u16 = 8000;
/// Default Stellar Core admin HTTP port.
pub const CORE_ADMIN_PORT: u16 = 11626;

/// Timeout for individual calls against the ephemeral network.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Connection URLs of a provisioned testnet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkUrls {
    /// Cluster-internal Soroban RPC endpoint.
    pub rpc: String,
    /// Cluster-internal friendbot endpoint.
    pub friendbot: String,
    /// Cluster-internal Stellar Core admin endpoint.
    pub core_admin: String,
}

/// Build the cluster-internal URLs for a testnet serviced by the
/// `<name>-rpc` and `<name>-core` Services.
pub fn cluster_urls(name: &str, namespace: &str) -> NetworkUrls {
    let base = format!("{name}-rpc.{namespace}.svc.cluster.local:{HTTP_PORT}");
    NetworkUrls {
        rpc: format!("http://{base}/rpc"),
        friendbot: format!("http://{base}/friendbot"),
        core_admin: format!("http://{name}-core.{namespace}.svc.cluster.local:{CORE_ADMIN_PORT}"),
    }
}

/// Build externally reachable URLs for an Ingress exposing the same Service
/// on port 80 (the standard `ingress-nginx` behaviour).
pub fn external_urls(host: &str) -> NetworkUrls {
    NetworkUrls {
        rpc: format!("http://{host}/rpc"),
        friendbot: format!("http://{host}/friendbot"),
        core_admin: String::new(),
    }
}

/// Errors returned by [`NetworkClient`].
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("unexpected response status {status}: {body}")]
    Status { status: u16, body: String },
    #[error("unexpected RPC response: {0}")]
    Protocol(String),
}

impl ClientError {
    /// Whether retrying the same request may succeed later (the network is
    /// still converging) as opposed to a permanent failure.
    pub fn is_retryable(&self) -> bool {
        match self {
            ClientError::Http(_) => true,
            ClientError::Status { status, .. } => *status == 429 || *status >= 500,
            ClientError::Protocol(_) => true,
        }
    }
}

/// Minimal client for the ephemeral network's RPC and friendbot endpoints.
#[derive(Debug, Clone)]
pub struct NetworkClient {
    http: reqwest::Client,
    urls: NetworkUrls,
}

impl NetworkClient {
    /// Create a client for the given URLs.
    pub fn new(http: reqwest::Client, urls: NetworkUrls) -> Self {
        Self { http, urls }
    }

    /// Convenience constructor addressing the network through its Services.
    pub fn for_testnet(http: reqwest::Client, name: &str, namespace: &str) -> Self {
        Self::new(http, cluster_urls(name, namespace))
    }

    /// The URLs this client talks to.
    pub fn urls(&self) -> &NetworkUrls {
        &self.urls
    }

    /// `getHealth` against the Soroban RPC endpoint.
    pub async fn get_health(&self) -> Result<bool, ClientError> {
        let response = self
            .http
            .post(&self.urls.rpc)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getHealth",
            }))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ClientError::Status {
                status: response.status().as_u16(),
                body: response.text().await.unwrap_or_default(),
            });
        }
        let body: serde_json::Value = response.json().await?;
        Ok(body["result"]["status"] == "healthy")
    }

    /// `getNetwork` against the Soroban RPC endpoint, returning the
    /// passphrase the network reports.
    pub async fn get_network_passphrase(&self) -> Result<String, ClientError> {
        let response = self
            .http
            .post(&self.urls.rpc)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getNetwork",
            }))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(ClientError::Status {
                status: response.status().as_u16(),
                body: response.text().await.unwrap_or_default(),
            });
        }
        let body: serde_json::Value = response.json().await?;
        body["result"]["passphrase"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| {
                ClientError::Protocol(format!("getNetwork returned no passphrase: {body}"))
            })
    }

    /// Ask friendbot to fund `public_key` with the network's starting
    /// balance. A successful response means the funding transaction was
    /// submitted and accepted.
    pub async fn fund(&self, public_key: &str) -> Result<(), ClientError> {
        let response = self
            .http
            .get(&self.urls.friendbot)
            .query(&[("addr", public_key)])
            .send()
            .await?;
        if response.status().is_success() {
            return Ok(());
        }
        Err(ClientError::Status {
            status: response.status().as_u16(),
            body: response.text().await.unwrap_or_default(),
        })
    }
}

/// Shared HTTP client factory with a timeout suited to a local network.
pub fn default_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("reqwest client with fixed settings builds")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_urls_use_generated_service_names() {
        let urls = cluster_urls("pr-961", "stellar-ci");
        assert_eq!(
            urls.rpc,
            "http://pr-961-rpc.stellar-ci.svc.cluster.local:8000/rpc"
        );
        assert_eq!(
            urls.friendbot,
            "http://pr-961-rpc.stellar-ci.svc.cluster.local:8000/friendbot"
        );
        assert_eq!(
            urls.core_admin,
            "http://pr-961-core.stellar-ci.svc.cluster.local:11626"
        );
    }

    #[test]
    fn external_urls_use_ingress_host() {
        let urls = external_urls("pr-961.stellar.example.com");
        assert_eq!(urls.rpc, "http://pr-961.stellar.example.com/rpc");
        assert_eq!(
            urls.friendbot,
            "http://pr-961.stellar.example.com/friendbot"
        );
    }

    #[test]
    fn retryability_classification() {
        let retryable = ClientError::Status {
            status: 503,
            body: String::new(),
        };
        assert!(retryable.is_retryable());

        let permanent = ClientError::Status {
            status: 400,
            body: "bad address".to_string(),
        };
        assert!(!permanent.is_retryable());

        let throttled = ClientError::Status {
            status: 429,
            body: String::new(),
        };
        assert!(throttled.is_retryable());
    }

    #[test]
    fn client_builds_with_timeout() {
        let client = default_http_client();
        let net = NetworkClient::new(client, cluster_urls("testnet", "default"));
        assert!(net.urls().rpc.ends_with("/rpc"));
    }
}
