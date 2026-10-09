//! One similarity contract for every compatibility decision that ranks,
//! deduplicates, or discovers memories (P1 Lance-authority wave).
//!
//! The similarity recipe is Jaccard overlap on lowercase whitespace tokens,
//! owned here and applied uniformly: Lance FTS proposes indexed candidates,
//! a pending-projection overlay scores unprojected canonical writes with the
//! same recipe, and a degraded snapshot scan (same recipe, explicitly
//! labelled by the caller) covers backends that are absent or not yet
//! FTS-indexed. Dense vectors contribute through the recall engine, never
//! through these decisions: an uncalibrated dense threshold could reject
//! legitimate writes, while the frozen duplicate rule is lexical.
//!
//! Candidate generation is rank-based (Lance BM25 order); every DECISION is
//! Jaccard in [0,1], so indexed and pending hits are directly comparable and
//! globally sortable. `DEDUP_JACCARD_THRESHOLD` is the frozen duplicate rule.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::retrieval::scope::sql_quote;
use crate::search::table::SearchTable;
use ltmrs_domain::command::DomainResult;
use ltmrs_domain::id::EntityId;
use ltmrs_service::repository::CanonicalRepository;

/// Frozen duplicate rule (upstream Lemma contract): Jaccard >= 0.80 rejects.
pub const DEDUP_JACCARD_THRESHOLD: f64 = 0.80;

/// Informational overlap band used for auto-link listings (as before).
pub const AUTOLINK_JACCARD_BAND: std::ops::Range<f64> = 0.25..0.95;

/// What the similarity query is for (documents intent; routing is shared).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimilarityPurpose {
    /// Duplicate rejection for a mutation preflight.
    Dedup,
    /// Related-memory listing / auto-link target planning.
    AutoLink,
    /// Contradiction candidate discovery.
    ConflictCandidates,
}

/// One similarity query: text compared under a project rule.
#[derive(Debug, Clone)]
pub struct SimilarityQuery {
    /// The candidate text (fragment under test).
    pub text: String,
    /// Dedup project rule (mirrors the frozen behavior): None scans every
    /// recallable memory; Some(p) scans project p plus unprojected (global)
    /// memories. Ignored for AutoLink/ConflictCandidates (global scan).
    pub project: Option<String>,
    /// A memory to exclude (the mutation target itself).
    pub exclude: Option<EntityId>,
    /// How many ranked hits to return.
    pub limit: usize,
    /// What the hits are for.
    pub purpose: SimilarityPurpose,
}

/// One ranked hit: Jaccard score in [0,1] whatever the source.
#[derive(Debug, Clone, PartialEq)]
pub struct SimilarityHit {
    /// The candidate memory.
    pub memory_id: EntityId,
    /// Jaccard overlap with the query text.
    pub score: f64,
    /// True when scored from the pending-projection overlay (canonical but
    /// not yet indexed) rather than Lance.
    pub pending: bool,
}

/// The single similarity service: Lance candidates + pending overlay +
/// degraded snapshot, one recipe throughout.
pub struct SimilarityService {
    repo: Arc<CanonicalRepository>,
    table: Option<SearchTable>,
}

impl SimilarityService {
    /// Build over a repository with an optional Lance table. `None` (or a
    /// table without a built FTS index) routes to the degraded snapshot
    /// scan — same recipe, explicitly the slow path.
    pub fn new(repo: Arc<CanonicalRepository>, table: Option<SearchTable>) -> Self {
        Self { repo, table }
    }

