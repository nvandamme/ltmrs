//! Projection-job queue (durable desired state) (moved verbatim from `repository.rs`).

use fjall::Readable;

use super::{CanonicalRepository, MAX_RETRIES, decode};
use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult};
use ltmrs_domain::id::EntityId;

impl CanonicalRepository {
    /// Whether a memory still has a pending projection (embedding not yet
    /// computed). Used by the projection worker to know what remains to index.
    pub fn has_pending_projection(&self, id: EntityId) -> DomainResult<bool> {
        Ok(self.projection_job(id)?.is_some())
    }

    /// Read the durable desired-state job for a memory, if any is pending.
    pub fn projection_job(
        &self,
        id: EntityId,
    ) -> DomainResult<Option<ltmrs_domain::projection::ProjectionJob>> {
        let snapshot = self.db.read_tx();
        let key = id.as_uuid().to_string();
        let raw = snapshot
            .get(&self.projections, &key)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        raw.map(|v| decode::<ltmrs_domain::projection::ProjectionJob>(v.as_ref()))
            .transpose()
    }

    /// List every pending projection job (the worker's durable work queue).
    pub fn projection_jobs(&self) -> DomainResult<Vec<ltmrs_domain::projection::ProjectionJob>> {
        let snapshot = self.db.read_tx();
        let mut out = Vec::new();
        for kv in snapshot.iter(&self.projections) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            out.push(decode::<ltmrs_domain::projection::ProjectionJob>(
                v.as_ref(),
            )?);
        }
        Ok(out)
    }

    /// Compare-and-clear a projection job. Returns true only if the stored job
    /// still carries exactly this seq — a stale worker (whose desired revision
    /// was superseded, or whose memory was forgotten) gets false and leaves no
    /// trace. Retries on storage conflict from a fresh snapshot.
    /// Acknowledge a projection job as published. Deliberately buffered
    /// (no durability barrier): losing an ack only republishes idempotent
    /// work, while every ack costs a barrier. Progress notes share this
    /// treatment; knowledge and protocol state always persist (see
    /// `persist_barrier`).
    pub fn acknowledge_projection(&self, id: EntityId, seq: u64) -> DomainResult<bool> {
        let _restore_guard = self.restore_lock.read().unwrap();
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let key = id.as_uuid().to_string();
            match tx
                .get(&self.projections, &key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                Some(raw) => {
                    let job: ltmrs_domain::projection::ProjectionJob = decode(raw.as_ref())?;
                    if job.memory_id != id || job.seq != seq {
                        tx.rollback();
                        return Ok(false);
                    }
                    tx.remove(&self.projections, &key);
                }
                None => {
                    tx.rollback();
                    return Ok(false);
                }
            }
            match tx.commit() {
                Ok(Ok(())) => return Ok(true),
                Ok(Err(_conflict)) => continue,
                Err(e) => return Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
            }
        }
        Err(Self::exhausted_contention(
            "max transaction retries exceeded",
        ))
    }

    /// The count of pending projection jobs (projection lag). A stalled worker or
    /// embedder shows up here as a non-zero, growing number — separate from both
    /// canonical durability and vector availability.
    pub fn projection_lag(&self) -> DomainResult<usize> {
        Ok(self.projection_jobs()?.len())
    }

    /// The age of the oldest pending job at `now_millis`, or None when nothing is
    /// pending. This exposes how long a write has waited to be projected (RQ-08).
    pub fn oldest_pending_age_millis(&self, now_millis: u64) -> DomainResult<Option<u64>> {
        let jobs = self.projection_jobs()?;
        Ok(jobs
            .iter()
            .map(|j| now_millis.saturating_sub(j.enqueued_at_millis))
            .max())
    }

    /// Enqueue a projection job only when none is pending (upgrade backfill):
    /// check-and-set in one transaction, so a concurrent mutation's own job
    /// can never be clobbered by a stale backfill write. Returns whether a
    /// job was enqueued.
    pub fn enqueue_projection_job_if_absent(
        &self,
        memory_id: EntityId,
        desired_document_revision: ltmrs_domain::id::DocumentRevision,
        seq: u64,
    ) -> DomainResult<bool> {
        let _restore_guard = self.restore_lock.read().unwrap();
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let key = memory_id.as_uuid().to_string();
            let present = tx
                .get(&self.projections, &key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
                .is_some();
            if present {
                tx.rollback();
                return Ok(false);
            }
            let job = ltmrs_domain::projection::ProjectionJob {
                memory_id,
                desired_document_revision,
                seq,
                enqueued_at_millis: self.clock.now_millis(),
                is_tombstone: false,
            };
            tx.insert(
                &self.projections,
                memory_id.as_uuid().to_string(),
                serde_json::to_vec(&job)
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
                    .as_slice(),
            );
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(true);
                }
                Ok(Err(_conflict)) => continue,
                Err(e) => return Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
            }
        }
        Err(Self::exhausted_contention(
            "max transaction retries exceeded",
        ))
    }

    /// Enqueue (or advance) a projection job using the repository's clock for
    /// the enqueue timestamp. Used by the projector to record semantic retries
    /// when an embedding pass fails; `seq` must be chosen by the caller so that
    /// stale acknowledgements cannot clear newer work.
    pub fn enqueue_projection_job(
        &self,
        memory_id: EntityId,
        desired_document_revision: ltmrs_domain::id::DocumentRevision,
        seq: u64,
        is_tombstone: bool,
    ) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let job = ltmrs_domain::projection::ProjectionJob {
            memory_id,
            desired_document_revision,
            seq,
            enqueued_at_millis: self.clock.now_millis(),
            is_tombstone,
        };

        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(
                &self.projections,
                memory_id.as_uuid().to_string(),
                serde_json::to_vec(&job)
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
                    .as_slice(),
            );
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(());
                }
                Ok(Err(_conflict)) => continue,
                Err(e) => return Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
            }
        }
        Err(Self::exhausted_contention(
            "max transaction retries exceeded",
        ))
    }

    pub fn feedback_events(&self) -> DomainResult<Vec<ltmrs_domain::session::FeedbackEvent>> {
        let snapshot = self.db.read_tx();
        let mut out = Vec::new();
        for kv in snapshot.iter(&self.feedback_events) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            out.push(decode::<ltmrs_domain::session::FeedbackEvent>(v.as_ref())?);
        }
        Ok(out)
    }
}
