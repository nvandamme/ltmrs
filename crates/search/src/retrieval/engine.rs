//! The recall engine (WP-07): the full retrieval pipeline.
//!
//! Resolve profile and scope
//!   -> direct-ID / empty-query routing
//!   -> lexical candidates + semantic candidates
//!   -> canonical eligibility/revision validation
//!   -> collapse chunks by parent memory
//!   -> rank fusion
//!   -> resolve authoritative supersession / conflict context
//!   -> bounded graph enrichment
//!   -> calibrated relevance and priority
//!   -> bundle-aware diversification
//!   -> context budget and explanation
//!
//! Each stage is a separate, tested function; the engine is their composition.

use std::collections::BTreeSet;

use crate::retrieval::bundles::{self, Bundle};
use crate::retrieval::context::{self, ContextBudget};
use crate::retrieval::explain::{RetrievalExplanation, ScoreComponents};
use crate::retrieval::graph_expansion::{self, GraphPolicy};
use crate::retrieval::mmr::{self, MmrCandidate, MmrConfig};
use crate::retrieval::ranking::{self, RankedList};
use crate::retrieval::scope::EffectiveScope;
use crate::search::row::SearchRow;
use crate::search::table::SearchTable;
use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult, Scope};
use ltmrs_domain::id::{EntityId, ModelFingerprint, StoreGeneration};
use ltmrs_domain::memory::Memory;

#[cfg(test)]
mod diagnostics_tests;
mod legs;
#[cfg(test)]
mod match_tests;
#[cfg(test)]
mod quality_tests;
#[cfg(test)]
mod recall_tests;
#[cfg(test)]
mod scoring_tests;
mod shaping;
#[cfg(test)]
mod test_support;

use shaping::{
    ExplanationInputs, build_explanation, collapse_by_parent, mean_vector, priority, span_for,
};

/// Boxed future resolving to one vector per passage input.
/// Module-level alias: the nested result type trips `type_complexity` inline.
pub type PassageVectorsFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = DomainResult<Vec<Vec<f32>>>> + Send + 'a>>;

/// The query embedder seam: the engine never blocks on inference; the daemon
/// provides an async embedding service behind this trait.
pub trait QueryEmbedder: Send + Sync {
    fn embed_query<'a>(
        &'a self,
        query: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = DomainResult<Vec<f32>>> + Send + 'a>>;

    /// Embed catalog texts with the Passage (document) role for dense
    /// candidate proposal (guide ranking). Defaults to unsupported so
    /// query-only embedders keep compiling; the token fallback covers them.
    /// Role separation is load-bearing: passages must never go through the
    /// Query-role seam above.
    fn embed_passages<'a>(&'a self, texts: &'a [String]) -> PassageVectorsFuture<'a> {
        let _ = texts;
        Box::pin(async move {
            Err(DomainError::new(
                DomainErrorCode::Validation,
                "passage embedding unsupported by this embedder",
            ))
        })
    }
}

/// Retrieval request.
#[derive(Debug, Clone)]
pub struct RetrievalRequest {
    /// The user query. Empty means list/priority mode (upstream behavior).
    pub query: String,
    /// Direct IDs requested verbatim (bypass the ranker).
    pub direct_ids: Vec<EntityId>,
    /// The raw request scope.
    pub scope: Scope,
    /// The store generation to read. None resolves the active pointer per
    /// request (blue-green cutover); Some pins a generation (rollback reads,
    /// tests). Never a commit cursor (RV-07).
    pub store_generation: Option<StoreGeneration>,
    /// The model fingerprint for the dense leg (None: dense leg disabled).
    pub model_fingerprint: Option<ModelFingerprint>,
    /// How many candidates to fetch per leg.
    pub candidate_limit: usize,
    /// Minimum cosine similarity for the dense leg (no-answer rule, RQ-14).
    /// `None` means no threshold (all nearest rows are candidates, including
    /// anti-correlated noise — explicit opt-out for calibration runs).
    /// A threshold is a calibration input (WP-12), never a universal truth
    /// gate. Real calibration on held-out labels is still open (task 9).
    pub min_similarity: Option<f64>,
    /// How many final results to return.
    pub result_limit: usize,
    /// Context budget for the assembled context.
    pub context_budget: ContextBudget,
    /// MMR configuration.
    pub mmr_config: MmrConfig,
    /// Graph expansion policy.
    pub graph_policy: GraphPolicy,
}

