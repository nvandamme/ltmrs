//! The projection worker loop (WP-05 tasks 5, 6; design §8.2).
//!
//! Reads durable desired-state jobs from the canonical repository, renders and
//! embeds only what changed, publishes idempotently to Lance under a per-entity
//! publication guard, then compare-and-clears the job it actually published.
//! Events are retryable wakeups — never an ordered commit log (RV-07).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::domain::command::{DomainError, DomainErrorCode, DomainResult};
use crate::domain::id::{ChunkId, EntityId, ModelFingerprint, StoreGeneration};
use crate::domain::projection::ProjectionJob;
use crate::search::row::SearchRow;
use crate::search::table::SearchTable;

/// A synchronous embedding provider. The projector never blocks Tokio I/O on it;
/// callers run this off-thread (WP-04's scheduler owns that). Failure leaves the
/// job pending so a stalled embedder retries instead of losing work or blocking
/// lexical indexing.
pub trait Embedder: Send {
    fn embed(&mut self, text: &str) -> Result<Vec<f32>, String>;

    /// Split rendered memory text into embeddable units (RQ-10). The default is
    /// a single unit covering the whole rendered text — byte-identical to the
    /// pre-chunking behavior. A model-aware override returns one unit per
    /// derived chunk so tail content beyond the first model window gets its
    /// own vector.
    ///
    /// Mapping contract for a future override over the E5 `chunk_passage`
    /// recipe (which yields unprefixed fragment spans with fragment-relative
    /// offsets): re-prefix each span for lexical searchability
    /// (`format!("{title}\n{span}")`) and shift its offsets by
    /// `title.len() + 1` into rendered coordinates; the model embed input
    /// additionally carries the recipe prefix (`passage: {title}\n{span}`).
    /// Precondition: changing the chunking policy bumps the chunker version
    /// (see `chunker_version`), never the model fingerprint — the fingerprint
    /// stays model-bound while the version attributes each row to the policy
    /// that produced it.
    fn chunk_text(&self, title: &str, fragment: &str) -> Vec<TextChunk> {
        let text = render_text(title, fragment);
        let len = text.len() as u64;
        vec![TextChunk {
            text,
            char_start: 0,
            char_end: len,
        }]
    }

    /// Chunking-policy version stamped on every projected row. Bump whenever
    /// the chunking policy changes so rows stay attributable per policy.
    fn chunker_version(&self) -> String {
        SINGLE_CHUNK_VERSION.to_string()
    }
}

/// Version stamped by the default single-unit chunking policy.
pub const SINGLE_CHUNK_VERSION: &str = "single-chunk-v1";

/// One embeddable unit of a memory: the row's lexical text plus the evidence
/// span of its matched content in rendered-text coordinates
/// (`render_text(title, fragment)` byte offsets).
#[derive(Debug, Clone, PartialEq)]
pub struct TextChunk {
    pub text: String,
    pub char_start: u64,
    pub char_end: u64,
}

/// Deterministic test embedder: a fixed-dimension vector derived from the text.
pub struct FixedEmbedder {
    pub dim: usize,
}

impl Embedder for FixedEmbedder {
    fn embed(&mut self, text: &str) -> Result<Vec<f32>, String> {
        Ok((0..self.dim)
            .map(|i| {
                ((text.bytes().fold(0u64, |a, b| a.wrapping_add(b as u64)) >> (i % 64)) ^ i as u64)
                    as f32
                    / 1e9
            })
            .collect())
    }
}

/// An embedder that always fails: models a stalled inference worker.
pub struct StalledEmbedder;

impl Embedder for StalledEmbedder {
    fn embed(&mut self, _text: &str) -> Result<Vec<f32>, String> {
        Err("embedding service unavailable".into())
    }
}

/// Outcome of processing one job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectorOutcome {
    /// Published (lexical + semantic when the embedder cooperates) and acknowledged.
    Published,
    /// Canonical revision moved on since the job was captured; work left pending.
    StaleRevision,
    /// The memory is no longer recallable (deleted/invalidated); rows removed.
    Tombstoned,
    /// Embedding failed this pass: lexical row published, vector retry stays pending.
    SemanticPending,
}

/// Per-entity publication guard (design §8.2 step 4): serializes the
/// validate-then-publish path per memory so a late old embedding cannot regress
/// a newer row under concurrency within the daemon process (WP-04 guarantees one
/// daemon per store; cross-process ordering is covered by the singleton lock).
#[derive(Default)]
struct PublicationGuards {
    locks: HashMap<EntityId, Arc<Mutex<()>>>,
}

