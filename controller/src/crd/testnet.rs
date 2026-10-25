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
//! `StellarTestnet` custom resource.
//!
//! A `StellarTestnet` describes an ephemeral, single-node Stellar network used
//! for CI/CD pipelines and local contract development. The operator reconciles
//! each resource into a lightweight Stellar Core standalone node (backed by
//! memory-resident databases) plus a Soroban RPC endpoint, and reports the
//! resulting connection URLs in [`StellarTestnetStatus`].
//!
//! ```yaml
//! apiVersion: stellar.org/v1alpha1
//! kind: StellarTestnet
//! metadata:
//!   name: pr-1234
//! spec:
//!   accounts:
//!     - name: deployer
//!   rpc:
//!     ingress:
//!       enabled: false
//! ```

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// API group of the `StellarTestnet` resource.
pub const GROUP: &str = "stellar.org";
/// API version of the `StellarTestnet` resource.
pub const VERSION: &str = "v1alpha1";
/// Fully qualified `apiVersion` of the `StellarTestnet` resource.
pub const API_VERSION: &str = "stellar.org/v1alpha1";
/// Kind of the `StellarTestnet` resource.
pub const KIND: &str = "StellarTestnet";
/// Plural resource name of the `StellarTestnet` resource.
pub const PLURAL: &str = "stellartestnets";
/// Finalizer that guarantees teardown of child resources before the
/// `StellarTestnet` object is removed from the API server.
pub const FINALIZER: &str = "stellar.org/testnet-finalizer";

/// Default network passphrase used by `stellar/quickstart --local`.
pub const DEFAULT_NETWORK_PASSPHRASE: &str = "Standalone Network ; February 2017";
/// Default container image providing Core, Soroban RPC and friendbot.
pub const DEFAULT_IMAGE: &str = "stellar/quickstart:latest";

/// The `StellarTestnet` custom resource.
#[derive(
    CustomResource, Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema, Default,
)]
#[kube(
    group = "stellar.org",
    version = "v1alpha1",
    kind = "StellarTestnet",
    plural = "stellartestnets",
    singular = "stellartestnet",
    shortname = "stn",
    namespaced,
    status = "StellarTestnetStatus",
    printcolumn = r#"{"name":"Phase","type":"string","description":"Lifecycle phase of the testnet","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"RPC","type":"string","description":"Soroban RPC endpoint","jsonPath":".status.rpcUrl"}"#,
    printcolumn = r#"{"name":"Age","type":"date","description":"Age of the testnet","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase", default)]
pub struct StellarTestnetSpec {
    /// Genesis ledger configuration for the ephemeral network.
    pub genesis: Option<GenesisSpec>,
    /// Pre-funded test accounts that are created (and funded via friendbot)
    /// as part of provisioning.
    pub accounts: Vec<TestAccountSpec>,
    /// Configuration of the Soroban RPC interface (currently ingress
    /// exposure).
    pub rpc: Option<RpcSpec>,
    /// Container image that provides Stellar Core, Soroban RPC and friendbot.
    /// Defaults to `stellar/quickstart:latest`.
    pub image: Option<String>,
    /// When set, the operator deletes the whole `StellarTestnet` this many
    /// seconds after the network first becomes `Ready`. Used by CI pipelines
    /// to guarantee teardown even when a job is killed before it can delete
    /// the manifest. Defaults to no automatic teardown.
    pub ttl_seconds_after_ready: Option<u64>,
}

/// Genesis ledger configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct GenesisSpec {
    /// Network passphrase signed into every transaction. Must match the
    /// passphrase of the container stack; defaults to the standalone
    /// quickstart passphrase.
    pub network_passphrase: Option<String>,
    /// Where the node databases live. `memory` (the default) keeps every
    /// database in RAM-backed `emptyDir` volumes so a deleted testnet leaves
    /// nothing behind; `disk` uses node-local (still ephemeral) storage.
    pub storage: Option<StorageMode>,
    /// Ledger protocol version the genesis node upgrades to on start
    /// (quickstart `--protocol-version`). Defaults to the image's own
    /// default. Pin it when the image's default protocol is older than the
    /// Soroban host its RPC ships with, otherwise contract simulation fails.
    #[schemars(range(min = 20))]
    pub protocol_version: Option<u32>,
}

/// Backing store for the Core/RPC databases of an ephemeral testnet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum StorageMode {
    /// RAM-backed `emptyDir` (`medium: Memory`) volumes.
    Memory,
    /// Ephemeral node-local disk `emptyDir` volumes.
    Disk,
}

/// A pre-funded test account requested in `spec.accounts`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct TestAccountSpec {
    /// Human readable name, unique within the resource. Used as the input for
    /// deterministic key derivation, so repeated reconciles always yield the
    /// same keypair for the same name.
    pub name: String,
    /// Optional externally managed public key (`G...`). When omitted the
    /// operator derives a fresh keypair for this account.
    pub public_key: Option<String>,
}

