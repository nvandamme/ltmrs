//! Retrieval shaping: explanations, collapse, spans, mean vectors, priority (moved verbatim from `engine.rs`).

use std::collections::{BTreeMap, BTreeSet};

use super::RetrievalRequest;
use crate::retrieval::bundles::Bundles;
use crate::retrieval::context;
use crate::retrieval::explain::{
    CandidateExplanation, GraphPathRecord, LegRank, RetrievalExplanation, ScoreComponents,
};
use crate::retrieval::graph_expansion;
use crate::retrieval::mmr;
use crate::retrieval::ranking;
use crate::retrieval::scope::EffectiveScope;
use crate::search::row::SearchRow;
use ltmrs_domain::id::{EntityId, ModelFingerprint, StoreGeneration};
use ltmrs_domain::memory::Memory;

pub(crate) struct ExplanationInputs<'a> {
    pub(crate) scope: &'a EffectiveScope,
    pub(crate) store_gen: StoreGeneration,
    pub(crate) req: &'a RetrievalRequest,
    pub(crate) rrf: &'a ranking::RrfResult,
    pub(crate) bundles: &'a Bundles,
    pub(crate) expansion: &'a graph_expansion::GraphExpansion,
    pub(crate) scored: &'a [(EntityId, ScoreComponents)],
    pub(crate) mmr: &'a mmr::MmrResult,
    pub(crate) selected: &'a [EntityId],
    pub(crate) context: &'a context::ContextResult,
    pub(crate) conflict_notice: Option<&'a str>,
}

/// Build the explanation for this call (task 12). Pure: deterministic from
/// the pipeline's stage outputs.
pub(crate) fn build_explanation(inputs: &ExplanationInputs<'_>) -> RetrievalExplanation {
    let store_gen = inputs.store_gen;
    let mut explanation = RetrievalExplanation::new(store_gen, inputs.req.model_fingerprint);
    explanation.scope_filter = inputs.scope.to_lance_filter();
    explanation.graph_truncated = inputs.expansion.truncated;
    explanation.conflict_notice = inputs.conflict_notice.map(|s| s.to_string());
    explanation.excluded_by_budget = inputs.context.excluded.clone();

    for (id, sc) in inputs.scored {
        let position = match inputs.selected.iter().position(|s| s == id) {
            Some(p) => p + 1,
            None => 0,
        };
        let leg_ranks = inputs
            .rrf
            .leg_ranks
            .get(id)
            .map(|v| {
                v.iter()
                    .map(|(leg, rank)| LegRank {
                        leg: leg.clone(),
                        rank: *rank,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let graph_path = inputs.expansion.nodes.get(id).map(|n| GraphPathRecord {
            nodes: n.path.nodes.clone(),
            edge_types: n
                .path
                .edge_types
                .iter()
                .map(|t| t.as_str().to_string())
                .collect(),
            depth: n.depth,
        });
        let excluded_by_diversity = inputs.mmr.excluded.contains(id);
        explanation.candidates.insert(
            *id,
            CandidateExplanation {
                id: *id,
                position,
                leg_ranks,
                scores: sc.clone(),
                graph_path,
                protected: inputs.bundles.protected.contains(id),
                excluded_by_diversity,
            },
        );
    }
    explanation
}

/// Collapse chunk hits by parent memory, preserving first-seen order.
pub(crate) fn collapse_by_parent(rows: &[SearchRow]) -> Vec<EntityId> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for row in rows {
        if seen.insert(row.memory_id) {
            out.push(row.memory_id);
        }
    }
    out
}

/// The best-matching span for a memory in a set of rows.
pub(crate) fn span_for(rows: &[SearchRow], id: EntityId) -> Option<(u64, u64)> {
    rows.iter()
        .find(|r| r.memory_id == id)
        .map(|r| (r.char_start, r.char_end))
}

/// Find a row for a memory (for its embedding).
/// Mean embedding over a memory's rows that carry one, unioned across both
/// legs and scoped to a single model space (AD-04: never mix vector
/// spaces). With an explicit fingerprint only that space averages; without
/// one (lexical-only requests) the dominant space wins (most rows, ties to
/// the smallest fingerprint) so blue-green windows still average coherently
/// instead of mixing. MMR diversity must see the whole document, not just
/// the first chunk: near-duplicate tails survive nothing otherwise. Lexical
/// rows come first so a chunk present in both legs counts once (deduped by
/// chunk id); rows outside the chosen space are skipped.
///
/// Two disclosed approximations: the election counts rows while the mean
/// dedupes chunks (asymmetric leg duplication can flip a close election
/// versus a per-chunk count), and each memory elects independently, so MMR
/// cosines across memories can span spaces mid-migration. Both degrade
/// ordering only, never safety: the fallback is still a real document
/// vector, never a zero placeholder.
pub(crate) fn mean_vector(
    lexical_rows: &[SearchRow],
    dense_rows: &[SearchRow],
    id: EntityId,
    fingerprint: Option<ModelFingerprint>,
) -> Option<Vec<f32>> {
    let fingerprint = fingerprint.or_else(|| {
        // Dominant space among this memory's embedded rows: count per
        // fingerprint over both legs, ties to the smallest (deterministic).
        let mut counts: BTreeMap<ModelFingerprint, usize> = BTreeMap::new();
        for row in lexical_rows.iter().chain(dense_rows.iter()) {
            if row.memory_id == id && row.embedding.is_some() {
                *counts.entry(row.model_fingerprint).or_default() += 1;
            }
        }
        counts
            .into_iter()
            .max_by(|(fa, ca), (fb, cb)| ca.cmp(cb).then(fb.cmp(fa)))
            .map(|(fp, _)| fp)
    });
    let fingerprint = fingerprint?;
    let mut sum: Option<Vec<f32>> = None;
    let mut count = 0usize;
    let mut seen_chunks = BTreeSet::new();
    for row in lexical_rows.iter().chain(dense_rows.iter()) {
        if row.memory_id != id || row.embedding.is_none() {
            continue;
        }
        if row.model_fingerprint != fingerprint {
            continue;
        }
        let vector = row.embedding.as_ref().expect("checked above");
        let entry = sum.get_or_insert_with(|| vec![0.0; vector.len()]);
        // Mixed widths cannot average: keep the first width found.
        if entry.len() != vector.len() {
            continue;
        }
        // Same chunk in both legs counts once (lexical rows come first).
        if !seen_chunks.insert(row.chunk_id) {
            continue;
        }
        for (acc, v) in entry.iter_mut().zip(vector.iter()) {
            *acc += *v;
        }
        count += 1;
    }
    if count == 0 {
        return None;
    }
    let mut mean = sum.expect("counted vector exists");
    for acc in mean.iter_mut() {
        *acc /= count as f32;
    }
    Some(mean)
}

/// Priority P(d) in [0,1]: feedback balance plus a small confidence term.
pub(crate) fn priority(m: &Memory) -> f64 {
    let pos = m.positive_feedback as f64;
    let neg = (m.negative_feedback + m.negative_hits) as f64;
    let balance = if pos + neg > 0.0 {
        pos / (pos + neg)
    } else {
        0.5
    };
    (0.5 * balance + 0.5 * m.confidence.clamp(0.0, 1.0)).clamp(0.0, 1.0)
}
