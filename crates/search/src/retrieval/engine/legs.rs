//! Retrieval legs: readiness, direct/list paths, hydration (moved verbatim from `engine.rs`).

use std::collections::{BTreeMap, BTreeSet};

use super::{Engine, ResultMemory, RetrievalRequest, RetrievalResult};

/// Leg readiness for a retrieval call (task 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Readiness {
    pub(crate) fts_ready: bool,
    pub(crate) dense_ready: bool,
    pub(crate) projection_lag: usize,
}
use crate::retrieval::context;
use crate::retrieval::explain::RetrievalExplanation;
use crate::retrieval::scope::EffectiveScope;
use ltmrs_domain::command::DomainResult;
use ltmrs_domain::id::{EntityId, StoreGeneration};
use ltmrs_domain::memory::Memory;
use ltmrs_domain::relation::Relation;

impl Engine {
    /// Readiness/partial-result reporting (task 10): whether each leg is ready
    /// and how much projection work is still pending. Never claims false
    /// completeness. Scoped to the requested generation/fingerprint so a
    /// blue-green deployment reports readiness for the space it actually reads.
    pub(crate) async fn readiness(
        &self,
        req: &RetrievalRequest,
        store_gen: StoreGeneration,
    ) -> DomainResult<Readiness> {
        let projection_lag = self.repo.projection_lag()?;
        let gen_filter = format!("store_generation = {}", store_gen.as_u64());
        // The FTS leg is ready only if the inverted index exists (without it,
        // lexical recall silently degrades to empty results).
        let fts_ready = self.table.fts_index_ready().await?;
        let dense_ready = if let Some(fp) = req.model_fingerprint {
            self.table
                .count_rows(Some(&format!(
                    "{gen_filter} AND embedding IS NOT NULL AND model_fingerprint = {}",
                    fp.as_u64()
                )))
                .await?
                > 0
        } else {
            false
        };
        Ok(Readiness {
            fts_ready,
            dense_ready,
            projection_lag,
        })
    }

    /// Direct-ID routing: bypasses the ranker; scope still enforced.
    pub(crate) async fn retrieve_direct(
        &self,
        ids: &[EntityId],
        scope: &EffectiveScope,
        store_gen: StoreGeneration,
        req: &RetrievalRequest,
    ) -> DomainResult<RetrievalResult> {
        let memories = self.repo.get_memories(ids)?;
        let mut results = Vec::new();
        for m in memories {
            if scope.is_eligible(&m) {
                results.push(ResultMemory {
                    memory: m,
                    matched_span: None,
                });
            }
        }
        let context = context::assemble_context(
            &results.iter().map(|r| r.memory.clone()).collect::<Vec<_>>(),
            &BTreeSet::new(),
            &req.context_budget,
        );
        let mut explanation = RetrievalExplanation::new(store_gen, req.model_fingerprint);
        let readiness = self.readiness(req, store_gen).await?;
        explanation.fts_ready = readiness.fts_ready;
        explanation.dense_ready = readiness.dense_ready;
        explanation.projection_lag = readiness.projection_lag;
        // Direct reads bypass the ranker, but pending projections still
        // mean canonical state the call cannot see: flag it uniformly.
        // The leg flags are informational here (legs are unused); only
        // the lag drives partial on this path, deliberately.
        explanation.partial = readiness.projection_lag > 0;
        Ok(RetrievalResult {
            results,
            context,
            explanation,
        })
    }

    /// Empty-query routing: upstream list/priority behavior (no dense recall).
    pub(crate) async fn retrieve_list(
        &self,
        scope: &EffectiveScope,
        store_gen: StoreGeneration,
        req: &RetrievalRequest,
    ) -> DomainResult<RetrievalResult> {
        let export = self.repo.export_snapshot()?;
        let mut eligible: Vec<Memory> = export
            .memories
            .into_iter()
            .filter(|m| scope.is_eligible(m))
            .collect();
        eligible.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.positive_feedback.cmp(&a.positive_feedback))
                .then_with(|| a.id.cmp(&b.id))
        });
        eligible.truncate(req.result_limit);

        let results = eligible
            .iter()
            .map(|m| ResultMemory {
                memory: m.clone(),
                matched_span: None,
            })
            .collect();
        let context = context::assemble_context(&eligible, &BTreeSet::new(), &req.context_budget);
        let mut explanation = RetrievalExplanation::new(store_gen, req.model_fingerprint);
        explanation.empty_query = true;
        // List reads bypass the legs, but pending projections still mean
        // the listing may be incomplete: flag it like every other path.
        // fts_ready/dense_ready stay at their defaults here: leg
        // readiness is meaningless for canonical-snapshot reads (never
        // false-completeness, since partial carries the lag).
        let lag = self.repo.projection_lag()?;
        explanation.projection_lag = lag;
        explanation.partial = lag > 0;
        Ok(RetrievalResult {
            results,
            context,
            explanation,
        })
    }

    /// Valid no-answer result: a nonempty query legitimately returns nothing.
    pub(crate) async fn no_answer_result(
        &self,
        scope: &EffectiveScope,
        store_gen: StoreGeneration,
        req: &RetrievalRequest,
        dense_failed: bool,
    ) -> DomainResult<RetrievalResult> {
        let mut explanation = RetrievalExplanation::new(store_gen, req.model_fingerprint);
        explanation.no_match = true;
        explanation.scope_filter = scope.to_lance_filter();
        let readiness = self.readiness(req, store_gen).await?;
        explanation.fts_ready = readiness.fts_ready;
        explanation.dense_ready = readiness.dense_ready && !dense_failed;
        explanation.projection_lag = readiness.projection_lag;
        // Same degraded-leg rule as the ranked path: a no-match with
        // pending projections, a missing dense leg, a failed dense leg, or
        // an unready lexical leg is partial — the absence of hits may be a
        // gap, not a true no-answer.
        explanation.partial = readiness.projection_lag > 0
            || (req.model_fingerprint.is_some() && !readiness.dense_ready)
            || dense_failed
            || !readiness.fts_ready;
        Ok(RetrievalResult {
            results: vec![],
            context: context::ContextResult::default(),
            explanation,
        })
    }

    /// Hydrate a set of IDs from canonical state (single snapshot).
    pub(crate) fn hydrate(&self, ids: &[EntityId]) -> DomainResult<BTreeMap<EntityId, Memory>> {
        let memories = self.repo.get_memories(ids)?;
        Ok(memories.into_iter().map(|m| (m.id, m)).collect())
    }

    /// Build the neighbor map for graph expansion from one relation snapshot.
    pub(crate) fn build_neighbor_map(
        &self,
        relations: &[Relation],
    ) -> DomainResult<BTreeMap<EntityId, Vec<Relation>>> {
        let mut map: BTreeMap<EntityId, Vec<Relation>> = BTreeMap::new();
        for r in relations {
            map.entry(r.source).or_default().push(r.clone());
            map.entry(r.target).or_default().push(r.clone());
        }
        Ok(map)
    }
}
