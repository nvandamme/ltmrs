//! Bounded graph expansion (WP-07 task 7, RQ-12).
//!
//! Expands a set of seed memories over the canonical relation graph with:
//! - Edge-specific policies (supersession vs contradiction vs relatedness).
//! - Bounded depth, fan-out and total node limits.
//! - Provenance: every reached node records the path that discovered it.
//! - No unlimited hub summation: a high-degree node contributes a bounded,
//!   deduplicated source contribution, never an unbounded bonus.
//!
//! The effective scope is applied to EVERY graph step — a neighbor is only
//! traversed if it is itself in scope (RV-13).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::retrieval::scope::EffectiveScope;
use ltmrs_domain::id::EntityId;
use ltmrs_domain::memory::Memory;
use ltmrs_domain::relation::{Relation, RelationType};

/// Edge-specific traversal policy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GraphPolicy {
    /// Maximum traversal depth (number of hops from a seed).
    pub max_depth: usize,
    /// Maximum neighbors to traverse per node.
    pub max_fan_out: usize,
    /// Maximum total nodes reachable (across all seeds).
    pub max_nodes: usize,
    /// Whether supersession edges (new -> old) are traversed.
    pub traverse_supersession: bool,
    /// Whether contradiction edges are traversed.
    pub traverse_contradiction: bool,
    /// Whether related/supports edges are traversed.
    pub traverse_related: bool,
}

impl Default for GraphPolicy {
    fn default() -> Self {
        Self {
            max_depth: 2,
            max_fan_out: 4,
            max_nodes: 24,
            traverse_supersession: true,
            traverse_contradiction: true,
            traverse_related: true,
        }
    }
}

/// The provenance path by which a node was reached.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphPath {
    /// The ordered node IDs from the seed to this node (inclusive).
    pub nodes: Vec<EntityId>,
    /// The edge types along the path (nodes.len() - 1 entries).
    pub edge_types: Vec<RelationType>,
}

/// One node reached by graph expansion.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphNode {
    pub id: EntityId,
    /// The seed from which this node was reached.
    pub seed: EntityId,
    /// The path from the seed to this node.
    pub path: GraphPath,
    /// The depth (hops) from the seed.
    pub depth: usize,
}

/// The result of bounded graph expansion.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GraphExpansion {
    /// Reached nodes, keyed by ID (deduplicated: first path wins).
    pub nodes: BTreeMap<EntityId, GraphNode>,
    /// Whether the expansion hit a limit (depth, fan-out or node cap).
    pub truncated: bool,
}

/// Expand the graph from the given seeds under the policy and scope.
///
/// `neighbors` provides the canonical edges for a node (from one snapshot).
/// `is_in_scope` is the scope predicate applied to every candidate neighbor —
/// the SAME effective scope used by the retrieval legs.
pub fn expand(
    seeds: &[EntityId],
    neighbors: &impl Fn(EntityId) -> Vec<Relation>,
    is_in_scope: &impl Fn(EntityId) -> bool,
    policy: &GraphPolicy,
) -> GraphExpansion {
    expand_ranked(seeds, neighbors, is_in_scope, &|_| 0.0, policy)
}