/// Soroban RPC interface configuration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct RpcSpec {
    /// Optional Ingress exposing the RPC (and friendbot) endpoints outside
    /// the cluster.
    pub ingress: Option<IngressSpec>,
}

/// Ingress exposure of the Soroban RPC endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct IngressSpec {
    /// Set to `true` to create an Ingress for this testnet.
    pub enabled: Option<bool>,
    /// Optional hostname rule. When omitted the Ingress matches any host.
    pub host: Option<String>,
    /// `ingressClassName` to request (for example `nginx`).
    pub class_name: Option<String>,
}

/// Lifecycle phase of a `StellarTestnet`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "PascalCase")]
pub enum Phase {
    /// The resource was accepted but resources have not been created yet.
    #[default]
    Pending,
    /// Child resources exist; the network is starting or accounts are being
    /// funded.
    Provisioning,
    /// Soroban RPC is healthy and every declared account is funded.
    Ready,
    /// Provisioning failed and needs attention.
    Failed,
    /// The resource is being torn down.
    Deleting,
}

/// Origin of a pre-funded account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AccountSource {
    /// Keypair derived deterministically by the operator.
    Generated,
    /// Public key supplied by the user in `spec.accounts`.
    Provided,
}

/// Account details reported in `status.accounts`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FundedAccount {
    /// Name from `spec.accounts`.
    pub name: String,
    /// Public key (`G...`) of the account.
    pub public_key: String,
    /// Secret key (`S...`) for operator-generated accounts. Never set for
    /// user-provided keys. These are throwaway keys for an ephemeral network
    /// and are only safe because the whole network disappears on delete.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_key: Option<String>,
    /// Whether the keypair was generated by the operator or supplied.
    pub source: AccountSource,
    /// Whether friendbot has confirmed funding.
    pub funded: bool,
}

/// A Kubernetes condition stored in `status.conditions`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StatusCondition {
    /// Condition type, `Ready` for the top-level readiness condition.
    #[serde(rename = "type")]
    pub condition_type: String,
    /// `"True"`, `"False"` or `"Unknown"`.
    pub status: String,
    /// Machine readable reason, for example `Provisioned`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Human readable detail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// RFC 3339 timestamp of the last transition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<String>,
}

/// Status reported by the operator, including all connection URLs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct StellarTestnetStatus {
    /// Current lifecycle phase.
    pub phase: Option<Phase>,
    /// Human readable detail for the current phase.
    pub message: Option<String>,
    /// `metadata.generation` this status was computed from.
    pub observed_generation: Option<i64>,
    /// Passphrase every transaction of this network must be signed with.
    pub network_passphrase: Option<String>,
    /// Genesis root account (`G...`) funded with the entire supply at
    /// genesis. Derived from the network passphrase.
    pub root_account: Option<String>,
    /// Cluster-internal Soroban RPC URL, e.g.
    /// `http://pr-1234-rpc.<ns>.svc.cluster.local:8000/rpc`.
    pub rpc_url: Option<String>,
    /// Externally reachable RPC URL when an Ingress is enabled.
    pub external_rpc_url: Option<String>,
    /// Cluster-internal friendbot URL used to fund accounts.
    pub friendbot_url: Option<String>,
    /// Cluster-internal Stellar Core admin HTTP URL.
    pub core_admin_url: Option<String>,
    /// Details of every pre-funded account declared in `spec.accounts`.
    pub accounts: Option<Vec<FundedAccount>>,
    /// RFC 3339 timestamp of the first `Ready` transition. Used for TTL
    /// based auto-teardown.
    pub ready_at: Option<String>,
    /// Conditions, including `Ready` (used by `kubectl wait`).
    pub conditions: Option<Vec<StatusCondition>>,
}

impl StellarTestnetSpec {
    /// Network passphrase with defaults applied.
    pub fn network_passphrase(&self) -> String {
        self.genesis
            .as_ref()
            .and_then(|g| g.network_passphrase.clone())
            .unwrap_or_else(|| DEFAULT_NETWORK_PASSPHRASE.to_string())
    }

    /// Requested ledger protocol version, if pinned.
    pub fn protocol_version(&self) -> Option<u32> {
        self.genesis.as_ref().and_then(|g| g.protocol_version)
    }

    /// Storage mode with defaults applied.
    pub fn storage_mode(&self) -> StorageMode {
        self.genesis
            .as_ref()
            .and_then(|g| g.storage)
            .unwrap_or(StorageMode::Memory)
    }

    /// Container image with defaults applied.
    pub fn image(&self) -> String {
        self.image
            .clone()
            .unwrap_or_else(|| DEFAULT_IMAGE.to_string())
    }

