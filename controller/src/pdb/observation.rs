// Copyright 2024 Stellar-K8s Contributors
// SPDX-License-Identifier: Apache-2.0
//! Turns `StellarNode` custom resources (as JSON) into validator health
//! observations and groups validators that share a quorum.
//!
//! Everything here is pure: it takes a [`serde_json::Value`] and a clock value,
//! so it is unit-testable without a cluster and independent of the `kube`
//! client types.
//!
//! # Mapping `StellarNode.status` → [`SyncState`]
//!
//! | status                                                                | state        |
//! |-----------------------------------------------------------------------|--------------|
//! | no `status`, deleting, `Failed`/`Degraded`/`Terminating`, not `Ready` | `Unknown`    |
//! | `Progressing=True`, or phase `Syncing`/`Creating`/`Pending`/`Remediating` | `CatchingUp` |
//! | `Ready=True`, not `Degraded`, not progressing                         | `Synced`     |
//!
//! For `Synced`, `stable_for` is the time since the `Ready` condition's
//! `lastTransitionTime`.
//!
//! # Groups
//!
//! Validators in the same namespace on the same network share a group (and so a
//! PDB). The `stellar.org/pdb-min-quorum` annotation on a `StellarNode`
//! overrides the number of healthy validators the group must keep; if members
//! disagree the highest (most conservative) value wins.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::calculator::{SyncState, ValidatorObservation};

/// Annotation overriding the minimum healthy validators a group must keep.
pub const MIN_QUORUM_ANNOTATION: &str = "stellar.org/pdb-min-quorum";

/// One validator `StellarNode`, reduced to what the PDB controller needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedValidator {
    /// Namespace of the `StellarNode`.
    pub namespace: String,
    /// Stable, DNS-1123-safe key for the node's network (`testnet`, `custom-1a2b3c4d`, ...).
    pub network_key: String,
    /// Value of [`MIN_QUORUM_ANNOTATION`], if set and valid.
    pub min_quorum: Option<usize>,
    /// Health observation (`id` is the `StellarNode` name).
    pub observation: ValidatorObservation,
}

/// Validators that share a namespace and network and therefore a PDB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatorGroup {
    /// Namespace.
    pub namespace: String,
    /// Network key, see [`ObservedValidator::network_key`].
    pub network_key: String,
    /// Members, sorted by id.
    pub members: Vec<ValidatorObservation>,
    /// Highest `stellar.org/pdb-min-quorum` among the members, if any.
    pub min_quorum: Option<usize>,
}

impl ValidatorGroup {
    /// Name of the `PodDisruptionBudget` that guards this group.
    pub fn pdb_name(&self) -> String {
        format!("stellar-validators-{}-pdb", self.network_key)
    }

    /// Member ids, sorted.
    pub fn member_ids(&self) -> Vec<String> {
        self.members.iter().map(|m| m.id.clone()).collect()
    }
}

/// Observe one `StellarNode`. Returns `None` for anything that is not an
/// active validator (other node types, suspended nodes, malformed objects).
pub fn observe_stellar_node(obj: &Value, now: DateTime<Utc>) -> Option<ObservedValidator> {
    let spec = obj.get("spec")?;
    if spec.get("nodeType").and_then(Value::as_str) != Some("Validator") {
        return None;
    }
    if spec.get("suspended").and_then(Value::as_bool) == Some(true) {
        return None;
    }

    let metadata = obj.get("metadata")?;
    let name = metadata.get("name").and_then(Value::as_str)?.to_string();
    let namespace = metadata
        .get("namespace")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_string();

    let min_quorum = metadata
        .get("annotations")
        .and_then(|a| a.get(MIN_QUORUM_ANNOTATION))
        .and_then(Value::as_str)
        .and_then(|v| v.trim().parse::<usize>().ok());

    let deleting = metadata
        .get("deletionTimestamp")
        .is_some_and(|v| !v.is_null());
    let (sync, stable_for) = if deleting {
        (SyncState::Unknown, Duration::ZERO)
    } else {
        derive_sync(obj.get("status"), now)
    };

    Some(ObservedValidator {
        namespace,
        network_key: network_key(spec.get("network")),
        min_quorum,
        observation: ValidatorObservation::new(name, sync, stable_for),
    })
}