/// Default dense floor: anti-correlated rows (negative cosine) are never
/// candidates. This is a principled floor, not a calibration: genuinely
/// similar content scores at or above zero, while uncalibrated nearest
/// noise below it is cut. Real thresholds calibrate on held-out labels
/// (WP-12 task 9, still open) and override this via `min_similarity`.
pub const DEFAULT_MIN_SIMILARITY: f64 = 0.0;

impl Default for RetrievalRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            direct_ids: vec![],
            scope: Scope::default(),
            store_generation: None,
            model_fingerprint: None,
            candidate_limit: 50,
            min_similarity: Some(DEFAULT_MIN_SIMILARITY),
            result_limit: 10,
            context_budget: ContextBudget::default(),
            mmr_config: MmrConfig::default(),
            graph_policy: GraphPolicy::default(),
        }
    }
}

/// One result memory with its matched spans.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultMemory {
    pub memory: Memory,
    /// The best-matching chunk span (byte offsets into the rendered text).
    pub matched_span: Option<(u64, u64)>,
}

/// The full retrieval result.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RetrievalResult {
    /// The final ranked results.
    pub results: Vec<ResultMemory>,
    /// The assembled context.
    pub context: context::ContextResult,
    /// The explanation for this call.
    pub explanation: RetrievalExplanation,
}

/// The recall engine: composes the repository (canonical truth) with the
/// search projection (candidate legs) into one consistent retrieval call.
pub struct Engine {
    repo: std::sync::Arc<ltmrs_service::repository::CanonicalRepository>,
    table: SearchTable,
}

impl Engine {
    pub fn new(
        repo: std::sync::Arc<ltmrs_service::repository::CanonicalRepository>,
        table: SearchTable,
    ) -> Self {
        Self { repo, table }
    }