/// Rank-ordered bounded expansion: the shared traversal core with
/// confidence-ordered candidate selection. Candidates at each node are
/// ordered by `rank` (descending, EntityId tiebreak) BEFORE the fanout cap
/// applies, so the strongest neighbors traverse when the cap binds. A
/// constant rank reproduces [`expand`]'s EntityId order exactly.
pub fn expand_ranked(
    seeds: &[EntityId],
    neighbors: &impl Fn(EntityId) -> Vec<Relation>,
    is_in_scope: &impl Fn(EntityId) -> bool,
    rank: &impl Fn(EntityId) -> f64,
    policy: &GraphPolicy,
) -> GraphExpansion {
    let mut result = GraphExpansion::default();
    let mut visited: BTreeSet<EntityId> = BTreeSet::new();
    let mut queue: VecDeque<(EntityId, EntityId, usize, GraphPath)> = VecDeque::new();

    for seed in seeds {
        if visited.contains(seed) {
            continue;
        }
        visited.insert(*seed);
        let path = GraphPath {
            nodes: vec![*seed],
            edge_types: vec![],
        };
        result.nodes.insert(
            *seed,
            GraphNode {
                id: *seed,
                seed: *seed,
                path,
                depth: 0,
            },
        );
        queue.push_back((
            *seed,
            *seed,
            0,
            GraphPath {
                nodes: vec![*seed],
                edge_types: vec![],
            },
        ));
    }

    while let Some((node, seed, depth, path)) = queue.pop_front() {
        if depth >= policy.max_depth {
            continue;
        }
        if result.nodes.len() >= policy.max_nodes {
            result.truncated = true;
            continue;
        }

        let edges = neighbors(node);
        let mut candidates: Vec<(EntityId, RelationType, f64)> = edges
            .iter()
            .filter_map(|e| {
                let other = if e.source == node { e.target } else { e.source };
                if other == node {
                    return None;
                }
                Some((other, e.relation_type, rank(other)))
            })
            .collect();

        // Rank order (strongest first), deterministic EntityId tiebreak.
        candidates.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        candidates.dedup_by_key(|(id, _, _)| *id);

        let mut traversed = 0usize;
        for (other, etype, _) in candidates {
            if traversed >= policy.max_fan_out {
                result.truncated = true;
                break;
            }
            if !traversable(etype, policy) {
                continue;
            }
            if visited.contains(&other) {
                continue;
            }
            // Scope is enforced on EVERY graph step (RV-13): an out-of-scope
            // neighbor is never traversed, even if it is a hub. Skipped
            // neighbors consume no budget: only actually traversed nodes
            // count, so eligible nodes are never starved by skipped ones.
            if !is_in_scope(other) {
                continue;
            }
            traversed += 1;
            visited.insert(other);

            if result.nodes.len() >= policy.max_nodes {
                result.truncated = true;
                continue;
            }

            let mut new_path = path.clone();
            new_path.nodes.push(other);
            new_path.edge_types.push(etype);

            result.nodes.insert(
                other,
                GraphNode {
                    id: other,
                    seed,
                    path: new_path.clone(),
                    depth: depth + 1,
                },
            );
            queue.push_back((other, seed, depth + 1, new_path));
        }
    }

    result
}

/// Whether an edge type is traversable under the policy.
fn traversable(etype: RelationType, policy: &GraphPolicy) -> bool {
    match etype {
        RelationType::Supersedes | RelationType::SupersededBy => policy.traverse_supersession,
        RelationType::Contradicts => policy.traverse_contradiction,
        RelationType::Supports | RelationType::RelatedTo => policy.traverse_related,
    }
}

/// Upstream reference decay for tool-level graph scores (upstream core.ts:
/// score = confidence x 0.6^depth). Single owner for the constant so the
/// engine and tool paths cannot drift.
pub const TOOL_SCORE_DECAY: f64 = 0.6;

/// Tool-level graph score: the reached memory's confidence decayed by depth.
pub fn tool_score(confidence: f64, depth: u32) -> f64 {
    confidence * TOOL_SCORE_DECAY.powi(depth as i32)
}

/// Tool-level traversal policy: the upstream bounds (fanout 5 per node,
/// depth 2) on the shared engine node budget. The fanout differs from
/// [`GraphPolicy::default`] by reference contract, not invention.
pub fn tool_policy() -> GraphPolicy {
    GraphPolicy {
        max_fan_out: 5,
        ..Default::default()
    }
}

/// The bounded graph contribution G(d) for a candidate (design §10.3).
///
/// G(d) is the maximum bounded eligible path contribution from the selected
/// seeds. A seed itself (depth 0) contributes 0.0: it is a query match, not a
/// graph path — giving it a bonus would let the graph term dominate relevance
/// for the top candidates (RV-11). Nodes reached via edges get a depth-decayed
/// contribution; deduplicated sources mean a hub node does not accumulate an
/// unlimited bonus.
pub fn graph_contribution(expansion: &GraphExpansion, candidate: EntityId) -> f64 {
    let Some(node) = expansion.nodes.get(&candidate) else {
        return 0.0;
    };
    if node.depth == 0 {
        return 0.0;
    }
    // Each hop halves: depth 1 -> 0.5, depth 2 -> 0.25. Bounded in (0, 0.5].
    0.5f64.powi(node.depth as i32)
}

