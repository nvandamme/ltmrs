//! Projection worker: job drives, publishing, convergence, rebuild (moved verbatim from `projector.rs`).

use std::sync::Arc;

use tokio::sync::Mutex;

use super::{Embedder, Projector, ProjectorOutcome, StalledEmbedder, TextChunk, render_text};
use crate::search::row::SearchRow;
use crate::search::table::SearchTable;
use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult};
use ltmrs_domain::id::{ChunkId, EntityId, StoreGeneration};
use ltmrs_domain::projection::ProjectionJob;

impl Projector {
    fn publication_lock(&mut self, id: EntityId) -> Arc<Mutex<()>> {
        self.guards
            .locks
            .entry(id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// One-shot production drive: project pending jobs with the given
    /// embedder under the E5 fingerprint at the repo's current generation.
    /// Reading the generation fresh on every call means a generation
    /// cutover can never strand a stale projector refusing publishes.
    /// Resolves at most `max_jobs` (fairness §7.3); the remainder stays
    /// pending for the next drive. Returns jobs fully resolved (published
    /// or tombstoned); embed failures stay pending for the next drive.
    pub async fn project_pending(
        repo: &Arc<ltmrs_service::repository::CanonicalRepository>,
        table: &SearchTable,
        embedder: Box<dyn Embedder>,
        max_jobs: usize,
    ) -> DomainResult<usize> {
        use ltmrs_embeddings::e5_small::E5_SMALL_FINGERPRINT;

        let generation = repo.store_generation()?;
        let mut projector = Projector::new(
            Arc::clone(repo),
            table.clone(),
            embedder,
            E5_SMALL_FINGERPRINT,
            generation,
        );
        projector.run_capped(max_jobs).await
    }

    /// One-shot lexical drive (no embedding model): publish text rows with
    /// NULL vectors and resolve the jobs. Rows carry the E5 fingerprint so
    /// a later E5 backfill recognizes and re-embeds exactly these rows; the
    /// dense leg excludes NULL vectors explicitly meanwhile.
    pub async fn project_pending_lexical(
        repo: &Arc<ltmrs_service::repository::CanonicalRepository>,
        table: &SearchTable,
        max_jobs: usize,
    ) -> DomainResult<usize> {
        use ltmrs_embeddings::e5_small::E5_SMALL_FINGERPRINT;

        let generation = repo.store_generation()?;
        let mut projector = Projector::new(
            Arc::clone(repo),
            table.clone(),
            Box::new(StalledEmbedder),
            E5_SMALL_FINGERPRINT,
            generation,
        );
        projector.lexical_only = true;
        projector.run_capped(max_jobs).await
    }

    /// Process one durable job end-to-end (design §8.2 steps 1-6).
    pub async fn process_job(&mut self, job: &ProjectionJob) -> DomainResult<ProjectorOutcome> {
        // Step 2: read canonical state fresh and capture its version.
        // A job whose memory is gone is ORPHANED, not healthy: explicitly
        // remove its stale rows and compare-and-clear the job itself, so
        // the pass retires real work instead of recounting a ghost every
        // tick (re-review P2-1). A superseded ack (false) means a newer job
        // owns this work — report stale, never false progress.
        let Some(memory) = self
            .repo
            .get_memories(std::slice::from_ref(&job.memory_id))?
            .pop()
        else {
            self.propagate_deletion(job.memory_id).await?;
            if self.repo.acknowledge_projection(job.memory_id, job.seq)? {
                return Ok(ProjectorOutcome::Tombstoned);
            }
            return Ok(ProjectorOutcome::StaleRevision);
        };

        // Tombstone path (task 7): a non-recallable memory must not be projected.
        if !memory.lifecycle.is_recallable() || job.is_tombstone {
            self.propagate_deletion(job.memory_id).await?;
            if self.repo.acknowledge_projection(job.memory_id, job.seq)? {
                return Ok(ProjectorOutcome::Tombstoned);
            }
            return Ok(ProjectorOutcome::StaleRevision);
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
        // pending while all chunks stay lexically indexed. Lexical-only mode
        // resolves instead: NULL-vector rows are the converged state there.
        if self.lexical_only {
            self.repo.acknowledge_projection(job.memory_id, job.seq)?;
            return Ok(ProjectorOutcome::Published);
        }
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
        memory: &ltmrs_domain::memory::Memory,
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
        let vectors: Vec<Option<Vec<f32>>> = self
            .embedder
            .embed_texts(&chunks.iter().map(|c| c.text.clone()).collect::<Vec<_>>())
            .into_iter()
            .map(|r| r.ok())
            .collect();
        assert_eq!(
            vectors.len(),
            chunks.len(),
            "batch embedder must answer per chunk"
        );
        chunks
            .iter()
            .enumerate()
            .zip(vectors)
            .map(|((i, c), embedding)| SearchRow {
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
                confidence: memory.confidence,
                embedding,
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

    /// Table-measured convergence check (cutover watermark verification):
    /// true when every live recallable canonical memory has at least one
    /// projected row in this generation AND model space. Lexical-only rows
    /// count (vectors are a separate readiness dimension); deleted/
    /// invalidated memories are excluded via the tombstone path, not
    /// required. The fingerprint scope matters: a wrong-space projector
    /// must not report converged off another space's rows. The operator
    /// calls this after the final build and before activation instead of
    /// trusting the reported numerator alone.
    pub async fn verify_generation_converged(
        &self,
        generation: StoreGeneration,
    ) -> DomainResult<bool> {
        let live: std::collections::BTreeSet<EntityId> = self
            .repo
            .export_snapshot()?
            .memories
            .into_iter()
            .filter(|m| m.lifecycle.is_recallable())
            .map(|m| m.id)
            .collect();
        if live.is_empty() {
            return Ok(true);
        }
        let filter = format!(
            "store_generation = {} AND model_fingerprint = {}",
            generation.as_u64(),
            self.fingerprint.as_u64()
        );
        let rows = self.table.rows_where(&filter).await?;
        Ok(live
            .iter()
            .all(|id| rows.iter().any(|r| r.memory_id == *id)))
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
        self.run_limited(None).await
    }

    /// Bounded drive: resolve at most `max_jobs`, leaving the remainder
    /// pending for the next tick. Fairness (§7.3): a bulk backfill must not
    /// turn one tick into an unbounded CPU pass starving interactive recall.
    pub async fn run_capped(&mut self, max_jobs: usize) -> DomainResult<usize> {
        self.run_limited(Some(max_jobs)).await
    }

    async fn run_limited(&mut self, limit: Option<usize>) -> DomainResult<usize> {
        let mut total_resolved = 0usize;
        loop {
            if limit.is_some_and(|max| total_resolved >= max) {
                return Ok(total_resolved);
            }
            let jobs = self.repo.projection_jobs()?;
            if jobs.is_empty() {
                return Ok(total_resolved);
            }

            // Processing order is irrelevant: stale jobs are refused by the
            // revision guard and left pending, so a retryable wakeup may arrive
            // in any sequence (RV-07).
            let mut resolved_this_pass = 0usize;
            for (attempted_this_pass, job) in jobs.iter().enumerate() {
                if limit.is_some_and(|max| total_resolved + resolved_this_pass >= max) {
                    break;
                }
                // Per-pass work budget (re-review P2-1): the cap bounds
                // attempted jobs, not just resolutions — a large set of
                // failing embeddings must not turn one pass into unbounded
                // CPU while resolving nothing.
                if limit.is_some_and(|max| attempted_this_pass >= max) {
                    break;
                }
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
