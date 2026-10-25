use k8s_openapi::{
    api::{core::v1::Pod, policy::v1::PodDisruptionBudget},
    apimachinery::pkg::{apis::meta::v1::ObjectMeta, util::intstr::IntOrString},
};
use kube::{
    api::{Api, ListParams, Patch, PatchParams, PostParams},
    Client,
};
use serde_json::json;
use std::{collections::BTreeMap, env, time::Duration};
use tracing::{debug, error, info, warn};

const STELLAR_LABEL: &str = "app=stellar";
const PDB_NAME: &str = "stellar-pdb";
const DEFAULT_RECONCILE_INTERVAL_SECS: u64 = 30;

pub struct PdbReconciler {
    client: Client,
    namespace: String,
}

impl PdbReconciler {
    pub fn new(client: Client) -> Self {
        let namespace = env::var("NAMESPACE").unwrap_or_else(|_| "default".to_string());
        Self { client, namespace }
    }

    pub async fn run(&self) {
        let interval = env::var("RECONCILE_INTERVAL_SECS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_RECONCILE_INTERVAL_SECS);

        let mut ticker = tokio::time::interval(Duration::from_secs(interval));
        loop {
            ticker.tick().await;
            info!("Reconciling PDB");
            if let Err(err) = self.reconcile().await {
                error!("Reconcile failed: {:?}", err);
            }
        }
    }

    async fn reconcile(&self) -> Result<(), kube::Error> {
        debug!("Listing Stellar pods with label {}", STELLAR_LABEL);
        let pods_api: Api<Pod> = Api::all(self.client.clone());
        let lp = ListParams::default().labels(STELLAR_LABEL);
        let pods = pods_api.list(&lp).await?;

        let mut any_syncing = false;
        let mut ready_count = 0;

        for pod in pods.items {
            let annotations = pod
                .metadata
                .annotations
                .as_ref()
                .unwrap_or(&BTreeMap::new());
            if annotations.get("stellar-sync-status").map(|v| v.as_str()) == Some("syncing") {
                any_syncing = true;
                warn!(
                    "Pod {} is currently syncing; PDB will block evictions",
                    pod.metadata.name.as_deref().unwrap_or("<unknown>")
                );
            }

            if let Some(status) = pod.status {
                if status.phase.as_deref() == Some("Running") {
                    if let Some(conditions) = status.conditions {
                        let ready = conditions.iter().any(|condition| {
                            condition.type_ == "Ready" && condition.status == "True"
                        });
                        if ready {
                            ready_count += 1;
                        }
                    } else {
                        ready_count += 1;
                    }
                }
            }
        }

        let desired_max_unavailable = if any_syncing {
            0
        } else if ready_count >= 2 {
            1
        } else {
            // TODO(exempt: quorum-aware PDB calculation deferred to follow-up): Implement full quorum calculation. For now, allow at
            // most one unreplicated pod to be removed when we have at least 2 ready.
            if ready_count >= 2 {
                1
            } else {
                0
            }
        };

        debug!(
            "Computed desired maxUnavailable: {} (sync: {}, ready: {})",
            desired_max_unavailable, any_syncing, ready_count
        );

        let pdb_api: Api<PodDisruptionBudget> =
            Api::namespaced(self.client.clone(), &self.namespace);
        let selector = k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector {
            match_labels: Some(BTreeMap::from([("app".to_string(), "stellar".to_string())])),
            ..Default::default()
        };

        match pdb_api.get(PDB_NAME).await {
            Ok(_pdb) => {
                info!(
                    "Updating existing PDB {} with maxUnavailable={}",
                    PDB_NAME, desired_max_unavailable
                );
                let patch = json!({
                    "spec": {
                        "maxUnavailable": desired_max_unavailable
                    }
                });
                let pp = PatchParams::apply("pdb-auto-tuner").force();
                let apply = Patch::Apply(&patch);
                pdb_api.patch(PDB_NAME, &pp, &apply).await?;
            }
            Err(kube::Error::Api(api_err)) if api_err.code == 404 => {
                info!(
                    "Creating PDB {} with maxUnavailable={}",
                    PDB_NAME, desired_max_unavailable
                );
                let pdb = PodDisruptionBudget {
                    metadata: ObjectMeta {
                        name: Some(PDB_NAME.to_string()),
                        namespace: Some(self.namespace.clone()),
                        ..Default::default()
                    },
                    spec: Some(k8s_openapi::api::policy::v1::PodDisruptionBudgetSpec {
                        min_available: None,
                        max_unavailable: Some(IntOrString::Int(desired_max_unavailable as i32)),
                        selector: Some(selector),
                    }),
                    status: None,
                };
                pdb_api.create(&PostParams::default(), &pdb).await?;
            }
            Err(err) => {
                warn!("Failed to fetch PDB {}: {:?}", PDB_NAME, err);
                return Err(err);
            }
        }

        Ok(())
    }
}
