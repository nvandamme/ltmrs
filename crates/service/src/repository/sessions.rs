//! Traced-session lifecycle: start, attempts, end, links, boosts (moved verbatim from `repository.rs`).

use fjall::{OptimisticWriteTx, Readable};

use super::{
    AdmittedScope, CanonicalRepository, MAX_RETRIES, SessionLinkField, op_seq_key_for_scope,
    op_seq_key_system,
};
use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult, OperationScope};
use ltmrs_domain::id::{EntityId, SessionHandle};
use ltmrs_domain::session::{Session, SessionOp, SessionReceipt};

impl CanonicalRepository {
    /// Start a traced session as ONE canonical operation (re-review P1-3):
    /// abandon-previous, attempt decay, session insert and operation
    /// receipt commit in a single transaction. A replay returns the
    /// recorded handle instead of abandoning and recreating; a digest
    /// mismatch rejects. The channel binding itself stays in the daemon
    /// registry (routing, not durability).
    #[allow(clippy::too_many_arguments)]
    pub fn session_start_tx(
        &self,
        scope: &OperationScope,
        handle: SessionHandle,
        project: Option<String>,
        task_type: Option<String>,
        technologies: Vec<String>,
        initial_approach: Option<String>,
        abandon: Option<SessionHandle>,
        now_millis: u64,
    ) -> DomainResult<SessionOp<SessionHandle>> {
        let _restore_guard = self.restore_lock.read().unwrap();
        self.validate_scope(scope)?;
        let seq_key = op_seq_key_for_scope(scope.frontend_id, scope.channel_id);
        let op_key = scope.op_key();
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if let Some(raw) = tx
                .get(&self.session_ops, &op_key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                let rec: SessionReceipt = serde_json::from_slice(raw.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                Self::check_scope_owner(rec.scope.as_ref(), scope)?;
                if rec.digest != scope.request_digest {
                    return Ok(SessionOp::Conflict);
                }
                // Durability is not inherited from a visible receipt
                // (re-review P1-1): flush again before acknowledging.
                self.persist_barrier()?;
                return Ok(SessionOp::Replayed(rec.session));
            }
            // Abandon the channel's previous session, if it can still end.
            if let Some(prev) = abandon {
                let pkey = prev.as_uuid().to_string();
                if let Some(raw) = tx
                    .get(&self.sessions, &pkey)
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
                {
                    let mut prev_session: Session =
                        serde_json::from_slice(raw.as_ref()).map_err(|e| {
                            DomainError::new(DomainErrorCode::Validation, e.to_string())
                        })?;
                    if prev_session.can_end() {
                        prev_session.status = ltmrs_domain::session::SessionStatus::Abandoned;
                        prev_session.outcome = Some(ltmrs_domain::session::TaskOutcome::Abandoned);
                        prev_session.ended_at =
                            Some(ltmrs_domain::memory::Instant::new(now_millis));
                        let raw = serde_json::to_vec(&prev_session).map_err(|e| {
                            DomainError::new(DomainErrorCode::Validation, e.to_string())
                        })?;
                        tx.insert(&self.sessions, &pkey, raw.as_slice());
                    }
                }
            }
            // Attempt-decay runs post-commit (see below): scanning the
            // sessions keyspace inside this transaction would turn
            // independent concurrent starts into optimistic conflicts.
            let session = Session {
                handle,
                channel_id: scope.channel_id,
                project: project.clone(),
                task_type: task_type.clone(),
                technologies: technologies.clone(),
                status: ltmrs_domain::session::SessionStatus::Active,
                attempts: Vec::new(),
                outcome: None,
                final_approach: None,
                lessons: Vec::new(),
                initial_approach: initial_approach.clone(),
                guides_used: Vec::new(),
                memories_read: Vec::new(),
                memories_created: Vec::new(),
                refinement_attempts: 0,
                self_critique_count: 0,
                started_at: ltmrs_domain::memory::Instant::new(now_millis),
                ended_at: None,
                is_virtual: false,
            };
            let hkey = handle.as_uuid().to_string();
            let raw = serde_json::to_vec(&session)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.sessions, &hkey, raw.as_slice());
            let receipt = SessionReceipt {
                digest: scope.request_digest.clone(),
                session: handle,
                seq: None,
                response: None,
                continuity_boosted: false,
                scope: Some(scope.clone()),
            };
            let log_raw = serde_json::to_vec(&receipt)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.session_ops, &op_key, log_raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    self.fire_commit_hook();
                    // Stale dead-end decay (same 0.002 policy the registry
                    // applied at start) runs here, outside the start
                    // transaction: the scan's keyspace-wide read dependency
                    // would otherwise turn independent concurrent starts
                    // into optimistic conflicts. Best-effort by contract:
                    // the start receipt already committed, so a decay
                    // failure must never turn this executed start into an
                    // error (a host retry would safely replay, but must
                    // never be invited by a post-commit failure).
                    // Loud on hard errors; conflicts skip silently with
                    // the next start retrying the pass.
                    if let Err(e) = self.decay_stale_attempts() {
                        eprintln!("ltmrs: post-commit attempt decay skipped: {}", e.message);
                    }
                    return Ok(SessionOp::Applied(handle));
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("session start conflicted"))
    }

    /// One best-effort attempt-dead-end decay pass (0.002 per attempt,
    /// floored at 0.0) in its own transaction. Returns Ok on a conflicted
    /// pass (the next session start retries it); hard errors propagate.
    fn decay_stale_attempts(&self) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let mut tx = self
            .db
            .write_tx()
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let mut changed: Vec<(String, Session)> = Vec::new();
        for kv in tx.iter(&self.sessions) {
            let (k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let mut s: Session = serde_json::from_slice(v.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let mut touched = false;
            for a in &mut s.attempts {
                let decayed = (a.confidence - 0.002).max(0.0);
                if decayed != a.confidence {
                    a.confidence = decayed;
                    touched = true;
                }
            }
            if touched {
                changed.push((String::from_utf8_lossy(k.as_ref()).into_owned(), s));
            }
        }
        for (key, s) in &changed {
            let raw = serde_json::to_vec(s)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.sessions, key, raw.as_slice());
        }
        match tx.commit() {
            Ok(Ok(())) => {
                self.persist_barrier()?;
                Ok(())
            }
            // Conflicted pass: skip silently, the next start retries it.
            Ok(Err(_)) => Ok(()),
            Err(e) => Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
        }
    }

    /// Look up a recorded session-attempt outcome WITHOUT touching session
    /// state: owner + digest checked, TTL deliberately not consulted —
    /// replaying a committed outcome creates no new effects, so a dead
    /// epoch or a terminal session must not hide it. Used by the adapter
    /// replay pre-check (a retried envelope replays even after its session
    /// ended). Digest mismatch rejects as key reuse, like the tx path.
    pub fn session_attempt_receipt(
        &self,
        scope: &OperationScope,
    ) -> DomainResult<Option<(SessionHandle, u32)>> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let snapshot = self.db.read_tx();
        let raw = snapshot
            .get(&self.session_ops, scope.op_key())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let rec: SessionReceipt = serde_json::from_slice(raw.as_ref())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Self::check_scope_owner(rec.scope.as_ref(), scope)?;
        if rec.digest != scope.request_digest {
            return Err(DomainError::new(
                DomainErrorCode::KeyReuseDifferentInput,
                "operation key reused with different input",
            ));
        }
        Ok(Some((rec.session, rec.seq.unwrap_or(0))))
    }

    /// Record a session attempt as ONE canonical operation (re-review P1-3):
    /// the attempt ID derives deterministically from the operation ID, so
    /// retries dedup; counters increment exactly once per operation; the
    /// receipt commits in the same transaction. Digest mismatch rejects.
    #[allow(clippy::too_many_arguments)]
    pub fn session_attempt_tx(
        &self,
        scope: &OperationScope,
        handle: SessionHandle,
        approach: String,
        outcome: ltmrs_domain::session::AttemptOutcome,
        critique: Option<String>,
        rationale: Option<String>,
        related_memory_id: Option<EntityId>,
        now_millis: u64,
    ) -> DomainResult<SessionOp<(SessionHandle, u32)>> {
        let _restore_guard = self.restore_lock.read().unwrap();
        self.validate_scope(scope)?;
        let attempt_id = EntityId::new(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            format!("ltmrs:attempt:{}", scope.operation_id.as_uuid()).as_bytes(),
        ));
        let seq_key = op_seq_key_for_scope(scope.frontend_id, scope.channel_id);
        let op_key = scope.op_key();
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if let Some(raw) = tx
                .get(&self.session_ops, &op_key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                let rec: SessionReceipt = serde_json::from_slice(raw.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                Self::check_scope_owner(rec.scope.as_ref(), scope)?;
                if rec.digest != scope.request_digest {
                    return Ok(SessionOp::Conflict);
                }
                self.persist_barrier()?;
                let seq = rec.seq.unwrap_or(0);
                return Ok(SessionOp::Replayed((rec.session, seq)));
            }
            let hkey = handle.as_uuid().to_string();
            let raw = tx
                .get(&self.sessions, &hkey)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(raw) = raw else {
                return Err(DomainError::new(
                    DomainErrorCode::NotFound,
                    "session not found",
                ));
            };
            let mut session: Session = serde_json::from_slice(raw.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if !session.can_end() {
                return Err(DomainError::new(
                    DomainErrorCode::Validation,
                    "session is already terminal",
                ));
            }
            let seq = match session.attempts.iter().find(|a| a.id == attempt_id) {
                Some(existing) => existing.seq,
                None => {
                    let next = session.attempts.len() as u32 + 1;
                    session.attempts.push(ltmrs_domain::session::Attempt {
                        id: attempt_id,
                        session_id: handle,
                        seq: next,
                        approach: approach.clone(),
                        outcome,
                        critique: critique.clone(),
                        rationale: rationale.clone(),
                        related_memory_id,
                        confidence: 1.0,
                        access_count: 0,
                        last_accessed_at: None,
                        created_at: ltmrs_domain::memory::Instant::new(now_millis),
                    });
                    session.refinement_attempts += 1;
                    if matches!(
                        outcome,
                        ltmrs_domain::session::AttemptOutcome::Rejected
                            | ltmrs_domain::session::AttemptOutcome::Partial
                    ) && critique.is_some()
                    {
                        session.self_critique_count += 1;
                    }
                    next
                }
            };
            let raw = serde_json::to_vec(&session)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.sessions, &hkey, raw.as_slice());
            let receipt = SessionReceipt {
                digest: scope.request_digest.clone(),
                session: handle,
                seq: Some(seq),
                response: None,
                continuity_boosted: false,
                scope: Some(scope.clone()),
            };
            let log_raw = serde_json::to_vec(&receipt)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.session_ops, &op_key, log_raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    self.fire_commit_hook();
                    return Ok(SessionOp::Applied((handle, seq)));
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("session attempt conflicted"))
    }

    /// Success-rate warning line for a guide after an outcome bump, or
    /// empty when the guide is healthy. Shared by the fresh end path and
    /// the replay path so both render identically.
    fn improvement_line(guide: &ltmrs_domain::guide::Guide) -> String {
        let total = guide.success_count + guide.failure_count;
        if total >= 3 {
            let rate = guide.success_count as f64 / total as f64;
            if rate < 0.4 {
                return format!(
                    "  [!] Guide \"{}\" success rate is {:.2} ({}/{total}). Consider refining with guide_update.",
                    guide.name, rate, guide.success_count
                );
            }
        }
        String::new()
    }

    /// Recompute improvement lines for a session's used guides from current
    /// store state (replay rendering): same rule as the fresh path.
    fn improvement_lines_tx(
        &self,
        tx: &mut OptimisticWriteTx,
        handle: SessionHandle,
    ) -> DomainResult<Vec<String>> {
        let hkey = handle.as_uuid().to_string();
        let raw = tx
            .get(&self.sessions, &hkey)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let Some(raw) = raw else {
            return Ok(Vec::new());
        };
        let session: Session = serde_json::from_slice(raw.as_ref())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let mut lines = Vec::new();
        for guide_name in &session.guides_used {
            let gkey = guide_name.to_lowercase();
            if let Some(graw) = tx
                .get(&self.guides, &gkey)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                let guide: ltmrs_domain::guide::Guide = serde_json::from_slice(graw.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                let line = Self::improvement_line(&guide);
                if !line.is_empty() {
                    lines.push(line);
                }
            }
        }
        Ok(lines)
    }

    /// End a session as ONE canonical operation (re-review P1-3): required
    /// guide outcomes, the terminal transition, improvement suggestions
    /// and the operation receipt commit in a single transaction — no
    /// observable partial completion, no mixed outcomes, no duplicate
    /// filing on duplicate delivery. Replay returns the recorded handle;
    /// digest mismatch rejects. Improvement lines are derived from the
    /// committed counts and returned for the response.
    #[allow(clippy::too_many_arguments)]
    pub fn session_end_tx(
        &self,
        scope: &OperationScope,
        handle: SessionHandle,
        outcome: ltmrs_domain::session::TaskOutcome,
        final_approach: Option<String>,
        lessons: Vec<String>,
        now_millis: u64,
    ) -> DomainResult<SessionOp<(SessionHandle, Vec<String>, bool)>> {
        let _restore_guard = self.restore_lock.read().unwrap();
        self.validate_scope(scope)?;
        let seq_key = op_seq_key_for_scope(scope.frontend_id, scope.channel_id);
        let op_key = scope.op_key();
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if let Some(raw) = tx
                .get(&self.session_ops, &op_key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                let rec: SessionReceipt = serde_json::from_slice(raw.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                Self::check_scope_owner(rec.scope.as_ref(), scope)?;
                if rec.digest != scope.request_digest {
                    return Ok(SessionOp::Conflict);
                }
                // Rebuild the improvement lines from current guide state so
                // the replayed response matches a fresh rendering: same
                // inputs, same text (rates only change via later ops, in
                // which case current truth is the right rendering).
                let lines = self.improvement_lines_tx(&mut tx, rec.session)?;
                tx.rollback();
                // Flush again before ack (re-review P1-1): the receipt may
                // predate an unflushed barrier.
                self.persist_barrier()?;
                return Ok(SessionOp::Replayed((rec.session, lines, true)));
            }
            let hkey = handle.as_uuid().to_string();
            let raw = tx
                .get(&self.sessions, &hkey)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(raw) = raw else {
                return Err(DomainError::new(
                    DomainErrorCode::NotFound,
                    "session not found",
                ));
            };
            let mut session: Session = serde_json::from_slice(raw.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if !session.can_end() {
                // Already terminal via another operation: no mutation, no
                // receipt — the caller reports "no active session". A
                // retry deterministically reports the same (nothing was
                // done, so there is nothing to make idempotent).
                tx.rollback();
                return Ok(SessionOp::Applied((handle, Vec::new(), false)));
            }
            // Required guide outcomes inside the SAME transaction.
            let mut improvement_lines: Vec<String> = Vec::new();
            if outcome == ltmrs_domain::session::TaskOutcome::Success
                || outcome == ltmrs_domain::session::TaskOutcome::Failure
            {
                for guide_name in session.guides_used.clone() {
                    let gkey = guide_name.to_lowercase();
                    let graw = tx.get(&self.guides, &gkey).map_err(|e| {
                        DomainError::new(DomainErrorCode::Validation, e.to_string())
                    })?;
                    let Some(graw) = graw else { continue };
                    let mut guide: ltmrs_domain::guide::Guide =
                        serde_json::from_slice(graw.as_ref()).map_err(|e| {
                            DomainError::new(DomainErrorCode::Validation, e.to_string())
                        })?;
                    if outcome == ltmrs_domain::session::TaskOutcome::Success {
                        guide.success_count += 1;
                    } else {
                        guide.failure_count += 1;
                    }
                    guide.entity_revision = guide.entity_revision.next();
                    guide.updated_at = ltmrs_domain::memory::Instant::new(now_millis);
                    if outcome == ltmrs_domain::session::TaskOutcome::Failure {
                        improvement_lines.push(Self::improvement_line(&guide));
                    }
                    let graw = serde_json::to_vec(&guide).map_err(|e| {
                        DomainError::new(DomainErrorCode::Validation, e.to_string())
                    })?;
                    tx.insert(&self.guides, &gkey, graw.as_slice());
                }
                improvement_lines.retain(|l| !l.is_empty());
            }
            session.status = ltmrs_domain::session::SessionStatus::Ended;
            session.outcome = Some(outcome);
            session.final_approach = final_approach.clone();
            session.lessons = lessons.clone();
            session.ended_at = Some(ltmrs_domain::memory::Instant::new(now_millis));
            let raw = serde_json::to_vec(&session)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.sessions, &hkey, raw.as_slice());
            // Improvement suggestions file in the SAME transaction: the
            // terminal transition, guide outcomes, suggestions and receipt
            // are one consistency boundary, so duplicate deliveries of
            // this operation can never file twice.
            self.file_suggestions_tx(&mut tx, handle, &improvement_lines, now_millis)?;
            let receipt = SessionReceipt {
                digest: scope.request_digest.clone(),
                session: handle,
                seq: None,
                response: None,
                continuity_boosted: false,
                scope: Some(scope.clone()),
            };
            let log_raw = serde_json::to_vec(&receipt)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.session_ops, &op_key, log_raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    self.fire_commit_hook();
                    return Ok(SessionOp::Applied((handle, improvement_lines, true)));
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("session end conflicted"))
    }

    /// Track session links (guides used, memories read/created) with
    /// order-preserving deduplication, committed atomically.
    pub fn track_session_link(
        &self,
        admitted: &AdmittedScope,
        handle: SessionHandle,
        field: SessionLinkField,
        ids: &[String],
    ) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let scope = admitted.scope();
        let seq_key = op_seq_key_for_scope(scope.frontend_id, scope.channel_id);
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let hkey = handle.as_uuid().to_string();
            let raw = tx
                .get(&self.sessions, &hkey)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(raw) = raw else {
                return Ok(());
            };
            let mut session: Session = serde_json::from_slice(raw.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let target = match field {
                SessionLinkField::GuideUsed => &mut session.guides_used,
                SessionLinkField::MemoryRead => &mut session.memories_read,
                SessionLinkField::MemoryCreated => &mut session.memories_created,
            };
            if field == SessionLinkField::GuideUsed {
                for id in ids {
                    let lower = id.to_lowercase();
                    if !target.contains(&lower) {
                        target.push(lower);
                    }
                }
            } else {
                for id in ids {
                    if !target.contains(id) {
                        target.push(id.clone());
                    }
                }
            }
            let raw = serde_json::to_vec(&session)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.sessions, &hkey, raw.as_slice());
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
        Err(Self::exhausted_contention("session link conflicted"))
    }

    /// Adjust one attempt's confidence (suggestion feedback): boost capped
    /// at 1.0, penalty floored at 0.0; access counters increment. No-op
    /// when the session or attempt is absent.
    pub fn adjust_attempt(
        &self,
        handle: SessionHandle,
        seq: u32,
        delta: f64,
        now_millis: u64,
    ) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key_system("session");
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            self.adjust_attempt_tx(&mut tx, handle, seq, delta, now_millis)?;
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
        Err(Self::exhausted_contention("attempt adjust conflicted"))
    }

    /// Attempt confidence adjustment inside the caller's transaction (tx
    /// core shared by the standalone adjust and the receipt-claimed
    /// continuity boost). Missing sessions/attempts adjust nothing and
    /// still succeed, matching the old leniency. Returns whether an
    /// attempt was adjusted.
    fn adjust_attempt_tx(
        &self,
        tx: &mut OptimisticWriteTx,
        handle: SessionHandle,
        seq: u32,
        delta: f64,
        now_millis: u64,
    ) -> DomainResult<bool> {
        let hkey = handle.as_uuid().to_string();
        let raw = tx
            .get(&self.sessions, &hkey)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let Some(raw) = raw else {
            return Ok(false);
        };
        let mut session: Session = serde_json::from_slice(raw.as_ref())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let mut adjusted = false;
        if let Some(a) = session.attempts.iter_mut().find(|a| a.seq == seq) {
            a.confidence = (a.confidence + delta).clamp(0.0, 1.0);
            a.access_count += 1;
            a.last_accessed_at = Some(ltmrs_domain::memory::Instant::new(now_millis));
            adjusted = true;
        }
        let raw = serde_json::to_vec(&session)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        tx.insert(&self.sessions, &hkey, raw.as_slice());
        Ok(adjusted)
    }

    /// Apply continuity-recall attempt boosts exactly once per session-start
    /// operation (P2-B): the boost targets plus the claimed flag commit in
    /// ONE transaction. Returns true when this call applied the boosts,
    /// false when a previous call already claimed them (crash-window
    /// continuation must not double-boost). Digest mismatch rejects.
    pub fn claim_continuity_boost(
        &self,
        admitted: &AdmittedScope,
        targets: &[(SessionHandle, u32)],
        delta: f64,
        now_millis: u64,
    ) -> DomainResult<bool> {
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
            if rec.continuity_boosted {
                self.persist_barrier()?;
                return Ok(false);
            }
            for (handle, seq) in targets {
                self.adjust_attempt_tx(&mut tx, *handle, *seq, delta, now_millis)?;
            }
            rec.continuity_boosted = true;
            let raw = serde_json::to_vec(&rec)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.session_ops, &op_key, raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(true);
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention(
            "continuity boost claim conflicted",
        ))
    }
}