    /// Ranked similarity hits, globally sorted by score (best first).
    /// Async core (tests and async callers await directly).
    pub async fn find_similar(&self, query: &SimilarityQuery) -> DomainResult<Vec<SimilarityHit>> {
        let scope_project: Option<String> = match query.purpose {
            // The frozen dedup rule keeps its project visibility.
            SimilarityPurpose::Dedup => query.project.clone(),
            // Auto-link and conflict discovery scan every recallable memory.
            SimilarityPurpose::AutoLink | SimilarityPurpose::ConflictCandidates => None,
        };
        // Without a table there is nothing async to do: the degraded
        // snapshot covers every memory (including pending ones) with no
        // runtime involved, so table-less callers work in any context.
        if self.table.is_none() {
            return self.hydrate_and_rank(
                query,
                self.snapshot_ids(scope_project.as_deref(), query.exclude)?,
                &BTreeSet::new(),
            );
        }
        // Lance answers only with a built FTS index; otherwise the
        // degraded snapshot below covers every memory (including pending
        // ones), so the overlay is redundant there.
        let table = self.table.as_ref().expect("checked above");
        if !self.fts_usable(table).await? {
            return self.hydrate_and_rank(
                query,
                self.snapshot_ids(scope_project.as_deref(), query.exclude)?,
                &BTreeSet::new(),
            );
        }
        // Indexed candidates when the table can answer lexically.
        let indexed = self
            .lance_candidates(table, &query.text, scope_project.as_deref(), query.limit)
            .await?;
        // Pending overlay: canonical writes not yet projected, same recipe.
        // A usable Lance that returns zero hits is a legitimate empty
        // answer on the indexed side; the overlay still covers the lag.
        let pending = self.pending_ids(scope_project.as_deref(), query.exclude)?;
        let mut ids = indexed;
        for id in pending.iter() {
            if !ids.contains(id) {
                ids.push(*id);
            }
        }
        self.hydrate_and_rank(query, ids, &pending)
    }