    /// Ingress configuration when enabled.
    pub fn ingress(&self) -> Option<&IngressSpec> {
        self.rpc
            .as_ref()
            .and_then(|r| r.ingress.as_ref())
            .filter(|i| i.enabled.unwrap_or(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    fn minimal() -> StellarTestnet {
        serde_yaml::from_str(
            r#"
            apiVersion: stellar.org/v1alpha1
            kind: StellarTestnet
            metadata:
              name: ci-246
              namespace: stellar-ci
            spec: {}
            "#,
        )
        .expect("minimal manifest parses")
    }

    #[test]
    fn minimal_manifest_applies_defaults() {
        let net = minimal();
        assert_eq!(net.spec.network_passphrase(), DEFAULT_NETWORK_PASSPHRASE);
        assert_eq!(net.spec.image(), DEFAULT_IMAGE);
        assert_eq!(net.spec.storage_mode(), StorageMode::Memory);
        assert!(net.spec.accounts.is_empty());
        assert!(net.spec.ingress().is_none());
        assert_eq!(net.spec.ttl_seconds_after_ready, None);
    }

    #[test]
    fn full_manifest_parses() {
        let net: StellarTestnet = serde_yaml::from_str(
            r#"
            apiVersion: stellar.org/v1alpha1
            kind: StellarTestnet
            metadata:
              name: pr-961
              namespace: stellar-ci
            spec:
              genesis:
                networkPassphrase: "Standalone Network ; February 2017"
                storage: memory
              accounts:
                - name: deployer
                - name: external
                  publicKey: GBZXN7PIRZGNMHGA7MUUUF4GWPY5AYPV6LY4UV2GL6VJGIQRXFDNMADI
              rpc:
                ingress:
                  enabled: true
                  host: pr-961.stellar.example.com
                  className: nginx
              image: stellar/quickstart:v23.0.0
              ttlSecondsAfterReady: 3600
            "#,
        )
        .expect("full manifest parses");

        let spec = net.spec;
        assert_eq!(spec.accounts.len(), 2);
        assert_eq!(spec.accounts[0].name, "deployer");
        assert_eq!(spec.accounts[0].public_key, None);
        assert_eq!(
            spec.accounts[1].public_key.as_deref(),
            Some("GBZXN7PIRZGNMHGA7MUUUF4GWPY5AYPV6LY4UV2GL6VJGIQRXFDNMADI")
        );
        assert_eq!(spec.storage_mode(), StorageMode::Memory);
        assert_eq!(spec.image(), "stellar/quickstart:v23.0.0");
        assert_eq!(spec.ttl_seconds_after_ready, Some(3600));

        let ingress = spec.ingress().expect("enabled ingress is exposed");
        assert_eq!(ingress.host.as_deref(), Some("pr-961.stellar.example.com"));
        assert_eq!(ingress.class_name.as_deref(), Some("nginx"));
    }

    #[test]
    fn disabled_ingress_is_not_exposed() {
        let net: StellarTestnet = serde_yaml::from_str(
            r#"
            apiVersion: stellar.org/v1alpha1
            kind: StellarTestnet
            metadata: { name: no-ingress }
            spec:
              rpc:
                ingress:
                  enabled: false
                  host: ignored.example.com
            "#,
        )
        .expect("manifest parses");
        assert!(net.spec.ingress().is_none());
    }

    #[test]
    fn crd_schema_is_generated() {
        let crd = StellarTestnet::crd();
        assert_eq!(
            crd.metadata.name.as_deref(),
            Some("stellartestnets.stellar.org")
        );
        assert_eq!(crd.spec.group, GROUP);

        let version = crd
            .spec
            .versions
            .iter()
            .find(|v| v.name == VERSION)
            .expect("v1alpha1 version present");
        assert!(version.served);
        assert!(version.storage);
        assert!(
            version
                .subresources
                .as_ref()
                .is_some_and(|subresources| subresources.status.is_some()),
            "status subresource"
        );
        assert!(version
            .additional_printer_columns
            .as_ref()
            .is_some_and(|columns| columns.len() >= 3));
        assert_eq!(
            version
                .schema
                .as_ref()
                .expect("openapi schema")
                .open_api_v3_schema
                .as_ref()
                .expect("openapi v3 schema")
                .type_
                .as_deref(),
            Some("object")
        );
    }

    #[test]
    fn phase_serializes_pascal_case() {
        assert_eq!(
            serde_json::to_string(&Phase::Provisioning).unwrap(),
            "\"Provisioning\""
        );
        assert_eq!(
            serde_json::to_string(&StorageMode::Memory).unwrap(),
            "\"memory\""
        );
        assert_eq!(
            serde_json::to_string(&AccountSource::Generated).unwrap(),
            "\"generated\""
        );
    }

    #[test]
    fn status_nulls_clear_fields_on_merge() {
        // The operator relies on merge patches: `null` clears a stale value.
        let status = StellarTestnetStatus {
            phase: Some(Phase::Ready),
            external_rpc_url: None,
            ..Default::default()
        };
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(value["phase"], "Ready");
        assert!(
            value.get("externalRpcUrl").is_some_and(|v| v.is_null()),
            "absent URLs must serialize as null so merge patches clear them"
        );
    }
}