/// Group observed validators by `(namespace, network)`, members sorted by id.
pub fn group_validators(observed: Vec<ObservedValidator>) -> Vec<ValidatorGroup> {
    let mut groups: BTreeMap<(String, String), ValidatorGroup> = BTreeMap::new();
    for v in observed {
        let group = groups
            .entry((v.namespace.clone(), v.network_key.clone()))
            .or_insert_with(|| ValidatorGroup {
                namespace: v.namespace.clone(),
                network_key: v.network_key.clone(),
                members: Vec::new(),
                min_quorum: None,
            });
        group.min_quorum = match (group.min_quorum, v.min_quorum) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        group.members.push(v.observation);
    }
    groups
        .into_values()
        .map(|mut g| {
            g.members.sort_by(|a, b| a.id.cmp(&b.id));
            g
        })
        .collect()
}

fn derive_sync(status: Option<&Value>, now: DateTime<Utc>) -> (SyncState, Duration) {
    let Some(status) = status else {
        return (SyncState::Unknown, Duration::ZERO);
    };

    let condition = |kind: &str| -> Option<&Value> {
        status
            .get("conditions")?
            .as_array()?
            .iter()
            .find(|c| c.get("type").and_then(Value::as_str) == Some(kind))
    };
    let is_true = |kind: &str| {
        condition(kind)
            .and_then(|c| c.get("status"))
            .and_then(Value::as_str)
            == Some("True")
    };

    let phase = status.get("phase").and_then(Value::as_str).unwrap_or("");

    if is_true("Progressing") || matches!(phase, "Syncing" | "Creating" | "Pending" | "Remediating")
    {
        return (SyncState::CatchingUp, Duration::ZERO);
    }

    let broken_phase = matches!(phase, "Failed" | "Degraded" | "Terminating");
    if is_true("Ready") && !is_true("Degraded") && !broken_phase {
        let since = condition("Ready")
            .and_then(|c| c.get("lastTransitionTime"))
            .and_then(Value::as_str)
            .and_then(parse_time);
        let stable_for = since
            .and_then(|t| (now - t).to_std().ok())
            .unwrap_or(Duration::ZERO);
        return (SyncState::Synced, stable_for);
    }

    (SyncState::Unknown, Duration::ZERO)
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// DNS-1123-safe key for `spec.network`.
///
/// `"testnet"` → `testnet`; `{"custom": "My Net"}` → `custom-<fnv1a32 hex>`.
/// A hash is used for custom networks because passphrases are arbitrary
/// strings; FNV-1a is used (not `DefaultHasher`) because it is stable across
/// Rust versions and processes.
pub fn network_key(network: Option<&Value>) -> String {
    match network {
        Some(Value::String(s)) => {
            let cleaned: String = s
                .to_lowercase()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .take(20)
                .collect();
            let cleaned = cleaned.trim_matches('-').to_string();
            if cleaned.is_empty() {
                "unknown".to_string()
            } else {
                cleaned
            }
        }
        Some(Value::Object(map)) => match map.get("custom").and_then(Value::as_str) {
            Some(name) => format!("custom-{:08x}", fnv1a32(name)),
            None => "unknown".to_string(),
        },
        _ => "unknown".to_string(),
    }
}

fn fnv1a32(s: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in s.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-19T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn node(name: &str, status: Value) -> Value {
        json!({
            "apiVersion": "stellar.org/v1alpha1",
            "kind": "StellarNode",
            "metadata": {"name": name, "namespace": "stellar"},
            "spec": {"nodeType": "Validator", "network": "testnet"},
            "status": status
        })
    }

    fn ready_since(ts: &str) -> Value {
        json!({
            "phase": "Running",
            "conditions": [
                {"type": "Ready", "status": "True", "lastTransitionTime": ts, "reason": "r", "message": "m"},
                {"type": "Progressing", "status": "False", "lastTransitionTime": ts, "reason": "r", "message": "m"},
                {"type": "Degraded", "status": "False", "lastTransitionTime": ts, "reason": "r", "message": "m"}
            ]
        })
    }

    #[test]
    fn ready_validator_reports_how_long_it_has_been_stable() {
        let obj = node("v1", ready_since("2026-10-19T11:58:30Z"));
        let v = observe_stellar_node(&obj, now()).unwrap();
        assert_eq!(v.observation.id, "v1");
        assert_eq!(v.observation.sync, SyncState::Synced);
        assert_eq!(v.observation.stable_for, Duration::from_secs(90));
        assert_eq!(v.namespace, "stellar");
        assert_eq!(v.network_key, "testnet");
    }

    #[test]
    fn future_or_bad_timestamps_never_count_as_stable() {
        let future = observe_stellar_node(&node("v", ready_since("2026-10-19T13:00:00Z")), now());
        assert_eq!(future.unwrap().observation.stable_for, Duration::ZERO);
        let junk = observe_stellar_node(&node("v", ready_since("not-a-time")), now());
        assert_eq!(junk.unwrap().observation.stable_for, Duration::ZERO);
    }

    #[test]
    fn progressing_condition_means_catching_up() {
        let status = json!({"phase": "Running", "conditions": [
            {"type": "Ready", "status": "True", "lastTransitionTime": "2026-10-19T00:00:00Z", "reason": "r", "message": "m"},
            {"type": "Progressing", "status": "True", "lastTransitionTime": "2026-10-19T11:59:00Z", "reason": "r", "message": "m"}
        ]});
        let v = observe_stellar_node(&node("v", status), now()).unwrap();
        assert_eq!(v.observation.sync, SyncState::CatchingUp);
        assert_eq!(v.observation.stable_for, Duration::ZERO);
    }

    #[test]
    fn syncing_style_phases_mean_catching_up_even_if_ready_is_stale() {
        for phase in ["Syncing", "Creating", "Pending", "Remediating"] {
            let mut status = ready_since("2026-10-19T00:00:00Z");
            status["phase"] = json!(phase);
            let v = observe_stellar_node(&node("v", status), now()).unwrap();
            assert_eq!(v.observation.sync, SyncState::CatchingUp, "{phase}");
        }
    }

    #[test]
    fn degraded_failed_terminating_or_not_ready_are_unknown() {
        let mut degraded = ready_since("2026-10-19T00:00:00Z");
        degraded["conditions"][2]["status"] = json!("True");
        let mut failed = ready_since("2026-10-19T00:00:00Z");
        failed["phase"] = json!("Failed");
        let not_ready = json!({"phase": "Running", "conditions": [
            {"type": "Ready", "status": "False", "lastTransitionTime": "2026-10-19T00:00:00Z", "reason": "r", "message": "m"}
        ]});
        let empty = json!({});
        for (label, status) in [
            ("degraded", degraded),
            ("failed", failed),
            ("not ready", not_ready),
            ("empty", empty),
        ] {
            let v = observe_stellar_node(&node("v", status), now()).unwrap();
            assert_eq!(v.observation.sync, SyncState::Unknown, "{label}");
        }
    }

    #[test]
    fn missing_status_is_unknown() {
        let mut obj = node("v", json!(null));
        obj.as_object_mut().unwrap().remove("status");
        let v = observe_stellar_node(&obj, now()).unwrap();
        assert_eq!(v.observation.sync, SyncState::Unknown);
    }

    #[test]
    fn deleting_validator_is_unknown_even_if_ready() {
        let mut obj = node("v", ready_since("2026-10-19T00:00:00Z"));
        obj["metadata"]["deletionTimestamp"] = json!("2026-10-19T11:59:59Z");
        let v = observe_stellar_node(&obj, now()).unwrap();
        assert_eq!(v.observation.sync, SyncState::Unknown);
    }

    #[test]
    fn non_validators_and_suspended_validators_are_ignored() {
        let mut horizon = node("h", ready_since("2026-10-19T00:00:00Z"));
        horizon["spec"]["nodeType"] = json!("Horizon");
        assert!(observe_stellar_node(&horizon, now()).is_none());

        let mut suspended = node("s", ready_since("2026-10-19T00:00:00Z"));
        suspended["spec"]["suspended"] = json!(true);
        assert!(observe_stellar_node(&suspended, now()).is_none());

        assert!(observe_stellar_node(&json!({"metadata": {"name": "x"}}), now()).is_none());
        assert!(observe_stellar_node(&json!({"spec": {"nodeType": "Validator"}}), now()).is_none());
    }

    #[test]
    fn namespace_defaults_and_min_quorum_annotation_is_parsed() {
        let mut obj = node("v", ready_since("2026-10-19T00:00:00Z"));
        obj["metadata"].as_object_mut().unwrap().remove("namespace");
        obj["metadata"]["annotations"] = json!({MIN_QUORUM_ANNOTATION: " 2 "});
        let v = observe_stellar_node(&obj, now()).unwrap();
        assert_eq!(v.namespace, "default");
        assert_eq!(v.min_quorum, Some(2));

        obj["metadata"]["annotations"] = json!({MIN_QUORUM_ANNOTATION: "lots"});
        assert_eq!(observe_stellar_node(&obj, now()).unwrap().min_quorum, None);
    }

    #[test]
    fn network_keys_are_dns_safe_and_stable() {
        assert_eq!(network_key(Some(&json!("testnet"))), "testnet");
        assert_eq!(network_key(Some(&json!("Mainnet"))), "mainnet");
        assert_eq!(network_key(Some(&json!("Weird Net!!"))), "weird-net");
        assert_eq!(network_key(None), "unknown");
        assert_eq!(network_key(Some(&json!(7))), "unknown");
        assert_eq!(network_key(Some(&json!({"other": 1}))), "unknown");

        let a = network_key(Some(&json!({"custom": "alpha"})));
        let b = network_key(Some(&json!({"custom": "beta"})));
        assert!(a.starts_with("custom-") && a.len() == "custom-".len() + 8);
        assert_ne!(a, b);
        // FNV-1a is deterministic: pin a known value so a change is noticed.
        assert_eq!(fnv1a32(""), 0x811c_9dc5);
        assert_eq!(fnv1a32("a"), 0xe40c_292c);
        assert_eq!(a, network_key(Some(&json!({"custom": "alpha"}))));
    }

    #[test]
    fn grouping_splits_by_namespace_and_network_and_sorts_members() {
        let mk = |ns: &str, net: &str, name: &str| {
            let mut o = node(name, ready_since("2026-10-19T00:00:00Z"));
            o["metadata"]["namespace"] = json!(ns);
            o["spec"]["network"] = json!(net);
            observe_stellar_node(&o, now()).unwrap()
        };
        let groups = group_validators(vec![
            mk("a", "testnet", "v2"),
            mk("a", "testnet", "v1"),
            mk("a", "mainnet", "m1"),
            mk("b", "testnet", "t1"),
        ]);
        assert_eq!(groups.len(), 3);
        assert_eq!(
            (groups[0].namespace.as_str(), groups[0].network_key.as_str()),
            ("a", "mainnet")
        );
        assert_eq!(
            (groups[1].namespace.as_str(), groups[1].network_key.as_str()),
            ("a", "testnet")
        );
        assert_eq!(groups[1].member_ids(), vec!["v1", "v2"]);
        assert_eq!(groups[2].namespace, "b");
        assert_eq!(groups[1].pdb_name(), "stellar-validators-testnet-pdb");
    }

    #[test]
    fn grouping_takes_the_most_conservative_min_quorum() {
        let mk = |name: &str, q: Option<&str>| {
            let mut o = node(name, ready_since("2026-10-19T00:00:00Z"));
            if let Some(q) = q {
                o["metadata"]["annotations"] = json!({MIN_QUORUM_ANNOTATION: q});
            }
            observe_stellar_node(&o, now()).unwrap()
        };
        let g = group_validators(vec![mk("a", Some("2")), mk("b", Some("3")), mk("c", None)]);
        assert_eq!(g[0].min_quorum, Some(3));
        let g = group_validators(vec![mk("a", None), mk("b", None)]);
        assert_eq!(g[0].min_quorum, None);
    }
}
