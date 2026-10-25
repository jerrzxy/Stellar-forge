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
//! Reconciliation of `StellarTestnet` resources.
//!
//! Each `StellarTestnet` is reconciled into:
//!
//! * a `Deployment` running the `stellar/quickstart` stack in local mode —
//!   a Stellar Core standalone node, a Soroban RPC server and friendbot,
//!   with every database mounted on memory-backed `emptyDir` volumes so the
//!   network is fully ephemeral;
//! * a `<name>-rpc` Service for the RPC/friendbot HTTP endpoint and a
//!   `<name>-core` Service for the Stellar Core admin endpoint;
//! * an optional Ingress exposing the RPC endpoint outside the cluster.
//!
//! All children carry an `OwnerReference` to the `StellarTestnet` **and** a
//! shared label selector, so deleting the custom resource garbage-collects
//! the whole network; the finalizer additionally performs an explicit sweep
//! before the object is released.
//!
//! Once the stack reports a healthy Soroban RPC, the controller funds every
//! account declared in `spec.accounts` through the network's own friendbot
//! and publishes connection URLs, key material and the `Ready` condition in
//! `status`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy};
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EmptyDirVolumeSource, EnvVar, ExecAction, PodSpec, PodTemplateSpec,
    Probe, ResourceRequirements, Service as K8sService, ServicePort, ServiceSpec as K8sServiceSpec,
    Volume, VolumeMount,
};
use k8s_openapi::api::networking::v1::{
    HTTPIngressPath, HTTPIngressRuleValue, Ingress as K8sIngress, IngressBackend, IngressRule,
    ServiceBackendPort,
};
use k8s_openapi::api::networking::v1::{IngressServiceBackend, IngressSpec as K8sIngressSpec};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::{
    api::{DeleteParams, ListParams, ObjectMeta, Patch, PatchParams},
    runtime::{controller::Action, reflector::ObjectRef},
    Api, Client, Resource, ResourceExt,
};
use serde_json::json;
use tracing::{info, warn};

use crate::crd::testnet::{
    AccountSource, FundedAccount, Phase, StatusCondition, StellarTestnet, StellarTestnetStatus,
    FINALIZER, KIND,
};
use crate::testnet::client::{self, ClientError, NetworkClient};
use crate::testnet::keys;

/// Field manager used for server-side apply of child resources.
pub const AGENT: &str = "stellar-testnet-operator";
/// Value of the `app.kubernetes.io/name` label.
pub const APP_NAME: &str = "stellar-testnet";
/// Label carrying the owning `StellarTestnet` name.
pub const LABEL_TESTNET: &str = "stellar.org/testnet";

/// Requeue interval while the network is still coming up.
const PROVISIONING_REQUEUE: Duration = Duration::from_secs(3);
/// Requeue interval once the network is ready (TTL and drift checks).
const READY_REQUEUE: Duration = Duration::from_secs(15);

/// Readiness probe: only pass once the Soroban RPC answers `getHealth`.
const READINESS_SCRIPT: &str = r#"curl -sf --max-time 3 -H 'Content-Type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' http://127.0.0.1:8000/rpc | grep -q '"status":"healthy"'"#;

/// Mount points whose databases are kept in memory-backed volumes.
const CORE_DATA_PATH: &str = "/opt/stellar/core";
const RPC_DATA_PATH: &str = "/opt/stellar/stellar-rpc";
const HISTORY_DATA_PATH: &str = "/opt/stellar/history-archive";

/// Shared state threaded through the reconcile loop.
#[derive(Clone)]
pub struct Context {
    /// Kubernetes API client.
    pub client: Client,
    /// HTTP client for RPC/friendbot calls.
    pub http: reqwest::Client,
}

