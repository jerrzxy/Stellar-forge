// Copyright 2024 Stellar-K8s Contributors
// SPDX-License-Identifier: Apache-2.0
//! Pure PodDisruptionBudget sizing logic for a group of validators.
//!
//! This module has **no Kubernetes dependency**: it takes observations of the
//! validators in a group plus the quorum topology and answers one question —
//! what `maxUnavailable` may the group's PodDisruptionBudget carry right now?
//!
//! # Rules
//!
//! The answer is `0` (block every voluntary disruption, e.g. `kubectl drain`)
//! unless **all** of the following hold:
//!
//! 1. the group is not empty;
//! 2. every member is [`SyncState::Synced`] and has been so for at least
//!    [`PdbPolicy::stability_window`] (a node that just finished catching up is
//!    not "stable" yet);
//! 3. the quorum topology is well formed (no missing dependencies, no two
//!    disjoint quorums possible); and
//! 4. evicting **any single** member would still leave every remaining healthy
//!    node with at least `quorum_threshold` healthy members in its quorum
//!    slice, and would leave at least one healthy node.
//!
//! Only then is `maxUnavailable` relaxed to [`RELAXED_MAX_UNAVAILABLE`] (`1`).
//! Rule 4 is what "strictly prevent PDB updates that would violate the minimum
//! quorum" means: the relaxation is *verified* ([`PdbDecision::quorum_verified`])
//! and a relaxation that is not verified is never produced. Unknown health
//! always fails closed.
//!
//! # Quorum slice semantics
//!
//! For a [`Node`](crate::quorum::graph::Node) `n`, its quorum slice is
//! `{n} ∪ n.validators`, and `n` is satisfied while at least
//! `n.quorum_threshold` members of that slice are healthy. This matches
//! stellar-core, where a node counts itself in its own quorum set.
//!
//! # Default topology
//!
//! Validators that belong to one group are modelled as a full mesh in which
//! every node requires [`default_quorum_threshold`] (`n - f`, with
//! `f = ⌊(n-1)/3⌋`) healthy members. Operators can lower or raise that with
//! the `stellar.org/pdb-min-quorum` annotation; see [`full_mesh_topology`].

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::time::Duration;

use crate::quorum::graph::{Node, TopologyGraph};
use crate::quorum::intersection::{analyze_quorum_intersection, TopologyStatus};

/// `maxUnavailable` while the group must not be disrupted at all.
pub const BLOCKED_MAX_UNAVAILABLE: u32 = 0;

/// `maxUnavailable` once the group is synced, stable and quorum-safe.
pub const RELAXED_MAX_UNAVAILABLE: u32 = 1;

/// How long a validator must stay synced before it counts as stable.
pub const DEFAULT_STABILITY_WINDOW: Duration = Duration::from_secs(60);

/// Sync health of a single validator, as observed from its `StellarNode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncState {
    /// Fully synced with the network and reporting ready.
    Synced,
    /// Still bootstrapping or catching up to the network.
    CatchingUp,
    /// Health cannot be determined (no status yet, failed, terminating, ...).
    /// Treated as unhealthy.
    Unknown,
}

/// One validator's observed health.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatorObservation {
    /// Identifier of the validator; the `StellarNode` name.
    pub id: String,
    /// Current sync state.
    pub sync: SyncState,
    /// How long the validator has continuously been in `sync`.
    pub stable_for: Duration,
}

impl ValidatorObservation {
    /// Create an observation.
    pub fn new(id: impl Into<String>, sync: SyncState, stable_for: Duration) -> Self {
        Self {
            id: id.into(),
            sync,
            stable_for,
        }
    }

    /// `true` when the validator is synced and has been for at least `window`.
    pub fn is_healthy(&self, window: Duration) -> bool {
        self.sync == SyncState::Synced && self.stable_for >= window
    }
}

/// Tunables for [`calculate`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PdbPolicy {
    /// Minimum time a validator must be continuously synced to be "stable".
    pub stability_window: Duration,
}

impl Default for PdbPolicy {
    fn default() -> Self {
        Self {
            stability_window: DEFAULT_STABILITY_WINDOW,
        }
    }
}

/// Why evicting one member would break a node's quorum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuorumViolation {
    /// The validator whose eviction was simulated.
    pub removed: String,
    /// The node whose quorum slice would no longer be satisfied.
    pub node: String,
    /// Healthy members left in that node's slice.
    pub healthy: usize,
    /// Members the node's `quorum_threshold` requires.
    pub required: usize,
}

