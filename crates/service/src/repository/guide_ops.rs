//! Idempotent guide-operation log: mutation dispatch, receipts, checks (moved verbatim from `repository.rs`).

use fjall::{OptimisticWriteTx, Readable};

use super::{
    AdmittedScope, CanonicalRepository, GuideMutation, GuideOpKind, GuideOpLog, MAX_RETRIES,
    PracticeLog, RecordedGuideOp, op_seq_key_for_scope,
};
use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult, OperationScope};

impl CanonicalRepository {
    /// Apply one guide tool mutation idempotently (P1-2 replay safety): the
    /// operation ID is logged atomically with the mutation in ONE
    /// transaction. A retried operation with the same ID + digest returns the
    /// RECORDED outcome (never re-applied, never recomputed from live
    /// state); the same ID with a different digest rejects as key reuse.
    /// Business errors (NotFound, RevisionConflict, Validation) propagate
    /// unlogged, so a retry re-evaluates against current state.
    pub fn guide_mutation_idempotent(
        &self,
        admitted: &AdmittedScope,
        mutation: GuideMutation,
    ) -> DomainResult<RecordedGuideOp> {
        // Admission-once: TTL was checked at tool entry, not revalidated here.
        let scope = admitted.scope();
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key_for_scope(scope.frontend_id, scope.channel_id);
        let op_key = scope.op_key();
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if let Some(recorded) = self.check_guide_op_tx(&tx, scope)? {
                // Durability is not inherited from a visible receipt
                // (re-review P1-1): flush again before acknowledging.
                self.persist_barrier()?;
                return Ok(recorded);
            }
            let (kind, guide, merged_sources) = match mutation.clone() {
                GuideMutation::Create { guide } => {
                    let written = self.put_guide_apply_tx(&mut tx, None, &guide)?;
                    (GuideOpKind::Create, Some(written), Vec::new())
                }
                GuideMutation::Update {
                    expected,
                    guide,
                    old_name,
                } => {
                    let written = match old_name {
                        Some(old) => {
                            let rev = expected.ok_or_else(|| {
                                DomainError::new(
                                    DomainErrorCode::Validation,
                                    "rename requires the planned source revision",
                                )
                            })?;
                            self.rename_guide_apply_tx(&mut tx, &old, rev, &guide)?
                        }
                        None => self.put_guide_apply_tx(&mut tx, expected, &guide)?,
                    };
                    (GuideOpKind::Update, Some(written), Vec::new())
                }
                GuideMutation::Forget { name } => {
                    match self.forget_guide_apply_tx(&mut tx, &name)? {
                        Some(deleted) => (GuideOpKind::Forget, Some(deleted), Vec::new()),
                        None => {
                            return Err(DomainError::new(
                                DomainErrorCode::NotFound,
                                format!("guide not found: {}", name.to_lowercase()),
                            ));
                        }
                    }
                }
                GuideMutation::Merge {
                    sources,
                    expected,
                    result,
                } => {
                    let source_keys: Vec<String> =
                        sources.iter().map(|n| n.to_lowercase()).collect();
                    let result_key = result.name.to_lowercase();
                    self.merge_guides_apply_tx(
                        &mut tx,
                        &source_keys,
                        &expected,
                        &result_key,
                        &result,
                    )?;
                    (GuideOpKind::Merge, Some(result), sources)
                }
                GuideMutation::CreateUpdate { expected, guide } => {
                    let written = self.put_guide_apply_tx(&mut tx, expected, &guide)?;
                    (GuideOpKind::CreateUpdate, Some(written), Vec::new())
                }
            };
            let log = GuideOpLog {
                digest: scope.request_digest.clone(),
                kind,
                recorded: guide.clone(),
                merged_sources: merged_sources.clone(),
                scope: Some(scope.clone()),
            };
            let raw = serde_json::to_vec(&log)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.guide_ops, &op_key, raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(RecordedGuideOp {
                        kind,
                        guide,
                        merged_sources,
                    });
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("guide mutation conflicted"))
    }

    /// Read back one recorded guide tool operation (P1-2): lets the exec
    /// layer replay before planning reads, so a retry never mistakes a
    /// concurrently changed store (renamed/consumed guides) for a failure.
    /// A digest mismatch rejects as key reuse. The durability barrier runs
    /// before acknowledging a replay, matching the in-transaction path.
    pub fn read_recorded_guide_op(
        &self,
        admitted: &AdmittedScope,
    ) -> DomainResult<Option<RecordedGuideOp>> {
        // Admission-once: replay reads ride the entry admission, so a dead
        // epoch never hides a durable receipt.
        let scope = admitted.scope();
        let _restore_guard = self.restore_lock.read().unwrap();
        let snapshot = self.db.read_tx();
        let raw = snapshot
            .get(&self.guide_ops, scope.op_key())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        if let Ok(log) = serde_json::from_slice::<GuideOpLog>(raw.as_ref()) {
            Self::check_scope_owner(log.scope.as_ref(), scope)?;
            if log.digest != scope.request_digest {
                return Err(DomainError::new(
                    DomainErrorCode::KeyReuseDifferentInput,
                    "operation key reused with different input",
                ));
            }
            self.persist_barrier()?;
            return Ok(Some(RecordedGuideOp {
                kind: log.kind,
                guide: log.recorded,
                merged_sources: log.merged_sources,
            }));
        }
        if serde_json::from_slice::<PracticeLog>(raw.as_ref()).is_ok() {
            return Err(DomainError::new(
                DomainErrorCode::KeyReuseDifferentInput,
                "operation key reused with different input",
            ));
        }
        Err(DomainError::new(
            DomainErrorCode::Validation,
            "corrupt guide operation log entry",
        ))
    }

    /// Read a recorded distill outcome (`PracticeLog` receipt): the recorded
    /// guide on digest match, key reuse on digest mismatch, None on miss.
    /// A `GuideOpLog`-shaped entry proves another operation kind owns this
    /// key and rejects the same way; corrupt entries fail loudly. Lets the
    /// distill tool check its receipt before resolving the memory, so a
    /// retry after the source vanished still replays instead of failing
    /// resolution.
    pub fn read_recorded_distill_op(
        &self,
        admitted: &AdmittedScope,
    ) -> DomainResult<Option<ltmrs_domain::guide::Guide>> {
        // Admission-once: replay reads ride the entry admission, so a dead
        // epoch never hides a durable receipt.
        let scope = admitted.scope();
        let _restore_guard = self.restore_lock.read().unwrap();
        let snapshot = self.db.read_tx();
        let raw = snapshot
            .get(&self.guide_ops, scope.op_key())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        if let Ok(log) = serde_json::from_slice::<PracticeLog>(raw.as_ref()) {
            Self::check_scope_owner(log.scope.as_ref(), scope)?;
            if log.digest != scope.request_digest {
                return Err(DomainError::new(
                    DomainErrorCode::KeyReuseDifferentInput,
                    "operation key reused with different input",
                ));
            }
            self.persist_barrier()?;
            return Ok(Some(log.recorded));
        }
        if serde_json::from_slice::<GuideOpLog>(raw.as_ref()).is_ok() {
            return Err(DomainError::new(
                DomainErrorCode::KeyReuseDifferentInput,
                "operation key reused with different input",
            ));
        }
        Err(DomainError::new(
            DomainErrorCode::Validation,
            "corrupt guide operation log entry",
        ))
    }

    /// Look up one guide tool operation in the `guide_ops` log inside the
    /// caller's transaction: a digest match returns the recorded outcome, a
    /// digest mismatch rejects as key reuse. Entries written by practice
    /// (different receipt shape, same log) prove the ID is owned by another
    /// operation and reject the same way; corrupt entries fail loudly rather
    /// than risk a double-apply.
    fn check_guide_op_tx(
        &self,
        tx: &OptimisticWriteTx,
        scope: &OperationScope,
    ) -> DomainResult<Option<RecordedGuideOp>> {
        let Some(raw) = tx
            .get(&self.guide_ops, scope.op_key())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
        else {
            return Ok(None);
        };
        if let Ok(log) = serde_json::from_slice::<GuideOpLog>(raw.as_ref()) {
            Self::check_scope_owner(log.scope.as_ref(), scope)?;
            if log.digest != scope.request_digest {
                return Err(DomainError::new(
                    DomainErrorCode::KeyReuseDifferentInput,
                    "operation key reused with different input",
                ));
            }
            return Ok(Some(RecordedGuideOp {
                kind: log.kind,
                guide: log.recorded,
                merged_sources: log.merged_sources,
            }));
        }
        if serde_json::from_slice::<PracticeLog>(raw.as_ref()).is_ok() {
            return Err(DomainError::new(
                DomainErrorCode::KeyReuseDifferentInput,
                "operation key reused with different input",
            ));
        }
        Err(DomainError::new(
            DomainErrorCode::Validation,
            "corrupt guide operation log entry",
        ))
    }
}