    /// Synchronous bridge for sync callers (tools on `spawn_blocking`;
    /// same blocking contract as `SearchBackend::retrieve_sync`: with a
    /// table attached the caller must provide a blocking context, otherwise
    /// this panics like any `block_on` in async context. Without a table —
    /// or without any runtime — it degrades to the snapshot scan with no
    /// runtime involved, so table-less callers work in any context.
    pub fn find_similar_sync(&self, query: &SimilarityQuery) -> DomainResult<Vec<SimilarityHit>> {
        let scope_project: Option<String> = match query.purpose {
            SimilarityPurpose::Dedup => query.project.clone(),
            SimilarityPurpose::AutoLink | SimilarityPurpose::ConflictCandidates => None,
        };
        if self.table.is_none() {
            return self.hydrate_and_rank(
                query,
                self.snapshot_ids(scope_project.as_deref(), query.exclude)?,
                &BTreeSet::new(),
            );
        }
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle.block_on(self.find_similar(query)),
            Err(_) => self.hydrate_and_rank(
                query,
                self.snapshot_ids(scope_project.as_deref(), query.exclude)?,
                &BTreeSet::new(),
            ),
        }
    }

    /// Hydrate candidates and score every one with the single recipe,
    /// globally sorted (best first), truncated to the query limit.
    fn hydrate_and_rank(
        &self,
        query: &SimilarityQuery,
        ids: Vec<EntityId>,
        pending: &BTreeSet<EntityId>,
    ) -> DomainResult<Vec<SimilarityHit>> {
        let scope_project: Option<String> = match query.purpose {
            SimilarityPurpose::Dedup => query.project.clone(),
            SimilarityPurpose::AutoLink | SimilarityPurpose::ConflictCandidates => None,
        };
        let mut seen: BTreeSet<EntityId> = BTreeSet::new();
        let mut order: Vec<EntityId> = Vec::new();
        for id in ids {
            if Some(id) != query.exclude && seen.insert(id) {
                order.push(id);
            }
        }
        let mut hits: Vec<SimilarityHit> = Vec::new();
        for memory in self.repo.get_memories(&order)? {
            if !memory.lifecycle.is_recallable()
                || !project_visible(scope_project.as_deref(), memory.project.as_deref())
            {
                continue;
            }
            hits.push(SimilarityHit {
                memory_id: memory.id,
                score: jaccard(&query.text, &memory.fragment),
                pending: pending.contains(&memory.id),
            });
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.memory_id.cmp(&b.memory_id))
        });
        hits.truncate(query.limit.max(1));
        Ok(hits)
    }

    /// Candidate IDs from the degraded snapshot scan (recallable, in scope).
    fn snapshot_ids(
        &self,
        project: Option<&str>,
        exclude: Option<EntityId>,
    ) -> DomainResult<Vec<EntityId>> {
        let export = self.repo.export_snapshot()?;
        Ok(export
            .memories
            .iter()
            .filter(|m| Some(m.id) != exclude && m.lifecycle.is_recallable())
            .filter(|m| project_visible(project, m.project.as_deref()))
            .map(|m| m.id)
            .collect())
    }

    /// IDs with pending projection jobs (recallable, in scope).
    fn pending_ids(
        &self,
        project: Option<&str>,
        exclude: Option<EntityId>,
    ) -> DomainResult<BTreeSet<EntityId>> {
        let jobs = self.repo.projection_jobs()?;
        if jobs.is_empty() {
            return Ok(BTreeSet::new());
        }
        let ids: Vec<EntityId> = jobs.into_iter().map(|j| j.memory_id).collect();
        let mut out = BTreeSet::new();
        for memory in self.repo.get_memories(&ids)? {
            if Some(memory.id) == exclude || !memory.lifecycle.is_recallable() {
                continue;
            }
            if !project_visible(project, memory.project.as_deref()) {
                continue;
            }
            out.insert(memory.id);
        }
        Ok(out)
    }

    /// Whether the table can answer lexical queries (built FTS index).
    async fn fts_usable(&self, table: &SearchTable) -> DomainResult<bool> {
        Ok(table.fts_index_ready().await.unwrap_or(false))
    }

    /// Lance FTS candidate IDs in BM25 rank order (recall only; the decision
    /// recipe applies later). Empty query text matches nothing.
    async fn lance_candidates(
        &self,
        table: &SearchTable,
        text: &str,
        project: Option<&str>,
        limit: usize,
    ) -> DomainResult<Vec<EntityId>> {
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let live = self.repo.store_generation()?;
        let mut filter = format!("store_generation = {}", live.as_u64());
        if let Some(p) = project {
            filter.push_str(&format!(
                " AND (project = '{}' OR project IS NULL)",
                sql_quote(p)
            ));
        }
        // Recall headroom: BM25 rank order is not Jaccard order, so fetch
        // generously and let the uniform recipe re-rank.
        let k = (limit.max(5) * 10).clamp(50, 500);
        let rows = table.fts_query(text, k, Some(&filter)).await?;
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for row in &rows {
            if seen.insert(row.memory_id) {
                out.push(row.memory_id);
            }
        }
        Ok(out)
    }
}

/// Project visibility for dedup-scope queries (frozen behavior): no project
/// scans everything; a project scans itself plus unprojected memories.
fn project_visible(filter: Option<&str>, record: Option<&str>) -> bool {
    match (filter, record) {
        (None, _) => true,
        (Some(p), Some(mp)) => p == mp,
        (Some(_), None) => true,
    }
}