/// The reason a [`PdbDecision`] came out the way it did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PdbReason {
    /// The group has no validators.
    EmptyGroup,
    /// These validators are still syncing or catching up.
    CatchingUp(Vec<String>),
    /// The health of these validators is unknown.
    HealthUnknown(Vec<String>),
    /// These validators are synced but not for long enough yet.
    NotYetStable(Vec<String>),
    /// The quorum topology is malformed or unsafe.
    TopologyUnsafe(String),
    /// Evicting one validator would violate a node's quorum.
    QuorumFloor(QuorumViolation),
    /// All validators are synced, stable and one can be lost safely.
    SyncedAndStable,
}

impl PdbReason {
    /// Short, stable, kebab-case code (used in PDB annotations and metrics).
    pub fn code(&self) -> &'static str {
        match self {
            PdbReason::EmptyGroup => "empty-group",
            PdbReason::CatchingUp(_) => "catching-up",
            PdbReason::HealthUnknown(_) => "health-unknown",
            PdbReason::NotYetStable(_) => "not-yet-stable",
            PdbReason::TopologyUnsafe(_) => "topology-unsafe",
            PdbReason::QuorumFloor(_) => "quorum-floor",
            PdbReason::SyncedAndStable => "synced-and-stable",
        }
    }
}

impl fmt::Display for PdbReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PdbReason::EmptyGroup => write!(f, "no validators in group"),
            PdbReason::CatchingUp(ids) => write!(f, "catching up: {}", ids.join(",")),
            PdbReason::HealthUnknown(ids) => write!(f, "health unknown: {}", ids.join(",")),
            PdbReason::NotYetStable(ids) => write!(f, "not yet stable: {}", ids.join(",")),
            PdbReason::TopologyUnsafe(why) => write!(f, "topology unsafe: {why}"),
            PdbReason::QuorumFloor(v) => write!(
                f,
                "evicting {} would leave {} with {} healthy of {} required",
                v.removed, v.node, v.healthy, v.required
            ),
            PdbReason::SyncedAndStable => write!(f, "synced and stable"),
        }
    }
}

/// The computed `maxUnavailable` for a validator group, with its justification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PdbDecision {
    /// Value for `spec.maxUnavailable`.
    pub max_unavailable: u32,
    /// Why.
    pub reason: PdbReason,
    /// `true` only when relaxing the PDB was checked against the quorum
    /// topology. Invariant: `max_unavailable > 0` implies `quorum_verified`.
    pub quorum_verified: bool,
}

impl PdbDecision {
    fn blocked(reason: PdbReason) -> Self {
        Self {
            max_unavailable: BLOCKED_MAX_UNAVAILABLE,
            reason,
            quorum_verified: false,
        }
    }

    /// `true` when this decision relaxes the PDB without having verified quorum
    /// safety. [`calculate`] never returns such a decision; the reconciler
    /// refuses to apply one anyway (defence in depth).
    pub fn violates_quorum_invariant(&self) -> bool {
        self.max_unavailable > BLOCKED_MAX_UNAVAILABLE && !self.quorum_verified
    }
}

/// `n - f` with `f = ⌊(n-1)/3⌋`: the usual BFT quorum size for `n` validators.
///
/// `0 → 0`, `1 → 1`, `3 → 3`, `4 → 3`, `7 → 5`, `10 → 7`.
pub fn default_quorum_threshold(n: usize) -> usize {
    if n == 0 {
        0
    } else {
        n - (n - 1) / 3
    }
}

/// Model `ids` as a full mesh where every node needs `min_quorum` healthy
/// members of the whole group (itself included). `None` uses
/// [`default_quorum_threshold`]. The value is clamped to `1..=ids.len()`.
pub fn full_mesh_topology(ids: &[String], min_quorum: Option<usize>) -> TopologyGraph {
    let mut graph = TopologyGraph::new();
    let n = ids.len();
    if n == 0 {
        return graph;
    }
    let threshold = min_quorum
        .unwrap_or_else(|| default_quorum_threshold(n))
        .clamp(1, n);
    for id in ids {
        graph.add_node(Node {
            id: id.clone(),
            quorum_threshold: threshold,
            validators: ids.iter().filter(|other| *other != id).cloned().collect(),
        });
    }
    graph
}