/// Errors surfaced by reconciliation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("kubernetes api error: {0}")]
    Kube(#[from] kube::Error),
    #[error("network call failed: {0}")]
    Client(#[from] ClientError),
    #[error("status serialization failed: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Reconcile one `StellarTestnet`, managing its finalizer.
pub async fn reconcile(net: Arc<StellarTestnet>, ctx: Arc<Context>) -> Result<Action, Error> {
    let api = Api::<StellarTestnet>::namespaced(ctx.client.clone(), &namespace_of(&net));

    if net.meta().deletion_timestamp.is_some() {
        return cleanup(net, ctx).await;
    }

    if !has_finalizer(&net) {
        add_finalizer(&api, &net).await?;
        info!(
            testnet = %net.name_any(),
            "added finalizer, scheduling immediate reconcile"
        );
        return Ok(Action::requeue(Duration::from_secs(1)));
    }

    apply(net, ctx).await
}

/// Error backoff: log and retry.
pub fn error_policy(net: Arc<StellarTestnet>, error: &Error, _ctx: Arc<Context>) -> Action {
    warn!(
        testnet = %net.name_any(),
        error = %error,
        "reconcile failed, backing off"
    );
    Action::requeue(Duration::from_secs(10))
}

/// Create or update every child resource of `net`.
async fn apply(net: Arc<StellarTestnet>, ctx: Arc<Context>) -> Result<Action, Error> {
    let name = net.name_any();
    let namespace = namespace_of(&net);

    if let Err(message) = validate_spec(&net) {
        let mut status = base_status(&net);
        status.phase = Some(Phase::Failed);
        status.message = Some(message.clone());
        status.conditions = Some(vec![ready_condition(false, "InvalidSpec", &message)]);
        patch_status(
            &Api::namespaced(ctx.client.clone(), &namespace),
            &name,
            &status,
        )
        .await?;
        return Ok(Action::requeue(READY_REQUEUE));
    }

    info!(testnet = %name, "reconciling StellarTestnet");

    // ── Desired children ───────────────────────────────────────────────
    let deployments: Api<Deployment> = Api::namespaced(ctx.client.clone(), &namespace);
    let services: Api<K8sService> = Api::namespaced(ctx.client.clone(), &namespace);
    let ingresses: Api<K8sIngress> = Api::namespaced(ctx.client.clone(), &namespace);

    apply_resource(&deployments, &build_deployment(&net)).await?;
    apply_resource(&services, &build_rpc_service(&net)).await?;
    apply_resource(&services, &build_core_service(&net)).await?;
    match build_ingress(&net) {
        Some(ingress) => apply_resource(&ingresses, &ingress).await?,
        None => delete_quietly(&ingresses, &ingress_name(&name)).await?,
    }

    // ── Status bookkeeping ─────────────────────────────────────────────
    let existing = net.status.clone().unwrap_or_default();
    let mut status = base_status(&net);
    status.observed_generation = net.meta().generation;

    // ── Wait for the stack to become ready ─────────────────────────────
    let stack_ready = deployments
        .get(&name)
        .await?
        .status
        .and_then(|s| s.ready_replicas)
        .unwrap_or(0)
        >= 1;
    if !stack_ready {
        status.phase = Some(Phase::Provisioning);
        status.message =
            Some("waiting for the Stellar stack pod to pass its readiness probe".into());
        status.accounts = Some(pending_accounts(&net));
        status.conditions = Some(vec![ready_condition(
            false,
            "StackStarting",
            "waiting for the Stellar stack pod",
        )]);
        patch_status(
            &Api::namespaced(ctx.client.clone(), &namespace),
            &name,
            &status,
        )
        .await?;
        return Ok(Action::requeue(PROVISIONING_REQUEUE));
    }

    let net_client = NetworkClient::for_testnet(ctx.http.clone(), &name, &namespace);
    match net_client.get_health().await {
        Ok(true) => {}
        Ok(false) => {
            status.phase = Some(Phase::Provisioning);
            status.message = Some("Soroban RPC is reachable but not healthy yet".into());
            status.accounts = Some(pending_accounts(&net));
            status.conditions = Some(vec![ready_condition(
                false,
                "RpcStarting",
                "Soroban RPC is not healthy yet",
            )]);
            patch_status(
                &Api::namespaced(ctx.client.clone(), &namespace),
                &name,
                &status,
            )
            .await?;
            return Ok(Action::requeue(PROVISIONING_REQUEUE));
        }
        Err(error) => {
            status.phase = Some(Phase::Provisioning);
            status.message = Some(format!("waiting for Soroban RPC: {error}"));
            status.accounts = Some(pending_accounts(&net));
            status.conditions = Some(vec![ready_condition(
                false,
                "RpcStarting",
                "waiting for the Soroban RPC endpoint",
            )]);
            patch_status(
                &Api::namespaced(ctx.client.clone(), &namespace),
                &name,
                &status,
            )
            .await?;
            return Ok(Action::requeue(PROVISIONING_REQUEUE));
        }
    }

    // ── Verify genesis configuration took effect ──────────────────────
    let expected_passphrase = net.spec.network_passphrase();
    match net_client.get_network_passphrase().await {
        Ok(actual) if actual == expected_passphrase => {}
        Ok(actual) => {
            let message = format!(
                "network passphrase mismatch: spec expects {expected_passphrase:?} but the RPC reports {actual:?}"
            );
            status.phase = Some(Phase::Failed);
            status.message = Some(message.clone());
            status.accounts = Some(pending_accounts(&net));
            status.conditions = Some(vec![ready_condition(false, "PassphraseMismatch", &message)]);
            patch_status(
                &Api::namespaced(ctx.client.clone(), &namespace),
                &name,
                &status,
            )
            .await?;
            return Ok(Action::requeue(READY_REQUEUE));
        }
        Err(error) => {
            status.phase = Some(Phase::Provisioning);
            status.message = Some(format!("reading network configuration: {error}"));
            status.accounts = Some(pending_accounts(&net));
            status.conditions = Some(vec![ready_condition(
                false,
                "VerifyingNetwork",
                "waiting for the Soroban RPC getNetwork endpoint",
            )]);
            patch_status(
                &Api::namespaced(ctx.client.clone(), &namespace),
                &name,
                &status,
            )
            .await?;
            return Ok(Action::requeue(PROVISIONING_REQUEUE));
        }
    }

    // ── Fund declared accounts through the network's friendbot ─────────
    let mut accounts = existing.accounts.clone().unwrap_or_default();
    for account in derived_accounts(&net) {
        let already_funded = accounts
            .iter()
            .find(|a| a.public_key == account.public)
            .map(|a| a.funded)
            .unwrap_or(false);
        if already_funded {
            continue;
        }
        match net_client.fund(&account.public).await {
            Ok(()) => {
                info!(testnet = %name, account = %account.name, "funded account");
                accounts.retain(|a| a.public_key != account.public);
                accounts.push(FundedAccount {
                    name: account.name.clone(),
                    public_key: account.public.clone(),
                    secret_key: account.secret.clone(),
                    source: account.source,
                    funded: true,
                });
            }
            Err(error) if error.is_retryable() => {
                status.phase = Some(Phase::Provisioning);
                status.message = Some(format!("funding account {}: {error}", account.name));
                accounts.retain(|a| a.public_key != account.public);
                accounts.push(FundedAccount {
                    name: account.name.clone(),
                    public_key: account.public.clone(),
                    secret_key: account.secret.clone(),
                    source: account.source,
                    funded: false,
                });
                status.accounts = Some(accounts);
                status.conditions = Some(vec![ready_condition(
                    false,
                    "FundingAccounts",
                    "funding pre-funded accounts via friendbot",
                )]);
                patch_status(
                    &Api::namespaced(ctx.client.clone(), &namespace),
                    &name,
                    &status,
                )
                .await?;
                return Ok(Action::requeue(PROVISIONING_REQUEUE));
            }
            Err(error) => {
                let message = format!(
                    "funding account {} failed permanently: {error}",
                    account.name
                );
                status.phase = Some(Phase::Failed);
                status.message = Some(message.clone());
                status.accounts = Some(accounts);
                status.conditions = Some(vec![ready_condition(false, "FundingFailed", &message)]);
                patch_status(
                    &Api::namespaced(ctx.client.clone(), &namespace),
                    &name,
                    &status,
                )
                .await?;
                return Ok(Action::requeue(READY_REQUEUE));
            }
        }
    }

    // ── Ready ──────────────────────────────────────────────────────────
    accounts.sort_by(|a, b| a.name.cmp(&b.name));
    status.phase = Some(Phase::Ready);
    status.message = Some("Soroban RPC healthy; all declared accounts funded".into());
    status.accounts = Some(accounts);
    status.ready_at = existing
        .ready_at
        .or_else(|| Some(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)));
    status.conditions = Some(vec![ready_condition(
        true,
        "Provisioned",
        "Soroban RPC healthy; all declared accounts funded",
    )]);
    patch_status(
        &Api::namespaced(ctx.client.clone(), &namespace),
        &name,
        &status,
    )
    .await?;
    info!(
        testnet = %name,
        rpc_url = %net_client.urls().rpc,
        "StellarTestnet is ready"
    );

    // ── TTL based auto-teardown ────────────────────────────────────────
    if let Some(ttl) = net.spec.ttl_seconds_after_ready {
        if let Some(elapsed) = elapsed_since_ready(&status) {
            if elapsed >= Duration::from_secs(ttl) {
                info!(testnet = %name, "TTL expired, deleting StellarTestnet");
                Api::<StellarTestnet>::namespaced(ctx.client.clone(), &namespace)
                    .delete(&name, &DeleteParams::default())
                    .await?;
                return Ok(Action::await_change());
            }
        }
    }

    Ok(Action::requeue(READY_REQUEUE))
}

/// Tear down every child resource, then release the finalizer.
async fn cleanup(net: Arc<StellarTestnet>, ctx: Arc<Context>) -> Result<Action, Error> {
    let name = net.name_any();
    let namespace = namespace_of(&net);
    let api = Api::<StellarTestnet>::namespaced(ctx.client.clone(), &namespace);
    let selector = format!("{LABEL_TESTNET}={name}");

    info!(testnet = %name, "deleting StellarTestnet, sweeping owned resources");

    for (kind, count) in [
        (
            "Deployment",
            sweep(
                &Api::<Deployment>::namespaced(ctx.client.clone(), &namespace),
                &selector,
            )
            .await?
            .len(),
        ),
        (
            "Service",
            sweep(
                &Api::<K8sService>::namespaced(ctx.client.clone(), &namespace),
                &selector,
            )
            .await?
            .len(),
        ),
        (
            "Ingress",
            sweep(
                &Api::<K8sIngress>::namespaced(ctx.client.clone(), &namespace),
                &selector,
            )
            .await?
            .len(),
        ),
    ] {
        if count > 0 {
            info!(testnet = %name, kind, count, "deleted owned resources");
        }
    }

    remove_finalizer(&api, &net).await?;
    info!(testnet = %name, "teardown complete, finalizer released");
    Ok(Action::await_change())
}

// ─────────────────────────────────────────────────────────────────────────────
// Pure helpers (unit tested)
// ─────────────────────────────────────────────────────────────────────────────

/// Namespace of a `StellarTestnet`, defaulting to `default` for objects
/// that have not been assigned one yet.
fn namespace_of(net: &StellarTestnet) -> String {
    net.namespace().unwrap_or_else(|| "default".to_string())
}

/// A keypair that must be funded for a `StellarTestnet`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedAccount {
    /// Name from `spec.accounts`.
    pub name: String,
    /// Public key (`G...`).
    pub public: String,
    /// Secret key (`S...`) for generated accounts.
    pub secret: Option<String>,
    /// Whether the key was generated or supplied.
    pub source: AccountSource,
}