impl Projector {
    fn publication_lock(&mut self, id: EntityId) -> Arc<Mutex<()>> {
        self.guards
            .locks
            .entry(id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

pub struct Projector {
    repo: Arc<crate::service::repository::CanonicalRepository>,
    table: SearchTable,
    embedder: Box<dyn Embedder>,
    fingerprint: ModelFingerprint,
    generation: StoreGeneration,
    guards: PublicationGuards,
}

impl Projector {
    pub fn new(
        repo: Arc<crate::service::repository::CanonicalRepository>,
        table: SearchTable,
        embedder: Box<dyn Embedder>,
        fingerprint: ModelFingerprint,
        generation: StoreGeneration,
    ) -> Self {
        Self {
            repo,
            table,
            embedder,
            fingerprint,
            generation,
            guards: PublicationGuards::default(),
        }
    }

    /// Process one durable job end-to-end (design §8.2 steps 1-6).
    pub async fn process_job(&mut self, job: &ProjectionJob) -> DomainResult<ProjectorOutcome> {
        // Step 2: read canonical state fresh and capture its version.
        let Some(memory) = self
            .repo
            .get_memories(std::slice::from_ref(&job.memory_id))?
            .pop()
        else {
            return Ok(ProjectorOutcome::Tombstoned);
        };

        // Tombstone path (task 7): a non-recallable memory must not be projected.
        if !memory.lifecycle.is_recallable() || job.is_tombstone {
            self.propagate_deletion(job.memory_id).await?;
            let _ = self.repo.acknowledge_projection(job.memory_id, job.seq);
            return Ok(ProjectorOutcome::Tombstoned);
        }

        // Step 4 guard: canonical revision must still match the job's desired
        // revision. A concurrent update that advanced it means this job is stale;
        // a newer job already carries the current work, so we leave it pending.
        if memory.document_revision != job.desired_document_revision {
            return Ok(ProjectorOutcome::StaleRevision);
        }

        // Step 3: render and embed only the changed fields, one row per chunk
        // (RQ-10). A chunk-aware embedder returns a unit per derived chunk so
        // tail content beyond the first model window gets its own vector.
        let rows = self.render_chunk_rows(&memory, job.memory_id);

        // Step 5: idempotent publication under the per-entity guard.
        if !self.publish_rows_guarded(&rows).await? {
            return Ok(ProjectorOutcome::StaleRevision);
        }

        // Step 6: compare-and-clear exactly the job we published — but only when
        // every chunk has its vector; a missing vector leaves semantic work
        // pending while all chunks stay lexically indexed.
        if rows.iter().all(|r| r.embedding.is_some()) {
            self.repo.acknowledge_projection(job.memory_id, job.seq)?;
            Ok(ProjectorOutcome::Published)
        } else {
            // Re-enqueue at the same revision so the next pass retries embedding;
            // a higher seq keeps stale acknowledgements from clearing it.
            self.repo.enqueue_projection_job(
                job.memory_id,
                memory.document_revision,
                job.seq + 1,
                false,
            )?;
            Ok(ProjectorOutcome::SemanticPending)
        }
    }

    /// Render one [`SearchRow`] per [`TextChunk`] of a memory at its current
    /// canonical revision. Each chunk is embedded independently; a chunk whose
    /// embedding fails keeps a lexical-only row so a stalled worker never
    /// blocks indexing. An embedder that wrongly returns zero chunks falls back
    /// to the default single unit rather than silently dropping the memory.
    fn render_chunk_rows(
        &mut self,
        memory: &crate::domain::memory::Memory,
        memory_id: EntityId,
    ) -> Vec<SearchRow> {
        let mut chunks = self.embedder.chunk_text(&memory.title, &memory.fragment);
        if chunks.is_empty() {
            let text = render_text(&memory.title, &memory.fragment);
            let len = text.len() as u64;
            chunks = vec![TextChunk {
                text,
                char_start: 0,
                char_end: len,
            }];
        }
        chunks
            .iter()
            .enumerate()
            .map(|(i, c)| SearchRow {
                store_generation: self.generation,
                memory_id,
                document_revision: memory.document_revision,
                model_fingerprint: self.fingerprint,
                chunk_id: ChunkId::new(i as u32),
                chunker_version: self.embedder.chunker_version(),
                lexical_text: c.text.clone(),
                char_start: c.char_start,
                char_end: c.char_end,
                project: memory.project.clone(),
                fragment_type: memory.fragment_type.as_str().to_string(),
                created_at_millis: memory.created_at.as_millis(),
                updated_at_millis: memory.updated_at.as_millis(),
                embedding: self.embedder.embed(&c.text).ok(),
            })
            .collect()
    }

    /// The publication guard (task 5): re-validate canonical state immediately
    /// before writing so a late old embedding cannot regress a newer row. Holds
    /// the per-entity lock across validate + write, serializing concurrent
    /// projectors on the same memory within this daemon. Also rejects rows from
    /// an inactive store generation (T-PROJ-02).
    pub async fn publish_guarded(&mut self, row: &SearchRow) -> DomainResult<bool> {
        self.publish_rows_guarded(std::slice::from_ref(row)).await
    }

    /// Multi-row publication guard: validates once, then publishes the whole
    /// chunk set of one memory in a single guarded section, so a chunked
    /// revision cannot interleave with a newer revision's chunks from another
    /// projector sharing this instance. (Crash/replay interleaving below the
    /// table's merge-then-cleanup phases is still covered by the revision
    /// re-validation on retry, not by this lock.) All rows must belong to one
    /// memory at one revision; violations are rejected, never asserted.
    pub async fn publish_rows_guarded(&mut self, rows: &[SearchRow]) -> DomainResult<bool> {
        let Some(first) = rows.first() else {
            return Ok(false);
        };
        if !rows.iter().all(|r| {
            r.memory_id == first.memory_id && r.document_revision == first.document_revision
        }) {
            return Err(DomainError::new(
                DomainErrorCode::Validation,
                "guarded chunk set must belong to one memory at one revision",
            ));
        }
        let lock = self.publication_lock(first.memory_id);
        let _lock = lock.lock().await;

        // Reject rows from a stale or future store generation — except rows
        // for a generation under construction (blue-green build): a staged
        // generation is writable before activation so the new vector space
        // can converge alongside the old. Readers only follow the active
        // pointer (or an explicit pin), never a partial build.
        if self.repo.store_generation()? != first.store_generation
            && !self.generation_under_construction(first.store_generation)?
        {
            return Ok(false);
        }

        // Reject if the canonical revision has moved past this set's.
        let Some(memory) = self
            .repo
            .get_memories(std::slice::from_ref(&first.memory_id))?
            .pop()
        else {
            return Ok(false);
        };
        if memory.document_revision != first.document_revision {
            return Ok(false);
        }
        // Reject non-recallable memories (deleted/invalidated/archived).
        if !memory.lifecycle.is_recallable() {
            return Ok(false);
        }

        self.table.publish_rows(rows).await?;
        Ok(true)
    }

    /// Whether a generation is under construction (staged but not yet
    /// active) for this projector's fingerprint: its rows may be published
    /// before activation. Retired, fingerprint-mismatched and unknown
    /// generations stay refused — a misconfigured projector advancing the
    /// wrong vector space, or a delayed worker writing a dead generation,
    /// cannot overwrite or resurrect state.
    fn generation_under_construction(&self, generation: StoreGeneration) -> DomainResult<bool> {
        self.repo
            .generation_under_construction(generation, self.fingerprint)
    }

    /// Delete propagation (task 7): remove every projected row for a memory so
    /// deleted knowledge cannot be recalled or resurrected by a delayed worker.
    pub async fn propagate_deletion(&self, memory_id: EntityId) -> DomainResult<()> {
        let filter = format!("memory_id = '{}'", memory_id.as_uuid());
        self.table.delete_where(&filter).await?;
        Ok(())
    }

    /// Worker loop (production seam): drain all pending jobs until a full pass
    /// makes no progress. The daemon spawns this off its I/O workers; it is the
    /// only caller needed to keep Lance converged with canonical state. Returns
    /// the number of jobs fully resolved (published or tombstoned).
    pub async fn run_until_idle(&mut self) -> DomainResult<usize> {
        let mut total_resolved = 0usize;
        loop {
            let jobs = self.repo.projection_jobs()?;
            if jobs.is_empty() {
                return Ok(total_resolved);
            }

            // Processing order is irrelevant: stale jobs are refused by the
            // revision guard and left pending, so a retryable wakeup may arrive
            // in any sequence (RV-07).
            let mut resolved_this_pass = 0usize;
            for job in &jobs {
                match self.process_job(job).await? {
                    ProjectorOutcome::Published | ProjectorOutcome::Tombstoned => {
                        resolved_this_pass += 1;
                    }
                    ProjectorOutcome::StaleRevision | ProjectorOutcome::SemanticPending => {}
                }
            }

            total_resolved += resolved_this_pass;

            // Converged: a full pass cleared nothing (only stale or semantic-
            // retry work remains that needs newer canonical state).
            if resolved_this_pass == 0 {
                return Ok(total_resolved);
            }
        }
    }

    /// Rebuild (task 7): re-project every recallable canonical memory under this
    /// projector's fingerprint/generation. Because it reads only live,
    /// recallable records and guards each publish by revision, a rebuild can
    /// never resurrect a deleted/invalidated generation or regress a newer row.
    pub async fn rebuild(&mut self) -> DomainResult<usize> {
        let memories = self.repo.export_snapshot()?.memories;
        let mut published = 0usize;

        for memory in &memories {
            if !memory.lifecycle.is_recallable() {
                // Deleted/invalidated/archived: propagate deletion, never project.
                self.propagate_deletion(memory.id).await?;
                continue;
            }

            let rows = self.render_chunk_rows(memory, memory.id);
            // A stalled embedder must not block lexical indexing (T-PROJ-03):
            // publish the lexical rows now and leave the vector retry pending.
            if self.publish_rows_guarded(&rows).await? {
                published += 1;
            }
        }

        Ok(published)
    }
}

/// Rendered searchable text: title prefix + fragment body.
pub fn render_text(title: &str, fragment: &str) -> String {
    format!("{title}\n{fragment}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::command::{DomainCommand, ForgetMode};
    use crate::domain::id::{DocumentRevision, FrontendId};
    use crate::domain::memory::{FragmentType, Memory, MemoryLifecycle, MemorySource};
    use uuid::Uuid;

    fn eid(n: u64) -> EntityId {
        EntityId::new(Uuid::from_u128(n as u128))
    }

    fn memory(id: EntityId, title: &str, fragment: &str) -> Memory {
        Memory {
            id,
            external_alias: None,
            title: title.to_string(),
            fragment: fragment.to_string(),
            description: String::new(),
            fragment_type: FragmentType::Fact,
            project: Some("ltmrs".into()),
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

    /// Open a repo with a namespace and wire up an in-memory Lance table. The
    /// returned guard keeps both backing dirs alive for the test's lifetime.
    async fn env() -> (
        Arc<crate::service::repository::CanonicalRepository>,
        SearchTable,
        EnvGuard,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();

        let clock = Arc::new(crate::domain::clock::FrozenClock::new(1000));
        let repo_path = dir.path().to_str().unwrap();
        let repo =
            crate::service::repository::CanonicalRepository::open_with_clock(repo_path, clock)
                .unwrap();
        let fe = FrontendId::new(Uuid::from_u128(1));
        let ns = repo.issue_namespace(fe, 1000).unwrap();
        assert_eq!(ns.retry_epoch, 1);

        let uri = lance_dir.path().to_str().unwrap().to_string();
        let table = SearchTable::open(&uri).await.unwrap();

        (
            Arc::new(repo),
            table,
            EnvGuard {
                _dir: dir,
                _lance_dir: lance_dir,
            },
        )
    }

    struct EnvGuard {
        _dir: tempfile::TempDir,
        _lance_dir: tempfile::TempDir,
    }

    fn add(
        repo: &crate::service::repository::CanonicalRepository,
        n: u64,
        title: &str,
        frag: &str,
    ) {
        repo.apply(
            &ctx(n),
            &DomainCommand::AddMemory {
                memory: memory(eid(n), title, frag),
                session: None,
            },
        )
        .unwrap();
    }

    fn projector(
        repo: Arc<crate::service::repository::CanonicalRepository>,
        table: SearchTable,
    ) -> Projector {
        Projector::new(
            repo,
            table,
            Box::new(FixedEmbedder { dim: 384 }),
            ModelFingerprint::new(1),
            StoreGeneration::FIRST,
        )
    }

    #[tokio::test]
    async fn process_job_publishes_and_acknowledges() {
        let (repo, table, _guard) = env().await;
        add(&repo, 1, "hello", "world");
        let job = repo.projection_job(eid(1)).unwrap().unwrap();

        let mut p = projector(repo.clone(), table.clone());
        assert_eq!(
            p.process_job(&job).await.unwrap(),
            ProjectorOutcome::Published
        );

        // Job acknowledged (no longer pending) and a row is in the projection.
        assert!(!repo.has_pending_projection(eid(1)).unwrap());
        let count = table.count_rows(None).await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn late_old_embedding_cannot_regress_published_revision() {
        let (repo, table, _guard) = env().await;

        // Add memory (canonical document_revision = 1).
        add(&repo, 1, "t", "f");
        // Update content → canonical document_revision = 2.
        let patch = crate::domain::command::MemoryPatch {
            fragment: Some("new body".into()),
            ..Default::default()
        };
        repo.apply(
            &ctx(2),
            &DomainCommand::UpdateMemory {
                id: eid(1),
                expected_revision: None,
                patch,
            },
        )
        .unwrap();

        let mut p = projector(repo.clone(), table.clone());

        // A stale worker holds an embedding rendered at document_revision=1.
        let stale_row = SearchRow {
            store_generation: StoreGeneration::FIRST,
            memory_id: eid(1),
            document_revision: DocumentRevision::new(1),
            model_fingerprint: ModelFingerprint::new(1),
            chunk_id: ChunkId::new(0),
            chunker_version: "single-chunk-v1".to_string(),
            lexical_text: "stale text".into(),
            char_start: 0,
            char_end: 10,
            project: None,
            fragment_type: "fact".into(),
            created_at_millis: 0,
            updated_at_millis: 0,
            embedding: Some(vec![0.0; 384]),
        };

        // The guard must reject the stale row (canonical is now at rev 2).
        assert!(
            !p.publish_guarded(&stale_row).await.unwrap(),
            "a late old embedding must not be published over a newer revision"
        );
        let count = table.count_rows(None).await.unwrap();
        assert_eq!(count, 0, "no stale row may land in the projection");
    }

    #[tokio::test]
    async fn publish_guarded_accepts_matching_revision() {
        let (repo, table, _guard) = env().await;
        add(&repo, 1, "t", "f");

        let mut p = projector(repo.clone(), table.clone());

        // A row at the current canonical revision (1) is accepted.
        let ok_row = SearchRow {
            store_generation: StoreGeneration::FIRST,
            memory_id: eid(1),
            document_revision: DocumentRevision::new(1),
            model_fingerprint: ModelFingerprint::new(1),
            chunk_id: ChunkId::new(0),
            chunker_version: "single-chunk-v1".to_string(),
            lexical_text: "current text".into(),
            char_start: 0,
            char_end: 12,
            project: None,
            fragment_type: "fact".into(),
            created_at_millis: 0,
            updated_at_millis: 0,
            embedding: Some(vec![1.0; 384]),
        };

        assert!(p.publish_guarded(&ok_row).await.unwrap());
        let count = table.count_rows(None).await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn deletion_propagates_to_projection() {
        let (repo, table, _guard) = env().await;
        add(&repo, 1, "t", "f");

        let mut p = projector(repo.clone(), table.clone());
        // Publish a row first.
        let ok_row = SearchRow {
            store_generation: StoreGeneration::FIRST,
            memory_id: eid(1),
            document_revision: DocumentRevision::new(1),
            model_fingerprint: ModelFingerprint::new(1),
            chunk_id: ChunkId::new(0),
            chunker_version: "single-chunk-v1".to_string(),
            lexical_text: "x".into(),
            char_start: 0,
            char_end: 1,
            project: None,
            fragment_type: "fact".into(),
            created_at_millis: 0,
            updated_at_millis: 0,
            embedding: Some(vec![1.0; 384]),
        };
        p.publish_guarded(&ok_row).await.unwrap();
        assert_eq!(table.count_rows(None).await.unwrap(), 1);

        // Forgetting the memory must remove its projected rows.
        repo.apply(
            &ctx(2),
            &DomainCommand::Forget {
                id: eid(1),
                mode: ForgetMode::Delete,
            },
        )
        .unwrap();
        p.propagate_deletion(eid(1)).await.unwrap();

        assert_eq!(
            table.count_rows(None).await.unwrap(),
            0,
            "deleted memory's rows must be removed from the projection"
        );
    }

    #[tokio::test]
    async fn process_job_returns_tombstoned_for_deleted_memory() {
        let (repo, table, _guard) = env().await;
        add(&repo, 1, "t", "f");

        // A job exists for the live memory.
        let job = repo.projection_job(eid(1)).unwrap().unwrap();

        // Forget it before projecting: rows must be removed and outcome is Tombstoned.
        repo.apply(
            &ctx(2),
            &DomainCommand::Forget {
                id: eid(1),
                mode: ForgetMode::Delete,
            },
        )
        .unwrap();

        let mut p = projector(repo.clone(), table.clone());
        assert_eq!(
            p.process_job(&job).await.unwrap(),
            ProjectorOutcome::Tombstoned
        );
        let count = table.count_rows(None).await.unwrap();
        assert_eq!(count, 0);
    }

    /// Task 6: events are retryable wakeups, not an ordered commit log. A job
    /// whose desired revision is already stale (a newer mutation superseded it)
    /// must be left pending, never force-published — so processing order cannot
    /// corrupt the projection.
    #[tokio::test]
    async fn out_of_order_stale_job_is_left_pending_not_published() {
        let (repo, table, _guard) = env().await;

        // Add memory → canonical document_revision 1, job seq=1 desired_rev=1.
        add(&repo, 1, "t", "f");
        let stale_job = repo.projection_job(eid(1)).unwrap().unwrap();
        assert_eq!(stale_job.seq, 1);

        // A concurrent update advances canonical to rev 2 and the job to seq=2.
        let patch = crate::domain::command::MemoryPatch {
            fragment: Some("v2".into()),
            ..Default::default()
        };
        repo.apply(
            &ctx(2),
            &DomainCommand::UpdateMemory {
                id: eid(1),
                expected_revision: None,
                patch,
            },
        )
        .unwrap();

        // A delayed worker replays the OLD job (seq=1) out of order. It must be
        // refused as stale — not published and not acknowledged.
        let mut p = projector(repo.clone(), table.clone());
        assert_eq!(
            p.process_job(&stale_job).await.unwrap(),
            ProjectorOutcome::StaleRevision,
            "a late/out-of-order job must be left pending"
        );

        // Work is still pending (the newer seq=2 job remains), and nothing stale landed.
        assert!(repo.has_pending_projection(eid(1)).unwrap());
        let count = table.count_rows(None).await.unwrap();
        assert_eq!(count, 0);

        // Processing the CURRENT job now converges to exactly one row at rev 2.
        let current_job = repo.projection_job(eid(1)).unwrap().unwrap();
        assert_eq!(current_job.seq, 2);
        assert_eq!(
            p.process_job(&current_job).await.unwrap(),
            ProjectorOutcome::Published
        );
        let count = table.count_rows(None).await.unwrap();
        assert_eq!(count, 1, "the projection converges to the latest revision");
    }

    /// Task 7: a rebuild must never resurrect a deleted generation. Deleted and
    /// live memories coexist canonically; the rebuild projects only the live one
    /// and removes any rows for the deleted one.
    #[tokio::test]
    async fn rebuild_does_not_resurrect_deleted_memory() {
        let (repo, table, _guard) = env().await;

        // Two memories: 1 will be deleted, 2 stays live.
        add(&repo, 1, "doomed", "gone");
        add(&repo, 2, "keeper", "stays");

        let mut p = projector(repo.clone(), table.clone());
        // First publish both while live (simulating prior state).
        for n in [1u64, 2] {
            if let Some(job) = repo.projection_job(eid(n)).unwrap() {
                let _ = p.process_job(&job).await.unwrap();
            }
        }
        assert_eq!(table.count_rows(None).await.unwrap(), 2);

        // Delete memory 1 canonically.
        repo.apply(
            &ctx(3),
            &DomainCommand::Forget {
                id: eid(1),
                mode: ForgetMode::Delete,
            },
        )
        .unwrap();

        // Rebuild from canonical state: only the live memory is projected and the
        // deleted one's rows are removed.
        let published = p.rebuild().await.unwrap();
        assert_eq!(published, 1, "only recallable memories may be rebuilt");

        // The deleted memory cannot be resurrected by the rebuild.
        let pred = format!("memory_id = '{}'", eid(1).as_uuid());
        assert_eq!(table.count_rows(Some(&pred)).await.unwrap(), 0);
        let total = table.count_rows(None).await.unwrap();
        assert_eq!(total, 1, "deleted generation must not reappear");
    }

    /// Task 8: blue-green model/index generation. A new fingerprint builds its own
    /// rows without touching the old generation's; publication is per-fingerprint,
    /// so readers of the old space keep working until they drain.
    #[tokio::test]
    async fn blue_green_generations_are_isolated_by_fingerprint() {
        let (repo, table, _guard) = env().await;
        add(&repo, 1, "t", "f");

        // Green projector with a NEW model fingerprint.
        let green_fp = ModelFingerprint::new(2);
        let mut p_green = Projector::new(
            repo.clone(),
            table.clone(),
            Box::new(FixedEmbedder { dim: 384 }),
            green_fp,
            StoreGeneration::FIRST,
        );

        // Build the new generation's rows.
        if let Some(job) = repo.projection_job(eid(1)).unwrap() {
            assert_eq!(
                p_green.process_job(&job).await.unwrap(),
                ProjectorOutcome::Published
            );
        }

        // The row is tagged with the NEW fingerprint only — the old space (fp=1)
        // has nothing, so a reader pinned to the old model sees no false hits.
        let pred_new = format!("model_fingerprint = {}", green_fp.as_u64());
        assert_eq!(table.count_rows(Some(&pred_new)).await.unwrap(), 1);

        let pred_old = "model_fingerprint = 1";
        assert_eq!(
            table.count_rows(Some(pred_old)).await.unwrap(),
            0,
            "a new model must not write into the old vector space"
        );
    }

    /// A projector with the wrong fingerprint cannot advance a staged build:
    /// staged publication is bound to the recorded build fingerprint so
    /// vector spaces never mix silently.
    #[tokio::test]
    async fn staged_build_rejects_wrong_fingerprint() {
        let (repo, table, _guard) = env().await;
        add(&repo, 1, "t", "f");
        let gen2 = repo.stage_generation(ModelFingerprint::new(7)).unwrap();

        let mut p_wrong = Projector::new(
            repo.clone(),
            table.clone(),
            Box::new(FixedEmbedder { dim: 384 }),
            ModelFingerprint::new(8),
            gen2,
        );
        let job = repo.projection_job(eid(1)).unwrap().unwrap();
        assert_eq!(
            p_wrong.process_job(&job).await.unwrap(),
            ProjectorOutcome::StaleRevision,
            "wrong-fingerprint rows must be refused during a staged build"
        );
        assert_eq!(table.count_rows(None).await.unwrap(), 0);
    }

    /// Task 6: replaying an already-acknowledged job is idempotent — a retried
    /// wakeup cannot double-publish or resurrect cleared work.
    #[tokio::test]
    async fn replayed_acknowledged_job_is_idempotent() {
        let (repo, table, _guard) = env().await;
        add(&repo, 1, "t", "f");
        let job = repo.projection_job(eid(1)).unwrap().unwrap();

        let mut p = projector(repo.clone(), table.clone());
        assert_eq!(
            p.process_job(&job).await.unwrap(),
            ProjectorOutcome::Published
        );
        assert!(!repo.has_pending_projection(eid(1)).unwrap());

        // The durable job is gone; a retried wakeup for the same seq finds no work.
        let outcome = p.process_job(&job).await.unwrap();
        // Re-processing an already-cleared, still-current memory republishes
        // idempotently (same revision) — never a duplicate row.
        assert_eq!(outcome, ProjectorOutcome::Published);
        let count = table.count_rows(None).await.unwrap();
        assert_eq!(count, 1, "replaying a wakeup must not create duplicates");
    }

    // ---- Plan acceptance tests (plans/03 §T-PROJ / T-SEARCH) ----

    /// T-PROJ-01: commit events in an order different from their UUID
    /// timestamps; kill between canonical commit, projection commit and
    /// acknowledgement. No late event is skipped; replay is idempotent; a newer
    /// desired revision stays pending.
    #[tokio::test]
    async fn t_proj_01_kill_between_commit_and_ack_keeps_newer_work_pending() {
        let (repo, table, _guard) = env().await;
        add(&repo, 1, "v1", "f");

        // Canonical commit is durable: a job exists. Simulate a crash BEFORE the
        // projector reads it — on restart the same job is still pending.
        let mut p = projector(repo.clone(), table.clone());
        let job_v1 = repo
            .projection_job(eid(1))
            .unwrap()
            .expect("job survives restart");

        // A late update commits while the old worker is still "in flight". The
        // old worker's compare-and-clear must be refused (seq advanced).
        let patch = crate::domain::command::MemoryPatch {
            fragment: Some("v2 body".into()),
            ..Default::default()
        };
        repo.apply(
            &ctx(2),
            &DomainCommand::UpdateMemory {
                id: eid(1),
                expected_revision: None,
                patch,
            },
        )
        .unwrap();
        let job_v2 = repo
            .projection_job(eid(1))
            .unwrap()
            .expect("update enqueues a newer job");
        assert!(job_v2.seq > job_v1.seq);
        assert_eq!(job_v2.desired_document_revision, DocumentRevision::new(2));

        // The stale worker's acknowledgement is refused: the newer revision stays
        // pending and no trace remains.
        assert!(!repo.acknowledge_projection(eid(1), job_v1.seq).unwrap());
        let still_pending = repo
            .projection_job(eid(1))
            .unwrap()
            .expect("newer work survives");
        assert_eq!(still_pending, job_v2);

        // Processing the newer job publishes exactly once; replaying it is
        // idempotent (same revision republishes, never duplicates).
        assert_eq!(
            p.process_job(&job_v2).await.unwrap(),
            ProjectorOutcome::Published
        );
        assert!(!repo.has_pending_projection(eid(1)).unwrap());
        let outcome = p.process_job(&job_v2).await.unwrap();
        assert_eq!(outcome, ProjectorOutcome::Published);
        assert_eq!(table.count_rows(None).await.unwrap(), 1);
    }

    /// T-PROJ-02: delay an old embedding, update the memory, then restore to a
    /// new store generation. Old jobs cannot overwrite or resurrect current
    /// state; generation/fingerprint guards reject stale publication.
    #[tokio::test]
    async fn t_proj_02_stale_publication_rejected_after_generation_change() {
        let (repo, table, _guard) = env().await;
        add(&repo, 1, "t", "f");

        // A projector pinned to the FIRST generation publishes normally.
        let mut p_old_gen = Projector::new(
            repo.clone(),
            table.clone(),
            Box::new(FixedEmbedder { dim: 384 }),
            ModelFingerprint::new(1),
            StoreGeneration::FIRST,
        );
        if let Some(job) = repo.projection_job(eid(1)).unwrap() {
            assert_eq!(
                p_old_gen.process_job(&job).await.unwrap(),
                ProjectorOutcome::Published
            );
        }
        assert_eq!(table.count_rows(None).await.unwrap(), 1);

        // Update the memory: a new pending job carries the current revision.
        let patch = crate::domain::command::MemoryPatch {
            fragment: Some("updated".into()),
            ..Default::default()
        };
        repo.apply(
            &ctx(2),
            &DomainCommand::UpdateMemory {
                id: eid(1),
                expected_revision: None,
                patch,
            },
        )
        .unwrap();

        // A restore switches the store to a new generation. The old-generation
        // projector must now be refused: its rows are stale by construction.
        repo.set_store_generation(StoreGeneration::new(2)).unwrap();

        let job = repo
            .projection_job(eid(1))
            .unwrap()
            .expect("update enqueues a job");
        assert_eq!(
            p_old_gen.process_job(&job).await.unwrap(),
            ProjectorOutcome::StaleRevision,
            "an old-generation projector must be refused after a restore"
        );

        // No stale publication landed: still only the original rev-1 row.
        let gen1 = table.rows_where("store_generation = 1").await.unwrap();
        assert_eq!(gen1.len(), 1);
        assert_eq!(gen1[0].document_revision, DocumentRevision::new(1));

        // A new-generation projector publishes the current state instead.
        let mut p_new_gen = Projector::new(
            repo.clone(),
            table.clone(),
            Box::new(FixedEmbedder { dim: 384 }),
            ModelFingerprint::new(2),
            StoreGeneration::new(2),
        );
        assert_eq!(
            p_new_gen.process_job(&job).await.unwrap(),
            ProjectorOutcome::Published
        );

        // The new-generation row carries the current revision.
        let gen2 = table.rows_where("store_generation = 2").await.unwrap();
        assert_eq!(gen2.len(), 1);
        assert_eq!(gen2[0].document_revision, DocumentRevision::new(2));
    }

    /// T-PROJ-03: add/update/delete and immediately read by ID, lexically and
    /// semantically; stall the embedder; reopen cached Lance readers. Direct
    /// read-your-writes holds; lexical/semantic readiness is accurate; a stalled
    /// embedder cannot prevent direct reads or lexical indexing.
    #[tokio::test]
    async fn t_proj_03_stalled_embedder_keeps_lexical_and_direct_reads() {
        let (repo, table, _guard) = env().await;

        // Add memory: canonical durability is immediate (direct read-your-writes).
        add(&repo, 1, "unique-term", "body");
        assert_eq!(repo.get_memories(&[eid(1)]).unwrap().len(), 1);

        // Stalled embedder: the worker still publishes the LEXICAL row and keeps
        // semantic work pending.
        let mut p_stalled = Projector::new(
            repo.clone(),
            table.clone(),
            Box::new(StalledEmbedder),
            ModelFingerprint::new(1),
            StoreGeneration::FIRST,
        );
        let resolved = p_stalled.run_until_idle().await.unwrap();
        assert_eq!(
            resolved, 0,
            "no job is fully resolvable while the embedder stalls"
        );

        // Lexical indexing works: one row exists with a NULL vector.
        assert_eq!(table.count_rows(None).await.unwrap(), 1);
        let null_vec = table.count_rows(Some("embedding IS NULL")).await.unwrap();
        assert_eq!(
            null_vec, 1,
            "a stalled embedder must not block lexical rows"
        );

        // Readiness is accurate: work is still pending (semantic phase).
        assert!(repo.has_pending_projection(eid(1)).unwrap());
        assert!(repo.projection_lag().unwrap() >= 1);

        // Update content: canonical read-your-writes holds immediately.
        let patch = crate::domain::command::MemoryPatch {
            fragment: Some("updated body".into()),
            ..Default::default()
        };
        repo.apply(
            &ctx(2),
            &DomainCommand::UpdateMemory {
                id: eid(1),
                expected_revision: None,
                patch,
            },
        )
        .unwrap();
        let rec = repo.get_memories(&[eid(1)]).unwrap().pop().unwrap();
        assert_eq!(rec.fragment, "updated body");

        // A working embedder now completes the semantic phase for the CURRENT
        // revision only (the stale lexical row is superseded and removed).
        let mut p_working = Projector::new(
            repo.clone(),
            table.clone(),
            Box::new(FixedEmbedder { dim: 384 }),
            ModelFingerprint::new(1),
            StoreGeneration::FIRST,
        );
        p_working.run_until_idle().await.unwrap();

        // Exactly one row at the latest revision with a vector; no stale text.
        assert_eq!(table.count_rows(None).await.unwrap(), 1);
        let vec_rows = table.rows_where("embedding IS NOT NULL").await.unwrap();
        assert_eq!(vec_rows.len(), 1);
        assert!(vec_rows[0].lexical_text.contains("updated body"));

        // Delete: rows propagate away; direct reads still observe the tombstone.
        repo.apply(
            &ctx(3),
            &DomainCommand::Forget {
                id: eid(1),
                mode: ForgetMode::Delete,
            },
        )
        .unwrap();
        p_working.run_until_idle().await.unwrap();
        assert_eq!(table.count_rows(None).await.unwrap(), 0);

        // A reopened reader observes the converged (empty) state.
        let mut fresh = table.clone();
        fresh.refresh().await.unwrap();
        assert_eq!(fresh.count_rows(None).await.unwrap(), 0);
    }

    /// T-SEARCH-01: empty database/query, FTS not yet built, null/missing
    /// vectors, and optimization concurrent with queries. Clear valid outcomes;
    /// no crash or false completeness.
    #[tokio::test]
    async fn t_search_01_empty_db_and_concurrent_optimization() {
        let dir = tempfile::tempdir().unwrap();
        let tbl = SearchTable::open(dir.path().to_str().unwrap())
            .await
            .unwrap();

        // Empty database: FTS query and predicate queries return clean empties.
        assert!(
            tbl.fts_query("anything", 10, None)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(tbl.count_rows(None).await.unwrap(), 0);
        assert!(
            tbl.rows_where("lexical_text LIKE '%x%'")
                .await
                .unwrap()
                .is_empty()
        );

        // Null-vector rows: semantic completeness must not be claimed.
        let only_text = SearchRow {
            store_generation: StoreGeneration::FIRST,
            memory_id: eid(1),
            document_revision: DocumentRevision::new(0),
            model_fingerprint: ModelFingerprint::new(1),
            chunk_id: ChunkId::new(0),
            chunker_version: "single-chunk-v1".to_string(),
            lexical_text: "only text".into(),
            char_start: 0,
            char_end: 9,
            project: None,
            fragment_type: "fact".into(),
            created_at_millis: 0,
            updated_at_millis: 0,
            embedding: None,
        };
        tbl.publish_rows(std::slice::from_ref(&only_text))
            .await
            .unwrap();
        let with_vectors = table_with_vector_count(&tbl).await;
        assert_eq!(with_vectors, 0);

        // Optimization concurrent with queries: both must succeed without crash.
        let tbl_opt = tbl.clone();
        let (opt_result, query_ok) = tokio::join!(tbl_opt.optimize(), tbl.count_rows(None));
        assert!(
            query_ok.is_ok(),
            "queries must work while optimization runs"
        );
        opt_result.unwrap();

        // Data intact after concurrent optimization.
        assert_eq!(tbl.count_rows(None).await.unwrap(), 1);
    }

    async fn table_with_vector_count(tbl: &SearchTable) -> u64 {
        tbl.count_rows(Some("embedding IS NOT NULL")).await.unwrap()
    }

    // ---- RQ-10 multi-chunk projection (WP-06 follow-up) ----

    /// Shared halving policy for the chunk-aware test doubles below: split the
    /// fragment at a line boundary when present, else at a char boundary near
    /// the midpoint. Each unit carries the title prefix for lexical
    /// searchability with offsets in rendered coordinates.
    fn halving_chunks(title: &str, fragment: &str) -> Vec<TextChunk> {
        let base = title.len() + 1; // "title\n" rendered prefix
        let mid = fragment.find('\n').map(|i| i + 1).unwrap_or_else(|| {
            let mut m = fragment.len() / 2;
            while !fragment.is_char_boundary(m) {
                m -= 1;
            }
            m
        });
        let (first, second) = fragment.split_at(mid);
        vec![
            TextChunk {
                text: format!("{title}\n{first}"),
                char_start: base as u64,
                char_end: (base + first.len()) as u64,
            },
            TextChunk {
                text: format!("{title}\n{second}"),
                char_start: (base + first.len()) as u64,
                char_end: (base + fragment.len()) as u64,
            },
        ]
    }

    /// Test embedder with a chunk-aware policy: splits the fragment into two
    /// halves (line boundary when present) and embeds each span separately.
    /// `fail` models a stalled worker for the SemanticPending policy test.
    struct HalvingEmbedder {
        dim: usize,
        fail: bool,
    }

    impl Embedder for HalvingEmbedder {
        fn embed(&mut self, text: &str) -> Result<Vec<f32>, String> {
            if self.fail {
                return Err("embedding service unavailable".into());
            }
            Ok((0..self.dim)
                .map(|i| {
                    ((text.bytes().fold(0u64, |a, b| a.wrapping_add(b as u64)) >> (i % 64))
                        ^ i as u64) as f32
                        / 1e9
                })
                .collect())
        }

        fn chunk_text(&self, title: &str, fragment: &str) -> Vec<TextChunk> {
            halving_chunks(title, fragment)
        }

        fn chunker_version(&self) -> String {
            "test-halving-v1".to_string()
        }
    }

    /// Test embedder whose second chunk always fails: pins the all-or-pending
    /// policy for mixed partial embeddings (an `any`-instead-of-`all` ack
    /// check must not pass this test). Shares the halving policy — and its
    /// version — with HalvingEmbedder so attribution stays exact.
    struct SecondChunkFailsEmbedder {
        dim: usize,
    }

    impl Embedder for SecondChunkFailsEmbedder {
        fn embed(&mut self, text: &str) -> Result<Vec<f32>, String> {
            if text.contains("beta-half") {
                return Err("tail chunk unavailable".into());
            }
            Ok(vec![0.5; self.dim])
        }

        fn chunk_text(&self, title: &str, fragment: &str) -> Vec<TextChunk> {
            halving_chunks(title, fragment)
        }

        fn chunker_version(&self) -> String {
            "test-halving-v1".to_string()
        }
    }

    /// Projected rows carry the embedder's chunker version so a policy
    /// change is attributable per row (never silently mixed).
    #[tokio::test]
    async fn projected_rows_carry_chunker_version() {
        let (repo, table, _guard) = env().await;
        add(&repo, 1, "long", "alpha-half\nbeta-half");
        let job = repo.projection_job(eid(1)).unwrap().unwrap();
        let mut p = Projector::new(
            repo.clone(),
            table.clone(),
            Box::new(HalvingEmbedder {
                dim: 384,
                fail: false,
            }),
            ModelFingerprint::new(1),
            StoreGeneration::FIRST,
        );
        assert_eq!(
            p.process_job(&job).await.unwrap(),
            ProjectorOutcome::Published
        );
        let rows = table
            .rows_where("chunker_version = 'test-halving-v1'")
            .await
            .unwrap();
        assert_eq!(rows.len(), 2, "every chunk row carries the version");
        assert_eq!(table.count_rows(None).await.unwrap(), 2);
    }

    /// RQ-10: a long memory projects one row per chunk (not a single
    /// first-window row). Each row carries its chunk id, evidence span and
    /// vector; the job is acknowledged only when every chunk is embedded.
    #[tokio::test]
    async fn long_memory_projects_one_row_per_chunk() {
        let (repo, table, _guard) = env().await;
        add(&repo, 1, "long", "alpha-half\nbeta-half");
        let job = repo.projection_job(eid(1)).unwrap().unwrap();

        let mut p = Projector::new(
            repo.clone(),
            table.clone(),
            Box::new(HalvingEmbedder {
                dim: 384,
                fail: false,
            }),
            ModelFingerprint::new(1),
            StoreGeneration::FIRST,
        );
        assert_eq!(
            p.process_job(&job).await.unwrap(),
            ProjectorOutcome::Published
        );
        assert!(!repo.has_pending_projection(eid(1)).unwrap());

        let rows = table.rows_where("embedding IS NOT NULL").await.unwrap();
        assert_eq!(rows.len(), 2, "one vector row per chunk expected");
        let mut chunk_ids: Vec<u32> = rows.iter().map(|r| r.chunk_id.as_u32()).collect();
        chunk_ids.sort_unstable();
        assert_eq!(chunk_ids, vec![0, 1]);
        // Evidence spans are disjoint and jointly cover the fragment body.
        let mut spans: Vec<(u64, u64)> = rows.iter().map(|r| (r.char_start, r.char_end)).collect();
        spans.sort_unstable();
        assert!(spans[0].1 <= spans[1].0, "chunk spans must not overlap");
        let title_len = "long".len() as u64;
        assert_eq!(
            spans[0].0,
            title_len + 1,
            "first span must start at the fragment body"
        );
        assert_eq!(
            spans[0].1, spans[1].0,
            "adjacent chunk spans must join with no dropped middle"
        );
        assert_eq!(
            spans[1].1,
            render_text("long", "alpha-half\nbeta-half").len() as u64,
            "last span must reach the end of the rendered text"
        );
        assert!(
            rows.iter().any(|r| r.lexical_text.contains("alpha-half")),
            "first-half content must be projected"
        );
        assert!(
            rows.iter().any(|r| r.lexical_text.contains("beta-half")),
            "tail content beyond the first window must be projected"
        );
    }

    /// A chunked rebuild supersedes as a unit: after a content update the new
    /// revision has exactly the new chunk set and no row of the old revision
    /// survives (exercises table.rs group cleanup with multi-chunk sets).
    #[tokio::test]
    async fn chunked_rebuild_supersedes_as_unit() {
        let (repo, table, _guard) = env().await;
        add(&repo, 1, "long", "alpha-half\nbeta-half");

        let mut p = Projector::new(
            repo.clone(),
            table.clone(),
            Box::new(HalvingEmbedder {
                dim: 384,
                fail: false,
            }),
            ModelFingerprint::new(1),
            StoreGeneration::FIRST,
        );
        assert_eq!(p.rebuild().await.unwrap(), 1);
        assert_eq!(table.count_rows(None).await.unwrap(), 2);

        // Update content: canonical document_revision advances to 2.
        let patch = crate::domain::command::MemoryPatch {
            fragment: Some("gamma-half\ndelta-half".into()),
            ..Default::default()
        };
        repo.apply(
            &ctx(2),
            &DomainCommand::UpdateMemory {
                id: eid(1),
                expected_revision: None,
                patch,
            },
        )
        .unwrap();
        assert_eq!(p.rebuild().await.unwrap(), 1);

        let rev2 = table.rows_where("document_revision = 2").await.unwrap();
        assert_eq!(rev2.len(), 2, "new revision must carry the full chunk set");
        let rev1 = table.rows_where("document_revision = 1").await.unwrap();
        assert_eq!(rev1.len(), 0, "no old-revision chunk may survive");
        assert_eq!(table.count_rows(None).await.unwrap(), 2);
    }

    /// All-or-pending chunk policy: when any chunk embedding fails, every
    /// chunk still gets its lexical row now, the job stays pending, and a
    /// recovered worker converges all chunks to vectors without duplicates.
    #[tokio::test]
    async fn stalled_chunking_embedder_leaves_all_chunks_lexical_and_pending() {
        let (repo, table, _guard) = env().await;
        add(&repo, 1, "long", "alpha-half\nbeta-half");
        let job = repo.projection_job(eid(1)).unwrap().unwrap();

        let mut p_stalled = Projector::new(
            repo.clone(),
            table.clone(),
            Box::new(HalvingEmbedder {
                dim: 384,
                fail: true,
            }),
            ModelFingerprint::new(1),
            StoreGeneration::FIRST,
        );
        assert_eq!(
            p_stalled.process_job(&job).await.unwrap(),
            ProjectorOutcome::SemanticPending
        );
        // Both chunks indexed lexically; no vector claimed.
        assert_eq!(table.count_rows(None).await.unwrap(), 2);
        assert_eq!(
            table.count_rows(Some("embedding IS NULL")).await.unwrap(),
            2
        );
        assert!(repo.has_pending_projection(eid(1)).unwrap());

        // A recovered worker completes every chunk; replay is idempotent.
        let mut p_working = Projector::new(
            repo.clone(),
            table.clone(),
            Box::new(HalvingEmbedder {
                dim: 384,
                fail: false,
            }),
            ModelFingerprint::new(1),
            StoreGeneration::FIRST,
        );
        p_working.run_until_idle().await.unwrap();
        assert!(!repo.has_pending_projection(eid(1)).unwrap());
        assert_eq!(table.count_rows(None).await.unwrap(), 2);
        assert_eq!(
            table
                .count_rows(Some("embedding IS NOT NULL"))
                .await
                .unwrap(),
            2
        );
    }

    /// Mixed partial embeddings stay pending: when only one chunk has a
    /// vector, the job must NOT be acknowledged (an `any`-instead-of-`all`
    /// ack check must fail this test). Both chunks stay lexically indexed.
    #[tokio::test]
    async fn mixed_partial_embedding_stays_pending() {
        let (repo, table, _guard) = env().await;
        add(&repo, 1, "long", "alpha-half\nbeta-half");
        let job = repo.projection_job(eid(1)).unwrap().unwrap();

        let mut p = Projector::new(
            repo.clone(),
            table.clone(),
            Box::new(SecondChunkFailsEmbedder { dim: 384 }),
            ModelFingerprint::new(1),
            StoreGeneration::FIRST,
        );
        assert_eq!(
            p.process_job(&job).await.unwrap(),
            ProjectorOutcome::SemanticPending,
            "one missing chunk vector must keep the job pending"
        );
        assert_eq!(table.count_rows(None).await.unwrap(), 2);
        assert_eq!(
            table
                .count_rows(Some("embedding IS NOT NULL"))
                .await
                .unwrap(),
            1,
            "only the successful chunk may carry a vector"
        );
        assert!(repo.has_pending_projection(eid(1)).unwrap());
    }
}