/// Returns a description of what is wrong with `topology`, if anything.
///
/// Combines the crate's existing [`analyze_quorum_intersection`] (missing
/// dependencies, split-brain heuristic) with a slice-aware intersection check:
/// if `2 * quorum_threshold <= |slice|`, two disjoint quorums could form.
fn topology_problem(topology: &TopologyGraph) -> Option<String> {
    match analyze_quorum_intersection(topology) {
        TopologyStatus::Healthy => {}
        TopologyStatus::MissingDependencies(missing) => {
            return Some(format!("missing dependencies: {}", missing.join(",")));
        }
        TopologyStatus::SplitBrainRisk => return Some("split-brain risk".to_string()),
    }

    let mut ids: Vec<&String> = topology.nodes.keys().collect();
    ids.sort();
    for id in ids {
        let node = &topology.nodes[id];
        let slice: BTreeSet<&str> = node
            .validators
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(node.id.as_str()))
            .collect();
        if node.quorum_threshold == 0 || node.quorum_threshold > slice.len() {
            return Some(format!(
                "{} has quorum threshold {} for a slice of {}",
                node.id,
                node.quorum_threshold,
                slice.len()
            ));
        }
        if node.quorum_threshold * 2 <= slice.len() {
            return Some(format!(
                "{} threshold {} of {} allows disjoint quorums",
                node.id,
                node.quorum_threshold,
                slice.len()
            ));
        }
    }
    None
}

/// Simulate evicting `removed` and report the first node whose quorum would no
/// longer be satisfied. `healthy` maps validator id → healthy; ids missing from
/// the map are treated as unhealthy (fail closed).
///
/// Nodes that are not healthy themselves are skipped — they are already down
/// and do not need a quorum. If no healthy node would remain at all, that is a
/// violation too (it is the only way a single-validator group is caught).
pub fn removal_violation(
    topology: &TopologyGraph,
    healthy: &HashMap<&str, bool>,
    removed: &str,
) -> Option<QuorumViolation> {
    let is_up = |id: &str| id != removed && healthy.get(id).copied().unwrap_or(false);

    let mut ids: Vec<&String> = topology.nodes.keys().collect();
    ids.sort();

    let mut remaining_up = 0usize;
    for id in ids {
        let node = &topology.nodes[id];
        if !is_up(&node.id) {
            continue;
        }
        remaining_up += 1;

        let slice: BTreeSet<&str> = node
            .validators
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(node.id.as_str()))
            .collect();
        let up = slice.iter().filter(|member| is_up(member)).count();
        if up < node.quorum_threshold {
            return Some(QuorumViolation {
                removed: removed.to_string(),
                node: node.id.clone(),
                healthy: up,
                required: node.quorum_threshold,
            });
        }
    }

    if remaining_up == 0 {
        return Some(QuorumViolation {
            removed: removed.to_string(),
            node: removed.to_string(),
            healthy: 0,
            required: 1,
        });
    }
    None
}

/// Compute `maxUnavailable` for the PDB covering `group`.
///
/// * `group` — the validators the PDB selects (any of them can be evicted).
/// * `topology` — the quorum topology the group participates in.
/// * `peers` — health of validators that are in `topology` but **not** in the
///   group (e.g. another group in the same topology). May be empty.
///
/// See the [module docs](self) for the rules. The result is deterministic.
pub fn calculate(
    group: &[ValidatorObservation],
    topology: &TopologyGraph,
    peers: &[ValidatorObservation],
    policy: &PdbPolicy,
) -> PdbDecision {
    if group.is_empty() {
        return PdbDecision::blocked(PdbReason::EmptyGroup);
    }

    let window = policy.stability_window;
    let mut catching_up = Vec::new();
    let mut unknown = Vec::new();
    let mut not_stable = Vec::new();
    for obs in group {
        match obs.sync {
            SyncState::CatchingUp => catching_up.push(obs.id.clone()),
            SyncState::Unknown => unknown.push(obs.id.clone()),
            SyncState::Synced if obs.stable_for < window => not_stable.push(obs.id.clone()),
            SyncState::Synced => {}
        }
    }
    if !catching_up.is_empty() {
        catching_up.sort();
        return PdbDecision::blocked(PdbReason::CatchingUp(catching_up));
    }
    if !unknown.is_empty() {
        unknown.sort();
        return PdbDecision::blocked(PdbReason::HealthUnknown(unknown));
    }
    if !not_stable.is_empty() {
        not_stable.sort();
        return PdbDecision::blocked(PdbReason::NotYetStable(not_stable));
    }

    if let Some(problem) = topology_problem(topology) {
        return PdbDecision::blocked(PdbReason::TopologyUnsafe(problem));
    }

    let mut ids: Vec<&str> = group.iter().map(|o| o.id.as_str()).collect();
    ids.sort_unstable();
    if let Some(missing) = ids
        .iter()
        .copied()
        .find(|id| !topology.nodes.contains_key(*id))
    {
        return PdbDecision::blocked(PdbReason::TopologyUnsafe(format!(
            "{missing} is not part of the quorum topology"
        )));
    }

    let health: HashMap<&str, bool> = peers
        .iter()
        .chain(group.iter())
        .map(|o| (o.id.as_str(), o.is_healthy(window)))
        .collect();

    // The PDB cannot choose which pod gets evicted, so relaxing is only safe if
    // losing *any* member is safe.
    for id in ids {
        if let Some(violation) = removal_violation(topology, &health, id) {
            return PdbDecision::blocked(PdbReason::QuorumFloor(violation));
        }
    }

    PdbDecision {
        max_unavailable: RELAXED_MAX_UNAVAILABLE,
        reason: PdbReason::SyncedAndStable,
        quorum_verified: true,
    }
}

