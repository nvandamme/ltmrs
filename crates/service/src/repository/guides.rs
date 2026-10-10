//! Guide reads, practice, session effects and distillation (moved verbatim from `repository.rs`).

use fjall::{OptimisticWriteTx, Readable};

use super::{
    AdmittedScope, CanonicalRepository, MAX_RETRIES, PracticeLog, decode, op_seq_key_for_scope,
    op_seq_key_system,
};
use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult, OperationScope};
use ltmrs_domain::memory::Memory;

impl CanonicalRepository {
    /// All guides from a single snapshot.
    pub fn get_guides(&self) -> DomainResult<Vec<ltmrs_domain::guide::Guide>> {
        let snapshot = self.db.read_tx();
        let mut out = Vec::new();
        for kv in snapshot.iter(&self.guides) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            out.push(decode::<ltmrs_domain::guide::Guide>(v.as_ref())?);
        }
        Ok(out)
    }

    /// A single guide by name (case-insensitive, matching upstream COLLATE NOCASE).
    pub fn get_guide(&self, name: &str) -> DomainResult<Option<ltmrs_domain::guide::Guide>> {
        let target = name.to_lowercase();
        Ok(self
            .get_guides()?
            .into_iter()
            .find(|g| g.name.eq_ignore_ascii_case(&target)))
    }

    /// Store a guide with revision enforcement (re-review P1-2): every
    /// mutation of an existing guide must invalidate concurrent plans.
    /// `expected=None` creates if absent and fails when the key already
    /// exists; `expected=Some(rev)` requires the live record at `rev`
    /// (else RevisionConflict) and advances it. The blind `put_guide`
    /// remains for seed paths that own their key outright.
    pub fn put_guide_checked(
        &self,
        expected: Option<ltmrs_domain::id::EntityRevision>,
        guide: &ltmrs_domain::guide::Guide,
    ) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key_system("guide");
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            self.put_guide_apply_tx(&mut tx, expected, guide)?;
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
        Err(Self::exhausted_contention("guide write conflicted"))
    }

    /// Revision-checked guide put inside the caller's transaction (tx core
    /// shared by the standalone write and the idempotent tool wrapper).
    /// Returns the guide as written (revision advanced on updates).
    pub(crate) fn put_guide_apply_tx(
        &self,
        tx: &mut OptimisticWriteTx,
        expected: Option<ltmrs_domain::id::EntityRevision>,
        guide: &ltmrs_domain::guide::Guide,
    ) -> DomainResult<ltmrs_domain::guide::Guide> {
        let key = guide.name.to_lowercase();
        let fresh: Option<ltmrs_domain::guide::Guide> = tx
            .get(&self.guides, &key)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            .map(|raw| {
                serde_json::from_slice(raw.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
            })
            .transpose()?;
        let mut guide = guide.clone();
        match (fresh, expected) {
            (None, None) => {}
            (Some(_), None) => {
                return Err(DomainError::new(
                    DomainErrorCode::Validation,
                    "guide already exists",
                ));
            }
            (None, Some(_)) => {
                return Err(DomainError::new(
                    DomainErrorCode::NotFound,
                    "guide not found",
                ));
            }
            (Some(live), Some(rev)) => {
                if live.entity_revision != rev {
                    return Err(DomainError::new(
                        DomainErrorCode::RevisionConflict,
                        "guide changed since read: re-read and re-plan",
                    ));
                }
                guide.entity_revision = live.entity_revision.next();
            }
        }
        let raw = serde_json::to_vec(&guide)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        tx.insert(&self.guides, &key, raw.as_slice());
        Ok(guide)
    }

    /// Store a guide (keyed by lowercased name). Blind write (no receipt,
    /// revision check or reference upkeep): tests, bench seeding and offline
    /// repair only — production tool writes go through `guide_mutation_idempotent`.
    pub fn put_guide(&self, guide: &ltmrs_domain::guide::Guide) -> DomainResult<()> {
        let _restore_guard = self.restore_lock.read().unwrap();
        let key = guide.name.to_lowercase();
        let raw = serde_json::to_vec(guide)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        let seq_key = op_seq_key_system("guide");
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.guides, &key, raw.as_slice());
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
        Err(Self::exhausted_contention("guide write conflicted"))
    }

    /// Practice a guide idempotently (P1 replay safety, hardened re-review
    /// R5): the operation ID is logged atomically with the guide mutation.
    /// A retried operation with the same ID + digest returns the RECORDED
    /// guide snapshot (not current contents); the same ID with a different
    /// digest is rejected as key reuse. Fresh-read inside each retry
    /// preserves concurrent updates.
    #[allow(clippy::too_many_arguments)]
    pub fn practice_guide_idempotent(
        &self,
        admitted: &AdmittedScope,
        guide_name: &str,
        category: &str,
        description: Option<&str>,
        contexts: &[String],
        learnings: &[String],
        validated_by: &[String],
        outcome: Option<bool>,
        now_millis: u64,
    ) -> DomainResult<ltmrs_domain::guide::Guide> {
        // Admission-once: the caller admitted this scope at tool entry, so
        // TTL is not revalidated here (only owner/digest checks remain on
        // the replay paths below).
        let scope = admitted.scope();
        let _restore_guard = self.restore_lock.read().unwrap();
        let seq_key = op_seq_key_for_scope(scope.frontend_id, scope.channel_id);
        let op_key = scope.op_key();
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            // Replay check first: same operation already applied.
            if let Some(raw) = tx
                .get(&self.guide_ops, scope.op_key())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                // New format: digest-bound recorded outcome. The barrier runs
                // again before acknowledging (re-review P1-1): a visible
                // receipt is not proof its flush succeeded — a prior
                // barrier failure must fail this replay too.
                if let Ok(log) = serde_json::from_slice::<PracticeLog>(raw.as_ref()) {
                    Self::check_scope_owner(log.scope.as_ref(), scope)?;
                    if log.digest != scope.request_digest {
                        return Err(DomainError::new(
                            DomainErrorCode::KeyReuseDifferentInput,
                            "operation key reused with different input",
                        ));
                    }
                    self.persist_barrier()?;
                    return Ok(log.recorded);
                }
                // Legacy bare-name entries (pre-digest): preserve exact old
                // semantics (current contents, or NotFound when forgotten) —
                // no re-application, no new counting.
                if let Ok(name) = serde_json::from_slice::<String>(raw.as_ref()) {
                    let key = name.to_lowercase();
                    if let Some(raw) = tx
                        .get(&self.guides, &key)
                        .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
                    {
                        let guide: ltmrs_domain::guide::Guide =
                            serde_json::from_slice(raw.as_ref()).map_err(|e| {
                                DomainError::new(DomainErrorCode::Validation, e.to_string())
                            })?;
                        return Ok(guide);
                    }
                    return Err(DomainError::new(
                        DomainErrorCode::NotFound,
                        "guide not found (practiced guide was forgotten)",
                    ));
                }
                return Err(DomainError::new(
                    DomainErrorCode::Validation,
                    "unrecognized practice log entry",
                ));
            }
            // Fresh guide state inside the tx.
            let key = guide_name.to_lowercase();
            let existing: Option<ltmrs_domain::guide::Guide> = tx
                .get(&self.guides, &key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
                .map(|raw| {
                    serde_json::from_slice(raw.as_ref())
                        .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
                })
                .transpose()?;
            let mut updated = match existing {
                None => {
                    let mut g = ltmrs_domain::guide::Guide {
                        name: guide_name.to_lowercase().trim().to_string(),
                        category: category.to_lowercase().trim().to_string(),
                        description: description.unwrap_or("").trim().to_string(),
                        contexts: contexts
                            .iter()
                            .map(|c| c.to_lowercase().trim().to_string())
                            .filter(|c| !c.is_empty())
                            .collect(),
                        learnings: learnings
                            .iter()
                            .map(|l| l.trim().to_string())
                            .filter(|l| !l.is_empty())
                            .collect(),
                        usage_count: 1,
                        last_used: Some(ltmrs_domain::memory::Instant::new(now_millis)),
                        success_count: 0,
                        failure_count: 0,
                        anti_patterns: vec![],
                        pitfalls: vec![],
                        depends_on: vec![],
                        enables: vec![],
                        source_memories: vec![],
                        validated_by: vec![],
                        superseded_by: None,
                        deprecated: false,
                        entity_revision: ltmrs_domain::id::EntityRevision::new(1),
                        created_at: ltmrs_domain::memory::Instant::new(now_millis),
                        updated_at: ltmrs_domain::memory::Instant::new(now_millis),
                    };
                    if outcome == Some(true) {
                        g.success_count = 1;
                    } else if outcome == Some(false) {
                        g.failure_count = 1;
                    }
                    g
                }
                Some(mut g) => {
                    g.usage_count += 1;
                    g.last_used = Some(ltmrs_domain::memory::Instant::new(now_millis));
                    if g.description.is_empty()
                        && let Some(desc) = description
                    {
                        g.description = desc.trim().to_string();
                    }
                    for ctx in contexts {
                        let normalized = ctx.to_lowercase().trim().to_string();
                        if !normalized.is_empty()
                            && !g
                                .contexts
                                .iter()
                                .any(|c| c.eq_ignore_ascii_case(&normalized))
                        {
                            g.contexts.push(normalized);
                        }
                    }
                    for learning in learnings {
                        let trimmed = learning.trim().to_string();
                        if !trimmed.is_empty() && !g.learnings.contains(&trimmed) {
                            g.learnings.push(trimmed);
                        }
                    }
                    if outcome == Some(true) {
                        g.success_count += 1;
                    } else if outcome == Some(false) {
                        g.failure_count += 1;
                    }
                    g.entity_revision = g.entity_revision.next();
                    g.updated_at = ltmrs_domain::memory::Instant::new(now_millis);
                    g
                }
            };
            for mem_id in validated_by {
                if !updated.validated_by.contains(mem_id) {
                    updated.validated_by.push(mem_id.clone());
                }
            }
            let raw = serde_json::to_vec(&updated)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.guides, &key, raw.as_slice());
            let log = PracticeLog {
                name: updated.name.clone(),
                digest: scope.request_digest.clone(),
                recorded: updated.clone(),
                scope: Some(scope.clone()),
            };
            let log_raw = serde_json::to_vec(&log)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.guide_ops, &op_key, log_raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(updated);
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("guide practice conflicted"))
    }

    /// Apply one session_end guide outcome atomically with its idempotency
    /// marker (re-review R5, hardened re-review P1-3): the success/failure
    /// count bump and the scoped `{key}:guide:{name}` marker commit in ONE
    /// transaction. Returns true when newly applied, false when the marker
    /// was already present for the same arguments (retry resumes without
    /// double-counting) or the guide is gone (forget wins — no marker
    /// written, so a later retry re-checks). The marker binds the scope
    /// AND outcome: a retry with changed arguments after partial
    /// effects rejects as key reuse instead of completing a mixed outcome.
    /// Entity revision advances so concurrent merges observe the change.
    pub fn apply_session_guide_effect(
        &self,
        scope: &OperationScope,
        guide_name: &str,
        success: bool,
        now_millis: u64,
    ) -> DomainResult<bool> {
        let _restore_guard = self.restore_lock.read().unwrap();
        self.validate_scope(scope)?;
        let marker = format!("{}:guide:{}", scope.op_key(), guide_name.to_lowercase());
        let seq_key = op_seq_key_system("session");
        for _attempt in 0..MAX_RETRIES {
            let mut tx = self
                .db
                .write_tx()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if let Some(raw) = tx
                .get(&self.guide_ops, &marker)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                // A completed marker for this effect binds its digest and
                // outcome: same arguments resume, changed arguments reject
                // — a mixed-outcome completion can never assemble.
                let marked: serde_json::Value = serde_json::from_slice(raw.as_ref())
                    .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
                let same = marked.get("digest").and_then(|d| d.as_str())
                    == Some(scope.request_digest.as_str())
                    && marked.get("success").and_then(|s| s.as_bool()) == Some(success);
                if same {
                    return Ok(false);
                }
                return Err(DomainError::new(
                    DomainErrorCode::KeyReuseDifferentInput,
                    "operation key reused with different input after partial effects",
                ));
            }
            let guide_key = guide_name.to_lowercase();
            let raw = tx
                .get(&self.guides, &guide_key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(raw) = raw else {
                return Ok(false);
            };
            let mut guide: ltmrs_domain::guide::Guide = serde_json::from_slice(raw.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            if success {
                guide.success_count += 1;
            } else {
                guide.failure_count += 1;
            }
            guide.entity_revision = guide.entity_revision.next();
            guide.updated_at = ltmrs_domain::memory::Instant::new(now_millis);
            let raw = serde_json::to_vec(&guide)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.guides, &guide_key, raw.as_slice());
            let marker_raw = serde_json::to_vec(&serde_json::json!({
                "applied": true,
                "digest": scope.request_digest,
                "success": success,
            }))
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.guide_ops, &marker, marker_raw.as_slice());
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
            "session guide effect conflicted",
        ))
    }

    /// Distill a memory fragment into a guide as ONE canonical operation
    /// (re-review R2): the memory and the guide are both read fresh inside
    /// the transaction, the learning/context merge, usage bump, source link
    /// and memory link patch commit together with an operation receipt. A
    /// concurrent content update can never be overwritten by a stale clone,
    /// because no pre-transaction snapshot exists. Replay returns the
    /// recorded guide; digest mismatch rejects.
    #[allow(clippy::too_many_arguments)]
    pub fn distill_memory_link(
        &self,
        admitted: &AdmittedScope,
        memory_id: ltmrs_domain::id::EntityId,
        guide_name: &str,
        category_default: &str,
        now_millis: u64,
    ) -> DomainResult<ltmrs_domain::guide::Guide> {
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
                .get(&self.guide_ops, &op_key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
            {
                // Recorded outcome — but durability is not inherited from a
                // visible receipt (re-review P1-1): flush again first.
                if let Ok(log) = serde_json::from_slice::<PracticeLog>(raw.as_ref()) {
                    Self::check_scope_owner(log.scope.as_ref(), scope)?;
                    if log.digest != scope.request_digest {
                        return Err(DomainError::new(
                            DomainErrorCode::KeyReuseDifferentInput,
                            "operation key reused with different input",
                        ));
                    }
                    self.persist_barrier()?;
                    return Ok(log.recorded);
                }
                return Err(DomainError::new(
                    DomainErrorCode::Validation,
                    "unrecognized distill log entry",
                ));
            }
            // Fresh memory read inside the transaction.
            let mem_key = memory_id.as_uuid().to_string();
            let raw = tx
                .get(&self.memories, &mem_key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let Some(raw) = raw else {
                return Err(DomainError::new(
                    DomainErrorCode::NotFound,
                    "memory fragment not found",
                ));
            };
            let mut memory: Memory = serde_json::from_slice(raw.as_ref())
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            // Fresh guide read inside the transaction.
            let guide_key = guide_name.to_lowercase();
            let existing: Option<ltmrs_domain::guide::Guide> = tx
                .get(&self.guides, &guide_key)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?
                .map(|raw| {
                    serde_json::from_slice(raw.as_ref())
                        .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
                })
                .transpose()?;
            let context = memory
                .project
                .clone()
                .unwrap_or_else(|| "global".to_string())
                .to_lowercase()
                .trim()
                .to_string();
            let mut updated = match existing {
                Some(mut g) => {
                    if !g.learnings.contains(&memory.fragment) {
                        g.learnings.push(memory.fragment.clone());
                    }
                    if !context.is_empty() && !g.contexts.contains(&context) {
                        g.contexts.push(context);
                    }
                    g.usage_count += 1;
                    g.last_used = Some(ltmrs_domain::memory::Instant::new(now_millis));
                    g.entity_revision = g.entity_revision.next();
                    g
                }
                None => {
                    let project_ctx = memory
                        .project
                        .clone()
                        .unwrap_or_else(|| "global".to_string())
                        .to_lowercase()
                        .trim()
                        .to_string();
                    ltmrs_domain::guide::Guide {
                        name: guide_name.to_lowercase().trim().to_string(),
                        category: category_default.to_lowercase().trim().to_string(),
                        description: "Created via distillation from memory.".to_string(),
                        contexts: if project_ctx.is_empty() {
                            vec![]
                        } else {
                            vec![project_ctx]
                        },
                        learnings: vec![memory.fragment.clone()],
                        usage_count: 1,
                        last_used: Some(ltmrs_domain::memory::Instant::new(now_millis)),
                        success_count: 0,
                        failure_count: 0,
                        anti_patterns: vec![],
                        pitfalls: vec![],
                        depends_on: vec![],
                        enables: vec![],
                        source_memories: vec![],
                        validated_by: vec![],
                        superseded_by: None,
                        deprecated: false,
                        entity_revision: ltmrs_domain::id::EntityRevision::new(1),
                        created_at: ltmrs_domain::memory::Instant::new(now_millis),
                        updated_at: ltmrs_domain::memory::Instant::new(now_millis),
                    }
                }
            };
            if !updated.source_memories.contains(&memory_id) {
                updated.source_memories.push(memory_id);
            }
            updated.updated_at = ltmrs_domain::memory::Instant::new(now_millis);
            // Memory link patch on the FRESH record: related_guides plus
            // distill_candidate clear, entity-only revision advance.
            let normalized_name = guide_name.to_lowercase().trim().to_string();
            if !memory.related_guides.iter().any(|g| g == &normalized_name) {
                memory.related_guides.push(normalized_name);
            }
            memory.distill_candidate = false;
            memory.entity_revision = memory.entity_revision.next();
            let guide_raw = serde_json::to_vec(&updated)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.guides, &guide_key, guide_raw.as_slice());
            let mem_raw = serde_json::to_vec(&memory)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.memories, &mem_key, mem_raw.as_slice());
            let log = PracticeLog {
                name: updated.name.clone(),
                digest: scope.request_digest.clone(),
                recorded: updated.clone(),
                scope: Some(scope.clone()),
            };
            let log_raw = serde_json::to_vec(&log)
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            tx.insert(&self.guide_ops, &op_key, log_raw.as_slice());
            self.bump_op_seq_tx(&mut tx, &seq_key)?;
            match tx.commit() {
                Ok(Ok(())) => {
                    self.persist_barrier()?;
                    return Ok(updated);
                }
                Ok(Err(_)) => continue,
                Err(e) => {
                    return Err(DomainError::new(DomainErrorCode::Validation, e.to_string()));
                }
            }
        }
        Err(Self::exhausted_contention("guide distill conflicted"))
    }
}
