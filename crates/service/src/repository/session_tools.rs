//! Session reads, tool-replay log and legacy import (moved verbatim from `repository.rs`).

use fjall::Readable;

use super::{
    AdmittedScope, CanonicalRepository, FrozenToolRecord, MAX_RETRIES, ToolReplayStatus, decode,
    op_seq_key_for_scope, op_seq_key_system,
};
use ltmrs_domain::command::{
    CommandReceipt, DomainError, DomainErrorCode, DomainResult, OperationScope,
};
use ltmrs_domain::id::SessionHandle;
use ltmrs_domain::session::{Session, SessionReceipt};

impl CanonicalRepository {
    /// Read one session by handle.
    pub fn get_session(&self, handle: SessionHandle) -> DomainResult<Option<Session>> {
        let snapshot = self.db.read_tx();
        let raw = snapshot
            .get(&self.sessions, handle.as_uuid().to_string())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        raw.map(|raw| {
            serde_json::from_slice(raw.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
        })
        .transpose()
    }

    /// All traced sessions (analytics, stats, continuity recall, backup).
    pub fn all_sessions(&self) -> DomainResult<Vec<Session>> {
        let snapshot = self.db.read_tx();
        let mut out = Vec::new();
        for kv in snapshot.iter(&self.sessions) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            out.push(
                serde_json::from_slice(v.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?,
            );
        }
        Ok(out)
    }

    /// Read one session operation receipt by scope (P2-1 frozen replay):
    /// a receipt recorded under another scope never resolves here.
    pub fn session_receipt(
        &self,
        admitted: &AdmittedScope,
    ) -> DomainResult<Option<SessionReceipt>> {
        let scope = admitted.scope();
        let snapshot = self.db.read_tx();
        let raw = snapshot
            .get(&self.session_ops, scope.op_key())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        raw.map(|raw| {
            let rec: SessionReceipt = serde_json::from_slice(raw.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            Self::check_scope_owner(rec.scope.as_ref(), scope)?;
            Ok(rec)
        })
        .transpose()
    }

    /// Replay a frozen compatibility-tool result (P1 replay depth): the
    /// primary sub-command receipt must exist with matching channel and
    /// digest (else miss / key-reuse, exactly like the gateway), and the
    /// frozen record must carry the same digest. The durability barrier
    /// runs before acknowledging — receipt visibility is never proof of
    /// durable completion. Returns None when the operation never ran
    /// (fresh execution proceeds) or froze nothing yet (crash window:
    /// the caller rebuilds from the receipt and freezes).
    pub fn replay_tool_result(
        &self,
        scope: &OperationScope,
    ) -> DomainResult<Option<ltmrs_domain::session::FrozenToolResponse>> {
        match self.check_tool_replay(scope)? {
            ToolReplayStatus::Frozen(response) => Ok(Some(response)),
            ToolReplayStatus::Miss | ToolReplayStatus::Unfrozen(_) => Ok(None),
        }
    }

    /// One read of a tool operation's replay state: never ran, ran and
    /// froze, or ran without freezing yet (crash window). Digest
    /// mismatches reject as key reuse; channel mismatches read as a
    /// miss (the gateway's own replay check refuses them authoritatively
    /// on the fresh path). Frozen hits run the durability barrier
    /// before acknowledging.
    pub fn check_tool_replay(&self, scope: &OperationScope) -> DomainResult<ToolReplayStatus> {
        let snapshot = self.db.read_tx();
        let raw = snapshot
            .get(&self.receipts, scope.op_key())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let Some(raw) = raw else {
            return Ok(ToolReplayStatus::Miss);
        };
        let receipt: CommandReceipt = decode(raw.as_ref())?;
        if receipt.channel_id != scope.channel_id {
            return Ok(ToolReplayStatus::Miss);
        }
        if receipt.request_digest != scope.request_digest {
            return Err(DomainError::new(
                DomainErrorCode::KeyReuseDifferentInput,
                "operation key reused with different input",
            ));
        }
        let frozen = snapshot
            .get(&self.tool_results, scope.op_key())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let Some(frozen) = frozen else {
            return Ok(ToolReplayStatus::Unfrozen(receipt));
        };
        let record: FrozenToolRecord = serde_json::from_slice(frozen.as_ref())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Self::check_scope_owner(record.scope.as_ref(), scope)?;
        if record.digest != scope.request_digest {
            return Err(DomainError::new(
                DomainErrorCode::KeyReuseDifferentInput,
                "operation key reused with different input",
            ));
        }
        self.persist_barrier()?;
        Ok(ToolReplayStatus::Frozen(record.response))
    }

    /// Freeze a compatibility-tool result after first execution (P2
    /// replay fidelity): stored under the primary sub-command's scoped
    /// key with its digest, so retries return the exact bytes —
    /// no re-planning, no recomputation. Takes the admitted token (no
    /// revalidation past the primary commit); the scope carries the key.
    /// The barrier runs before acknowledging.
    pub fn freeze_tool_result(
        &self,
        admitted: &AdmittedScope,
        scope: &OperationScope,
        response: &ltmrs_domain::session::FrozenToolResponse,
    ) -> DomainResult<()> {
        let admitted_scope = admitted.scope();
        if admitted_scope.store_generation != scope.store_generation
            || admitted_scope.frontend_id != scope.frontend_id
            || admitted_scope.channel_id != scope.channel_id
            || admitted_scope.retry_epoch != scope.retry_epoch
        {
            return Err(DomainError::new(
                DomainErrorCode::Validation,
                "frozen scope does not belong to the admitted operation",
            ));
        }
        let _restore_guard = self.restore_lock.read().unwrap();
        let record = FrozenToolRecord {
            digest: scope.request_digest.clone(),
            response: response.clone(),
            scope: Some(scope.clone()),
        };
        let raw = serde_json::to_vec(&record)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            // First freeze wins transactionally: a duplicate in-flight
            // execution must never overwrite the already-frozen verbatim
            // response. A same-key re-freeze (replay path) is a no-op.
            if tx
                .get(&self.tool_results, scope.op_key())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
                .is_some()
            {
                tx.rollback();
                return Ok(());
            }
            tx.insert(&self.tool_results, scope.op_key(), raw.as_slice());
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(());
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("tool result freeze conflicted"))
    }

    /// Freeze the tool response into a session operation receipt (P2-1):
    /// stored after first execution so a lost-response retry returns the
    /// original verbatim. The digest must still match (else key reuse); a
    /// receipt lost to a concurrent restore errors loudly instead of
    /// resurrecting a stale identity.
    pub fn store_session_response(
        &self,
        admitted: &AdmittedScope,
        response: &ltmrs_domain::session::FrozenToolResponse,
    ) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let scope = admitted.scope();
        let seq_key = op_seq_key_for_scope(scope.frontend_id, scope.channel_id);
        let op_key = scope.op_key();
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let raw = tx
                .get(&self.session_ops, &op_key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(raw) = raw else {
                return Err(DomainError::new(
                    DomainErrorCode::NotFound,
                    "session operation receipt not found",
                ));
            };
            let mut rec: SessionReceipt = serde_json::from_slice(raw.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            Self::check_scope_owner(rec.scope.as_ref(), scope)?;
            if rec.digest != scope.request_digest {
                return Err(DomainError::new(
                    DomainErrorCode::KeyReuseDifferentInput,
                    "operation key reused with different input",
                ));
            }
            // First freeze wins: duplicate in-flight executions that both
            // passed the unfrozen check must converge on one response, or
            // two callers of the same operation id receive divergent
            // "verbatim" replays.
            if rec.response.is_some() {
                self.persist_barrier()?;
                return Ok(());
            }
            rec.response = Some(response.clone());
            let raw = serde_json::to_vec(&rec)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.session_ops, &op_key, raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(());
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention(
            "session response freeze conflicted",
        ))
    }

    /// One-time import of pre-migration registry state (traced sessions +
    /// operation receipts from sessions.json): inserts only absent records,
    /// so repeated starts never duplicate. Virtual sessions and bindings
    /// stay in the registry file.
    pub fn import_legacy_sessions(
        &self,
        sessions: Vec<Session>,
        receipts: Vec<(String, SessionReceipt)>,
    ) -> DomainResult<usize> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key_system("import");
        let mut imported = 0usize;
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let mut dirty = false;
            for s in &sessions {
                let key = s.handle.as_uuid().to_string();
                let exists = tx
                    .get(&self.sessions, &key)
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
                    .is_some();
                if !exists {
                    let raw = serde_json::to_vec(s).map_err(|e| {
                        DomainError::new(DomainErrorCode::Validation, e.to_string())
                    })?;
                    tx.insert(&self.sessions, &key, raw.as_slice());
                    imported += 1;
                    dirty = true;
                }
            }
            for (op_id, rec) in &receipts {
                let exists = tx
                    .get(&self.session_ops, op_id)
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
                    .is_some();
                if !exists {
                    let raw = serde_json::to_vec(rec).map_err(|e| {
                        DomainError::new(DomainErrorCode::Validation, e.to_string())
                    })?;
                    tx.insert(&self.session_ops, op_id, raw.as_slice());
                    dirty = true;
                }
            }
            if !dirty {
                return Ok(imported);
            }
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(imported);
                }
                Ok(Err(_)) => {
                    imported = 0;
                    continue;
                }
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention(
            "legacy session import conflicted",
        ))
    }
}