    /// Execute a retrieval request end-to-end.
    pub async fn retrieve(
        &self,
        req: &RetrievalRequest,
        embedder: &dyn QueryEmbedder,
    ) -> DomainResult<RetrievalResult> {
        // Stage 1: resolve the effective scope ONCE.
        let scope = EffectiveScope::resolve(&req.scope)?;
        // Unpinned requests follow the active pointer per call so a cutover
        // takes effect on the next query; pinned requests stay put. A
        // repository error propagates (never masked as generation 1).
        // A retired pin fails loudly: canonical rows carry no generation
        // stamp, so serving current data under an old pin would lie about
        // what was read. Generation-scoped history needs versioned storage
        // first (recorded as future work, not silently faked here).
        let live = self.repo.store_generation()?;
        if let Some(pinned) = req.store_generation
            && pinned != live
        {
            return Err(DomainError::new(
                DomainErrorCode::StaleGeneration,
                format!(
                    "pinned generation {} is retired (live is {}): unpin or re-pin to live",
                    pinned.as_u64(),
                    live.as_u64()
                ),
            ));
        }
        let store_gen = req.store_generation.unwrap_or(live);

        // Stage 2: direct-ID routing (bypasses the ranker entirely).
        if !req.direct_ids.is_empty() {
            return self
                .retrieve_direct(&req.direct_ids, &scope, store_gen, req)
                .await;
        }

        // Stage 3: empty-query routing (list/priority behavior).
        if req.query.trim().is_empty() {
            return self.retrieve_list(&scope, store_gen, req).await;
        }

        // Stage 4: lexical + dense candidate legs (separate, for explanations).
        let lance_filter = scope.to_lance_filter();
        let gen_filter = format!("store_generation = {}", store_gen.as_u64());
        let combined_filter = match &lance_filter {
            Some(lf) => Some(format!("({gen_filter}) AND ({lf})")),
            None => Some(gen_filter),
        };
        let cf = combined_filter.as_deref();

        let lexical_rows = self
            .table
            .fts_query(&req.query, req.candidate_limit, cf)
            .await?;

        let (dense_rows, dense_failed): (Vec<SearchRow>, bool) =
            if let Some(fp) = req.model_fingerprint {
                // AD-04: never mix vectors from different model spaces. The dense
                // leg is constrained to the requested model fingerprint so a
                // blue-green deployment never compares incompatible embeddings.
                // Embedding-free rows are excluded explicitly: newer Lance
                // versions skip NULL vectors, but the invariant belongs to
                // the query, not to engine-version luck.
                let dense_filter = match cf {
                    Some(f) => format!(
                        "{f} AND model_fingerprint = {} AND embedding IS NOT NULL",
                        fp.as_u64()
                    ),
                    None => format!(
                        "model_fingerprint = {} AND embedding IS NOT NULL",
                        fp.as_u64()
                    ),
                };
                // Graceful degradation: a failing dense leg (backpressure,
                // shutdown race, misconfigured embedder) falls back to
                // lexical-only with partial=true instead of failing recall
                // that already won lexical results.
                match embedder.embed_query(&req.query).await {
                    Ok(qvec) => match self
                        .table
                        .vector_query(&qvec, req.candidate_limit, Some(&dense_filter))
                        .await
                    {
                        Ok(hits) => (
                            hits.into_iter()
                                .filter(|(_row, distance)| match req.min_similarity {
                                    // No-answer rule (RQ-14): nearest-neighbor rank
                                    // is not proof of relevance. When a threshold
                                    // is configured, drop rows whose cosine
                                    // similarity falls below it (Lance cosine
                                    // distance = 1 - similarity).
                                    Some(min) => (1.0 - *distance as f64) >= min,
                                    None => true,
                                })
                                .map(|(row, _d)| row)
                                .collect(),
                            false,
                        ),
                        Err(_) => (vec![], true),
                    },
                    Err(_) => (vec![], true),
                }
            } else {
                (vec![], false)
            };

        // Stage 5: canonical hydration + eligibility validation.
        //
        // RV-13: eligibility (project, type, date, min_confidence, lifecycle)
        // is a frequently-mutable predicate. We must NOT filter only the first
        // N hits and return the survivors as "the best N" — that drops eligible
        // candidates that ranked lower. Instead we hydrate the FULL bounded
        // candidate pool from canonical state and keep every eligible candidate,
        // preserving each leg's rank order. The pool is bounded by
        // `candidate_limit`, so this is always finite.
        let lexical_ids = collapse_by_parent(&lexical_rows);
        let dense_ids = collapse_by_parent(&dense_rows);

        // Hydrate the union of both legs in one snapshot-consistent read.
        let mut union: Vec<EntityId> = Vec::new();
        for id in lexical_ids.iter().chain(dense_ids.iter()) {
            if !union.contains(id) {
                union.push(*id);
            }
        }
        let eligible: BTreeSet<EntityId> = self
            .repo
            .get_memories(&union)?
            .into_iter()
            .filter(|m| scope.is_eligible(m))
            .map(|m| m.id)
            .collect();

        // Keep each leg's rank order, restricted to eligible candidates.
        let lexical_ids: Vec<EntityId> = lexical_ids
            .into_iter()
            .filter(|id| eligible.contains(id))
            .collect();
        let dense_ids: Vec<EntityId> = dense_ids
            .into_iter()
            .filter(|id| eligible.contains(id))
            .collect();

        // Stage 6: rank fusion (one-based RRF over the eligible legs).
        let mut legs = Vec::new();
        if !lexical_ids.is_empty() {
            legs.push(RankedList {
                leg: "lexical",
                weight: 1.0,
                entries: lexical_ids
                    .iter()
                    .enumerate()
                    .map(|(i, id)| (*id, i + 1))
                    .collect(),
            });
        }
        if !dense_ids.is_empty() {
            legs.push(RankedList {
                leg: "dense",
                weight: 1.0,
                entries: dense_ids
                    .iter()
                    .enumerate()
                    .map(|(i, id)| (*id, i + 1))
                    .collect(),
            });
        }

        let rrf = ranking::rrf_fuse(&legs);
        let mut rrf_order: Vec<EntityId> = rrf.scores.keys().copied().collect();
        rrf_order.sort_by(|a, b| {
            rrf.scores[b]
                .partial_cmp(&rrf.scores[a])
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.cmp(b))
        });

        if rrf_order.is_empty() {
            return self
                .no_answer_result(&scope, store_gen, req, dense_failed)
                .await;
        }

        // Stage 7: supersession/conflict bundles (protected context).
        let relations = self.repo.all_relations()?;
        let in_scope = |id: EntityId| {
            self.repo
                .get_memories(std::slice::from_ref(&id))
                .map(|v| v.first().is_some_and(|m| scope.is_eligible(m)))
                .unwrap_or(false)
        };
        let bundles = bundles::resolve_bundles(&rrf_order, &relations, &in_scope);

        // Stage 8: bounded graph expansion from the top seeds.
        let seeds: Vec<EntityId> = rrf_order.iter().take(5).copied().collect();
        let neighbor_map = self.build_neighbor_map(&relations)?;
        let expansion = graph_expansion::expand(
            &seeds,
            &|id| neighbor_map.get(&id).cloned().unwrap_or_default(),
            &|id| {
                self.repo
                    .get_memories(std::slice::from_ref(&id))
                    .map(|v| v.first().is_some_and(|m| scope.is_eligible(m)))
                    .unwrap_or(false)
            },
            &req.graph_policy,
        );

