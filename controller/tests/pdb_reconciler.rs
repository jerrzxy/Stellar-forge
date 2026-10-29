// Copyright 2024 Stellar-K8s Contributors
// SPDX-License-Identifier: Apache-2.0
//! Behavioural tests for the PDB controller against an in-memory cluster.
//!
//! The fake cluster also models the Kubernetes **eviction API**: an eviction
//! (what `kubectl drain` performs) is refused when it would exceed the PDB's
//! `maxUnavailable`, exactly like the real API server (HTTP 429).

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use controller::pdb::{
    ManagedPdb, PdbCluster, PdbController, PdbError, PdbPolicy, SyncState, ValidatorGroup,
    ValidatorObservation,
};
use serde_json::Value;

#[derive(Default)]
struct State {
    /// validator id -> (sync state, stable for)
    validators: BTreeMap<String, (SyncState, Duration)>,
    pdbs: BTreeMap<(String, String), Value>,
    /// Foreign (unmanaged) PDBs must never be touched.
    foreign: Vec<(String, String)>,
    fail_observe: bool,
    fail_apply: bool,
    writes: usize,
}

#[derive(Default)]
struct FakeCluster(Mutex<State>);

impl FakeCluster {
    fn set(&self, id: &str, s: SyncState, stable: u64) {
        self.0
            .lock()
            .unwrap()
            .validators
            .insert(id.into(), (s, Duration::from_secs(stable)));
    }
    fn max_unavailable(&self) -> Option<u64> {
        let st = self.0.lock().unwrap();
        st.pdbs
            .values()
            .next()
            .and_then(|m| m["spec"]["maxUnavailable"].as_u64())
    }
    fn writes(&self) -> usize {
        self.0.lock().unwrap().writes
    }

    /// Simulates `kubectl drain` evicting `n` validator pods at once, on top
    /// of `already_down` pods that are already unavailable.
    fn try_evict(&self, already_down: u64, n: u64) -> Result<(), &'static str> {
        match self.max_unavailable() {
            Some(max) if already_down + n > max => Err("429 Cannot evict pod: violates PDB"),
            _ => Ok(()),
        }
    }
}

impl PdbCluster for FakeCluster {
    async fn observe_groups(&self) -> Result<Vec<ValidatorGroup>, PdbError> {
        let st = self.0.lock().unwrap();
        if st.fail_observe {
            return Err(PdbError::cluster("api server unreachable"));
        }
        if st.validators.is_empty() {
            return Ok(vec![]);
        }
        Ok(vec![ValidatorGroup {
            namespace: "stellar".into(),
            network_key: "testnet".into(),
            members: st
                .validators
                .iter()
                .map(|(id, (s, d))| ValidatorObservation::new(id.clone(), *s, *d))
                .collect(),
            min_quorum: None,
        }])
    }

    async fn list_managed_pdbs(&self) -> Result<Vec<ManagedPdb>, PdbError> {
        let st = self.0.lock().unwrap();
        Ok(st
            .pdbs
            .iter()
            .map(|((ns, name), m)| ManagedPdb {
                namespace: ns.clone(),
                name: name.clone(),
                spec: m["spec"].clone(),
                reason: m["metadata"]["annotations"]["stellar.org/pdb-reason"]
                    .as_str()
                    .map(str::to_string),
            })
            .collect())
    }

    async fn apply_pdb(&self, manifest: &Value) -> Result<(), PdbError> {
        let mut st = self.0.lock().unwrap();
        if st.fail_apply {
            return Err(PdbError::cluster("forbidden"));
        }
        let key = (
            manifest["metadata"]["namespace"]
                .as_str()
                .unwrap()
                .to_string(),
            manifest["metadata"]["name"].as_str().unwrap().to_string(),
        );
        st.pdbs.insert(key, manifest.clone());
        st.writes += 1;
        Ok(())
    }

    async fn delete_pdb(&self, namespace: &str, name: &str) -> Result<(), PdbError> {
        let mut st = self.0.lock().unwrap();
        assert!(
            !st.foreign
                .contains(&(namespace.to_string(), name.to_string())),
            "controller tried to delete a PDB it does not manage"
        );
        st.pdbs.remove(&(namespace.to_string(), name.to_string()));
        st.writes += 1;
        Ok(())
    }
}

fn controller_with(fake: FakeCluster) -> PdbController<FakeCluster> {
    PdbController::new(fake, PdbPolicy::default())
}

fn four_validators(fake: &FakeCluster) {
    for id in ["v1", "v2", "v3", "v4"] {
        fake.set(id, SyncState::Synced, 3600);
    }
}

#[tokio::test]
async fn drain_is_blocked_while_a_validator_is_syncing() {
    let fake = FakeCluster::default();
    four_validators(&fake);
    fake.set("v1", SyncState::CatchingUp, 0);
    let c = controller_with(fake);

    let report = c.reconcile_once().await.unwrap();
    assert_eq!(report.created, 1);
    assert_eq!(c.cluster().max_unavailable(), Some(0));
    // `kubectl drain` of the node running the syncing validator is refused,
    // and so is draining a synced peer (quorum depends on the syncing one).
    assert!(c.cluster().try_evict(0, 1).is_err());
}

