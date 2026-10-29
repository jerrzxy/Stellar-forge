// Copyright 2024 Stellar-K8s Contributors
// SPDX-License-Identifier: Apache-2.0
//! [`PdbCluster`] backed by a real Kubernetes API server (feature
//! `kube-backend`).
//!
//! * Reads `StellarNode` custom resources (`stellar.org/v1alpha1`) as untyped
//!   JSON, so this crate does not depend on the operator's CRD types.
//! * Writes `policy/v1` PodDisruptionBudgets with server-side apply.
//! * [`watch_stellar_nodes`] fires the controller's trigger whenever a
//!   `StellarNode` changes, so PDBs are tightened in real time.

use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use kube::api::{
    Api, ApiResource, DeleteParams, DynamicObject, GroupVersionKind, ListParams, Patch, PatchParams,
};
use kube::runtime::{watcher, WatchStreamExt};
use kube::Client;
use serde_json::Value;
use tokio::sync::Notify;
use tracing::warn;

use super::observation::{group_validators, observe_stellar_node, ValidatorGroup};
use super::reconciler::{
    ManagedPdb, PdbCluster, PdbError, FIELD_MANAGER, MANAGED_BY_LABEL, MANAGED_BY_VALUE,
    REASON_ANNOTATION,
};

fn stellar_node_resource() -> ApiResource {
    ApiResource::from_gvk_with_plural(
        &GroupVersionKind::gvk("stellar.org", "v1alpha1", "StellarNode"),
        "stellarnodes",
    )
}

/// A live Kubernetes cluster.
#[derive(Clone)]
pub struct KubeCluster {
    client: Client,
}

impl KubeCluster {
    /// Wrap a client.
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    /// Build a client from the in-cluster or local kubeconfig environment.
    pub async fn try_default() -> Result<Self, PdbError> {
        Client::try_default()
            .await
            .map(Self::new)
            .map_err(PdbError::cluster)
    }

    fn nodes(&self) -> Api<DynamicObject> {
        Api::all_with(self.client.clone(), &stellar_node_resource())
    }
}

impl PdbCluster for KubeCluster {
    async fn observe_groups(&self) -> Result<Vec<ValidatorGroup>, PdbError> {
        let list = self
            .nodes()
            .list(&ListParams::default())
            .await
            .map_err(PdbError::cluster)?;
        let now = chrono::Utc::now();
        let observed = list
            .items
            .iter()
            .filter_map(|o| serde_json::to_value(o).ok())
            .filter_map(|v| observe_stellar_node(&v, now))
            .collect();
        Ok(group_validators(observed))
    }

    async fn list_managed_pdbs(&self) -> Result<Vec<ManagedPdb>, PdbError> {
        let api: Api<PodDisruptionBudget> = Api::all(self.client.clone());
        let lp = ListParams::default().labels(&format!("{MANAGED_BY_LABEL}={MANAGED_BY_VALUE}"));
        let list = api.list(&lp).await.map_err(PdbError::cluster)?;
        Ok(list
            .items
            .into_iter()
            .filter_map(|p| {
                let spec = serde_json::to_value(p.spec.as_ref()?).ok()?;
                Some(ManagedPdb {
                    namespace: p.metadata.namespace.clone()?,
                    name: p.metadata.name.clone()?,
                    reason: p
                        .metadata
                        .annotations
                        .as_ref()
                        .and_then(|a| a.get(REASON_ANNOTATION).cloned()),
                    spec,
                })
            })
            .collect())
    }

    async fn apply_pdb(&self, manifest: &Value) -> Result<(), PdbError> {
        let ns = manifest["metadata"]["namespace"]
            .as_str()
            .ok_or_else(|| PdbError::cluster("manifest has no namespace"))?;
        let name = manifest["metadata"]["name"]
            .as_str()
            .ok_or_else(|| PdbError::cluster("manifest has no name"))?;
        let api: Api<PodDisruptionBudget> = Api::namespaced(self.client.clone(), ns);
        api.patch(
            name,
            &PatchParams::apply(FIELD_MANAGER).force(),
            &Patch::Apply(manifest),
        )
        .await
        .map(|_| ())
        .map_err(PdbError::cluster)
    }

    async fn delete_pdb(&self, namespace: &str, name: &str) -> Result<(), PdbError> {
        let api: Api<PodDisruptionBudget> = Api::namespaced(self.client.clone(), namespace);
        match api.delete(name, &DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(()),
            Err(e) => Err(PdbError::cluster(e)),
        }
    }
}

/// Watch `StellarNode`s and call `trigger.notify_one()` on every change.
/// Runs until the stream ends; watcher errors are retried with backoff.
pub async fn watch_stellar_nodes(client: Client, trigger: Arc<Notify>) {
    let api: Api<DynamicObject> = Api::all_with(client, &stellar_node_resource());
    let mut events = watcher(api, watcher::Config::default())
        .default_backoff()
        .boxed();
    while let Some(ev) = events.next().await {
        match ev {
            Ok(_) => trigger.notify_one(),
            Err(e) => warn!("StellarNode watch error: {e}"),
        }
    }
}