/// Build the scope predicate closure helper for graph expansion.
pub fn scope_predicate<'a>(
    scope: &'a EffectiveScope,
    memories: &'a BTreeMap<EntityId, Memory>,
) -> impl Fn(EntityId) -> bool + 'a {
    move |id: EntityId| match memories.get(&id) {
        Some(m) => scope.is_eligible(m),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ltmrs_domain::command::Scope;
    use ltmrs_domain::id::EntityId;
    use ltmrs_domain::memory::Instant;
    use uuid::Uuid;

    fn eid(n: u64) -> EntityId {
        EntityId::new(Uuid::from_u128(n as u128))
    }

    fn rel(id: u64, s: EntityId, t: EntityId, ty: RelationType) -> Relation {
        Relation::new(eid(id), s, t, ty, None, Instant::new(1))
    }

    #[test]
    fn expansion_respects_depth_limit() {
        // 1 -> 2 -> 3 chain; depth limit 1 must stop at 2.
        let edges: BTreeMap<EntityId, Vec<Relation>> = BTreeMap::from([
            (
                eid(1),
                vec![rel(10, eid(1), eid(2), RelationType::RelatedTo)],
            ),
            (
                eid(2),
                vec![rel(11, eid(2), eid(3), RelationType::RelatedTo)],
            ),
        ]);
        let policy = GraphPolicy {
            max_depth: 1,
            ..Default::default()
        };
        let exp = expand(
            &[eid(1)],
            &|id| edges.get(&id).cloned().unwrap_or_default(),
            &|_| true,
            &policy,
        );
        assert!(exp.nodes.contains_key(&eid(1)));
        assert!(exp.nodes.contains_key(&eid(2)));
        assert!(
            !exp.nodes.contains_key(&eid(3)),
            "depth 1 must not reach the 2nd hop"
        );
    }

    #[test]
    fn expansion_respects_node_limit() {
        // Hub 1 connected to 10 leaves; max_nodes 5 must truncate.
        let mut edges: BTreeMap<EntityId, Vec<Relation>> = BTreeMap::new();
        let mut hub_edges = Vec::new();
        for i in 1..=10 {
            hub_edges.push(rel(i, eid(1), eid(i + 10), RelationType::RelatedTo));
        }
        edges.insert(eid(1), hub_edges);

        let policy = GraphPolicy {
            max_nodes: 5,
            max_fan_out: 10,
            ..Default::default()
        };
        let exp = expand(
            &[eid(1)],
            &|id| edges.get(&id).cloned().unwrap_or_default(),
            &|_| true,
            &policy,
        );
        assert!(exp.truncated);
        assert!(exp.nodes.len() <= 5);
    }

    /// Wave C0: traversal order is pinned, not incidental. `expand` must
    /// visit same-rank neighbors in EntityId order even when the adjacency
    /// list arrives scrambled ( BTreeMap iteration is sorted, but storage
    /// order is not the contract — selection order is).
    #[test]
    fn expansion_order_is_deterministic_entity_id() {
        // Hub 1 with leaves inserted in scrambled order; fanout 2 must
        // take the two smallest EntityIds regardless of insertion order.
        let mut hub_edges = Vec::new();
        for i in [30u64, 10, 50, 20, 40] {
            hub_edges.push(rel(i, eid(1), eid(i), RelationType::RelatedTo));
        }
        let edges: BTreeMap<EntityId, Vec<Relation>> = BTreeMap::from([(eid(1), hub_edges)]);
        let policy = GraphPolicy {
            max_fan_out: 2,
            ..Default::default()
        };
        let exp = expand(
            &[eid(1)],
            &|id| edges.get(&id).cloned().unwrap_or_default(),
            &|_| true,
            &policy,
        );
        assert!(exp.nodes.contains_key(&eid(1)));
        assert!(
            exp.nodes.contains_key(&eid(10)),
            "smallest leaf must traverse"
        );
        assert!(
            exp.nodes.contains_key(&eid(20)),
            "second-smallest leaf must traverse"
        );
        assert!(
            !exp.nodes.contains_key(&eid(30)),
            "fanout cap must stop at 2 leaves"
        );
        assert!(exp.truncated, "capped traversal must report truncation");
    }

    /// Wave C0: ranked selection takes the strongest neighbors first, with
    /// EntityId order as the deterministic tiebreak.
    #[test]
    fn ranked_expansion_selects_strongest_first_with_id_tiebreak() {
        let mut hub_edges = Vec::new();
        for i in [10u64, 20, 30] {
            hub_edges.push(rel(i, eid(1), eid(i), RelationType::RelatedTo));
        }
        let edges: BTreeMap<EntityId, Vec<Relation>> = BTreeMap::from([(eid(1), hub_edges)]);
        let rank = |id: EntityId| {
            if id == eid(20) {
                0.5
            } else {
                0.9
            }
        };
        let policy = GraphPolicy {
            max_fan_out: 2,
            ..Default::default()
        };
        let exp = expand_ranked(
            &[eid(1)],
            &|id| edges.get(&id).cloned().unwrap_or_default(),
            &|_| true,
            &rank,
            &policy,
        );
        // Tie 30 vs 10 at 0.9 breaks toward the smaller id.
        assert!(
            exp.nodes.contains_key(&eid(10)),
            "tie must break to smaller id"
        );
        assert!(exp.nodes.contains_key(&eid(30)), "0.9 outranks 0.5");
        assert!(
            !exp.nodes.contains_key(&eid(20)),
            "fanout cap must drop the weakest"
        );
        assert!(exp.truncated);
    }

    #[test]
    fn expansion_applies_scope_to_every_step() {
        // 1 -> 2 -> 3; node 2 is out of scope, so 3 must not be reached via 2.
        let edges: BTreeMap<EntityId, Vec<Relation>> = BTreeMap::from([
            (
                eid(1),
                vec![rel(10, eid(1), eid(2), RelationType::RelatedTo)],
            ),
            (
                eid(2),
                vec![rel(11, eid(2), eid(3), RelationType::RelatedTo)],
            ),
        ]);
        let in_scope = |id: EntityId| id != eid(2);
        let exp = expand(
            &[eid(1)],
            &|id| edges.get(&id).cloned().unwrap_or_default(),
            &in_scope,
            &GraphPolicy::default(),
        );
        assert!(exp.nodes.contains_key(&eid(1)));
        assert!(
            !exp.nodes.contains_key(&eid(2)),
            "out-of-scope node must not be traversed"
        );
        assert!(
            !exp.nodes.contains_key(&eid(3)),
            "a node only reachable through an out-of-scope hop must not appear"
        );
    }

    #[test]
    fn expansion_records_provenance_paths() {
        let edges: BTreeMap<EntityId, Vec<Relation>> = BTreeMap::from([(
            eid(1),
            vec![rel(10, eid(1), eid(2), RelationType::Supersedes)],
        )]);
        let exp = expand(
            &[eid(1)],
            &|id| edges.get(&id).cloned().unwrap_or_default(),
            &|_| true,
            &GraphPolicy::default(),
        );
        let node = &exp.nodes[&eid(2)];
        assert_eq!(node.seed, eid(1));
        assert_eq!(node.depth, 1);
        assert_eq!(node.path.nodes, vec![eid(1), eid(2)]);
        assert_eq!(node.path.edge_types, vec![RelationType::Supersedes]);
    }

    #[test]
    fn hub_does_not_accumulate_unlimited_bonus() {
        // A hub reached from 5 seeds still contributes at most the depth-decay
        // bound — deduplicated, not summed per seed.
        let mut edges: BTreeMap<EntityId, Vec<Relation>> = BTreeMap::new();
        for i in 1..=5u64 {
            edges.insert(
                eid(i),
                vec![rel(100 + i, eid(i), eid(99), RelationType::RelatedTo)],
            );
        }
        let exp = expand(
            (1..=5).map(eid).collect::<Vec<_>>().as_slice(),
            &|id| edges.get(&id).cloned().unwrap_or_default(),
            &|_| true,
            &GraphPolicy::default(),
        );
        // Hub 99 is reached once (first path wins), depth 1.
        let g = graph_contribution(&exp, eid(99));
        assert!(
            (g - 0.5).abs() < 1e-12,
            "hub contribution must be bounded, not summed"
        );
    }

    #[test]
    fn edge_policy_filters_supersession() {
        let edges: BTreeMap<EntityId, Vec<Relation>> = BTreeMap::from([(
            eid(1),
            vec![rel(10, eid(1), eid(2), RelationType::Supersedes)],
        )]);
        let policy = GraphPolicy {
            traverse_supersession: false,
            ..Default::default()
        };
        let exp = expand(
            &[eid(1)],
            &|id| edges.get(&id).cloned().unwrap_or_default(),
            &|_| true,
            &policy,
        );
        assert!(
            !exp.nodes.contains_key(&eid(2)),
            "supersession edges must not be traversed when disabled"
        );
    }

    #[test]
    fn graph_contribution_zero_for_unreached() {
        let exp = GraphExpansion::default();
        assert_eq!(graph_contribution(&exp, eid(1)), 0.0);
    }

    /// Skipped neighbors must not consume the fan-out budget: a hub whose
    /// low-ID neighbors are out of scope still reaches the eligible one.
    #[test]
    fn skipped_neighbors_do_not_consume_fan_out() {
        use std::collections::BTreeMap;
        let edges: BTreeMap<EntityId, Vec<Relation>> = BTreeMap::from([(
            eid(1),
            vec![
                rel(10, eid(1), eid(2), RelationType::Supports),
                rel(11, eid(1), eid(3), RelationType::Supports),
                rel(12, eid(1), eid(4), RelationType::Supports),
            ],
        )]);
        let policy = GraphPolicy {
            max_fan_out: 2,
            ..Default::default()
        };
        let in_scope = |id: EntityId| id == eid(1) || id == eid(4);
        let exp = expand(
            &[eid(1)],
            &|id| edges.get(&id).cloned().unwrap_or_default(),
            &in_scope,
            &policy,
        );
        assert!(
            exp.nodes.contains_key(&eid(4)),
            "eligible neighbor must be reached despite skipped ones"
        );
        assert!(!exp.nodes.contains_key(&eid(2)));
        assert!(!exp.nodes.contains_key(&eid(3)));
    }

    #[allow(dead_code)]
    fn _scope_predicate_helper() {
        // Verify the helper compiles and is usable.
        let scope = EffectiveScope::resolve(&Scope::default()).unwrap();
        let memories: BTreeMap<EntityId, Memory> = BTreeMap::new();
        let pred = scope_predicate(&scope, &memories);
        assert!(!pred(eid(1)));
    }

    /// Wave B5a: rank-ordered selection — a hub with more neighbors than
    /// the fanout cap traverses the highest-ranked ones (confidence order
    /// for the tool path), and the cap is indicated as truncation.
    #[test]
    fn ranked_expansion_prefers_high_confidence_neighbors() {
        let mut hub_edges = Vec::new();
        for i in 1..=8u64 {
            hub_edges.push(rel(i, eid(1), eid(i + 10), RelationType::RelatedTo));
        }
        let edges: BTreeMap<EntityId, Vec<Relation>> = BTreeMap::from([(eid(1), hub_edges)]);
        // Rank = confidence doubles as the order key: leaf 11 lowest.
        let rank = |id: EntityId| (id.as_uuid().as_u128() - 10) as f64 / 10.0;
        let exp = expand_ranked(
            &[eid(1)],
            &|id| edges.get(&id).cloned().unwrap_or_default(),
            &|_| true,
            &rank,
            &tool_policy(),
        );
        assert!(exp.truncated, "fanout cap must be indicated");
        let mut reached: Vec<EntityId> = exp
            .nodes
            .keys()
            .copied()
            .filter(|id| *id != eid(1))
            .collect();
        reached.sort();
        let expected: Vec<EntityId> = (14..=18).map(eid).collect();
        assert_eq!(reached, expected, "only the top-5 ranks may traverse");
    }

    /// Wave B5a: tool scores follow the upstream reference contract
    /// confidence x 0.6^depth (depth 0 is the seed: full confidence).
    #[test]
    fn tool_score_matches_upstream_decay() {
        assert!((tool_score(0.4, 1) - 0.24).abs() < 1e-12);
        assert!((tool_score(0.95, 2) - 0.95 * 0.36).abs() < 1e-12);
        assert!((tool_score(0.7, 0) - 0.7).abs() < 1e-12);
    }

    /// Wave B5a: the tool policy pins the upstream bounds (fanout 5,
    /// depth 2) on the shared engine budget (max nodes).
    #[test]
    fn tool_policy_matches_upstream_bounds() {
        let policy = tool_policy();
        assert_eq!(policy.max_fan_out, 5);
        assert_eq!(policy.max_depth, 2);
        assert_eq!(policy.max_nodes, GraphPolicy::default().max_nodes);
    }
}