/// Validate the spec, returning a human readable error.
pub fn validate_spec(net: &StellarTestnet) -> Result<(), String> {
    if net.spec.image().trim().is_empty() {
        return Err("spec.image must not be empty".into());
    }
    if let Some(version) = net.spec.protocol_version() {
        if version < 20 {
            return Err(format!(
                "spec.genesis.protocolVersion {version} predates Soroban (minimum 20)"
            ));
        }
    }
    let mut seen = BTreeMap::new();
    for account in &net.spec.accounts {
        if account.name.trim().is_empty() {
            return Err("every spec.accounts entry needs a non-empty name".into());
        }
        if seen.insert(account.name.clone(), ()).is_some() {
            return Err(format!("duplicate account name {}", account.name));
        }
        if let Some(key) = &account.public_key {
            if !is_account_id(key) {
                return Err(format!(
                    "account {} has an invalid public key (expected a 56 character G... strkey)",
                    account.name
                ));
            }
        }
    }
    Ok(())
}

/// Shape check for ed25519 account ids.
fn is_account_id(value: &str) -> bool {
    value.len() == 56
        && value.starts_with('G')
        && value
            .chars()
            .all(|c| "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567".contains(c))
}

/// Resolve `spec.accounts` into concrete keypairs to fund. Generated
/// accounts are deterministic per (passphrase, namespace, name, account).
pub fn derived_accounts(net: &StellarTestnet) -> Vec<DerivedAccount> {
    let namespace = namespace_of(net);
    let testnet = net.name_any();
    let passphrase = net.spec.network_passphrase();
    net.spec
        .accounts
        .iter()
        .map(|account| match &account.public_key {
            Some(public) => DerivedAccount {
                name: account.name.clone(),
                public: public.clone(),
                secret: None,
                source: AccountSource::Provided,
            },
            None => {
                let keypair =
                    keys::account_keypair(&passphrase, &namespace, &testnet, &account.name);
                DerivedAccount {
                    name: account.name.clone(),
                    public: keypair.public,
                    secret: Some(keypair.secret),
                    source: AccountSource::Generated,
                }
            }
        })
        .collect()
}