/// Derive the topology used for the calculation from the validators' names and
/// a minimum quorum size.
///
/// This is used by the reconciler when it needs to build a topology from the
/// `StellarNode` resources it is watching.
pub fn topology_from_validators<'a, I>(validators: I, min_quorum: usize) -> TopologyGraph
where
    I: IntoIterator<Item = &'a str>,
{
    let mut ids: Vec<String> = validators.into_iter().map(str::to_string).collect();
    ids.sort();
    ids.dedup();
    full_mesh_topology(&ids, Some(min_quorum))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Duration = DEFAULT_STABILITY_WINDOW;

    fn stable(id: &str) -> ValidatorObservation {
        ValidatorObservation::new(id, SyncState::Synced, WINDOW * 10)
    }

    fn catching_up(id: &str) -> ValidatorObservation {
        ValidatorObservation::new(id, SyncState::CatchingUp, Duration::ZERO)
    }

    fn ids(group: &[ValidatorObservation]) -> Vec<String> {
        group.iter().map(|o| o.id.clone()).collect()
    }

    fn decide(group: &[ValidatorObservation], min_quorum: Option<usize>) -> PdbDecision {
        let topology = full_mesh_topology(&ids(group), min_quorum);
        calculate(group, &topology, &[], &PdbPolicy::default())
    }

    fn four_stable() -> Vec<ValidatorObservation> {
        ["v1", "v2", "v3", "v4"].iter().map(|i| stable(i)).collect()
    }

    #[test]
    fn default_threshold_is_n_minus_f() {
        let got: Vec<usize> = (0..=10).map(default_quorum_threshold).collect();
        assert_eq!(got, vec![0, 1, 2, 3, 3, 4, 5, 5, 6, 7, 7]);
    }

    #[test]
    fn empty_group_is_blocked() {
        let d = calculate(&[], &TopologyGraph::new(), &[], &PdbPolicy::default());
        assert_eq!(d.max_unavailable, 0);
        assert_eq!(d.reason, PdbReason::EmptyGroup);
        assert!(!d.quorum_verified);
    }

    #[test]
    fn all_synced_and_stable_with_headroom_relaxes_to_one() {
        let d = decide(&four_stable(), None);
        assert_eq!(d.max_unavailable, RELAXED_MAX_UNAVAILABLE);
        assert_eq!(d.reason, PdbReason::SyncedAndStable);
        assert!(d.quorum_verified);
        assert!(!d.violates_quorum_invariant());
    }

    #[test]
    fn any_catching_up_validator_blocks_the_whole_group() {
        let mut group = four_stable();
        group[2] = catching_up("v3");
        let d = decide(&group, None);
        assert_eq!(d.max_unavailable, 0);
        assert_eq!(d.reason, PdbReason::CatchingUp(vec!["v3".to_string()]));
    }

    #[test]
    fn catching_up_ids_are_reported_sorted() {
        let group = vec![catching_up("b"), catching_up("a"), stable("c"), stable("d")];
        let d = decide(&group, None);
        assert_eq!(
            d.reason,
            PdbReason::CatchingUp(vec!["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn unknown_health_fails_closed() {
        let mut group = four_stable();
        group[0] = ValidatorObservation::new("v1", SyncState::Unknown, Duration::ZERO);
        let d = decide(&group, None);
        assert_eq!(d.max_unavailable, 0);
        assert_eq!(d.reason, PdbReason::HealthUnknown(vec!["v1".to_string()]));
    }

    #[test]
    fn freshly_synced_validator_is_not_stable_yet() {
        let mut group = four_stable();
        group[1] =
            ValidatorObservation::new("v2", SyncState::Synced, WINDOW - Duration::from_secs(1));
        let d = decide(&group, None);
        assert_eq!(d.max_unavailable, 0);
        assert_eq!(d.reason, PdbReason::NotYetStable(vec!["v2".to_string()]));
    }

    #[test]
    fn stability_window_boundary_is_inclusive() {
        let mut group = four_stable();
        group[1] = ValidatorObservation::new("v2", SyncState::Synced, WINDOW);
        assert_eq!(decide(&group, None).max_unavailable, 1);
    }

    #[test]
    fn zero_stability_window_only_requires_synced() {
        let group = vec![
            ValidatorObservation::new("v1", SyncState::Synced, Duration::ZERO),
            ValidatorObservation::new("v2", SyncState::Synced, Duration::ZERO),
            ValidatorObservation::new("v3", SyncState::Synced, Duration::ZERO),
            ValidatorObservation::new("v4", SyncState::Synced, Duration::ZERO),
        ];
        let topology = full_mesh_topology(&ids(&group), None);
        let policy = PdbPolicy {
            stability_window: Duration::ZERO,
        };
        assert_eq!(
            calculate(&group, &topology, &[], &policy).max_unavailable,
            1
        );
    }

    #[test]
    fn precedence_catching_up_over_unknown_over_not_stable() {
        let group = vec![
            catching_up("a"),
            ValidatorObservation::new("b", SyncState::Unknown, Duration::ZERO),
            ValidatorObservation::new("c", SyncState::Synced, Duration::ZERO),
            stable("d"),
        ];
        assert_eq!(decide(&group, None).reason.code(), "catching-up");

        let group = vec![
            ValidatorObservation::new("b", SyncState::Unknown, Duration::ZERO),
            ValidatorObservation::new("c", SyncState::Synced, Duration::ZERO),
            stable("d"),
            stable("e"),
        ];
        assert_eq!(decide(&group, None).reason.code(), "health-unknown");
    }

    #[test]
    fn three_validators_at_default_threshold_cannot_lose_one() {
        // n=3 → f=0 → all three are required.
        let group: Vec<_> = ["a", "b", "c"].iter().map(|i| stable(i)).collect();
        let d = decide(&group, None);
        assert_eq!(d.max_unavailable, 0);
        assert_eq!(d.reason.code(), "quorum-floor");
        match d.reason {
            PdbReason::QuorumFloor(v) => {
                assert_eq!(v.healthy, 2);
                assert_eq!(v.required, 3);
            }
            other => panic!("unexpected reason {other:?}"),
        }
    }

    #[test]
    fn operator_can_lower_min_quorum_to_allow_a_disruption() {
        let group: Vec<_> = ["a", "b", "c"].iter().map(|i| stable(i)).collect();
        let d = decide(&group, Some(2));
        assert_eq!(d.max_unavailable, 1);
        assert!(d.quorum_verified);
    }

    #[test]
    fn raising_min_quorum_to_group_size_blocks() {
        let d = decide(&four_stable(), Some(4));
        assert_eq!(d.max_unavailable, 0);
        assert_eq!(d.reason.code(), "quorum-floor");
    }

    #[test]
    fn single_validator_is_never_relaxed() {
        let d = decide(&[stable("only")], None);
        assert_eq!(d.max_unavailable, 0);
        assert_eq!(d.reason.code(), "quorum-floor");
    }

    #[test]
    fn two_validators_are_never_relaxed() {
        let group = vec![stable("a"), stable("b")];
        assert_eq!(decide(&group, None).max_unavailable, 0);
    }

    #[test]
    fn min_quorum_is_clamped_into_a_valid_range() {
        // 0 clamps up to 1, which allows disjoint quorums → unsafe topology.
        let d = decide(&four_stable(), Some(0));
        assert_eq!(d.max_unavailable, 0);
        assert_eq!(d.reason.code(), "topology-unsafe");
        // 99 clamps down to 4 → no headroom.
        let d = decide(&four_stable(), Some(99));
        assert_eq!(d.reason.code(), "quorum-floor");
    }

    #[test]
    fn low_threshold_that_permits_disjoint_quorums_is_unsafe() {
        // 2 of 4 lets {a,b} and {c,d} each form a quorum.
        let d = decide(&four_stable(), Some(2));
        assert_eq!(d.max_unavailable, 0);
        match d.reason {
            PdbReason::TopologyUnsafe(why) => assert!(why.contains("disjoint"), "{why}"),
            other => panic!("unexpected reason {other:?}"),
        }
    }

    #[test]
    fn missing_dependency_in_topology_is_unsafe() {
        let group = four_stable();
        let mut topology = full_mesh_topology(&ids(&group), None);
        topology
            .nodes
            .get_mut("v1")
            .unwrap()
            .validators
            .push("ghost".to_string());
        let d = calculate(&group, &topology, &[], &PdbPolicy::default());
        assert_eq!(d.max_unavailable, 0);
        match d.reason {
            PdbReason::TopologyUnsafe(why) => assert!(why.contains("ghost"), "{why}"),
            other => panic!("unexpected reason {other:?}"),
        }
    }

    #[test]
    fn group_member_missing_from_topology_fails_closed() {
        let group = four_stable();
        let topology = full_mesh_topology(&["v1".to_string(), "v2".to_string()], None);
        let d = calculate(&group, &topology, &[], &PdbPolicy::default());
        assert_eq!(d.max_unavailable, 0);
        assert_eq!(d.reason.code(), "topology-unsafe");
    }

    #[test]
    fn unhealthy_peer_outside_the_group_removes_the_headroom() {
        // Topology: 4 validators, one of which (v4) is outside the PDB group and down.
        let all = vec!["v1", "v2", "v3", "v4"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        let topology = full_mesh_topology(&all, None); // threshold 3 of 4
        let group = vec![stable("v1"), stable("v2"), stable("v3")];

        let healthy_peer = [stable("v4")];
        assert_eq!(
            calculate(&group, &topology, &healthy_peer, &PdbPolicy::default()).max_unavailable,
            1
        );

        // v4 down: evicting any of v1..v3 leaves only 2 healthy of 4 (< 3).
        let down_peer = [catching_up("v4")];
        let d = calculate(&group, &topology, &down_peer, &PdbPolicy::default());
        assert_eq!(d.max_unavailable, 0);
        assert_eq!(d.reason.code(), "quorum-floor");

        // v4 absent from the observations entirely → also fail closed.
        let d = calculate(&group, &topology, &[], &PdbPolicy::default());
        assert_eq!(d.max_unavailable, 0);
    }

    #[test]
    fn larger_groups_tolerate_one_disruption_at_the_default_threshold() {
        for n in 4..=12 {
            let group: Vec<_> = (0..n).map(|i| stable(&format!("v{i}"))).collect();
            let d = decide(&group, None);
            assert_eq!(d.max_unavailable, 1, "n={n}");
            assert!(d.quorum_verified, "n={n}");
        }
    }

    #[test]
    fn decision_is_independent_of_member_order() {
        let mut group = four_stable();
        let forward = decide(&group, None);
        group.reverse();
        assert_eq!(decide(&group, None), forward);

        let mut group = vec![catching_up("z"), catching_up("a"), stable("m"), stable("n")];
        let one = decide(&group, None);
        group.reverse();
        assert_eq!(decide(&group, None), one);
    }

    #[test]
    fn removal_violation_reports_the_affected_node() {
        let topology = full_mesh_topology(&["a".into(), "b".into(), "c".into()], None);
        let health: HashMap<&str, bool> = [("a", true), ("b", true), ("c", true)].into();
        let v = removal_violation(&topology, &health, "a").expect("violation");
        assert_eq!(v.removed, "a");
        assert_eq!(v.required, 3);
        assert_eq!(v.healthy, 2);
        assert!(v.node == "b" || v.node == "c");
    }

    #[test]
    fn reason_codes_and_display_are_stable() {
        assert_eq!(PdbReason::SyncedAndStable.code(), "synced-and-stable");
        assert_eq!(
            PdbReason::CatchingUp(vec!["a".into(), "b".into()]).to_string(),
            "catching up: a,b"
        );
        let v = QuorumViolation {
            removed: "a".into(),
            node: "b".into(),
            healthy: 2,
            required: 3,
        };
        assert_eq!(
            PdbReason::QuorumFloor(v).to_string(),
            "evicting a would leave b with 2 healthy of 3 required"
        );
    }

    #[test]
    fn unverified_relaxation_is_flagged() {
        let bad = PdbDecision {
            max_unavailable: 1,
            reason: PdbReason::SyncedAndStable,
            quorum_verified: false,
        };
        assert!(bad.violates_quorum_invariant());
    }
}