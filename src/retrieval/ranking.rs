//! Rank fusion and scoring (WP-07 task 6, RQ-11, RQ-14).
//!
//! Three separate, tested scoring functions:
//! - [`rrf_fuse`]: deterministic one-based Reciprocal Rank Fusion.
//! - [`legacy_reference_score`]: the upstream raw-scale formula, retained as a
//!   reference oracle/baseline for tests — never the native ranking policy.
//! - [`native_score`]: the calibrated native scorer with all components on a
//!   documented [0,1] scale.
//!
//! Zero, NaN and non-finite values never enter fusion (RQ-14).

/// The RRF constant k (upstream value).
pub const RRF_K: f64 = 60.0;

/// One ranked candidate list from a retrieval leg.
///
/// `rank` is ONE-BASED and must be dense (1, 2, 3, ...) over the leg's results.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedList {
    /// The leg name (for explanations): "lexical" or "dense".
    pub leg: &'static str,
    /// The explicit weight for this leg in fusion.
    pub weight: f64,
    /// (entity_id, one_based_rank) pairs, in rank order.
    pub entries: Vec<(crate::domain::id::EntityId, usize)>,
}

/// RRF contribution per entity across legs.
#[derive(Debug, Clone, Default)]
pub struct RrfResult {
    /// entity -> accumulated raw RRF score.
    pub scores: std::collections::BTreeMap<crate::domain::id::EntityId, f64>,
    /// entity -> per-leg ranks (for explanations).
    pub leg_ranks: std::collections::BTreeMap<crate::domain::id::EntityId, Vec<(String, usize)>>,
}

/// Deterministic one-based RRF over the given ranked lists.
///
/// `RRF(d) = sum_j w_j / (k + rank_j(d))`
///
/// Lists with a non-positive weight are ignored (they contribute nothing).
/// Duplicate entries within a single leg are refused (the caller must dedupe).
pub fn rrf_fuse(legs: &[RankedList]) -> RrfResult {
    let mut result = RrfResult::default();
    for leg in legs {
        if !(leg.weight > 0.0 && leg.weight.is_finite()) {
            continue;
        }
        for (id, rank) in &leg.entries {
            debug_assert!(*rank >= 1, "RRF ranks must be one-based");
            let contribution = leg.weight / (RRF_K + *rank as f64);
            *result.scores.entry(*id).or_insert(0.0) += contribution;
            result
                .leg_ranks
                .entry(*id)
                .or_default()
                .push((leg.leg.to_string(), *rank));
        }
    }
    result
}

/// Normalize a raw RRF score to [0,1].
///
/// `R(d) = RRF(d) / (sum_j (w_j / (k + 1)))`
///
/// The denominator uses the ACTIVE rankers (weight > 0) and their explicit
/// weights; the maximum possible raw RRF (every active leg ranking d first)
/// maps to exactly 1.0.
pub fn normalize_rrf(raw: f64, legs: &[RankedList]) -> f64 {
    let denom: f64 = legs
        .iter()
        .filter(|l| l.weight > 0.0 && l.weight.is_finite())
        .map(|l| l.weight / (RRF_K + 1.0))
        .sum();
    if denom <= 0.0 || !raw.is_finite() {
        0.0
    } else {
        (raw / denom).clamp(0.0, 1.0)
    }
}

/// The upstream raw-scale reference scorer (oracle/baseline only).
///
/// `score = raw_rrf + priority_bonus` where `priority_bonus` is the legacy
/// clamped priority in [0, 0.05].
///
/// Retained for test oracles and scale-regression detection (T-RANK-02): it
/// demonstrates the documented scale problem where raw RRF (~0.03 max) is
/// dwarfed by unit-scale cosine penalties in MMR.
pub fn legacy_reference_score(raw_rrf: f64, priority: f64) -> f64 {
    if !raw_rrf.is_finite() {
        return 0.0;
    }
    raw_rrf + priority.clamp(0.0, 0.05)
}

