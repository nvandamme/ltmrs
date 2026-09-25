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

use std::collections::{BTreeMap, BTreeSet};

use crate::domain::command::{DomainResult, Scope};
use crate::domain::id::{EntityId, ModelFingerprint, StoreGeneration};
use crate::domain::memory::Memory;
use crate::domain::relation::Relation;
use crate::retrieval::bundles::{self, Bundle, Bundles};
use crate::retrieval::context::{self, ContextBudget};
use crate::retrieval::explain::{
    CandidateExplanation, GraphPathRecord, LegRank, RetrievalExplanation, ScoreComponents,
};
use crate::retrieval::graph_expansion::{self, GraphPolicy};
use crate::retrieval::mmr::{self, MmrCandidate, MmrConfig};
use crate::retrieval::ranking::{self, RankedList};
use crate::retrieval::scope::EffectiveScope;
use crate::search::row::SearchRow;
use crate::search::table::SearchTable;

/// The query embedder seam: the engine never blocks on inference; the daemon
/// provides an async embedding service behind this trait.
pub trait QueryEmbedder: Send + Sync {
    fn embed_query<'a>(
        &'a self,
        query: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = DomainResult<Vec<f32>>> + Send + 'a>>;
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
    /// None means no threshold (all nearest rows are candidates). A threshold
    /// is a calibration input (WP-12), never a universal truth gate.
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

impl Default for RetrievalRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            direct_ids: vec![],
            scope: Scope::default(),
            store_generation: None,
            model_fingerprint: None,
            candidate_limit: 50,
            min_similarity: None,
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

/// Leg readiness for a retrieval call (task 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Readiness {
    fts_ready: bool,
    dense_ready: bool,
    projection_lag: usize,
}

/// The recall engine: composes the repository (canonical truth) with the
/// search projection (candidate legs) into one consistent retrieval call.
pub struct Engine {
    repo: std::sync::Arc<crate::service::repository::CanonicalRepository>,
    table: SearchTable,
}