/// Jaccard overlap on lowercase whitespace tokens (the single similarity
/// recipe; moved here from the compatibility layer so every decision —
/// dedup, auto-link, conflict candidates — shares it).
pub fn jaccard(a: &str, b: &str) -> f64 {
    let ta: BTreeSet<String> = a
        .to_lowercase()
        .split_whitespace()
        .map(|w| w.to_string())
        .collect();
    let tb: BTreeSet<String> = b
        .to_lowercase()
        .split_whitespace()
        .map(|w| w.to_string())
        .collect();
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let inter = ta.intersection(&tb).count();
    let union = ta.union(&tb).count();
    inter as f64 / union as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use ltmrs_domain::clock::FrozenClock;
    use ltmrs_domain::command::{CommandContext, DomainCommand};
    use ltmrs_domain::id::{
        ChannelId, DocumentRevision, EligibilityRevision, EntityRevision, FrontendId,
    };
    use ltmrs_domain::memory::{FragmentType, Instant, Memory, MemoryLifecycle, MemorySource};
    use uuid::Uuid;

    use crate::search::projector::{FixedEmbedder, Projector};

    fn eid(n: u64) -> EntityId {
        EntityId::new(Uuid::from_u128(n as u128))
    }

    fn memory(id: EntityId, project: Option<&str>, fragment: &str) -> Memory {
        Memory {
            id,
            external_alias: None,
            title: format!("title-{}", id.as_uuid()),
            fragment: fragment.to_string(),
            description: String::new(),
            fragment_type: FragmentType::Fact,
            project: project.map(|p| p.to_string()),
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
            entity_revision: EntityRevision::new(1),
            document_revision: DocumentRevision::new(1),
            eligibility_revision: EligibilityRevision::new(1),
            created_at: Instant::new(100),
            updated_at: Instant::new(100),
            raw_created: None,
            unknown_fields: std::collections::BTreeMap::new(),
        }
    }

    fn ctx(op_num: u64) -> CommandContext {
        CommandContext {
            store_generation: ltmrs_domain::id::StoreGeneration::FIRST,
            frontend_id: FrontendId::new(Uuid::from_u128(1)),
            channel_id: ChannelId::new(Uuid::from_u128(2)),
            session: None,
            operation_id: ltmrs_domain::id::OperationId::new(Uuid::from_u128(op_num as u128)),
            request_digest: format!("d{op_num}"),
            deadline_millis: None,
            scope: Default::default(),
            retry_epoch: 1,
        }
    }

    fn repo() -> (Arc<CanonicalRepository>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let clock: Arc<dyn ltmrs_domain::clock::Clock + Send + Sync> =
            Arc::new(FrozenClock::new(1000));
        let repo =
            CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), clock).unwrap();
        repo.issue_namespace(
            FrontendId::new(Uuid::from_u128(1)),
            ChannelId::new(Uuid::from_u128(2)),
            1000,
        )
        .unwrap();
        (Arc::new(repo), dir)
    }

    fn add(
        repo: &CanonicalRepository,
        op: u64,
        id: EntityId,
        project: Option<&str>,
        fragment: &str,
    ) {
        repo.apply(
            &ctx(op),
            &DomainCommand::AddMemory {
                memory: memory(id, project, fragment),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    }

    const DUP_A: &str = "the quick brown fox jumps over the lazy dog";
    const DUP_B: &str = "the quick brown fox jumps over the lazy dog today";
    const OTHER: &str = "quantum entanglement photon polarization field";

    #[test]
    fn jaccard_recipe_basics() {
        assert_eq!(DEDUP_JACCARD_THRESHOLD, 0.80);
        assert_eq!(jaccard(DUP_A, DUP_A), 1.0);
        assert_eq!(jaccard(DUP_A, OTHER), 0.0);
        assert!(jaccard(DUP_A, DUP_B) >= DEDUP_JACCARD_THRESHOLD);
        assert_eq!(jaccard("", DUP_A), 0.0);
    }

    #[tokio::test]
    async fn degraded_path_finds_duplicate_without_table() {
        let (repo, _dir) = repo();
        add(&repo, 1, eid(1), None, DUP_A);
        add(&repo, 2, eid(2), None, OTHER);
        let svc = SimilarityService::new(repo, None);
        let hits = svc
            .find_similar(&SimilarityQuery {
                text: DUP_B.to_string(),
                project: None,
                exclude: None,
                limit: 5,
                purpose: SimilarityPurpose::Dedup,
            })
            .await
            .unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].memory_id, eid(1));
        assert!(hits[0].score >= DEDUP_JACCARD_THRESHOLD);
        assert!(!hits[0].pending);
    }

    #[tokio::test]
    async fn pending_overlay_catches_unprojected_duplicate() {
        let (repo, _dir) = repo();
        let lance_dir = tempfile::tempdir().unwrap();
        let table = SearchTable::open(lance_dir.path().to_str().unwrap())
            .await
            .unwrap();
        table.ensure_fts_index().await.unwrap();
        // Indexed decoy: projected, unrelated.
        add(&repo, 1, eid(1), None, OTHER);
        Projector::project_pending(&repo, &table, Box::new(FixedEmbedder { dim: 32 }), 10)
            .await
            .unwrap();
        // Unprojected duplicate: never indexed, still pending.
        add(&repo, 2, eid(2), None, DUP_A);
        assert!(repo.has_pending_projection(eid(2)).unwrap());
        let svc = SimilarityService::new(repo, Some(table));
        let hits = svc
            .find_similar(&SimilarityQuery {
                text: DUP_B.to_string(),
                project: None,
                exclude: None,
                limit: 5,
                purpose: SimilarityPurpose::Dedup,
            })
            .await
            .unwrap();
        let hit = hits
            .iter()
            .find(|h| h.memory_id == eid(2))
            .expect("overlay hit");
        assert!(hit.score >= DEDUP_JACCARD_THRESHOLD);
        assert!(hit.pending, "unprojected hit must be flagged pending");
    }

    #[tokio::test]
    async fn indexed_and_pending_hits_merge_globally_sorted() {
        let (repo, _dir) = repo();
        let lance_dir = tempfile::tempdir().unwrap();
        let table = SearchTable::open(lance_dir.path().to_str().unwrap())
            .await
            .unwrap();
        // Indexed near-duplicate (slightly weaker than the pending one).
        add(&repo, 1, eid(1), None, DUP_A);
        Projector::project_pending(&repo, &table, Box::new(FixedEmbedder { dim: 32 }), 10)
            .await
            .unwrap();
        table.ensure_fts_index().await.unwrap();
        // Pending exact duplicate: must outrank the indexed near-duplicate.
        add(&repo, 2, eid(2), None, DUP_B);
        let svc = SimilarityService::new(repo, Some(table));
        let hits = svc
            .find_similar(&SimilarityQuery {
                text: DUP_B.to_string(),
                project: None,
                exclude: None,
                limit: 5,
                purpose: SimilarityPurpose::AutoLink,
            })
            .await
            .unwrap();
        assert_eq!(hits[0].memory_id, eid(2));
        assert!(hits[0].pending);
        assert_eq!(hits[1].memory_id, eid(1));
        assert!(!hits[1].pending);
        assert!(hits[0].score >= hits[1].score);
    }

    #[tokio::test]
    async fn dedup_project_rule_matches_frozen_behavior() {
        let (repo, _dir) = repo();
        add(&repo, 1, eid(1), Some("x"), DUP_A);
        add(&repo, 2, eid(2), Some("y"), DUP_A);
        add(&repo, 3, eid(3), None, DUP_A);
        let svc = SimilarityService::new(repo, None);
        async fn query_ids(svc: &SimilarityService, project: Option<&str>) -> Vec<EntityId> {
            svc.find_similar(&SimilarityQuery {
                text: DUP_B.to_string(),
                project: project.map(|p| p.to_string()),
                exclude: None,
                limit: 10,
                purpose: SimilarityPurpose::Dedup,
            })
            .await
            .unwrap()
            .into_iter()
            .map(|h| h.memory_id)
            .collect::<Vec<_>>()
        }
        // No project scans everything; a project scans itself plus globals.
        assert_eq!(query_ids(&svc, None).await, vec![eid(1), eid(2), eid(3)]);
        assert_eq!(query_ids(&svc, Some("x")).await, vec![eid(1), eid(3)]);
        assert_eq!(query_ids(&svc, Some("zzz")).await, vec![eid(3)]);
    }

    #[tokio::test]
    async fn exclude_self_removes_target() {
        let (repo, _dir) = repo();
        add(&repo, 1, eid(1), None, DUP_A);
        let svc = SimilarityService::new(repo, None);
        let hits = svc
            .find_similar(&SimilarityQuery {
                text: DUP_A.to_string(),
                project: None,
                exclude: Some(eid(1)),
                limit: 5,
                purpose: SimilarityPurpose::Dedup,
            })
            .await
            .unwrap();
        assert!(hits.is_empty());
    }
}