#[tokio::test]
async fn drain_is_allowed_one_at_a_time_once_synced_and_stable() {
    let fake = FakeCluster::default();
    four_validators(&fake);
    let c = controller_with(fake);

    c.reconcile_once().await.unwrap();
    assert_eq!(c.cluster().max_unavailable(), Some(1));
    assert!(c.cluster().try_evict(0, 1).is_ok());
    assert!(
        c.cluster().try_evict(1, 1).is_err(),
        "second concurrent eviction refused"
    );
}

#[tokio::test]
async fn pdb_tightens_when_a_validator_starts_syncing_and_relaxes_after_stability() {
    let fake = FakeCluster::default();
    four_validators(&fake);
    let c = controller_with(fake);

    c.reconcile_once().await.unwrap();
    assert_eq!(c.cluster().max_unavailable(), Some(1));

    c.cluster().set("v2", SyncState::CatchingUp, 0);
    let r = c.reconcile_once().await.unwrap();
    assert_eq!(r.updated, 1);
    assert_eq!(c.cluster().max_unavailable(), Some(0));

    // Just caught up: still inside the stability window, stays blocked.
    c.cluster().set("v2", SyncState::Synced, 5);
    c.reconcile_once().await.unwrap();
    assert_eq!(c.cluster().max_unavailable(), Some(0));

    c.cluster().set("v2", SyncState::Synced, 600);
    c.reconcile_once().await.unwrap();
    assert_eq!(c.cluster().max_unavailable(), Some(1));
}

#[tokio::test]
async fn quorum_floor_is_never_relaxed_even_when_fully_synced() {
    let fake = FakeCluster::default();
    for id in ["a", "b", "c"] {
        fake.set(id, SyncState::Synced, 3600);
    }
    let c = controller_with(fake);
    c.reconcile_once().await.unwrap();
    assert_eq!(c.cluster().max_unavailable(), Some(0));
    assert!(c.cluster().try_evict(0, 1).is_err());
}

#[tokio::test]
async fn reconcile_is_idempotent() {
    let fake = FakeCluster::default();
    four_validators(&fake);
    let c = controller_with(fake);
    c.reconcile_once().await.unwrap();
    let writes = c.cluster().writes();
    let r = c.reconcile_once().await.unwrap();
    assert_eq!((r.created, r.updated, r.unchanged), (0, 0, 1));
    assert_eq!(c.cluster().writes(), writes, "no redundant writes");
}

#[tokio::test]
async fn unobservable_cluster_changes_nothing() {
    let fake = FakeCluster::default();
    four_validators(&fake);
    let c = controller_with(fake);
    c.reconcile_once().await.unwrap();
    let writes = c.cluster().writes();

    c.cluster().0.lock().unwrap().fail_observe = true;
    assert!(c.reconcile_once().await.is_err());
    assert_eq!(c.cluster().writes(), writes, "stale PDB kept, not deleted");
    assert_eq!(c.cluster().max_unavailable(), Some(1));
}

#[tokio::test]
async fn orphaned_managed_pdb_is_garbage_collected() {
    let fake = FakeCluster::default();
    four_validators(&fake);
    let c = controller_with(fake);
    c.reconcile_once().await.unwrap();

    c.cluster().0.lock().unwrap().validators.clear();
    let r = c.reconcile_once().await.unwrap();
    assert_eq!(r.deleted, 1);
    assert_eq!(c.cluster().max_unavailable(), None);
}

#[tokio::test]
async fn apply_failure_is_reported_not_fatal() {
    let fake = FakeCluster::default();
    four_validators(&fake);
    fake.0.lock().unwrap().fail_apply = true;
    let c = controller_with(fake);
    let r = c.reconcile_once().await.unwrap();
    assert_eq!(r.errors.len(), 1);
    assert_eq!(r.groups[0].outcome, "error");
    assert_eq!(r.created, 0);
}

#[tokio::test]
async fn run_loop_reacts_to_trigger_and_shuts_down() {
    let fake = FakeCluster::default();
    four_validators(&fake);
    let c = controller_with(fake);
    let trigger = c.trigger();
    let (tx, rx) = tokio::sync::watch::channel(false);

    let c = std::sync::Arc::new(c);
    let runner = {
        let c = c.clone();
        tokio::spawn(async move { c.run(Duration::from_secs(3600), rx).await })
    };

    wait_for(|| c.cluster().max_unavailable() == Some(1)).await;
    c.cluster().set("v1", SyncState::CatchingUp, 0);
    trigger.notify_one();
    wait_for(|| c.cluster().max_unavailable() == Some(0)).await;

    tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), runner)
        .await
        .expect("controller stops on shutdown")
        .unwrap();
}

#[tokio::test]
async fn spawn_and_shutdown_handle() {
    let fake = FakeCluster::default();
    four_validators(&fake);
    let handle = controller_with(fake).spawn(Duration::from_millis(20));
    tokio::time::sleep(Duration::from_millis(80)).await;
    tokio::time::timeout(Duration::from_secs(5), handle.shutdown())
        .await
        .expect("shutdown completes");
}

async fn wait_for(mut cond: impl FnMut() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached in time");
}
