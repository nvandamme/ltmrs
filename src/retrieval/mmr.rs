//! Bundle-aware diversification via MMR (WP-07 task 9, RQ-11).
//!
//! `MMR(d) = lambda * S(d) - (1 - lambda) * max_selected max(0, cosine(d, s))`
//!
//! Uses the CALIBRATED native score S(d) in [0,1] (not raw RRF), so diversity
//! cannot overwhelm relevance by construction (RV-11). Missing vectors use a
//! documented lexical diversification fallback (penalty 0), never a
//! zero-vector placeholder. Protected bundle members are never dropped.

use std::collections::BTreeSet;

use crate::domain::id::EntityId;
use crate::retrieval::ranking::cosine;

/// MMR configuration. The lambda is a tuning seed, frozen for held-out
/// evaluation (RQ-24).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MmrConfig {
    /// Relevance weight in [0,1].
    pub lambda: f64,
    /// Maximum number of items to select.
    pub max_items: usize,
}

impl Default for MmrConfig {
    fn default() -> Self {
        Self {
            lambda: 0.70,
            max_items: 10,
        }
    }
}

/// One candidate for MMR selection.
#[derive(Debug, Clone, PartialEq)]
pub struct MmrCandidate {
    pub id: EntityId,
    /// The calibrated native score S(d) in [0,1].
    pub score: f64,
    /// The embedding, if present.
    pub vector: Option<Vec<f32>>,
}

/// The outcome of MMR selection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MmrResult {
    /// Selected IDs in selection order.
    pub selected: Vec<EntityId>,
    /// IDs that were eligible but not selected (truncated for diversity or
    /// budget), in their original candidate order.
    pub excluded: Vec<EntityId>,
    /// Whether the selection hit max_items.
    pub truncated: bool,
}