impl Engine {
    pub fn new(
        repo: std::sync::Arc<crate::service::repository::CanonicalRepository>,
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
        let scope = EffectiveScope::resolve(&req.scope);
        // Unpinned requests follow the active pointer per call so a cutover
        // takes effect on the next query; pinned requests stay put. A
        // repository error propagates (never masked as generation 1).
        let store_gen = match req.store_generation {
            Some(g) => g,
            None => self.repo.store_generation()?,
        };

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

        let dense_rows: Vec<SearchRow> = if let Some(fp) = req.model_fingerprint {
            // AD-04: never mix vectors from different model spaces. The dense
            // leg is constrained to the requested model fingerprint so a
            // blue-green deployment never compares incompatible embeddings.
            let dense_filter = match cf {
                Some(f) => format!("{f} AND model_fingerprint = {}", fp.as_u64()),
                None => format!("model_fingerprint = {}", fp.as_u64()),
            };
            let qvec = embedder.embed_query(&req.query).await?;
            let hits = self
                .table
                .vector_query(&qvec, req.candidate_limit, Some(&dense_filter))
                .await?;
            // No-answer rule (RQ-14): nearest-neighbor rank is not proof of
            // relevance. When a threshold is configured, drop rows whose
            // cosine similarity falls below it (Lance cosine distance =
            // 1 - similarity).
            hits.into_iter()
                .filter(|(_row, distance)| match req.min_similarity {
                    Some(min) => (1.0 - *distance as f64) >= min,
                    None => true,
                })
                .map(|(row, _d)| row)
                .collect()
        } else {
            vec![]
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
            return self.no_answer_result(&scope, store_gen, req).await;
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
                vector: memories_map
                    .get(id)
                    .and_then(|_| row_for(&lexical_rows, *id))
                    .or_else(|| row_for(&dense_rows, *id))
                    .and_then(|r| r.embedding.clone()),
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

        let context = context::assemble_context(
            &results.iter().map(|r| r.memory.clone()).collect::<Vec<_>>(),
            &bundles.protected,
            &req.context_budget,
        );

        // A conflict notice is required when a complete conflict bundle cannot
        // fit in the FINAL budgeted context (design §10.4): never silently show
        // only one side. Check the assembled results, not just MMR selection.
        let result_ids: BTreeSet<EntityId> = results.iter().map(|r| r.memory.id).collect();
        let conflict_notice = bundles.conflicts.iter().find_map(|b| match b {
            Bundle::Conflict { members, .. } => {
                let all_in = members.iter().all(|m| result_ids.contains(m));
                if all_in {
                    None
                } else {
                    Some(context::conflict_notice(members))
                }
            }
            _ => None,
        });

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
        explanation.dense_ready = readiness.dense_ready;
        explanation.projection_lag = readiness.projection_lag;
        // A non-empty query with pending projections, or a requested-but-missing
        // dense leg, is a PARTIAL result: recall may be incomplete.
        explanation.partial = readiness.projection_lag > 0
            || (req.model_fingerprint.is_some() && !readiness.dense_ready);

        Ok(RetrievalResult {
            results,
            context,
            explanation,
        })
    }

    /// Readiness/partial-result reporting (task 10): whether each leg is ready
    /// and how much projection work is still pending. Never claims false
    /// completeness. Scoped to the requested generation/fingerprint so a
    /// blue-green deployment reports readiness for the space it actually reads.
    async fn readiness(
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
    async fn retrieve_direct(
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
        Ok(RetrievalResult {
            results,
            context,
            explanation,
        })
    }

    /// Empty-query routing: upstream list/priority behavior (no dense recall).
    async fn retrieve_list(
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
        Ok(RetrievalResult {
            results,
            context,
            explanation,
        })
    }

    /// Valid no-answer result: a nonempty query legitimately returns nothing.
    async fn no_answer_result(
        &self,
        scope: &EffectiveScope,
        store_gen: StoreGeneration,
        req: &RetrievalRequest,
    ) -> DomainResult<RetrievalResult> {
        let mut explanation = RetrievalExplanation::new(store_gen, req.model_fingerprint);
        explanation.no_match = true;
        explanation.scope_filter = scope.to_lance_filter();
        let readiness = self.readiness(req, store_gen).await?;
        explanation.fts_ready = readiness.fts_ready;
        explanation.dense_ready = readiness.dense_ready;
        explanation.projection_lag = readiness.projection_lag;
        // A no-match with pending projections or a missing dense leg is partial:
        // the absence of hits may be a projection gap, not a true no-answer.
        explanation.partial = readiness.projection_lag > 0
            || (req.model_fingerprint.is_some() && !readiness.dense_ready);
        Ok(RetrievalResult {
            results: vec![],
            context: context::ContextResult::default(),
            explanation,
        })
    }

    /// Hydrate a set of IDs from canonical state (single snapshot).
    fn hydrate(&self, ids: &[EntityId]) -> DomainResult<BTreeMap<EntityId, Memory>> {
        let memories = self.repo.get_memories(ids)?;
        Ok(memories.into_iter().map(|m| (m.id, m)).collect())
    }

    /// Build the neighbor map for graph expansion from one relation snapshot.
    fn build_neighbor_map(
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

/// The inputs to explanation building (task 12), bundled to keep the
/// signature stable as the schema grows.
struct ExplanationInputs<'a> {
    scope: &'a EffectiveScope,
    store_gen: StoreGeneration,
    req: &'a RetrievalRequest,
    rrf: &'a ranking::RrfResult,
    bundles: &'a Bundles,
    expansion: &'a graph_expansion::GraphExpansion,
    scored: &'a [(EntityId, ScoreComponents)],
    mmr: &'a mmr::MmrResult,
    selected: &'a [EntityId],
    context: &'a context::ContextResult,
    conflict_notice: Option<&'a str>,
}

/// Build the explanation for this call (task 12). Pure: deterministic from
/// the pipeline's stage outputs.
fn build_explanation(inputs: &ExplanationInputs<'_>) -> RetrievalExplanation {
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
fn collapse_by_parent(rows: &[SearchRow]) -> Vec<EntityId> {
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
fn span_for(rows: &[SearchRow], id: EntityId) -> Option<(u64, u64)> {
    rows.iter()
        .find(|r| r.memory_id == id)
        .map(|r| (r.char_start, r.char_end))
}

/// Find a row for a memory (for its embedding).
fn row_for(rows: &[SearchRow], id: EntityId) -> Option<&SearchRow> {
    rows.iter().find(|r| r.memory_id == id)
}

/// Priority P(d) in [0,1]: feedback balance plus a small confidence term.
fn priority(m: &Memory) -> f64 {
    let pos = m.positive_feedback as f64;
    let neg = (m.negative_feedback + m.negative_hits) as f64;
    let balance = if pos + neg > 0.0 {
        pos / (pos + neg)
    } else {
        0.5
    };
    (0.5 * balance + 0.5 * m.confidence.clamp(0.0, 1.0)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::command::DomainCommand;
    use crate::domain::id::{DocumentRevision, FrontendId};
    use crate::domain::memory::{FragmentType, Memory, MemoryLifecycle, MemorySource};
    use crate::search::projector::{FixedEmbedder, Projector};
    use uuid::Uuid;

    fn eid(n: u64) -> EntityId {
        EntityId::new(Uuid::from_u128(n as u128))
    }

    fn memory(id: EntityId, title: &str, fragment: &str, project: Option<&str>) -> Memory {
        Memory {
            id,
            external_alias: None,
            title: title.to_string(),
            fragment: fragment.to_string(),
            description: String::new(),
            fragment_type: FragmentType::Fact,
            project: project.map(|s| s.to_string()),
            source: MemorySource::Ai,
            confidence: 0.5,
            quality_score: None,
            lifecycle: MemoryLifecycle::Live,
            tags: vec![],
            associated_with: vec![],
            relations: vec![],
            parent_id: None,
            child_ids: vec![],
            session_id: None,
            task_type: None,
            related_guides: vec![],
            evidence: vec![],
            access_count: 0,
            last_accessed_at: None,
            positive_feedback: 0,
            negative_feedback: 0,
            negative_hits: 0,
            refinement_count: 0,
            distill_candidate: false,
            entity_revision: crate::domain::id::EntityRevision::new(1),
            document_revision: DocumentRevision::new(1),
            eligibility_revision: crate::domain::id::EligibilityRevision::new(1),
            created_at: crate::domain::memory::Instant::new(100),
            updated_at: crate::domain::memory::Instant::new(100),
            raw_created: None,
            unknown_fields: std::collections::BTreeMap::new(),
        }
    }

    fn ctx(op_num: u64) -> crate::domain::command::CommandContext {
        crate::domain::command::CommandContext {
            store_generation: StoreGeneration::FIRST,
            frontend_id: FrontendId::new(Uuid::from_u128(1)),
            channel_id: crate::domain::id::ChannelId::new(Uuid::from_u128(2)),
            session: None,
            operation_id: crate::domain::id::OperationId::new(Uuid::from_u128(op_num as u128)),
            request_digest: format!("d{op_num}"),
            deadline_millis: None,
            scope: Default::default(),
            retry_epoch: 1,
        }
    }

    /// A deterministic query embedder: the SAME derivation as FixedEmbedder,
    /// with an optional prefix so a prefixed query matches a document exactly
    /// (mirroring the E5 query/passage prefix asymmetry).
    struct TestQueryEmbedder {
        prefix: &'static str,
    }

    impl TestQueryEmbedder {
        fn hash_vec(text: &str) -> Vec<f32> {
            (0..384)
                .map(|i| {
                    ((text.bytes().fold(0u64, |a, b| a.wrapping_add(b as u64)) >> (i % 64))
                        ^ i as u64) as f32
                        / 1e9
                })
                .collect()
        }
    }

    impl QueryEmbedder for TestQueryEmbedder {
        fn embed_query<'a>(
            &'a self,
            query: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = DomainResult<Vec<f32>>> + Send + 'a>>
        {
            let text = format!("{}{}", self.prefix, query);
            Box::pin(async move { Ok(Self::hash_vec(&text)) })
        }
    }

    async fn env() -> (
        std::sync::Arc<crate::service::repository::CanonicalRepository>,
        SearchTable,
        Projector,
        (tempfile::TempDir, tempfile::TempDir),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();

        let clock = std::sync::Arc::new(crate::domain::clock::FrozenClock::new(1000));
        let repo_path = dir.path().to_str().unwrap();
        let repo =
            crate::service::repository::CanonicalRepository::open_with_clock(repo_path, clock)
                .unwrap();
        let fe = FrontendId::new(Uuid::from_u128(1));
        let ns = repo.issue_namespace(fe, 1000).unwrap();
        assert_eq!(ns.retry_epoch, 1);

        let uri = lance_dir.path().to_str().unwrap().to_string();
        let table = SearchTable::open(&uri).await.unwrap();
        let repo_arc = std::sync::Arc::new(repo);
        let projector = Projector::new(
            repo_arc.clone(),
            table.clone(),
            Box::new(FixedEmbedder { dim: 384 }),
            ModelFingerprint::new(1),
            StoreGeneration::FIRST,
        );
        (repo_arc, table, projector, (dir, lance_dir))
    }

    fn add(
        repo: &crate::service::repository::CanonicalRepository,
        n: u64,
        title: &str,
        frag: &str,
        project: Option<&str>,
    ) {
        repo.apply(
            &ctx(n),
            &DomainCommand::AddMemory {
                memory: memory(eid(n), title, frag, project),
                session: None,
            },
        )
        .unwrap();
    }

    fn base_req() -> RetrievalRequest {
        RetrievalRequest {
            model_fingerprint: Some(ModelFingerprint::new(1)),
            ..Default::default()
        }
    }

    /// T-RANK-04: a nonempty query with zero overlap must return a valid
    /// no-answer, not nearest-neighbor noise presented as truth.
    #[tokio::test]
    async fn no_answer_when_nothing_matches() {
        let (repo, table, mut proj, _guard) = env().await;
        add(&repo, 1, "rust async", "tokio runtime details", None);
        proj.run_until_idle().await.unwrap();

        let req = RetrievalRequest {
            query: "zzqqxx completely unrelated terms".into(),
            min_similarity: Some(1.0),
            ..base_req()
        };
        let result = Engine::new(repo, table)
            .retrieve(&req, &TestQueryEmbedder { prefix: "" })
            .await
            .unwrap();
        assert!(result.results.is_empty());
        assert!(result.explanation.no_match);
    }

    /// Cutover readers: a request without a pinned generation reads the
    /// active pointer per call; an explicit generation stays pinned
    /// (rollback reads). Uses the ranked path: list mode reads canonical
    /// state directly and is generation-agnostic by design.
    #[tokio::test]
    async fn none_generation_resolves_active_pointer() {
        let (repo, table, _proj, _guard) = env().await;
        add(&repo, 1, "gen two", "second generation body", None);
        // Build generation 2 alongside generation 1, before activation.
        let gen2 = repo.stage_generation(ModelFingerprint::new(2)).unwrap();
        let mut proj2 = Projector::new(
            repo.clone(),
            table.clone(),
            Box::new(FixedEmbedder { dim: 384 }),
            ModelFingerprint::new(2),
            gen2,
        );
        proj2.run_until_idle().await.unwrap();
        repo.note_generation_progress(gen2, 1).unwrap();
        table.create_fts_index().await.unwrap();

        let req = RetrievalRequest {
            query: "generation".into(),
            ..Default::default()
        };
        assert!(req.store_generation.is_none());
        // Pre-activation: the converging build is invisible to default readers.
        let pre = Engine::new(repo.clone(), table.clone())
            .retrieve(&req, &TestQueryEmbedder { prefix: "" })
            .await
            .unwrap();
        assert!(
            pre.results.is_empty(),
            "unactivated build must stay invisible"
        );

        repo.activate_generation(gen2).unwrap();
        // Unpinned request follows the active pointer to the gen-2 row.
        let post = Engine::new(repo.clone(), table.clone())
            .retrieve(&req, &TestQueryEmbedder { prefix: "" })
            .await
            .unwrap();
        assert_eq!(post.results.len(), 1, "active pointer must resolve");

        // Explicit pin to the old generation sees nothing projected there.
        let pinned = RetrievalRequest {
            query: "generation".into(),
            store_generation: Some(StoreGeneration::FIRST),
            ..Default::default()
        };
        let result = Engine::new(repo, table)
            .retrieve(&pinned, &TestQueryEmbedder { prefix: "" })
            .await
            .unwrap();
        assert!(result.results.is_empty(), "pinned old generation is stable");
    }

    /// T-SEARCH-02: exact technical identifiers (flags, paths, underscores)
    /// survive the lexical leg and return current canonical IDs + spans.
    #[tokio::test]
    async fn exact_identifiers_survive_lexical_leg() {
        let (repo, table, mut proj, _guard) = env().await;
        add(
            &repo,
            1,
            "flag note",
            "use --no-pager with git commands in /home/user/Work space",
            None,
        );
        proj.run_until_idle().await.unwrap();
        table.create_fts_index().await.unwrap();

        let req = RetrievalRequest {
            query: "--no-pager".into(),
            model_fingerprint: None,
            ..Default::default()
        };
        let result = Engine::new(repo, table)
            .retrieve(&req, &TestQueryEmbedder { prefix: "" })
            .await
            .unwrap();
        assert_eq!(result.results.len(), 1);
        assert_eq!(result.results[0].memory.id, eid(1));
        assert!(result.results[0].matched_span.is_some());
    }

    /// T-SCOPE-01: out-of-scope memories (and their graph traps) are never
    /// returned or traversed, on any leg.
    #[tokio::test]
    async fn scope_applies_to_all_legs_and_graph() {
        let (repo, table, mut proj, _guard) = env().await;
        add(&repo, 1, "in scope", "alpha beta gamma", Some("app"));
        add(
            &repo,
            2,
            "out of scope trap",
            "alpha beta gamma",
            Some("other"),
        );
        proj.run_until_idle().await.unwrap();
        table.create_fts_index().await.unwrap();

        repo.apply(
            &ctx(10),
            &DomainCommand::Relate {
                relation: crate::domain::relation::Relation::new(
                    eid(100),
                    eid(1),
                    eid(2),
                    crate::domain::relation::RelationType::RelatedTo,
                    None,
                    crate::domain::memory::Instant::new(1),
                ),
            },
        )
        .unwrap();

        let req = RetrievalRequest {
            query: "alpha beta gamma".into(),
            scope: Scope {
                project: Some("app".into()),
                ..Default::default()
            },
            ..base_req()
        };
        let result = Engine::new(repo, table)
            .retrieve(&req, &TestQueryEmbedder { prefix: "" })
            .await
            .unwrap();
        let ids: Vec<EntityId> = result.results.iter().map(|r| r.memory.id).collect();
        assert!(ids.contains(&eid(1)));
        assert!(
            !ids.contains(&eid(2)),
            "out-of-scope graph trap must not be traversed or returned"
        );
    }

    /// T-RANK-03: a superseded memory is excluded from the primary answer and
    /// the current one is protected; lineage is preserved in the explanation.
    #[tokio::test]
    async fn superseded_memory_excluded_current_protected() {
        let (repo, table, mut proj, _guard) = env().await;
        add(
            &repo,
            1,
            "old advice",
            "use feature flag A for rollout",
            None,
        );
        add(
            &repo,
            2,
            "new advice",
            "use feature flag B for rollout",
            None,
        );
        proj.run_until_idle().await.unwrap();
        table.create_fts_index().await.unwrap();

        // 2 supersedes 1.
        repo.apply(
            &ctx(10),
            &DomainCommand::Relate {
                relation: crate::domain::relation::Relation::new(
                    eid(100),
                    eid(2),
                    eid(1),
                    crate::domain::relation::RelationType::Supersedes,
                    None,
                    crate::domain::memory::Instant::new(1),
                ),
            },
        )
        .unwrap();

        let req = RetrievalRequest {
            query: "feature flag rollout".into(),
            ..base_req()
        };
        let result = Engine::new(repo, table)
            .retrieve(&req, &TestQueryEmbedder { prefix: "" })
            .await
            .unwrap();

        let ids: Vec<EntityId> = result.results.iter().map(|r| r.memory.id).collect();
        assert!(
            ids.contains(&eid(2)),
            "the current memory must be in the primary answer"
        );
        assert!(
            !ids.contains(&eid(1)),
            "obsolete advice must be excluded from the primary answer"
        );
        // Lineage is preserved in the explanation.
        let exp = &result.explanation;
        assert!(
            exp.candidates
                .values()
                .any(|c| c.protected && c.id == eid(2))
        );
    }

    /// T-RANK-03: when ONLY the stale memory matches the query (the current
    /// record uses different wording), the stale advice must still be excluded
    /// and the current record redirected in — the chain is resolvable from the
    /// full relation graph, not just among recalled candidates.
    #[tokio::test]
    async fn stale_only_recall_redirects_to_current() {
        let (repo, table, mut proj, _guard) = env().await;
        add(
            &repo,
            1,
            "old advice",
            "use feature flag A for rollout",
            None,
        );
        add(&repo, 2, "new advice", "use dark mode by default", None);
        proj.run_until_idle().await.unwrap();
        table.create_fts_index().await.unwrap();

        // 2 supersedes 1.
        repo.apply(
            &ctx(10),
            &DomainCommand::Relate {
                relation: crate::domain::relation::Relation::new(
                    eid(100),
                    eid(2),
                    eid(1),
                    crate::domain::relation::RelationType::Supersedes,
                    None,
                    crate::domain::memory::Instant::new(1),
                ),
            },
        )
        .unwrap();

        // Query matches ONLY the stale memory's text.
        let req = RetrievalRequest {
            query: "feature flag A rollout".into(),
            ..base_req()
        };
        let result = Engine::new(repo, table)
            .retrieve(&req, &TestQueryEmbedder { prefix: "" })
            .await
            .unwrap();

        let ids: Vec<EntityId> = result.results.iter().map(|r| r.memory.id).collect();
        assert!(
            !ids.contains(&eid(1)),
            "stale advice recalled alone must still be excluded"
        );
        assert!(
            ids.contains(&eid(2)),
            "the current record must be redirected into the primary answer"
        );
    }

    /// T-RANK-03 / T-GRAPH-02: an unresolved contradiction bundle preserves
    /// both sides (retrieval-side graph semantics).
    #[tokio::test]
    async fn conflict_bundle_preserves_both_sides() {
        let (repo, table, mut proj, _guard) = env().await;
        add(&repo, 1, "claim one", "the timeout is 30 seconds", None);
        add(&repo, 2, "claim two", "the timeout is 60 seconds", None);
        proj.run_until_idle().await.unwrap();
        table.create_fts_index().await.unwrap();

        repo.apply(
            &ctx(10),
            &DomainCommand::Relate {
                relation: crate::domain::relation::Relation::new(
                    eid(100),
                    eid(1),
                    eid(2),
                    crate::domain::relation::RelationType::Contradicts,
                    None,
                    crate::domain::memory::Instant::new(1),
                ),
            },
        )
        .unwrap();

        let req = RetrievalRequest {
            query: "timeout seconds".into(),
            ..base_req()
        };
        let result = Engine::new(repo, table)
            .retrieve(&req, &TestQueryEmbedder { prefix: "" })
            .await
            .unwrap();
        let ids: Vec<EntityId> = result.results.iter().map(|r| r.memory.id).collect();
        assert!(
            ids.contains(&eid(1)) && ids.contains(&eid(2)),
            "both sides of a contradiction must survive MMR"
        );
    }

    /// Direct-ID routing bypasses the ranker; scope still enforced.
    #[tokio::test]
    async fn direct_ids_bypass_ranker_scope_enforced() {
        let (repo, table, _proj, _guard) = env().await;
        add(&repo, 1, "a", "alpha", Some("app"));
        add(&repo, 2, "b", "beta", Some("other"));

        let req = RetrievalRequest {
            direct_ids: vec![eid(1), eid(2)],
            scope: Scope {
                project: Some("app".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let result = Engine::new(repo, table)
            .retrieve(&req, &TestQueryEmbedder { prefix: "" })
            .await
            .unwrap();
        let ids: Vec<EntityId> = result.results.iter().map(|r| r.memory.id).collect();
        assert_eq!(ids, vec![eid(1)]);
    }

    /// Empty-query routing: list/priority behavior, no dense recall.
    #[tokio::test]
    async fn empty_query_is_list_mode() {
        let (repo, table, _proj, _guard) = env().await;
        add(&repo, 1, "a", "alpha", None);
        add(&repo, 2, "b", "beta", None);

        let req = RetrievalRequest {
            query: "   ".into(),
            ..Default::default()
        };
        let result = Engine::new(repo, table)
            .retrieve(&req, &TestQueryEmbedder { prefix: "" })
            .await
            .unwrap();
        assert!(result.explanation.empty_query);
        assert_eq!(result.results.len(), 2);
    }

    /// Semantic recall: a query identical to a document's text retrieves it.
    #[tokio::test]
    async fn semantic_recall_finds_identical_text() {
        let (repo, table, mut proj, _guard) = env().await;
        add(
            &repo,
            1,
            "t1",
            "the quick brown fox jumps over the lazy dog",
            None,
        );
        add(&repo, 2, "t2", "rust borrow checker rules explained", None);
        proj.run_until_idle().await.unwrap();

        // The query embedder uses a prefix that reconstructs the exact
        // rendered document text, so the query vector equals the document
        // vector (mirroring the E5 query/passage prefix asymmetry).
        let req = RetrievalRequest {
            query: "the quick brown fox jumps over the lazy dog".into(),
            model_fingerprint: Some(ModelFingerprint::new(1)),
            ..Default::default()
        };
        let embedder = TestQueryEmbedder { prefix: "t1\n" };
        let result = Engine::new(repo, table)
            .retrieve(&req, &embedder)
            .await
            .unwrap();
        assert!(!result.results.is_empty());
        assert_eq!(result.results[0].memory.id, eid(1));
    }

    /// AD-04: the dense leg never mixes vectors from different model spaces.
    /// A row published under a different fingerprint is invisible to a dense
    /// query for the requested fingerprint, even if its vector is a perfect
    /// match.
    #[tokio::test]
    async fn dense_leg_respects_model_fingerprint_isolation() {
        let (repo, table, _proj, _guard) = env().await;
        add(
            &repo,
            1,
            "t1",
            "the quick brown fox jumps over the lazy dog",
            None,
        );

        // Publish a row under a DIFFERENT fingerprint whose vector is a perfect
        // match for the query vector (the "old" model space).
        let mut old_row = SearchRow {
            store_generation: StoreGeneration::FIRST,
            memory_id: eid(1),
            document_revision: DocumentRevision::new(1),
            model_fingerprint: ModelFingerprint::new(999),
            chunk_id: crate::domain::id::ChunkId::new(0),
            lexical_text: "the quick brown fox jumps over the lazy dog".into(),
            char_start: 0,
            char_end: 46,
            project: None,
            fragment_type: "fact".into(),
            created_at_millis: 100,
            updated_at_millis: 100,
            embedding: Some(TestQueryEmbedder::hash_vec(
                "t1\nthe quick brown fox jumps over the lazy dog",
            )),
        };
        old_row.document_revision = repo
            .get_memories(&[eid(1)])
            .unwrap()
            .first()
            .unwrap()
            .document_revision;
        table
            .publish_rows(std::slice::from_ref(&old_row))
            .await
            .unwrap();

        // Query for fingerprint 1 (the "new" model space). The perfect-match
        // row under fingerprint 999 must NOT be returned.
        let req = RetrievalRequest {
            query: "the quick brown fox jumps over the lazy dog".into(),
            model_fingerprint: Some(ModelFingerprint::new(1)),
            ..Default::default()
        };
        let embedder = TestQueryEmbedder { prefix: "t1\n" };
        let result = Engine::new(repo, table)
            .retrieve(&req, &embedder)
            .await
            .unwrap();
        // No rows exist under fingerprint 1, so the dense leg returns nothing.
        assert!(
            result.results.is_empty(),
            "dense leg must not return rows from a different model fingerprint"
        );
    }

    /// T-SEARCH-02: French accents and mixed-case identifiers survive.
    #[tokio::test]
    async fn french_accents_and_mixed_case_survive() {
        let (repo, table, mut proj, _guard) = env().await;
        add(
            &repo,
            1,
            "config note",
            "comment configurer la persistance avec Fjall DB",
            None,
        );
        add(
            &repo,
            2,
            "env note",
            "set the DATABASE_URL environment variable correctly",
            None,
        );
        proj.run_until_idle().await.unwrap();
        table.create_fts_index().await.unwrap();

        // Query with French accents.
        let req = RetrievalRequest {
            query: "configurer la persistance".into(),
            model_fingerprint: None,
            ..Default::default()
        };
        let result = Engine::new(repo.clone(), table.clone())
            .retrieve(&req, &TestQueryEmbedder { prefix: "" })
            .await
            .unwrap();
        assert!(
            result.results.iter().any(|r| r.memory.id == eid(1)),
            "French-accented query must find the matching memory"
        );

        // Query with mixed-case environment variable.
        let req = RetrievalRequest {
            query: "DATABASE_URL".into(),
            model_fingerprint: None,
            ..Default::default()
        };
        let result = Engine::new(repo, table)
            .retrieve(&req, &TestQueryEmbedder { prefix: "" })
            .await
            .unwrap();
        assert!(
            result.results.iter().any(|r| r.memory.id == eid(2)),
            "mixed-case identifier must be found"
        );
    }

    /// T-SEARCH-02: underscores and paths are preserved.
    #[tokio::test]
    async fn underscores_and_paths_preserved() {
        let (repo, table, mut proj, _guard) = env().await;
        add(
            &repo,
            1,
            "path note",
            "the config lives at /etc/ltmrs/config.toml",
            None,
        );
        proj.run_until_idle().await.unwrap();
        table.create_fts_index().await.unwrap();

        let req = RetrievalRequest {
            query: "/etc/ltmrs/config.toml".into(),
            model_fingerprint: None,
            ..Default::default()
        };
        let result = Engine::new(repo, table)
            .retrieve(&req, &TestQueryEmbedder { prefix: "" })
            .await
            .unwrap();
        assert!(
            result.results.iter().any(|r| r.memory.id == eid(1)),
            "path with underscores must be found"
        );
    }

    /// T-SCOPE-02: min_confidence is enforced via canonical backfill, not just
    /// the first N hits.
    #[tokio::test]
    async fn min_confidence_backfilled_from_canonical() {
        let (repo, table, mut proj, _guard) = env().await;
        // Two memories with the same text but different confidence.
        let mut low = memory(eid(1), "note", "shared query terms here", None);
        low.confidence = 0.3;
        let mut high = memory(eid(2), "note", "shared query terms here", None);
        high.confidence = 0.9;
        repo.apply(
            &ctx(1),
            &DomainCommand::AddMemory {
                memory: low,
                session: None,
            },
        )
        .unwrap();
        repo.apply(
            &ctx(2),
            &DomainCommand::AddMemory {
                memory: high,
                session: None,
            },
        )
        .unwrap();
        proj.run_until_idle().await.unwrap();
        table.create_fts_index().await.unwrap();

        // Request with min_confidence 0.5: only the high-confidence memory is eligible.
        let req = RetrievalRequest {
            query: "shared query terms".into(),
            scope: Scope {
                min_confidence: Some(0.5),
                ..Default::default()
            },
            model_fingerprint: None,
            ..Default::default()
        };
        let result = Engine::new(repo, table)
            .retrieve(&req, &TestQueryEmbedder { prefix: "" })
            .await
            .unwrap();
        let ids: Vec<EntityId> = result.results.iter().map(|r| r.memory.id).collect();
        assert!(
            ids.contains(&eid(2)),
            "high-confidence memory must be returned"
        );
        assert!(
            !ids.contains(&eid(1)),
            "low-confidence memory must be filtered by min_confidence"
        );
    }

    /// T-SCOPE-01: date filters apply consistently.
    #[tokio::test]
    async fn date_filters_apply_consistently() {
        let (repo, table, mut proj, _guard) = env().await;
        let mut old = memory(eid(1), "old", "query terms for old memory", None);
        old.created_at = crate::domain::memory::Instant::new(100);
        let mut new = memory(eid(2), "new", "query terms for new memory", None);
        new.created_at = crate::domain::memory::Instant::new(5000);
        repo.apply(
            &ctx(1),
            &DomainCommand::AddMemory {
                memory: old,
                session: None,
            },
        )
        .unwrap();
        repo.apply(
            &ctx(2),
            &DomainCommand::AddMemory {
                memory: new,
                session: None,
            },
        )
        .unwrap();
        proj.run_until_idle().await.unwrap();
        table.create_fts_index().await.unwrap();

        // after=1000: only the new memory is in scope.
        let req = RetrievalRequest {
            query: "query terms for".into(),
            scope: Scope {
                after: Some(1000),
                ..Default::default()
            },
            model_fingerprint: None,
            ..Default::default()
        };
        let result = Engine::new(repo, table)
            .retrieve(&req, &TestQueryEmbedder { prefix: "" })
            .await
            .unwrap();
        let ids: Vec<EntityId> = result.results.iter().map(|r| r.memory.id).collect();
        assert!(ids.contains(&eid(2)));
        assert!(!ids.contains(&eid(1)));
    }

    /// T-RANK-04: the dense leg respects the no-answer similarity threshold.
    /// A row with an orthogonal vector (similarity 0) must be filtered out by a
    /// high threshold, so nearest-neighbor rank is never treated as truth.
    #[tokio::test]
    async fn dense_no_answer_threshold_filters_noise() {
        let (repo, table, _proj, _guard) = env().await;

        // Publish a row whose vector is orthogonal to the query vector.
        let mut row = SearchRow {
            store_generation: StoreGeneration::FIRST,
            memory_id: eid(1),
            document_revision: DocumentRevision::new(1),
            model_fingerprint: ModelFingerprint::new(1),
            chunk_id: crate::domain::id::ChunkId::new(0),
            lexical_text: "tokio runtime details".into(),
            char_start: 0,
            char_end: 23,
            project: None,
            fragment_type: "fact".into(),
            created_at_millis: 100,
            updated_at_millis: 100,
            embedding: Some(vec![1.0f32; 384]),
        };
        // Ensure the canonical memory exists and is eligible.
        add(&repo, 1, "rust async", "tokio runtime details", None);
        row.document_revision = repo
            .get_memories(&[eid(1)])
            .unwrap()
            .first()
            .unwrap()
            .document_revision;
        table
            .publish_rows(std::slice::from_ref(&row))
            .await
            .unwrap();

        // Query vector orthogonal to [1;384]: similarity = 0.
        struct OrthoEmbedder;
        impl QueryEmbedder for OrthoEmbedder {
            fn embed_query<'a>(
                &'a self,
                _q: &'a str,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = DomainResult<Vec<f32>>> + Send + 'a>,
            > {
                // Orthogonal to [1;384]: sum of components is 0.
                Box::pin(async move {
                    Ok((0..384)
                        .map(|i| if i < 192 { 1.0f32 } else { -1.0 })
                        .collect())
                })
            }
        }

        let req = RetrievalRequest {
            query: "zzqqxx unrelated".into(),
            min_similarity: Some(0.5),
            model_fingerprint: Some(ModelFingerprint::new(1)),
            ..Default::default()
        };
        let result = Engine::new(repo, table)
            .retrieve(&req, &OrthoEmbedder)
            .await
            .unwrap();
        assert!(
            result.results.is_empty(),
            "orthogonal candidate (similarity 0) must be filtered by the threshold"
        );
        assert!(result.explanation.no_match);
    }

    /// Explanation records leg ranks, score components, and protected status.
    #[tokio::test]
    async fn explanation_records_full_diagnostics() {
        let (repo, table, mut proj, _guard) = env().await;
        add(&repo, 1, "rust async", "tokio runtime details", None);
        proj.run_until_idle().await.unwrap();
        table.create_fts_index().await.unwrap();

        let req = RetrievalRequest {
            query: "tokio runtime".into(),
            ..base_req()
        };
        let result = Engine::new(repo, table)
            .retrieve(&req, &TestQueryEmbedder { prefix: "" })
            .await
            .unwrap();
        assert!(!result.results.is_empty());

        let exp = &result.explanation;
        assert_eq!(
            exp.profile_version,
            crate::retrieval::explain::RETRIEVAL_PROFILE_VERSION
        );
        assert!(!exp.no_match);
        // The top result has a position and score components.
        let top_id = result.results[0].memory.id;
        let cand = &exp.candidates[&top_id];
        assert_eq!(cand.position, 1);
        assert!(cand.scores.native_score > 0.0);
        assert!(cand.scores.native_score <= 1.0);
        // Legacy reference is separate evidence.
        assert!(cand.scores.legacy_reference < 0.1);
    }

    /// Task 10: partial-result reporting. A query issued while the projection
    /// is not yet converged must be flagged partial, not claim completeness.
    #[tokio::test]
    async fn partial_reported_when_projection_pending() {
        let (repo, table, _proj, _guard) = env().await;
        // Add a memory but do NOT run the projector: projection is pending.
        add(&repo, 1, "rust async", "tokio runtime details", None);
        assert!(
            repo.has_pending_projection(eid(1)).unwrap(),
            "projection must be pending before the worker runs"
        );

        let req = RetrievalRequest {
            query: "tokio runtime".into(),
            model_fingerprint: Some(ModelFingerprint::new(1)),
            ..Default::default()
        };
        let result = Engine::new(repo, table)
            .retrieve(&req, &TestQueryEmbedder { prefix: "" })
            .await
            .unwrap();
        // No rows are projected yet, so the result is empty but flagged partial.
        assert!(result.results.is_empty());
        assert!(
            result.explanation.partial,
            "a query with pending projections must be flagged partial"
        );
        assert!(result.explanation.projection_lag >= 1);
        assert!(!result.explanation.dense_ready);
    }

    /// Task 10: once the projection converges, the same query is complete.
    #[tokio::test]
    async fn complete_after_projection_converges() {
        let (repo, table, mut proj, _guard) = env().await;
        add(&repo, 1, "rust async", "tokio runtime details", None);
        proj.run_until_idle().await.unwrap();
        table.create_fts_index().await.unwrap();

        let req = RetrievalRequest {
            query: "tokio runtime".into(),
            model_fingerprint: Some(ModelFingerprint::new(1)),
            ..Default::default()
        };
        let result = Engine::new(repo, table)
            .retrieve(&req, &TestQueryEmbedder { prefix: "" })
            .await
            .unwrap();
        assert!(
            !result.explanation.partial,
            "a converged projection must not be flagged partial"
        );
        assert!(result.explanation.dense_ready);
        assert!(result.explanation.fts_ready);
    }

    /// Task 10: finite-score validation. Every returned candidate's score
    /// components must be finite and within their documented bounds.
    #[tokio::test]
    async fn all_scores_finite_and_bounded() {
        let (repo, table, mut proj, _guard) = env().await;
        add(&repo, 1, "rust async", "tokio runtime details", None);
        add(
            &repo,
            2,
            "python asyncio",
            "asyncio event loop details",
            None,
        );
        proj.run_until_idle().await.unwrap();
        table.create_fts_index().await.unwrap();

        let req = RetrievalRequest {
            query: "async runtime event loop".into(),
            ..base_req()
        };
        let result = Engine::new(repo, table)
            .retrieve(&req, &TestQueryEmbedder { prefix: "" })
            .await
            .unwrap();

        for cand in result.explanation.candidates.values() {
            let s = &cand.scores;
            assert!(s.rrf_normalized.is_finite());
            assert!(s.graph.is_finite());
            assert!(s.priority.is_finite());
            assert!(s.native_score.is_finite());
            assert!(s.legacy_reference.is_finite());
            assert!((0.0..=1.0).contains(&s.rrf_normalized));
            assert!((0.0..=1.0).contains(&s.graph));
            assert!((0.0..=1.0).contains(&s.priority));
            assert!((0.0..=1.0).contains(&s.native_score));
        }
    }
}
