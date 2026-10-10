//! Suggestion storage, responses and filing (moved verbatim from `repository.rs`).

use fjall::{OptimisticWriteTx, Readable};

use super::{
    AdmittedScope, CanonicalRepository, MAX_RETRIES, RecordedSuggestionOp, SuggestionOpLog, decode,
    op_seq_key_for_scope, op_seq_key_system,
};
use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult};
use ltmrs_domain::id::SessionHandle;
use ltmrs_domain::session::Session;

impl CanonicalRepository {
    /// All suggestions from a single snapshot.
    pub fn get_suggestions(&self) -> DomainResult<Vec<ltmrs_domain::session::Suggestion>> {
        let snapshot = self.db.read_tx();
        let mut out = Vec::new();
        for kv in snapshot.iter(&self.suggestions) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            out.push(decode::<ltmrs_domain::session::Suggestion>(v.as_ref())?);
        }
        Ok(out)
    }

    /// A single suggestion by ID.
    pub fn get_suggestion(
        &self,
        id: u64,
    ) -> DomainResult<Option<ltmrs_domain::session::Suggestion>> {
        Ok(self.get_suggestions()?.into_iter().find(|s| s.id == id))
    }

    /// Store a suggestion (keyed by ID). Blind write (no receipt): tests,
    /// bench seeding and offline repair only — production tool writes go
    /// through `respond_suggestion_idempotent`.
    pub fn put_suggestion(
        &self,
        suggestion: &ltmrs_domain::session::Suggestion,
    ) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let key = suggestion.id.to_string();
        let raw = serde_json::to_vec(suggestion)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let seq_key = op_seq_key_system("suggestion");
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.suggestions, &key, raw.as_slice());
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
        Err(Self::exhausted_contention("suggestion write conflicted"))
    }

    /// Respond to a suggestion idempotently (P1-2 replay safety): the status
    /// transition and all attempt confidence adjustments commit atomically
    /// with the operation receipt in ONE transaction. A retried respond with
    /// the same ID + digest returns the RECORDED outcome (no second
    /// adjustment); the same ID with a different digest rejects as key
    /// reuse. A missing suggestion errors unlogged, so a retry re-evaluates.
    pub fn respond_suggestion_idempotent(
        &self,
        admitted: &AdmittedScope,
        suggestion_id: u64,
        status: ltmrs_domain::session::SuggestionStatus,
        now_millis: u64,
    ) -> DomainResult<RecordedSuggestionOp> {
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
            if let Some(raw) = tx
                .get(&self.suggestion_ops, &op_key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                let log: SuggestionOpLog = serde_json::from_slice(raw.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                Self::check_scope_owner(log.scope.as_ref(), scope)?;
                if log.digest != scope.request_digest {
                    return Err(DomainError::new(
                        DomainErrorCode::KeyReuseDifferentInput,
                        "operation key reused with different input",
                    ));
                }
                // Flush again before ack (re-review P1-1): a visible receipt
                // is not proof its flush succeeded.
                self.persist_barrier()?;
                return Ok(RecordedSuggestionOp {
                    suggestion_id: log.suggestion_id,
                    status: log.status,
                    adjusted: log.adjusted,
                });
            }
            let skey = suggestion_id.to_string();
            let raw = tx
                .get(&self.suggestions, &skey)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(raw) = raw else {
                return Err(DomainError::new(
                    DomainErrorCode::NotFound,
                    "Could not update this suggestion in the store.",
                ));
            };
            let mut suggestion: ltmrs_domain::session::Suggestion =
                serde_json::from_slice(raw.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            suggestion.status = status;
            suggestion.resolved_at = Some(ltmrs_domain::memory::Instant::new(now_millis));
            let raw = serde_json::to_vec(&suggestion)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.suggestions, &skey, raw.as_slice());
            // Attempt confidence adjustments in the same tx (previously one
            // tx per attempt, best-effort). Missing sessions/attempts adjust
            // nothing and still succeed, matching the old leniency.
            let mut adjusted = 0u32;
            if let Some(handle) = suggestion
                .session_id
                .as_deref()
                .and_then(|s| uuid::Uuid::parse_str(s).ok())
                .map(ltmrs_domain::id::SessionHandle::new)
            {
                let hkey = handle.as_uuid().to_string();
                if let Some(sraw) = tx
                    .get(&self.sessions, &hkey)
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
                {
                    let mut session: Session =
                        serde_json::from_slice(sraw.as_ref()).map_err(|e| {
                            DomainError::new(DomainErrorCode::Validation, e.to_string())
                        })?;
                    for a in &mut session.attempts {
                        let applies = match status {
                            ltmrs_domain::session::SuggestionStatus::Dismissed => matches!(
                                a.outcome,
                                ltmrs_domain::session::AttemptOutcome::Rejected
                                    | ltmrs_domain::session::AttemptOutcome::Partial
                            ),
                            ltmrs_domain::session::SuggestionStatus::Accepted => {
                                a.outcome == ltmrs_domain::session::AttemptOutcome::Promising
                            }
                            ltmrs_domain::session::SuggestionStatus::Pending => false,
                        };
                        if applies {
                            let delta =
                                if status == ltmrs_domain::session::SuggestionStatus::Dismissed {
                                    -0.02
                                } else {
                                    0.02
                                };
                            a.confidence = (a.confidence + delta).clamp(0.0, 1.0);
                            a.access_count += 1;
                            a.last_accessed_at =
                                Some(ltmrs_domain::memory::Instant::new(now_millis));
                            adjusted += 1;
                        }
                    }
                    let sraw = serde_json::to_vec(&session).map_err(|e| {
                        DomainError::new(DomainErrorCode::Validation, e.to_string())
                    })?;
                    tx.insert(&self.sessions, &hkey, sraw.as_slice());
                }
            }
            let log = SuggestionOpLog {
                digest: scope.request_digest.clone(),
                suggestion_id,
                status,
                adjusted,
                scope: Some(scope.clone()),
            };
            let raw = serde_json::to_vec(&log)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.suggestion_ops, &op_key, raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(RecordedSuggestionOp {
                        suggestion_id,
                        status,
                        adjusted,
                    });
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("suggestion respond conflicted"))
    }

    /// File improvement suggestions inside the caller's transaction:
    /// content-deduplicated per session+text, IDs allocated max+1 in-tx.
    /// Used by `session_end_tx` so the terminal transition, guide outcomes,
    /// suggestions and receipt commit as one boundary — duplicate
    /// deliveries of the same end operation can never file twice.
    pub(crate) fn file_suggestions_tx(
        &self,
        tx: &mut OptimisticWriteTx,
        session: SessionHandle,
        lines: &[String],
        now_millis: u64,
    ) -> DomainResult<()> {
        let session_key = session.as_uuid().to_string();
        let mut max: u64 = 0;
        let mut present = std::collections::HashSet::new();
        for kv in tx.iter(&self.suggestions) {
            let (k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if let Ok(id) = String::from_utf8_lossy(k.as_ref()).parse::<u64>() {
                max = max.max(id);
            }
            if let Ok(s) = serde_json::from_slice::<ltmrs_domain::session::Suggestion>(v.as_ref()) {
                present.insert((s.session_id, s.suggestion));
            }
        }
        for line in lines {
            let text = line.trim().to_string();
            if text.is_empty() || present.contains(&(Some(session_key.clone()), text.clone())) {
                continue;
            }
            max += 1;
            let suggestion = ltmrs_domain::session::Suggestion {
                id: max,
                session_id: Some(session_key.clone()),
                suggestion: text.clone(),
                status: ltmrs_domain::session::SuggestionStatus::Pending,
                created_at: ltmrs_domain::memory::Instant::new(now_millis),
                resolved_at: None,
            };
            let raw = serde_json::to_vec(&suggestion)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.suggestions, suggestion.id.to_string(), raw.as_slice());
            present.insert((Some(session_key.clone()), text));
        }
        Ok(())
    }

    pub fn file_suggestion(
        &self,
        session_id: Option<String>,
        text: String,
        now_millis: u64,
    ) -> DomainResult<ltmrs_domain::session::Suggestion> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key_system("suggestion");
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let mut max: u64 = 0;
            for kv in tx.iter(&self.suggestions) {
                let (k, _) = kv
                    .into_inner()
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                if let Ok(id) = String::from_utf8_lossy(k.as_ref()).parse::<u64>() {
                    max = max.max(id);
                }
            }
            let suggestion = ltmrs_domain::session::Suggestion {
                id: max + 1,
                session_id: session_id.clone(),
                suggestion: text.clone(),
                status: ltmrs_domain::session::SuggestionStatus::Pending,
                created_at: ltmrs_domain::memory::Instant::new(now_millis),
                resolved_at: None,
            };
            let raw = serde_json::to_vec(&suggestion)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.suggestions, suggestion.id.to_string(), raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(suggestion);
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("suggestion filing conflicted"))
    }
}
