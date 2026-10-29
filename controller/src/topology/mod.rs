//! Dynamic node topology spread constraint enforcement.
//!
//! This module inspects the availability-zone topology of the active cluster
//! and generates Kubernetes `topologySpreadConstraints` (and a matching
//! pod anti-affinity fallback) for node StatefulSet manifests.
//!
//! - [`rules`] builds the spread/anti-affinity rules from observed zone data.
//! - [`enforcer`] is the mutating controller that inspects node labels and
//!   injects the generated rules into StatefulSet manifests.

pub mod enforcer;
pub mod rules;

pub use enforcer::{TopologyEnforcer, TopologyError};
pub use rules::{
    build_anti_affinity, build_topology_spread_constraints, EnforcementMode, TopologyRuleSet,
    ZoneCapacity,
};