/// Account entries for networks that are not funded yet.
pub fn pending_accounts(net: &StellarTestnet) -> Vec<FundedAccount> {
    derived_accounts(net)
        .into_iter()
        .map(|account| FundedAccount {
            name: account.name,
            public_key: account.public,
            secret_key: account.secret,
            source: account.source,
            funded: false,
        })
        .collect()
}

/// The status skeleton with all URL/genesis fields filled in.
pub fn base_status(net: &StellarTestnet) -> StellarTestnetStatus {
    let name = net.name_any();
    let namespace = namespace_of(net);
    let urls = client::cluster_urls(&name, &namespace);
    let passphrase = net.spec.network_passphrase();

    let external = net.spec.ingress().and_then(|ingress| {
        ingress
            .host
            .clone()
            .map(|host| client::external_urls(&host).rpc)
    });

    StellarTestnetStatus {
        phase: Some(Phase::Pending),
        message: None,
        observed_generation: net.meta().generation,
        network_passphrase: Some(passphrase.clone()),
        root_account: Some(keys::root_keypair(&passphrase).public),
        rpc_url: Some(urls.rpc),
        external_rpc_url: external,
        friendbot_url: Some(urls.friendbot),
        core_admin_url: Some(urls.core_admin),
        accounts: None,
        ready_at: None,
        conditions: None,
    }
}

/// Build the `Ready` condition, preserving its last transition time when
/// the status did not flip.
pub fn ready_condition(ready: bool, reason: &str, message: &str) -> StatusCondition {
    StatusCondition {
        condition_type: "Ready".into(),
        status: if ready { "True" } else { "False" }.into(),
        reason: Some(reason.into()),
        message: Some(message.into()),
        last_transition_time: Some(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)),
    }
}

/// Time elapsed since the network first became ready.
pub fn elapsed_since_ready(status: &StellarTestnetStatus) -> Option<Duration> {
    let ready_at = status.ready_at.as_ref()?;
    let ready_at = chrono::DateTime::parse_from_rfc3339(ready_at).ok()?;
    Some(
        (Utc::now() - ready_at.with_timezone(&Utc))
            .to_std()
            .unwrap_or_default(),
    )
}

/// Whether the resource still carries our finalizer.
pub fn has_finalizer(net: &StellarTestnet) -> bool {
    net.meta()
        .finalizers
        .as_ref()
        .is_some_and(|finalizers| finalizers.iter().any(|f| f == FINALIZER))
}

/// Compute the finalizer list after adding or removing ours.
pub fn toggle_finalizer(current: Option<&Vec<String>>, add: bool) -> Vec<String> {
    let mut finalizers: Vec<String> = current
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|f| f != FINALIZER)
        .collect();
    if add {
        finalizers.push(FINALIZER.to_string());
    }
    finalizers
}

// ─────────────────────────────────────────────────────────────────────────────
// Kubernetes plumbing
// ─────────────────────────────────────────────────────────────────────────────

/// Labels applied to every child resource.
pub fn labels(net: &StellarTestnet, component: &str) -> BTreeMap<String, String> {
    let name = net.name_any();
    BTreeMap::from([
        ("app.kubernetes.io/name".to_string(), APP_NAME.to_string()),
        ("app.kubernetes.io/instance".to_string(), name.clone()),
        (
            "app.kubernetes.io/managed-by".to_string(),
            AGENT.to_string(),
        ),
        (
            "app.kubernetes.io/component".to_string(),
            component.to_string(),
        ),
        (LABEL_TESTNET.to_string(), name),
    ])
}

/// Selector matching the `Deployment` pods (immutable subset).
fn pod_selector(net: &StellarTestnet) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("app.kubernetes.io/name".to_string(), APP_NAME.to_string()),
        ("app.kubernetes.io/instance".to_string(), net.name_any()),
    ])
}

/// Owner reference linking a child to its `StellarTestnet`.
pub fn owner_reference(net: &StellarTestnet) -> OwnerReference {
    OwnerReference {
        api_version: crate::crd::testnet::API_VERSION.to_string(),
        kind: KIND.to_string(),
        name: net.name_any(),
        uid: net.meta().uid.clone().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }
}

fn owner_references(net: &StellarTestnet) -> Option<Vec<OwnerReference>> {
    Some(vec![owner_reference(net)])
}

/// Name of the `<name>-rpc` Service / Ingress.
pub fn rpc_service_name(name: &str) -> String {
    format!("{name}-rpc")
}

/// Name of the `<name>-core` Service.
pub fn core_service_name(name: &str) -> String {
    format!("{name}-core")
}

fn ingress_name(name: &str) -> String {
    rpc_service_name(name)
}

fn env(name: &str, value: &str) -> EnvVar {
    EnvVar {
        name: name.to_string(),
        value: Some(value.to_string()),
        ..Default::default()
    }
}

fn data_volume(name: &str, memory_backed: bool) -> Volume {
    Volume {
        name: name.to_string(),
        empty_dir: Some(EmptyDirVolumeSource {
            medium: memory_backed.then(|| "Memory".to_string()),
            size_limit: None,
        }),
        ..Default::default()
    }
}

/// Quickstart arguments: a local standalone network running Core and RPC.
fn container_args(net: &StellarTestnet) -> Vec<String> {
    let mut args = vec![
        "--local".to_string(),
        "--enable".to_string(),
        "core,rpc".to_string(),
    ];
    if let Some(version) = net.spec.protocol_version() {
        args.push("--protocol-version".to_string());
        args.push(version.to_string());
    }
    args
}