/// The calibrated native scorer (design §10.3).
///
/// All components share a documented scale:
/// - `r`: normalized RRF in [0,1]
/// - `g`: bounded graph contribution in [0,1]
/// - `p`: clamped priority in [0,1]
///
/// `S(d) = 0.90 * max(R(d), 0.80 * G(d)) + 0.10 * P(d)`
///
/// The sample coefficients are tuning seeds, not performance claims; they are
/// frozen here so held-out evaluation (RQ-24) has a stable reference.
pub fn native_score(r: f64, g: f64, p: f64) -> f64 {
    let r = if r.is_finite() {
        r.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let g = if g.is_finite() {
        g.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let p = if p.is_finite() {
        p.clamp(0.0, 1.0)
    } else {
        0.0
    };
    (0.90 * r.max(0.80 * g) + 0.10 * p).clamp(0.0, 1.0)
}

/// Cosine similarity between two vectors.
///
/// Returns 0.0 for null, zero-norm, or non-finite vectors — never a
/// NaN or a zero-vector placeholder.
pub fn cosine(a: Option<&[f32]>, b: Option<&[f32]>) -> f64 {
    let (Some(a), Some(b)) = (a, b) else {
        return 0.0;
    };
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b.iter()) {
        if !x.is_finite() || !y.is_finite() {
            return 0.0;
        }
        let (x, y) = (*x as f64, *y as f64);
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 || !dot.is_finite() {
        return 0.0;
    }
    (dot / (na.sqrt() * nb.sqrt())).clamp(-1.0, 1.0)
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
    fn rrf_hand_computed_one_based() {
        // T-RANK-01: exact one-based arithmetic.
        // a: rank 1, b: rank 2, c: rank 3 (lexical, w=1)
        // a: rank 1, b: rank 2 (dense, w=1)
        let legs = vec![
            RankedList {
                leg: "lexical",
                weight: 1.0,
                entries: vec![(eid(1), 1), (eid(2), 2), (eid(3), 3)],
            },
            RankedList {
                leg: "dense",
                weight: 1.0,
                entries: vec![(eid(1), 1), (eid(2), 2)],
            },
        ];
        let r = rrf_fuse(&legs);

        // a: 1/(60+1) + 1/(60+1) = 2/61
        assert!((r.scores[&eid(1)] - 2.0 / 61.0).abs() < 1e-12);
        // b: 1/(60+2) + 1/(60+2) = 2/62
        assert!((r.scores[&eid(2)] - 2.0 / 62.0).abs() < 1e-12);
        // c: 1/(60+3) = 1/63 (lexical only)
        assert!((r.scores[&eid(3)] - 1.0 / 63.0).abs() < 1e-12);
    }

    #[test]
    fn rrf_missing_leg_still_fuses() {
        // T-RANK-01: a candidate appearing in only one leg is still ranked.
        let legs = vec![RankedList {
            leg: "lexical",
            weight: 1.0,
            entries: vec![(eid(1), 1), (eid(2), 2)],
        }];
        let r = rrf_fuse(&legs);
        assert_eq!(r.scores.len(), 2);
        assert!((r.scores[&eid(1)] - 1.0 / 61.0).abs() < 1e-12);
    }

    #[test]
    fn rrf_empty_rankings_yield_empty() {
        let r = rrf_fuse(&[]);
        assert!(r.scores.is_empty());
    }

    #[test]
    fn rrf_zero_weight_leg_ignored() {
        let legs = vec![
            RankedList {
                leg: "lexical",
                weight: 1.0,
                entries: vec![(eid(1), 1)],
            },
            RankedList {
                leg: "dense",
                weight: 0.0,
                entries: vec![(eid(2), 1)],
            },
        ];
        let r = rrf_fuse(&legs);
        assert_eq!(r.scores.len(), 1);
        assert!(r.scores.contains_key(&eid(1)));
        assert!(!r.scores.contains_key(&eid(2)));
    }

    #[test]
    fn normalize_rrf_max_maps_to_one() {
        // T-RANK-01: two legs, both ranking d first -> R = 1.0 exactly.
        let legs = vec![
            RankedList {
                leg: "lexical",
                weight: 1.0,
                entries: vec![(eid(1), 1)],
            },
            RankedList {
                leg: "dense",
                weight: 1.0,
                entries: vec![(eid(1), 1)],
            },
        ];
        let raw = 2.0 / 61.0;
        assert!((normalize_rrf(raw, &legs) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn normalize_rrf_uses_active_rankers_only() {
        // A zero-weight leg must not enter the denominator.
        let legs = vec![
            RankedList {
                leg: "lexical",
                weight: 1.0,
                entries: vec![(eid(1), 1)],
            },
            RankedList {
                leg: "dense",
                weight: 0.0,
                entries: vec![],
            },
        ];
        // Single active leg ranking first: 1/61 / (1/61) = 1.0
        assert!((normalize_rrf(1.0 / 61.0, &legs) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn normalize_rrf_rejects_non_finite() {
        let legs = vec![RankedList {
            leg: "lexical",
            weight: 1.0,
            entries: vec![],
        }];
        assert_eq!(normalize_rrf(f64::NAN, &legs), 0.0);
        assert_eq!(normalize_rrf(f64::INFINITY, &legs), 0.0);
    }

    #[test]
    fn legacy_reference_score_scale_documented() {
        // T-RANK-02: the raw-scale formula is detectably different.
        // Max raw RRF (2/61 ~ 0.03279) + max priority (0.05) ~ 0.08279.
        let max_raw = 2.0 / 61.0;
        let s = legacy_reference_score(max_raw, 1.0);
        assert!((s - (max_raw + 0.05)).abs() < 1e-12);
        assert!(s < 0.09, "legacy raw scale stays tiny vs cosine ~1.0");
    }

    #[test]
    fn native_score_bounded_and_calibrated() {
        // T-RANK-02: native S is in [0,1] and cosine-scale compatible.
        assert!((native_score(1.0, 0.0, 0.0) - 0.90).abs() < 1e-12);
        assert!((native_score(0.0, 1.0, 0.0) - 0.72).abs() < 1e-12);
        assert!((native_score(1.0, 1.0, 1.0) - 1.0).abs() < 1e-12);
        assert!(native_score(0.0, 0.0, 0.0) == 0.0);
        // Non-finite inputs are treated as zero, never propagated.
        assert_eq!(native_score(f64::NAN, f64::NAN, f64::NAN), 0.0);
    }

    #[test]
    fn cosine_handles_missing_and_zero_vectors() {
        // RQ-14: missing vectors and zero vectors yield 0.0, not NaN.
        assert_eq!(cosine(None, Some(&[1.0, 0.0])), 0.0);
        assert_eq!(cosine(Some(&[0.0, 0.0]), Some(&[1.0, 1.0])), 0.0);
        assert_eq!(cosine(Some(&[1.0]), Some(&[1.0, 1.0])), 0.0);
        let nan = vec![f32::NAN, 1.0];
        assert_eq!(cosine(Some(&nan), Some(&[1.0, 1.0])), 0.0);
    }

    #[test]
    fn cosine_unit_vectors() {
        let a = vec![1.0f32, 0.0];
        let b = vec![1.0f32, 0.0];
        let c = vec![0.0f32, 1.0];
        assert!((cosine(Some(&a), Some(&b)) - 1.0).abs() < 1e-12);
        assert!((cosine(Some(&a), Some(&c)) - 0.0).abs() < 1e-12);
    }
}