        let mut all_ids = rrf_order.clone();
        for id in expansion.nodes.keys() {
            if !all_ids.contains(id) {
                all_ids.push(*id);
            }
        }
        // Redirect actionable context to the current record (design §10.4):
        // if a candidate is superseded by an in-scope current, pull that
        // current into the pool so it can be returned in place of the obsolete.
        for chain in &bundles.supersessions {
            if !all_ids.contains(&chain.current) {
                all_ids.push(chain.current);
            }
        }

        // Stage 9: calibrated scoring.
        let memories_map = self.hydrate(&all_ids)?;
        let mut scored: Vec<(EntityId, ScoreComponents)> = Vec::new();
        for id in &all_ids {
            let Some(m) = memories_map.get(id) else {
                continue;
            };
            let raw_rrf = rrf.scores.get(id).copied().unwrap_or(0.0);
            let r = ranking::normalize_rrf(raw_rrf, &legs);
            let g = graph_expansion::graph_contribution(&expansion, *id);
            let p = priority(m);
            let s = ranking::native_score(r, g, p);
            let legacy = ranking::legacy_reference_score(raw_rrf, p);
            scored.push((
                *id,
                ScoreComponents {
                    rrf_normalized: r,
                    graph: g,
                    priority: p,
                    native_score: s,
                    legacy_reference: legacy,
                },
            ));
        }
        scored.sort_by(|a, b| {
            b.1.native_score
                .partial_cmp(&a.1.native_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });

        // Stage 10: bundle-aware diversification (MMR).
        // Obsolete (superseded) memories are excluded from the primary answer
        // by default (design §10.4): their lineage lives in the explanation,
        // not in the results.
        let mmr_candidates: Vec<MmrCandidate> = scored
            .iter()
            .filter(|(id, _)| !bundles.obsolete.contains(id))
            .map(|(id, sc)| MmrCandidate {
                id: *id,
                score: sc.native_score,
                vector: mean_vector(&lexical_rows, &dense_rows, *id, req.model_fingerprint),
            })
            .collect();
        let mmr = mmr::mmr_select(&mmr_candidates, &bundles.protected, &req.mmr_config);

        // Stage 11: context budget and explanation.
        let selected: Vec<EntityId> = mmr
            .selected
            .iter()
            .take(req.result_limit)
            .copied()
            .collect();

        let mut results = Vec::new();
        for id in &selected {
            if let Some(m) = memories_map.get(id).cloned() {
                results.push(ResultMemory {
                    matched_span: span_for(&lexical_rows, *id)
                        .or_else(|| span_for(&dense_rows, *id)),
                    memory: m,
                });
            }
        }

        let mut context = context::assemble_context(
            &results.iter().map(|r| r.memory.clone()).collect::<Vec<_>>(),
            &bundles.protected,
            &req.context_budget,
        );

        // A conflict notice is required when a complete conflict bundle cannot
        // fit in the FINAL budgeted context (design §10.4): never silently show
        // only one side. Check the assembled items, not pre-budget selection:
        // a pair straddling the budget cutoff must still warn.
        let item_ids: BTreeSet<EntityId> =
            context.items.iter().map(|item| item.memory_id).collect();
        let conflict_notice = bundles.conflicts.iter().find_map(|b| match b {
            Bundle::Conflict { members, .. } => {
                let all_in = members.iter().all(|m| item_ids.contains(m));
                if all_in {
                    None
                } else {
                    Some(context::conflict_notice(members))
                }
            }
            _ => None,
        });
        context.conflict_notice = conflict_notice.clone();

        let explanation = build_explanation(&ExplanationInputs {
            scope: &scope,
            store_gen,
            req,
            rrf: &rrf,
            bundles: &bundles,
            expansion: &expansion,
            scored: &scored,
            mmr: &mmr,
            selected: &selected,
            context: &context,
            conflict_notice: conflict_notice.as_deref(),
        });

        let mut explanation = explanation;
        let readiness = self.readiness(req, store_gen).await?;
        explanation.fts_ready = readiness.fts_ready;
        explanation.dense_ready = readiness.dense_ready && !dense_failed;
        explanation.projection_lag = readiness.projection_lag;
        // A non-empty query with pending projections, a requested-but-missing
        // dense leg, a failed dense leg, or an unready lexical leg is a
        // PARTIAL result: recall may be incomplete. (This stage only runs
        // for non-empty queries; empty queries return via list mode above.)
        explanation.partial = readiness.projection_lag > 0
            || (req.model_fingerprint.is_some() && !readiness.dense_ready)
            || dense_failed
            || !readiness.fts_ready;

        Ok(RetrievalResult {
            results,
            context,
            explanation,
        })
    }
}