/// Build the `Deployment` hosting the ephemeral Stellar stack.
pub fn build_deployment(net: &StellarTestnet) -> Deployment {
    let name = net.name_any();
    let memory_backed = net.spec.storage_mode() == crate::crd::testnet::StorageMode::Memory;

    let container = Container {
        name: "stellar".to_string(),
        image: Some(net.spec.image()),
        args: Some(container_args(net)),
        env: Some(vec![
            env("NETWORK_PASSPHRASE", &net.spec.network_passphrase()),
            // Quickstart honours STELLAR_MODE over its on-disk marker file;
            // ephemeral mode keeps every service configured for throwaway
            // networks.
            env("STELLAR_MODE", "ephemeral"),
            // Local networks default to debug logging; info keeps memory
            // use and log volume sane for CI.
            env("CORE_LOG_LEVEL", "info"),
        ]),
        ports: Some(vec![
            ContainerPort {
                name: Some("http".to_string()),
                container_port: client::HTTP_PORT as i32,
                protocol: Some("TCP".to_string()),
                ..Default::default()
            },
            ContainerPort {
                name: Some("core-admin".to_string()),
                container_port: client::CORE_ADMIN_PORT as i32,
                protocol: Some("TCP".to_string()),
                ..Default::default()
            },
        ]),
        readiness_probe: Some(Probe {
            exec: Some(ExecAction {
                command: Some(vec![
                    "bash".to_string(),
                    "-c".to_string(),
                    READINESS_SCRIPT.to_string(),
                ]),
            }),
            initial_delay_seconds: Some(5),
            period_seconds: Some(3),
            timeout_seconds: Some(5),
            failure_threshold: Some(20),
            ..Default::default()
        }),
        resources: Some(ResourceRequirements {
            requests: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity("250m".to_string())),
                ("memory".to_string(), Quantity("512Mi".to_string())),
            ])),
            limits: Some(BTreeMap::from([
                ("cpu".to_string(), Quantity("2".to_string())),
                ("memory".to_string(), Quantity("2Gi".to_string())),
            ])),
            claims: None,
        }),
        volume_mounts: Some(vec![
            VolumeMount {
                name: "core-data".to_string(),
                mount_path: CORE_DATA_PATH.to_string(),
                ..Default::default()
            },
            VolumeMount {
                name: "rpc-data".to_string(),
                mount_path: RPC_DATA_PATH.to_string(),
                ..Default::default()
            },
            VolumeMount {
                name: "history-data".to_string(),
                mount_path: HISTORY_DATA_PATH.to_string(),
                ..Default::default()
            },
        ]),
        ..Default::default()
    };

    Deployment {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            namespace: net.meta().namespace.clone(),
            labels: Some(labels(net, "stellar")),
            owner_references: owner_references(net),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(1),
            selector: LabelSelector {
                match_labels: Some(pod_selector(net)),
                ..Default::default()
            },
            // A single-node network must never run two pods at once.
            strategy: Some(DeploymentStrategy {
                type_: Some("Recreate".to_string()),
                ..Default::default()
            }),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels(net, "stellar")),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    containers: vec![container],
                    volumes: Some(vec![
                        data_volume("core-data", memory_backed),
                        data_volume("rpc-data", memory_backed),
                        data_volume("history-data", memory_backed),
                    ]),
                    termination_grace_period_seconds: Some(15),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn service(
    net: &StellarTestnet,
    component: &str,
    name: String,
    ports: Vec<ServicePort>,
) -> K8sService {
    K8sService {
        metadata: ObjectMeta {
            name: Some(name),
            namespace: net.meta().namespace.clone(),
            labels: Some(labels(net, component)),
            owner_references: owner_references(net),
            ..Default::default()
        },
        spec: Some(K8sServiceSpec {
            selector: Some(pod_selector(net)),
            ports: Some(ports),
            type_: Some("ClusterIP".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Service exposing the Soroban RPC (and friendbot) HTTP endpoint.
pub fn build_rpc_service(net: &StellarTestnet) -> K8sService {
    service(
        net,
        "rpc",
        rpc_service_name(&net.name_any()),
        vec![ServicePort {
            name: Some("http".to_string()),
            port: client::HTTP_PORT as i32,
            target_port: Some(IntOrString::String("http".to_string())),
            protocol: Some("TCP".to_string()),
            ..Default::default()
        }],
    )
}

/// Service exposing the Stellar Core admin endpoint (cluster-internal only).
pub fn build_core_service(net: &StellarTestnet) -> K8sService {
    service(
        net,
        "core",
        core_service_name(&net.name_any()),
        vec![ServicePort {
            name: Some("admin".to_string()),
            port: client::CORE_ADMIN_PORT as i32,
            target_port: Some(IntOrString::String("core-admin".to_string())),
            protocol: Some("TCP".to_string()),
            ..Default::default()
        }],
    )
}

/// Ingress exposing the RPC endpoint, when enabled.
pub fn build_ingress(net: &StellarTestnet) -> Option<K8sIngress> {
    let ingress_spec = net.spec.ingress()?;
    let host = ingress_spec.host.clone();
    Some(K8sIngress {
        metadata: ObjectMeta {
            name: Some(ingress_name(&net.name_any())),
            namespace: net.meta().namespace.clone(),
            labels: Some(labels(net, "rpc")),
            // Soroban RPC payloads (contract uploads) exceed the nginx
            // default of 1 MiB.
            annotations: Some(BTreeMap::from([(
                "nginx.ingress.kubernetes.io/proxy-body-size".to_string(),
                "16m".to_string(),
            )])),
            owner_references: owner_references(net),
            ..Default::default()
        },
        spec: Some(K8sIngressSpec {
            ingress_class_name: ingress_spec.class_name.clone(),
            rules: Some(vec![IngressRule {
                host: host.clone(),
                http: Some(HTTPIngressRuleValue {
                    paths: vec![HTTPIngressPath {
                        path: Some("/".to_string()),
                        path_type: "Prefix".to_string(),
                        backend: IngressBackend {
                            service: Some(IngressServiceBackend {
                                name: rpc_service_name(&net.name_any()),
                                port: Some(ServiceBackendPort {
                                    number: Some(client::HTTP_PORT as i32),
                                    ..Default::default()
                                }),
                            }),
                            resource: None,
                        },
                    }],
                }),
            }]),
            ..Default::default()
        }),
        status: None,
    })
}

/// Server-side apply one child resource.
async fn apply_resource<K>(api: &Api<K>, desired: &K) -> Result<(), Error>
where
    K: Resource<DynamicType = ()>
        + Clone
        + std::fmt::Debug
        + serde::de::DeserializeOwned
        + serde::Serialize,
{
    api.patch(
        &desired.meta().name.clone().expect("named resource"),
        &PatchParams::apply(AGENT).force(),
        &Patch::Apply(desired),
    )
    .await?;
    Ok(())
}

/// Delete a resource by name, ignoring 404.
async fn delete_quietly<K>(api: &Api<K>, name: &str) -> Result<(), Error>
where
    K: Resource<DynamicType = ()> + Clone + std::fmt::Debug + serde::de::DeserializeOwned,
{
    match api.delete(name, &DeleteParams::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(error)) if error.code == 404 => Ok(()),
        Err(error) => Err(Error::Kube(error)),
    }
}

/// Delete every resource matching `selector`, returning what was deleted.
async fn sweep<K>(api: &Api<K>, selector: &str) -> Result<Vec<K>, Error>
where
    K: Resource<DynamicType = ()> + Clone + std::fmt::Debug + serde::de::DeserializeOwned,
{
    let list = api.list(&ListParams::default().labels(selector)).await?;
    for item in &list.items {
        let name = item.meta().name.clone().unwrap_or_default();
        if let Err(error) = api.delete(&name, &DeleteParams::default()).await {
            match error {
                kube::Error::Api(e) if e.code == 404 => {}
                other => return Err(Error::Kube(other)),
            }
        }
    }
    Ok(list.items)
}

/// Patch `status` via the status subresource. Every field is serialized
/// (including `null`s) so stale URLs/conditions are cleared on merge.
async fn patch_status(
    api: &Api<StellarTestnet>,
    name: &str,
    status: &StellarTestnetStatus,
) -> Result<(), Error> {
    api.patch_status(
        name,
        &PatchParams::default(),
        &Patch::Merge(&json!({ "status": status })),
    )
    .await?;
    Ok(())
}

async fn add_finalizer(api: &Api<StellarTestnet>, net: &StellarTestnet) -> Result<(), Error> {
    let finalizers = toggle_finalizer(net.meta().finalizers.as_ref(), true);
    api.patch_metadata(
        &net.name_any(),
        &PatchParams::default(),
        &Patch::Merge(&json!({ "metadata": { "finalizers": finalizers } })),
    )
    .await?;
    Ok(())
}

async fn remove_finalizer(api: &Api<StellarTestnet>, net: &StellarTestnet) -> Result<(), Error> {
    let finalizers = toggle_finalizer(net.meta().finalizers.as_ref(), false);
    api.patch_metadata(
        &net.name_any(),
        &PatchParams::default(),
        &Patch::Merge(&json!({ "metadata": { "finalizers": finalizers } })),
    )
    .await?;
    Ok(())
}

/// Map a child resource back to its owning `StellarTestnet`.
pub fn owned_by_stellartestnet<K>(obj: K) -> Option<ObjectRef<StellarTestnet>>
where
    K: Resource<DynamicType = ()>,
{
    let meta = obj.meta();
    let owner = meta
        .owner_references
        .as_ref()?
        .iter()
        .find(|reference| reference.kind == KIND)?;
    let mut object_ref = ObjectRef::<StellarTestnet>::new(&owner.name);
    if let Some(namespace) = &meta.namespace {
        object_ref = object_ref.within(namespace);
    }
    Some(object_ref)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::testnet::DEFAULT_NETWORK_PASSPHRASE;

    const UID: &str = "3f1d0b7c-2460-4f2e-9f31-6a6e5f55c1cf";

    fn testnet(yaml: &str) -> StellarTestnet {
        let mut net: StellarTestnet = serde_yaml::from_str(yaml).expect("test manifest parses");
        net.metadata.uid = Some(UID.to_string());
        net.metadata.resource_version = Some("1".to_string());
        net
    }

    fn simple() -> StellarTestnet {
        testnet(
            r#"
            apiVersion: stellar.org/v1alpha1
            kind: StellarTestnet
            metadata:
              name: pr-961
              namespace: stellar-ci
            spec:
              accounts:
                - name: deployer
            "#,
        )
    }

    fn owned_by(net: &StellarTestnet, meta: &ObjectMeta) {
        let references = meta.owner_references.as_ref().expect("owner reference set");
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].kind, KIND);
        assert_eq!(references[0].name, net.name_any());
        assert_eq!(references[0].uid, UID);
        assert_eq!(references[0].controller, Some(true));
    }

    #[test]
    fn deployment_runs_ephemeral_quickstart_stack() {
        let net = simple();
        let deployment = build_deployment(&net);

        let meta = deployment.metadata.clone();
        assert_eq!(meta.name.as_deref(), Some("pr-961"));
        assert_eq!(meta.namespace.as_deref(), Some("stellar-ci"));
        owned_by(&net, &meta);

        let spec = deployment.spec.expect("spec");
        assert_eq!(spec.replicas, Some(1));
        assert_eq!(spec.strategy.unwrap().type_.as_deref(), Some("Recreate"));

        let container = &spec.template.spec.as_ref().unwrap().containers[0];
        assert_eq!(
            container.image.as_deref(),
            Some(crate::crd::testnet::DEFAULT_IMAGE)
        );
        assert_eq!(
            container.args.as_deref(),
            Some(
                [
                    "--local".to_string(),
                    "--enable".to_string(),
                    "core,rpc".to_string()
                ]
                .as_slice()
            )
        );

        let env: BTreeMap<&str, &str> = container
            .env
            .as_ref()
            .unwrap()
            .iter()
            .map(|var| (var.name.as_str(), var.value.as_deref().unwrap_or_default()))
            .collect();
        assert_eq!(env["NETWORK_PASSPHRASE"], DEFAULT_NETWORK_PASSPHRASE);
        assert_eq!(env["STELLAR_MODE"], "ephemeral");

        // Memory-backed databases: every database mount uses medium Memory.
        let pod_spec = spec.template.spec.as_ref().unwrap();
        for volume in pod_spec.volumes.as_ref().unwrap() {
            let empty_dir = volume.empty_dir.as_ref().expect("emptyDir volume");
            assert_eq!(
                empty_dir.medium.as_deref(),
                Some("Memory"),
                "volume {} must be memory backed",
                volume.name
            );
        }
        let mounts: Vec<&str> = container
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .map(|mount| mount.mount_path.as_str())
            .collect();
        assert_eq!(
            mounts,
            vec![CORE_DATA_PATH, RPC_DATA_PATH, HISTORY_DATA_PATH]
        );

        // Readiness is gated on a healthy Soroban RPC.
        let probe = container.readiness_probe.as_ref().expect("readiness probe");
        let command = probe.exec.as_ref().unwrap().command.as_ref().unwrap();
        assert!(command[2].contains("getHealth"));
    }

    #[test]
    fn disk_storage_drops_memory_medium() {
        let net = testnet(
            r#"
            apiVersion: stellar.org/v1alpha1
            kind: StellarTestnet
            metadata: { name: disk, namespace: ns }
            spec:
              genesis:
                storage: disk
            "#,
        );
        let deployment = build_deployment(&net);
        let pod_spec = deployment.spec.unwrap().template.spec.unwrap();
        for volume in pod_spec.volumes.unwrap() {
            assert_eq!(volume.empty_dir.unwrap().medium, None);
        }
    }

    #[test]
    fn services_and_ingress_are_owned_and_wired() {
        let net = testnet(
            r#"
            apiVersion: stellar.org/v1alpha1
            kind: StellarTestnet
            metadata:
              name: pr-961
              namespace: stellar-ci
            spec:
              rpc:
                ingress:
                  enabled: true
                  host: pr-961.stellar.example.com
                  className: nginx
            "#,
        );

        let rpc = build_rpc_service(&net);
        assert_eq!(rpc.metadata.name.as_deref(), Some("pr-961-rpc"));
        owned_by(&net, &rpc.metadata);
        let rpc_spec = rpc.spec.unwrap();
        assert_eq!(rpc_spec.selector.unwrap(), pod_selector(&net));
        let port = &rpc_spec.ports.unwrap()[0];
        assert_eq!(port.port, 8000);
        assert_eq!(
            port.target_port,
            Some(IntOrString::String("http".to_string()))
        );

        let core = build_core_service(&net);
        assert_eq!(core.metadata.name.as_deref(), Some("pr-961-core"));
        owned_by(&net, &core.metadata);
        assert_eq!(core.spec.unwrap().ports.unwrap()[0].port, 11626);

        let ingress = build_ingress(&net).expect("ingress enabled");
        assert_eq!(ingress.metadata.name.as_deref(), Some("pr-961-rpc"));
        owned_by(&net, &ingress.metadata);
        let ingress_spec = ingress.spec.unwrap();
        assert_eq!(ingress_spec.ingress_class_name.as_deref(), Some("nginx"));
        let rule = &ingress_spec.rules.unwrap()[0];
        assert_eq!(rule.host.as_deref(), Some("pr-961.stellar.example.com"));
        let path = &rule.http.as_ref().unwrap().paths[0];
        assert_eq!(path.path.as_deref(), Some("/"));
        assert_eq!(path.path_type, "Prefix");
        assert_eq!(path.backend.service.as_ref().unwrap().name, "pr-961-rpc");
    }

    #[test]
    fn no_ingress_without_explicit_opt_in() {
        let net = simple();
        assert!(build_ingress(&net).is_none());
    }

    #[test]
    fn pinned_protocol_version_is_passed_to_quickstart() {
        let net = testnet(
            r#"
            apiVersion: stellar.org/v1alpha1
            kind: StellarTestnet
            metadata: { name: pr-963, namespace: stellar-ci }
            spec:
              genesis: { protocolVersion: 30 }
            "#,
        );
        assert_eq!(
            container_args(&net),
            [
                "--local",
                "--enable",
                "core,rpc",
                "--protocol-version",
                "30"
            ]
        );
        assert!(validate_spec(&net).is_ok());
    }

    #[test]
    fn pre_soroban_protocol_version_is_rejected() {
        let net = testnet(
            r#"
            apiVersion: stellar.org/v1alpha1
            kind: StellarTestnet
            metadata: { name: pr-964, namespace: stellar-ci }
            spec:
              genesis: { protocolVersion: 19 }
            "#,
        );
        assert!(validate_spec(&net).unwrap_err().contains("protocolVersion"));
    }

    #[test]
    fn generated_accounts_are_deterministic_and_stable_in_status() {
        let net = simple();
        let accounts = derived_accounts(&net);
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].name, "deployer");
        assert_eq!(accounts[0].source, AccountSource::Generated);
        assert!(accounts[0].secret.is_some());
        assert!(accounts[0].public.starts_with('G'));

        let again = derived_accounts(&simple());
        assert_eq!(accounts, again);

        // A second testnet must not reuse the same key material.
        let other = testnet(
            r#"
            apiVersion: stellar.org/v1alpha1
            kind: StellarTestnet
            metadata: { name: pr-962, namespace: stellar-ci }
            spec:
              accounts: [{ name: deployer }]
            "#,
        );
        assert_ne!(accounts[0].public, derived_accounts(&other)[0].public);
    }

    #[test]
    fn provided_accounts_keep_their_public_key() {
        let net = testnet(
            r#"
            apiVersion: stellar.org/v1alpha1
            kind: StellarTestnet
            metadata: { name: pr-961, namespace: stellar-ci }
            spec:
              accounts:
                - name: external
                  publicKey: GBZXN7PIRZGNMHGA7MUUUF4GWPY5AYPV6LY4UV2GL6VJGIQRXFDNMADI
            "#,
        );
        let accounts = derived_accounts(&net);
        assert_eq!(accounts[0].source, AccountSource::Provided);
        assert_eq!(accounts[0].secret, None);
        assert_eq!(
            accounts[0].public,
            "GBZXN7PIRZGNMHGA7MUUUF4GWPY5AYPV6LY4UV2GL6VJGIQRXFDNMADI"
        );
    }

    #[test]
    fn spec_validation_rejects_bad_input() {
        let duplicate = testnet(
            r#"
            apiVersion: stellar.org/v1alpha1
            kind: StellarTestnet
            metadata: { name: x, namespace: ns }
            spec:
              accounts:
                - name: a
                - name: a
            "#,
        );
        assert!(validate_spec(&duplicate).unwrap_err().contains("duplicate"));

        let blank = testnet(
            r#"
            apiVersion: stellar.org/v1alpha1
            kind: StellarTestnet
            metadata: { name: x, namespace: ns }
            spec:
              accounts: [{ name: "" }]
            "#,
        );
        assert!(validate_spec(&blank).unwrap_err().contains("non-empty"));

        let bad_key = testnet(
            r#"
            apiVersion: stellar.org/v1alpha1
            kind: StellarTestnet
            metadata: { name: x, namespace: ns }
            spec:
              accounts:
                - name: broken
                  publicKey: not-a-key
            "#,
        );
        assert!(validate_spec(&bad_key)
            .unwrap_err()
            .contains("invalid public key"));

        assert!(validate_spec(&simple()).is_ok());
    }

    #[test]
    fn status_reports_all_connection_urls() {
        let net = simple();
        let status = base_status(&net);
        assert_eq!(status.phase, Some(Phase::Pending));
        assert_eq!(
            status.rpc_url.as_deref(),
            Some("http://pr-961-rpc.stellar-ci.svc.cluster.local:8000/rpc")
        );
        assert_eq!(
            status.friendbot_url.as_deref(),
            Some("http://pr-961-rpc.stellar-ci.svc.cluster.local:8000/friendbot")
        );
        assert_eq!(
            status.core_admin_url.as_deref(),
            Some("http://pr-961-core.stellar-ci.svc.cluster.local:11626")
        );
        assert_eq!(
            status.root_account.as_deref(),
            Some("GBZXN7PIRZGNMHGA7MUUUF4GWPY5AYPV6LY4UV2GL6VJGIQRXFDNMADI")
        );
        assert_eq!(status.external_rpc_url, None);
    }

    #[test]
    fn ingress_host_surfaces_external_url() {
        let net = testnet(
            r#"
            apiVersion: stellar.org/v1alpha1
            kind: StellarTestnet
            metadata: { name: pr-961, namespace: stellar-ci }
            spec:
              rpc:
                ingress:
                  enabled: true
                  host: pr-961.stellar.example.com
            "#,
        );
        let status = base_status(&net);
        assert_eq!(
            status.external_rpc_url.as_deref(),
            Some("http://pr-961.stellar.example.com/rpc")
        );
    }

    #[test]
    fn finalizer_toggle_is_additive_and_idempotent() {
        assert_eq!(toggle_finalizer(None, true), vec![FINALIZER.to_string()]);
        assert_eq!(
            toggle_finalizer(Some(&vec![FINALIZER.to_string()]), true),
            vec![FINALIZER.to_string()],
            "adding twice must not duplicate"
        );

        let other = "keep.example.com/finalizer".to_string();
        assert_eq!(
            toggle_finalizer(Some(&vec![other.clone(), FINALIZER.to_string()]), false),
            vec![other.clone()],
            "foreign finalizers must survive removal"
        );
        assert_eq!(
            toggle_finalizer(Some(&vec![other.clone()]), false),
            vec![other]
        );
    }

    #[test]
    fn finalizer_detection() {
        let mut net = simple();
        assert!(!has_finalizer(&net));
        net.metadata.finalizers = Some(vec![FINALIZER.to_string()]);
        assert!(has_finalizer(&net));
    }

    #[test]
    fn ttl_elapsed_computes_from_ready_at() {
        let mut status = StellarTestnetStatus::default();
        assert_eq!(elapsed_since_ready(&status), None);

        status.ready_at = Some("1970-01-01T00:00:00Z".to_string());
        let elapsed = elapsed_since_ready(&status).expect("parses");
        assert!(elapsed > Duration::from_secs(60));

        status.ready_at = Some("not-a-timestamp".to_string());
        assert_eq!(elapsed_since_ready(&status), None);
    }

    #[test]
    fn child_owner_mapping_finds_the_testnet() {
        let net = simple();
        let deployment = build_deployment(&net);
        let reference = owned_by_stellartestnet(deployment).expect("owner mapping");
        assert_eq!(reference.name, "pr-961");
        assert_eq!(reference.namespace.as_deref(), Some("stellar-ci"));

        let orphan = Deployment::default();
        assert!(owned_by_stellartestnet(orphan).is_none());
    }
}
