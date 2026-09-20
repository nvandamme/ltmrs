//! Supersession chains and protected conflict/correction bundles (WP-07 task 8, RQ-12).
//!
//! A known valid superseding memory is not merely a slightly higher-scored
//! neighbor: actionable context redirects to the current applicable record and
//! obsolete advice is excluded from the primary answer by default. Two
//! competing active replacements or explicit contradictions form an UNRESOLVED
//! CONFLICT BUNDLE: both required sides are preserved when within scope and
//! budget, and MMR must not delete the contradiction because the sentences are
//! semantically similar.

use std::collections::{BTreeMap, BTreeSet};

use crate::domain::id::EntityId;
use crate::domain::relation::RelationType;

/// A resolved supersession chain: one current winner plus its obsolete lineage.
#[derive(Debug, Clone, PartialEq)]
pub struct SupersessionChain {
    /// The current (newest) memory in the chain.
    pub current: EntityId,
    /// The obsolete memories it supersedes, newest-first.
    pub superseded: Vec<EntityId>,
}

/// A protected bundle that must survive diversification.
#[derive(Debug, Clone, PartialEq)]
pub enum Bundle {
    /// A supersession chain: only `current` is primary; lineage is preserved
    /// in the explanation, obsolete advice is excluded from the primary answer.
    Supersession(SupersessionChain),
    /// An unresolved conflict: BOTH sides are required; if the budget cannot
    /// hold both, a concise conflict notice with IDs must be emitted instead of
    /// silently showing only one claim.
    Conflict {
        /// The competing memories (2+), stable order.
        members: Vec<EntityId>,
        /// The explicit contradiction edges linking them.
        edges: Vec<EntityId>,
    },
}

/// The outcome of bundle resolution over a candidate set.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bundles {
    /// Supersession chains among the candidates.
    pub supersessions: Vec<SupersessionChain>,
    /// Unresolved conflict bundles among the candidates.
    pub conflicts: Vec<Bundle>,
    /// Candidate IDs that are obsolete (superseded by an in-scope current).
    /// These are excluded from the primary answer by default.
    pub obsolete: BTreeSet<EntityId>,
    /// Candidate IDs that are protected (must not be dropped by MMR).
    pub protected: BTreeSet<EntityId>,
}

/// Resolve supersession chains and conflict bundles among the candidates.
///
/// `relations` is the full canonical relation set (from one snapshot).
/// `is_in_scope` is the effective scope predicate: an out-of-scope replacement
/// must NOT leak or silently re-promote obsolete advice.
pub fn resolve_bundles(
    candidates: &[EntityId],
    relations: &[crate::domain::relation::Relation],
    is_in_scope: &impl Fn(EntityId) -> bool,
) -> Bundles {
    let candidate_set: BTreeSet<EntityId> = candidates.iter().copied().collect();

    // Supersession edges: source -> supersedes -> target (source is newer).
    // Build the "who supersedes me" direction over the FULL relation graph so a
    // stale candidate is caught even when its successor is not itself recalled.
    let mut superseded_by: BTreeMap<EntityId, Vec<EntityId>> = BTreeMap::new();
    for r in relations {
        if r.relation_type == RelationType::Supersedes && r.source != r.target {
            superseded_by.entry(r.target).or_default().push(r.source);
        }
    }

    let mut bundles = Bundles::default();
    let mut chains: BTreeMap<EntityId, Vec<EntityId>> = BTreeMap::new();

    for id in candidates {
        // Walk forward to the current head of this candidate's chain.
        let mut head = *id;
        let mut seen = BTreeSet::new();
        while let Some(nexts) = superseded_by.get(&head) {
            // Deterministic: take the smallest successor.
            let next = *nexts.iter().min().unwrap();
            if !seen.insert(next) || next == head {
                break;
            }
            head = next;
        }
        if head == *id {
            // This candidate is already the current record.
            continue;
        }
        // The candidate is superseded by `head`. The chain is actionable only if
        // the current record is in scope; an out-of-scope replacement must not
        // leak or silently re-promote the obsolete instruction.
        if !is_in_scope(head) {
            continue;
        }
        bundles.obsolete.insert(*id);
        bundles.protected.insert(head);
        chains.entry(head).or_default().push(*id);
    }

    for (head, mut superseded) in chains {
        superseded.sort();
        bundles.supersessions.push(SupersessionChain {
            current: head,
            superseded,
        });
    }

    // Conflict bundles: explicit contradicts edges between active candidates.
    let mut conflict_edges: Vec<&crate::domain::relation::Relation> = relations
        .iter()
        .filter(|r| {
            r.relation_type == RelationType::Contradicts
                && candidate_set.contains(&r.source)
                && candidate_set.contains(&r.target)
        })
        .collect();
    conflict_edges.sort_by_key(|r| r.id);
    for r in conflict_edges {
        // Both sides must be in scope to form a required bundle; an out-of-scope
        // side is not leaked.
        if !is_in_scope(r.source) || !is_in_scope(r.target) {
            continue;
        }
        let bundle = Bundle::Conflict {
            members: vec![r.source, r.target],
            edges: vec![r.id],
        };
        if !bundles
            .conflicts
            .iter()
            .any(|existing| same_conflict(existing, &bundle))
        {
            bundles.conflicts.push(bundle.clone());
            bundles.protected.insert(r.source);
            bundles.protected.insert(r.target);
        }
    }

    bundles
}

