//! Topology spread and anti-affinity rule generation for node workloads.
//!
//! This module turns observed availability-zone capacity into Kubernetes
//! scheduling rules. Hard enforcement is only emitted when the cluster has
//! enough zones to actually spread across; otherwise we fall back to soft
//! affinity so single-zone local development clusters keep scheduling.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Well-known label carrying the cloud provider availability zone.
pub const ZONE_LABEL: &str = "topology.kubernetes.io/zone";

/// Label selector used to match the node pods a rule applies to.
pub const NODE_APP_LABEL: &str = "app.kubernetes.io/name";

/// Minimum number of distinct zones required before hard enforcement.
pub const MIN_ZONES_FOR_HARD_ENFORCEMENT: usize = 2;

/// A single `topologySpreadConstraint` entry in a StatefulSet manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopologySpreadConstraint {
    pub max_skew: u32,
    pub topology_key: String,
    pub when_unsatisfiable: WhenUnsatisfiable,
    pub label_selector: LabelSelector,
}

/// Scheduling behaviour when a spread constraint cannot be satisfied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum WhenUnsatisfiable {
    DoNotSchedule,
    ScheduleAnyway,
}

/// Minimal label selector representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelSelector {
    pub match_labels: BTreeMap<String, String>,
}

impl LabelSelector {
    pub fn matching(app: &str) -> Self {
        let mut match_labels = BTreeMap::new();
        match_labels.insert(NODE_APP_LABEL.to_string(), app.to_string());
        Self { match_labels }
    }
}

/// A pod anti-affinity term preventing co-location on a single host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PodAntiAffinityTerm {
    pub topology_key: String,
    pub label_selector: LabelSelector,
}

/// Observed zone capacity across the cluster.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZoneCapacity {
    /// Zone name -> number of schedulable nodes in that zone.
    pub zones: BTreeMap<String, usize>,
}

impl ZoneCapacity {
    pub fn new(zones: BTreeMap<String, usize>) -> Self {
        Self { zones }
    }

    /// Number of zones that actually have schedulable capacity.
    pub fn active_zone_count(&self) -> usize {
        self.zones.values().filter(|count| **count > 0).count()
    }

    /// Whether the cluster can support hard zone enforcement.
    pub fn supports_hard_enforcement(&self) -> bool {
        self.active_zone_count() >= MIN_ZONES_FOR_HARD_ENFORCEMENT
    }
}

/// The scheduling rules generated for a node workload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulingRules {
    pub topology_spread_constraints: Vec<TopologySpreadConstraint>,
    pub pod_anti_affinity: Vec<PodAntiAffinityTerm>,
    pub hard_enforced: bool,
}

/// Generate scheduling rules for a node workload given observed zone capacity.
///
/// On multi-zone clusters we emit hard (`DoNotSchedule`) zone spread plus host
/// anti-affinity. On single-zone clusters we degrade to soft (`ScheduleAnyway`)
/// rules so local development keeps working.
pub fn generate_rules(app: &str, capacity: &ZoneCapacity) -> SchedulingRules {
    let selector = LabelSelector::matching(app);
    let hard_enforced = capacity.supports_hard_enforcement();

    let when_unsatisfiable = if hard_enforced {
        WhenUnsatisfiable::DoNotSchedule
    } else {
        WhenUnsatisfiable::ScheduleAnyway
    };

    let topology_spread_constraints = vec![
        TopologySpreadConstraint {
            max_skew: 1,
            topology_key: ZONE_LABEL.to_string(),
            when_unsatisfiable,
            label_selector: selector.clone(),
        },
        TopologySpreadConstraint {
            max_skew: 1,
            topology_key: "kubernetes.io/hostname".to_string(),
            when_unsatisfiable,
            label_selector: selector.clone(),
        },
    ];

    let pod_anti_affinity = vec![PodAntiAffinityTerm {
        topology_key: "kubernetes.io/hostname".to_string(),
        label_selector: selector,
    }];

    SchedulingRules {
        topology_spread_constraints,
        pod_anti_affinity,
        hard_enforced,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capacity(zones: &[(&str, usize)]) -> ZoneCapacity {
        ZoneCapacity::new(
            zones
                .iter()
                .map(|(zone, count)| (zone.to_string(), *count))
                .collect(),
        )
    }

    #[test]
    fn multi_zone_uses_hard_enforcement() {
        let cap = capacity(&[("zone-a", 3), ("zone-b", 3), ("zone-c", 3)]);
        let rules = generate_rules("node", &cap);

        assert!(rules.hard_enforced);
        assert!(rules
            .topology_spread_constraints
            .iter()
            .all(|c| c.when_unsatisfiable == WhenUnsatisfiable::DoNotSchedule));
        assert_eq!(rules.topology_spread_constraints[0].topology_key, ZONE_LABEL);
    }

    #[test]
    fn single_zone_falls_back_to_soft() {
        let cap = capacity(&[("zone-a", 5)]);
        let rules = generate_rules("node", &cap);

        assert!(!rules.hard_enforced);
        assert!(rules
            .topology_spread_constraints
            .iter()
            .all(|c| c.when_unsatisfiable == WhenUnsatisfiable::ScheduleAnyway));
    }

    #[test]
    fn empty_zones_are_not_counted() {
        let cap = capacity(&[("zone-a", 3), ("zone-b", 0)]);
        assert_eq!(cap.active_zone_count(), 1);
        assert!(!cap.supports_hard_enforcement());
    }
}
