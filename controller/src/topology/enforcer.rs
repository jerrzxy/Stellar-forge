//! Dynamic node topology spread constraint enforcer.
//!
//! This mutating controller inspects the active node provider zone labels and
//! injects `topologySpreadConstraints` (and a matching pod anti-affinity) into
//! node StatefulSet manifests so that node pods are spread evenly across
//! availability zones. On single-zone clusters it automatically falls back to
//! soft (ScheduleAnyway) rules so local development keeps working.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::topology::rules::{self, ZoneCapacity, ZoneSpreadMode};

/// Well-known label used by cloud providers to advertise the zone of a node.
pub const ZONE_LABEL: &str = "topology.kubernetes.io/zone";

/// Label selector key used to identify node pods managed by this controller.
pub const NODE_APP_LABEL: &str = "app.kubernetes.io/name";

/// Mutating controller that enforces topology spread on node StatefulSets.
#[derive(Debug, Clone)]
pub struct TopologyEnforcer {
    /// Minimum number of zones required before hard enforcement is applied.
    pub min_zones_for_hard: usize,
    /// Maximum skew tolerated between zones for hard enforcement.
    pub max_skew: u32,
}

impl Default for TopologyEnforcer {
    fn default() -> Self {
        Self {
            min_zones_for_hard: 2,
            max_skew: 1,
        }
    }
}

impl TopologyEnforcer {
    pub fn new(min_zones_for_hard: usize, max_skew: u32) -> Self {
        Self {
            min_zones_for_hard,
            max_skew,
        }
    }

    /// Inspect the active nodes and aggregate capacity per availability zone.
    ///
    /// Nodes without a zone label are ignored; nodes that are not ready are
    /// skipped so that draining/terminating capacity does not influence the
    /// enforcement decision.
    pub fn inspect_zones(&self, nodes: &[Value]) -> ZoneCapacity {
        let mut capacity: BTreeMap<String, u32> = BTreeMap::new();

        for node in nodes {
            if !is_node_ready(node) {
                continue;
            }
            let zone = node
                .pointer("/metadata/labels")
                .and_then(|labels| labels.get(ZONE_LABEL))
                .and_then(Value::as_str);

            if let Some(zone) = zone {
                if !zone.is_empty() {
                    *capacity.entry(zone.to_string()).or_insert(0) += 1;
                }
            }
        }

        ZoneCapacity::from_map(capacity)
    }

    /// Decide the spread mode based on observed zone capacity.
    ///
    /// Hard enforcement is only applied when there are enough zones with
    /// capacity to actually satisfy the constraint; otherwise we fall back to
    /// soft affinity so single-zone clusters still schedule.
    pub fn spread_mode(&self, capacity: &ZoneCapacity) -> ZoneSpreadMode {
        if capacity.zone_count() >= self.min_zones_for_hard {
            ZoneSpreadMode::Hard
        } else {
            ZoneSpreadMode::Soft
        }
    }

    /// Mutate a StatefulSet manifest in place, injecting topology spread
    /// constraints and pod anti-affinity derived from the observed zones.
    pub fn enforce(&self, statefulset: &mut Value, nodes: &[Value]) -> Result<(), String> {
        let capacity = self.inspect_zones(nodes);
        let mode = self.spread_mode(&capacity);

        let app_name = statefulset
            .pointer("/metadata/name")
            .and_then(Value::as_str)
            .ok_or_else(|| "statefulset is missing metadata.name".to_string())?
            .to_string();

        let constraints = rules::topology_spread_constraints(&app_name, mode, self.max_skew);
        let anti_affinity = rules::pod_anti_affinity(&app_name, mode);

        let spec = statefulset
            .pointer_mut("/spec/template/spec")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| "statefulset is missing spec.template.spec".to_string())?;

        spec.insert("topologySpreadConstraints".to_string(), constraints);

        let affinity = spec
            .entry("affinity".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(affinity_obj) = affinity.as_object_mut() {
            affinity_obj.insert("podAntiAffinity".to_string(), anti_affinity);
        }

        Ok(())
    }
}

/// Returns true when the node reports a Ready condition of `True`.
fn is_node_ready(node: &Value) -> bool {
    node.pointer("/status/conditions")
        .and_then(Value::as_array)
        .map(|conditions| {
            conditions.iter().any(|condition| {
                condition.get("type").and_then(Value::as_str) == Some("Ready")
                    && condition.get("status").and_then(Value::as_str) == Some("True")
            })
        })
        .unwrap_or(false)
}

/// Convenience helper used by tests and callers to build a minimal node object.
pub fn node_with_zone(name: &str, zone: &str, ready: bool) -> Value {
    json!({
        "metadata": {
            "name": name,
            "labels": { ZONE_LABEL: zone }
        },
        "status": {
            "conditions": [
                { "type": "Ready", "status": if ready { "True" } else { "False" } }
            ]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statefulset(name: &str) -> Value {
        json!({
            "metadata": { "name": name },
            "spec": { "template": { "spec": { "containers": [] } } }
        })
    }

    #[test]
    fn single_zone_falls_back_to_soft() {
        let enforcer = TopologyEnforcer::default();
        let nodes = vec![node_with_zone("n1", "zone-a", true)];
        let mut sts = statefulset("node");

        enforcer.enforce(&mut sts, &nodes).unwrap();

        let constraints = sts
            .pointer("/spec/template/spec/topologySpreadConstraints")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(constraints.len(), 1);
        assert_eq!(
            constraints[0].get("whenUnsatisfiable").and_then(Value::as_str),
            Some("ScheduleAnyway")
        );
    }

    #[test]
    fn multi_zone_applies_hard_enforcement() {
        let enforcer = TopologyEnforcer::default();
        let nodes = vec![
            node_with_zone("n1", "zone-a", true),
            node_with_zone("n2", "zone-b", true),
            node_with_zone("n3", "zone-c", true),
        ];
        let mut sts = statefulset("node");

        enforcer.enforce(&mut sts, &nodes).unwrap();

        let constraints = sts
            .pointer("/spec/template/spec/topologySpreadConstraints")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(
            constraints[0].get("whenUnsatisfiable").and_then(Value::as_str),
            Some("DoNotSchedule")
        );
        assert_eq!(
            constraints[0].get("topologyKey").and_then(Value::as_str),
            Some(ZONE_LABEL)
        );
    }

    #[test]
    fn not_ready_nodes_are_ignored() {
        let enforcer = TopologyEnforcer::default();
        let nodes = vec![
            node_with_zone("n1", "zone-a", true),
            node_with_zone("n2", "zone-b", false),
        ];
        let capacity = enforcer.inspect_zones(&nodes);
        assert_eq!(capacity.zone_count(), 1);
    }
}