fn same_conflict(a: &Bundle, b: &Bundle) -> bool {
    match (a, b) {
        (Bundle::Conflict { members: ma, .. }, Bundle::Conflict { members: mb, .. }) => ma == mb,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::memory::Instant;
    use crate::domain::relation::Relation;
    use uuid::Uuid;

    fn eid(n: u64) -> EntityId {
        EntityId::new(Uuid::from_u128(n as u128))
    }

    fn rel(id: u64, s: EntityId, t: EntityId, ty: RelationType) -> Relation {
        Relation::new(eid(id), s, t, ty, None, Instant::new(1))
    }

    #[test]
    fn supersession_chain_marks_obsolete_and_protects_current() {
        // 3 -> 2 -> 1 chain: 3 is current, 2 and 1 are obsolete.
        let relations = vec![
            rel(100, eid(3), eid(2), RelationType::Supersedes),
            rel(101, eid(2), eid(1), RelationType::Supersedes),
        ];
        let b = resolve_bundles(&[eid(1), eid(2), eid(3)], &relations, &|_| true);

        assert_eq!(b.supersessions.len(), 1);
        assert_eq!(b.supersessions[0].current, eid(3));
        assert_eq!(b.supersessions[0].superseded, vec![eid(1), eid(2)]);
        assert!(b.obsolete.contains(&eid(1)));
        assert!(b.obsolete.contains(&eid(2)));
        assert!(b.protected.contains(&eid(3)));
        assert!(!b.obsolete.contains(&eid(3)));
    }

    #[test]
    fn out_of_scope_replacement_does_not_leak() {
        // 2 supersedes 1, but 2 is out of scope: 1 must NOT be marked obsolete
        // (the replacement cannot be shown, so the old one is not silently
        // re-demoted either — the chain is simply not actionable).
        let relations = vec![rel(100, eid(2), eid(1), RelationType::Supersedes)];
        let b = resolve_bundles(&[eid(1), eid(2)], &relations, &|id| id == eid(1));
        assert!(b.supersessions.is_empty());
        assert!(b.obsolete.is_empty());
    }

    #[test]
    fn conflict_bundle_preserves_both_sides() {
        let relations = vec![rel(100, eid(1), eid(2), RelationType::Contradicts)];
        let b = resolve_bundles(&[eid(1), eid(2)], &relations, &|_| true);
        assert_eq!(b.conflicts.len(), 1);
        match &b.conflicts[0] {
            Bundle::Conflict { members, .. } => {
                assert_eq!(members, &vec![eid(1), eid(2)]);
            }
            _ => panic!("expected conflict bundle"),
        }
        assert!(b.protected.contains(&eid(1)));
        assert!(b.protected.contains(&eid(2)));
    }

    #[test]
    fn out_of_scope_conflict_side_not_leaked() {
        let relations = vec![rel(100, eid(1), eid(2), RelationType::Contradicts)];
        let b = resolve_bundles(&[eid(1), eid(2)], &relations, &|id| id == eid(1));
        assert!(b.conflicts.is_empty());
    }

    #[test]
    fn duplicate_conflict_edges_deduplicated() {
        let relations = vec![
            rel(100, eid(1), eid(2), RelationType::Contradicts),
            rel(101, eid(1), eid(2), RelationType::Contradicts),
        ];
        let b = resolve_bundles(&[eid(1), eid(2)], &relations, &|_| true);
        assert_eq!(b.conflicts.len(), 1);
    }

    #[test]
    fn self_supersession_is_ignored() {
        let relations = vec![rel(100, eid(1), eid(1), RelationType::Supersedes)];
        let b = resolve_bundles(&[eid(1)], &relations, &|_| true);
        assert!(b.supersessions.is_empty());
        assert!(b.obsolete.is_empty());
    }
}