/// Run MMR over the candidates.
///
/// `protected` are bundle members that must be selected whenever they are
/// eligible (MMR must not delete a contradiction because the sentences are
/// semantically similar).
pub fn mmr_select(
    candidates: &[MmrCandidate],
    protected: &BTreeSet<EntityId>,
    config: &MmrConfig,
) -> MmrResult {
    let mut result = MmrResult::default();
    if candidates.is_empty() || config.max_items == 0 {
        return result;
    }

    // Deterministic order: score descending, then ID ascending (stable tie-break).
    let mut ordered: Vec<&MmrCandidate> = candidates.iter().collect();
    ordered.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut selected_vecs: Vec<(EntityId, Option<Vec<f32>>)> = Vec::new();
    let mut selected_ids: BTreeSet<EntityId> = BTreeSet::new();
    let mut remaining: Vec<&MmrCandidate> = ordered.clone();

    // Protected members are selected first, in their ranked order, so a
    // conflict pair cannot be dropped for being similar to each other.
    remaining.retain(|c| {
        if !protected.contains(&c.id) {
            return true;
        }
        if result.selected.len() >= config.max_items {
            result.truncated = true;
            return true;
        }
        result.selected.push(c.id);
        selected_ids.insert(c.id);
        selected_vecs.push((c.id, c.vector.clone()));
        false
    });

    // Greedy MMR: at each step, pick the candidate maximizing
    //   lambda * S(d) - (1 - lambda) * max_selected max(0, cosine(d, s))
    // among the remaining candidates. Ties break by (score desc, ID asc).
    while !remaining.is_empty() && result.selected.len() < config.max_items {
        let mut best_idx = 0usize;
        let mut best_mmr = f64::NEG_INFINITY;
        for (i, c) in remaining.iter().enumerate() {
            let mut max_sim = 0.0f64;
            for (_, svec) in &selected_vecs {
                let sim = cosine(c.vector.as_deref(), svec.as_deref());
                if sim > max_sim {
                    max_sim = sim;
                }
            }
            let mmr = config.lambda * c.score - (1.0 - config.lambda) * max_sim;
            // Strictly greater, or equal with a smaller ID, wins the tie-break.
            if mmr > best_mmr || (mmr == best_mmr && c.id < remaining[best_idx].id) {
                best_mmr = mmr;
                best_idx = i;
            }
        }
        let chosen = remaining.remove(best_idx);
        result.selected.push(chosen.id);
        selected_ids.insert(chosen.id);
        selected_vecs.push((chosen.id, chosen.vector.clone()));
    }

    if !remaining.is_empty() && result.selected.len() >= config.max_items {
        result.truncated = true;
    }

    // Excluded: eligible candidates not selected, in original order.
    for c in candidates {
        if !selected_ids.contains(&c.id) {
            result.excluded.push(c.id);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::EntityId;
    use uuid::Uuid;

    fn eid(n: u64) -> EntityId {
        EntityId::new(Uuid::from_u128(n as u128))
    }

    #[test]
    fn mmr_prefers_relevance_with_diversity_penalty() {
        // T-RANK-02: a strongly relevant near-duplicate vs irrelevant diverse
        // candidates. With calibrated scores, relevance wins over the tiny raw
        // RRF scale problem.
        let candidates = vec![
            MmrCandidate {
                id: eid(1),
                score: 0.90,
                vector: Some(vec![1.0, 0.0]),
            },
            MmrCandidate {
                id: eid(2),
                score: 0.89,
                vector: Some(vec![0.999, 0.045]), // near-duplicate of 1
            },
            MmrCandidate {
                id: eid(3),
                score: 0.85,
                vector: Some(vec![0.0, 1.0]), // diverse
            },
        ];
        let r = mmr_select(&candidates, &BTreeSet::new(), &MmrConfig::default());
        // 1 first (highest score). 3 before 2: 2's similarity penalty to 1
        // (cosine ~0.9998 * 0.3 = ~0.30) exceeds 3's (0.0).
        assert_eq!(r.selected[0], eid(1));
        assert_eq!(r.selected[1], eid(3));
        assert_eq!(r.selected[2], eid(2));
    }

    #[test]
    fn protected_conflict_pair_never_dropped() {
        // T-RANK-03: near-identical contradictory claims must both survive.
        let candidates = vec![
            MmrCandidate {
                id: eid(1),
                score: 0.5,
                vector: Some(vec![1.0, 0.0]),
            },
            MmrCandidate {
                id: eid(2),
                score: 0.4,
                vector: Some(vec![1.0, 0.0]), // identical to 1
            },
            MmrCandidate {
                id: eid(3),
                score: 0.3,
                vector: Some(vec![0.0, 1.0]),
            },
        ];
        let protected = BTreeSet::from([eid(1), eid(2)]);
        let r = mmr_select(&candidates, &protected, &MmrConfig::default());
        assert!(r.selected.contains(&eid(1)));
        assert!(r.selected.contains(&eid(2)));
        // Both protected are selected before the non-protected 3.
        assert_eq!(r.selected[0], eid(1));
        assert_eq!(r.selected[1], eid(2));
    }

    #[test]
    fn missing_vectors_use_lexical_fallback() {
        // RQ-14: missing vectors never use a zero-vector placeholder; they
        // simply get no diversity penalty and are ranked by score.
        let candidates = vec![
            MmrCandidate {
                id: eid(1),
                score: 0.9,
                vector: None,
            },
            MmrCandidate {
                id: eid(2),
                score: 0.8,
                vector: None,
            },
        ];
        let r = mmr_select(&candidates, &BTreeSet::new(), &MmrConfig::default());
        assert_eq!(r.selected, vec![eid(1), eid(2)]);
    }

    #[test]
    fn stable_tie_breaking_by_id() {
        let candidates = vec![
            MmrCandidate {
                id: eid(2),
                score: 0.5,
                vector: None,
            },
            MmrCandidate {
                id: eid(1),
                score: 0.5,
                vector: None,
            },
        ];
        let r = mmr_select(&candidates, &BTreeSet::new(), &MmrConfig::default());
        assert_eq!(r.selected, vec![eid(1), eid(2)]);
    }

    #[test]
    fn max_items_truncates_and_reports() {
        let candidates = (0..5)
            .map(|i| MmrCandidate {
                id: eid(i),
                score: 0.5,
                vector: None,
            })
            .collect::<Vec<_>>();
        let config = MmrConfig {
            max_items: 2,
            ..Default::default()
        };
        let r = mmr_select(&candidates, &BTreeSet::new(), &config);
        assert_eq!(r.selected.len(), 2);
        assert!(r.truncated);
        assert_eq!(r.excluded.len(), 3);
    }

    #[test]
    fn empty_candidates_yield_empty() {
        let r = mmr_select(&[], &BTreeSet::new(), &MmrConfig::default());
        assert!(r.selected.is_empty());
        assert!(r.excluded.is_empty());
        assert!(!r.truncated);
    }
}
