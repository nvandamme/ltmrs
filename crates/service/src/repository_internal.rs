//! Internal command application for the canonical repository.
//!
//! Mirrors the reference interpreter's state transitions, but reads and writes
//! through a Fjall write transaction so that precondition validation and the
//! resulting writes share one atomic snapshot.

use fjall::{OptimisticTxKeyspace, OptimisticWriteTx, Readable};

use ltmrs_domain::command::{
    CommandContext, DomainCommand, DomainError, DomainErrorCode, DomainResult, MemoryPatch,
    ReceiptOutcome,
};
use ltmrs_domain::graph::{GraphValidation, validate_new_edge};
use ltmrs_domain::id::{EntityId, EntityRevision, ExternalAlias};
use ltmrs_domain::memory::{Instant, Memory, MemoryLifecycle};
use ltmrs_domain::projection::{GenerationStatus, ProjectionJob};
use ltmrs_domain::relation::{Relation, RelationType};

use serde::de::DeserializeOwned;

pub(crate) enum TxAction<T> {
    Commit(T),
    Replay(T),
}

pub(crate) struct CommandState<'a> {
    tx: &'a mut OptimisticWriteTx,
    memories: &'a OptimisticTxKeyspace,
    relations: &'a OptimisticTxKeyspace,
    aliases: &'a OptimisticTxKeyspace,
    projections: &'a OptimisticTxKeyspace,
    generations: &'a OptimisticTxKeyspace,
    feedback_events: &'a OptimisticTxKeyspace,
    /// Snapshot of the clock at command-application time, used to stamp
    /// projection jobs with their enqueue instant (for oldest-pending-age).
    now_millis: u64,
}

impl<'a> CommandState<'a> {
    // Eight args: one handle per keyspace plus tx and clock. Bundling them
    // into a params struct is churn without benefit while each call site
    // passes the same fixed set; revisit if a ninth arg appears.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        tx: &'a mut OptimisticWriteTx,
        memories: &'a OptimisticTxKeyspace,
        relations: &'a OptimisticTxKeyspace,
        aliases: &'a OptimisticTxKeyspace,
        projections: &'a OptimisticTxKeyspace,
        generations: &'a OptimisticTxKeyspace,
        feedback_events: &'a OptimisticTxKeyspace,
        now_millis: u64,
    ) -> Self {
        Self {
            tx,
            memories,
            relations,
            aliases,
            projections,
            generations,
            feedback_events,
            now_millis,
        }
    }

    fn get_memory(&self, id: EntityId) -> DomainResult<Option<Memory>> {
        let key = id.as_uuid().to_string();
        let raw = self
            .tx
            .get(self.memories, key)
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        raw.map(|v| decode::<Memory>(v.as_ref())).transpose()
    }

    fn put_memory(&mut self, m: &Memory) -> DomainResult<()> {
        let key = m.id.as_uuid().to_string();
        let raw = encode(m)?;
        self.tx.insert(self.memories, key, &raw);
        Ok(())
    }

    fn get_all_relations(&self) -> DomainResult<Vec<Relation>> {
        let mut out = Vec::new();
        for kv in self.tx.iter(self.relations) {
            let (_k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            out.push(decode::<Relation>(v.as_ref())?);
        }
        Ok(out)
    }

    fn put_relation(&mut self, r: &Relation) -> DomainResult<()> {
        let key = r.id.as_uuid().to_string();
        let raw = encode(r)?;
        self.tx.insert(self.relations, key, &raw);
        Ok(())
    }

    fn remove_relation_by_endpoint(
        &mut self,
        source: EntityId,
        target: EntityId,
        ty: RelationType,
    ) -> DomainResult<bool> {
        let matches = self
            .get_all_relations()?
            .into_iter()
            .filter(|r| r.source == source && r.target == target && r.relation_type == ty)
            .map(|r| r.id.as_uuid().to_string())
            .collect::<Vec<_>>();
        if matches.is_empty() {
            return Ok(false);
        }
        for key in matches {
            self.tx.remove(self.relations, key);
        }
        Ok(true)
    }

    fn alias_exists(&self, alias: &ExternalAlias) -> DomainResult<bool> {
        let raw = self
            .tx
            .get(self.aliases, alias.as_str())
            .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
        Ok(raw.is_some())
    }

    fn put_alias(&mut self, alias: &ExternalAlias, id: EntityId) -> DomainResult<()> {
        let raw = id.as_uuid().to_string();
        self.tx.insert(self.aliases, alias.as_str(), raw.as_bytes());
        Ok(())
    }

    /// Remove every edge whose source or target is the given memory.
    fn remove_edges_involving(&mut self, id: EntityId) -> DomainResult<usize> {
        let to_remove = self
            .get_all_relations()?
            .into_iter()
            .filter(|r| r.source == id || r.target == id)
            .map(|r| r.id.as_uuid().to_string())
            .collect::<Vec<_>>();
        for key in &to_remove {
            self.tx.remove(self.relations, key);
        }
        Ok(to_remove.len())
    }

    /// Record or advance the durable desired-state job for a memory. Called
    /// inside the command transaction so the work is atomic with the mutation.
    /// The seq advances monotonically per memory and is the compare-and-clear
    /// token (RV-07): it is never derived from an identifier.
    fn record_pending_projection(&mut self, id: EntityId, now_millis: u64) -> DomainResult<()> {
        let key = id.as_uuid().to_string();
        let raw = self.tx.get(self.projections, &key);
        let existing = match raw {
            Ok(Some(v)) => Some(decode::<ProjectionJob>(v.as_ref())?),
            Ok(None) => None,
            Err(e) => return Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
        };

        // The canonical memory's current document revision is the desired one.
        let memory = self.get_memory(id)?.ok_or_else(|| {
            DomainError::new(
                DomainErrorCode::Validation,
                "cannot record projection for missing memory",
            )
        })?;

        let job = ProjectionJob {
            memory_id: id,
            desired_document_revision: memory.document_revision,
            seq: existing.map(|j| j.seq + 1).unwrap_or(1),
            enqueued_at_millis: now_millis,
            is_tombstone: false,
        };
        self.tx
            .insert(self.projections, &key, encode(&job)?.as_slice());
        Ok(())
    }

    /// Mark every open pipeline dirty: canonical memory state just mutated,
    /// so a staged generation may not have converged it. Written atomically
    /// with the mutation, so activation can never observe the write without
    /// observing the flag. No pipeline open means no records touched.
    fn mark_build_dirty(&mut self) -> DomainResult<()> {
        // Collect before writing (same rule as restore_replace/gc_expired):
        // inserting while iterating the same keyspace risks skipping
        // entries on iterators without snapshot isolation.
        let mut dirty = Vec::new();
        for kv in self.tx.iter(self.generations) {
            let (k, v) = kv
                .into_inner()
                .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))?;
            let mut rec: ltmrs_domain::projection::GenerationRecord = decode(v.as_ref())?;
            if matches!(
                rec.status,
                GenerationStatus::Staged | GenerationStatus::Building | GenerationStatus::Ready
            ) && !rec.build_dirty
            {
                rec.build_dirty = true;
                let raw = encode(&rec)?;
                dirty.push((k, raw));
            }
        }
        for (k, raw) in dirty {
            self.tx.insert(self.generations, k, &raw);
        }
        Ok(())
    }

    /// Record a tombstone projection job so the worker removes this memory's rows.
    /// Written atomically with the lifecycle change (design §8) so deletion
    /// propagates without an external sweep, and a delayed worker holding an older
    /// seq cannot resurrect it (seq is incremented). Replaces the old silent
    /// removal: instead of dropping the work item we record explicit delete intent.
    fn invalidate_projection(&mut self, id: EntityId) -> DomainResult<()> {
        let key = id.as_uuid().to_string();
        let raw = self.tx.get(self.projections, &key);
        let existing = match raw {
            Ok(Some(v)) => Some(decode::<ProjectionJob>(v.as_ref())?),
            Ok(None) => None,
            Err(e) => return Err(DomainError::new(DomainErrorCode::Validation, e.to_string())),
        };

        // Capture the current document revision so a stale worker's row (written at
        // an older revision) is unambiguously superseded by this tombstone.
        let memory = self.get_memory(id)?.ok_or_else(|| {
            DomainError::new(
                DomainErrorCode::Validation,
                "cannot record tombstone for missing memory",
            )
        })?;

        let job = ProjectionJob {
            memory_id: id,
            desired_document_revision: memory.document_revision,
            seq: existing.map(|j| j.seq + 1).unwrap_or(1),
            enqueued_at_millis: self.now_millis,
            is_tombstone: true,
        };
        self.tx
            .insert(self.projections, &key, encode(&job)?.as_slice());
        Ok(())
    }
}

fn encode<T: serde::Serialize>(value: &T) -> DomainResult<Vec<u8>> {
    serde_json::to_vec(value)
        .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> DomainResult<T> {
    serde_json::from_slice(bytes)
        .map_err(|e| DomainError::new(DomainErrorCode::Validation, e.to_string()))
}

pub(crate) fn apply_command(
    state: &mut CommandState<'_>,
    ctx: &CommandContext,
    cmd: &DomainCommand,
) -> DomainResult<ReceiptOutcome> {
    match cmd {
        DomainCommand::AddMemory {
            memory, auto_link, ..
        } => apply_add_memory(state, memory, auto_link.as_ref()),
        DomainCommand::UpdateMemory {
            id,
            expected_revision,
            patch,
        } => apply_update_memory(state, *id, *expected_revision, patch),
        DomainCommand::Feedback { memory_id, useful } => {
            apply_feedback(state, ctx, *memory_id, *useful)
        }
        DomainCommand::Relate { relation } => apply_relate(state, relation),
        DomainCommand::Unrelate {
            source,
            target,
            relation_type,
        } => apply_unrelate(state, *source, *target, *relation_type),
        DomainCommand::Merge {
            source_ids,
            result,
            consolidate,
        } => apply_merge(state, source_ids, result, *consolidate),
        DomainCommand::Forget { id, mode } => apply_forget(state, *id, *mode),
        DomainCommand::Access {
            memory_ids,
            context,
        } => apply_access(state, memory_ids, context.as_deref()),
        DomainCommand::BoostConfidence { memory_ids } => apply_boost_confidence(state, memory_ids),
        // Session/guide commands are handled by their dedicated work packages;
        // the canonical gateway rejects them until those land.
        _ => Err(DomainError::new(
            DomainErrorCode::Validation,
            "command not yet supported by the canonical gateway",
        )),
    }
}

fn apply_add_memory(
    state: &mut CommandState<'_>,
    memory: &Memory,
    auto_link: Option<&ltmrs_domain::relation::Relation>,
) -> DomainResult<ReceiptOutcome> {
    if state.get_memory(memory.id)?.is_some() {
        return Err(DomainError::new(
            DomainErrorCode::Validation,
            "memory already exists",
        ));
    }
    if let Some(alias) = &memory.external_alias
        && state.alias_exists(alias)?
    {
        return Err(DomainError::new(
            DomainErrorCode::DuplicateAlias,
            "alias already in use",
        ));
    }
    state.put_memory(memory)?;
    if let Some(alias) = &memory.external_alias {
        state.put_alias(alias, memory.id)?;
    }
    // Atomic write set (design §5.3 Add memory row): memory + alias + pending
    // projection. No acknowledged memory is left unindexable without a tracked
    // work item.
    let now = state.now_millis;
    state.record_pending_projection(memory.id, now)?;
    // The projected set changed: any open build must be refreshed.
    state.mark_build_dirty()?;
    let mut affected = vec![memory.id];
    // Planned auto-link, executed in the same transaction (both endpoints
    // live: the memory was just written, the target was live at planning).
    // A duplicate edge (concurrent identical link) skips silently — the
    // link exists, which is all any response claims. Other rejections
    // fail closed: a dead target must re-plan, not silently drop.
    if let Some(rel) = auto_link {
        let live_memory_ids = |id: EntityId| -> bool {
            state
                .get_memory(id)
                .map(|m| m.map(|m| m.lifecycle.is_recallable()).unwrap_or(false))
                .unwrap_or(false)
        };
        let relations = state.get_all_relations()?;
        match validate_new_edge(rel, relations.iter(), live_memory_ids) {
            GraphValidation::Ok => {
                state.put_relation(rel)?;
                affected.push(rel.id);
            }
            GraphValidation::Reject(DomainErrorCode::DuplicateEdge) => {}
            GraphValidation::Reject(code) => {
                return Err(DomainError::new(code, "graph validation failed"));
            }
        }
    }
    Ok(ReceiptOutcome::Success { affected })
}

fn apply_update_memory(
    state: &mut CommandState<'_>,
    id: EntityId,
    expected_revision: Option<EntityRevision>,
    patch: &MemoryPatch,
) -> DomainResult<ReceiptOutcome> {
    let mut memory = state
        .get_memory(id)?
        .ok_or_else(|| DomainError::new(DomainErrorCode::NotFound, "memory not found"))?;

    if !matches!(memory.lifecycle, MemoryLifecycle::Live) {
        return Err(DomainError::new(
            DomainErrorCode::Validation,
            "memory is not live",
        ));
    }

    // Pre-patch confidence: the refresh rule below compares against it so
    // no-op writes (identical absolute values) enqueue nothing.
    let old_confidence = memory.confidence;

    if let Some(expected) = expected_revision
        && memory.entity_revision != expected
    {
        return Err(DomainError::new(
            DomainErrorCode::RevisionConflict,
            "stale revision",
        ));
    }

    let mut content_changed = false;
    if let Some(title) = &patch.title {
        memory.title = title.clone();
        content_changed = true;
    }
    if let Some(fragment) = &patch.fragment {
        memory.fragment = fragment.clone();
        content_changed = true;
    }
    if let Some(description) = &patch.description {
        memory.description = description.clone();
        content_changed = true;
    }
    if let Some(fragment_type) = patch.fragment_type {
        memory.fragment_type = fragment_type;
        content_changed = true;
    }
    if let Some(project) = &patch.project {
        memory.project = project.clone();
        // Project is a Lance-indexed, scope-filtered column: changing it
        // must re-project exactly like content changes.
        content_changed = true;
    }
    if let Some(confidence) = patch.confidence {
        memory.confidence = confidence;
    }
    if let Some(quality_score) = &patch.quality_score {
        memory.quality_score = *quality_score;
    }
    if let Some(tags) = &patch.tags {
        memory.tags = tags.clone();
    }
    if let Some(evidence) = &patch.evidence {
        memory.evidence = evidence.clone();
    }

    if content_changed {
        memory.advance_document();
    } else {
        // Absolute-only writes (confidence, tags, evidence) still advance
        // the entity revision: concurrent writers holding the same expected
        // revision must conflict instead of silently last-writer-winning.
        // The document revision stays put (nothing to re-project).
        memory.entity_revision = memory.entity_revision.next();
    }
    memory.updated_at = Instant::new(state.now_millis);

    state.put_memory(&memory)?;
    // Content mutations enqueue a newer desired-state job so the worker
    // re-projects at the new document revision.
    if content_changed {
        let now = state.now_millis;
        state.record_pending_projection(id, now)?;
        // The projected set changed: any open build must be refreshed.
        state.mark_build_dirty()?;
    } else if patch.confidence.is_some_and(|c| c != old_confidence) {
        // Confidence is a filter-relevant projection column (source
        // pre-filter): a changed confidence must re-publish, or
        // post-convergence drift silently breaks eligibility. The document
        // revision stays put (no re-chunking); the set is unchanged, so no
        // build-dirty. (Unlike feedback/access/boost there is no clamp
        // here: absolute writes store raw, so only identical values skip.)
        // Jobs coalesce per memory, bounding hot-path churn.
        let now = state.now_millis;
        state.record_pending_projection(id, now)?;
    }
    Ok(ReceiptOutcome::Success { affected: vec![id] })
}

fn apply_feedback(
    state: &mut CommandState<'_>,
    ctx: &CommandContext,
    memory_id: EntityId,
    useful: bool,
) -> DomainResult<ReceiptOutcome> {
    let mut memory = state
        .get_memory(memory_id)?
        .ok_or_else(|| DomainError::new(DomainErrorCode::NotFound, "memory not found"))?;
    let old_confidence = memory.confidence;

    // Domain state: the observable counters and confidence are the
    // compatibility-visible effects, persisted atomically with the command.
    // Upstream contract: positive = boostOnAccess (+0.015 conf, access bump)
    // + positive_feedback++; negative = recordNegativeHit (-0.02 conf,
    // negative_hits++) + negative_feedback++.
    if useful {
        memory.positive_feedback += 1;
        memory.confidence = (memory.confidence + 0.015).min(1.0);
        memory.access_count += 1;
        memory.last_accessed_at = Some(Instant::new(state.now_millis));
    } else {
        memory.negative_feedback += 1;
        memory.negative_hits += 1;
        memory.confidence = (memory.confidence - 0.02).max(0.0);
    }
    state.put_memory(&memory)?;

    // Feedback moves confidence: refresh the projection so the source
    // pre-filter reads the adjusted value (same rule as absolute writes).
    // Saturated clamps that change nothing enqueue nothing.
    if memory.confidence != old_confidence {
        state.record_pending_projection(memory_id, state.now_millis)?;
    }

    // Diagnostic telemetry: the feedback event log is separate from domain
    // state. One logical feedback produces exactly one event, keyed by the
    // operation so a replay cannot double-record it. The event ID is a
    // deterministic derivation of the operation ID (distinct namespace bit).
    let op_uuid = ctx.operation_id.as_uuid();
    let op_bytes = op_uuid.as_bytes();
    let mut event_uuid_bytes = [0u8; 16];
    for i in 0..16 {
        event_uuid_bytes[i] = op_bytes[i] ^ 0xF0;
    }
    let event = ltmrs_domain::session::FeedbackEvent {
        id: EntityId::new(uuid::Uuid::from_bytes(event_uuid_bytes)),
        memory_id,
        useful,
        timestamp: Instant::new(state.now_millis),
    };
    let key = format!("feedback:{}", ctx.operation_id.as_uuid());
    let raw = encode(&event)?;
    state.tx.insert(state.feedback_events, key, &raw);

    Ok(ReceiptOutcome::Success {
        affected: vec![memory_id],
    })
}

fn apply_access(
    state: &mut CommandState<'_>,
    memory_ids: &[EntityId],
    context: Option<&str>,
) -> DomainResult<ReceiptOutcome> {
    let now = state.now_millis;
    let mut affected = Vec::new();
    for id in memory_ids {
        if let Some(mut memory) = state.get_memory(*id)? {
            // Contract-visible read side effects (upstream boostOnAccess):
            // confidence +0.015, access_count +1, last_accessed_at, context tag.
            let old_confidence = memory.confidence;
            memory.confidence = (memory.confidence + 0.015).min(1.0);
            memory.access_count += 1;
            memory.last_accessed_at = Some(Instant::new(now));
            if let Some(tag) = context
                .map(|t| t.trim().to_lowercase())
                .filter(|t| !t.is_empty())
                && !memory.tags.contains(&tag)
            {
                memory.tags.push(tag);
            }
            state.put_memory(&memory)?;
            // Read-side confidence bump: refresh the projection (same rule).
            // Saturated clamps that change nothing enqueue nothing.
            // Disclosed loop: reads enqueue jobs, so projection_lag > 0
            // makes the next retrieval report partial=true until the worker
            // passes. Conservative (never claims false completeness) and
            // self-healing; jobs coalesce per memory.
            if memory.confidence != old_confidence {
                state.record_pending_projection(*id, now)?;
            }
            affected.push(*id);
        }
    }
    Ok(ReceiptOutcome::Success { affected })
}

/// Upstream boostConfidence (session_start pre-load): +0.02 confidence,
/// +1 access_count, last_accessed_at. No context tag, no quality recompute.
fn apply_boost_confidence(
    state: &mut CommandState<'_>,
    memory_ids: &[EntityId],
) -> DomainResult<ReceiptOutcome> {
    let now = state.now_millis;
    let mut affected = Vec::new();
    for id in memory_ids {
        if let Some(mut memory) = state.get_memory(*id)? {
            let old_confidence = memory.confidence;
            // Upstream boostConfidence is +0.02 (oracle + tools agree); the
            // +0.015 here was a transcription slip from boostOnAccess.
            memory.confidence = (memory.confidence + 0.02).min(1.0);
            memory.access_count += 1;
            memory.last_accessed_at = Some(Instant::new(now));
            state.put_memory(&memory)?;
            // Confidence bump: refresh the projection (same rule).
            // Saturated clamps that change nothing enqueue nothing.
            if memory.confidence != old_confidence {
                state.record_pending_projection(*id, now)?;
            }
            affected.push(*id);
        }
    }
    Ok(ReceiptOutcome::Success { affected })
}

fn apply_relate(state: &mut CommandState<'_>, relation: &Relation) -> DomainResult<ReceiptOutcome> {
    // Relation ids are bound to their full input: reusing an id with any
    // divergence (endpoints, type, note, timestamp) fails instead of
    // silently overwriting. An exact duplicate still falls through to the
    // edge validator (DuplicateEdge) — unchanged pre-existing behavior.
    if let Some(existing) = state
        .get_all_relations()?
        .into_iter()
        .find(|r| r.id == relation.id)
        && (existing.source != relation.source
            || existing.target != relation.target
            || existing.relation_type != relation.relation_type
            || existing.note != relation.note
            || existing.created_at != relation.created_at)
    {
        return Err(DomainError::new(
            DomainErrorCode::KeyReuseDifferentInput,
            "relation id reused with different input",
        ));
    }
    let relations = state.get_all_relations()?;
    let live_memory_ids = |id: EntityId| -> bool {
        state
            .get_memory(id)
            .map(|m| m.map(|m| m.lifecycle.is_recallable()).unwrap_or(false))
            .unwrap_or(false)
    };

    match validate_new_edge(relation, relations.iter(), live_memory_ids) {
        GraphValidation::Ok => {}
        GraphValidation::Reject(code) => {
            return Err(DomainError::new(code, "graph validation failed"));
        }
    }

    state.put_relation(relation)?;
    Ok(ReceiptOutcome::Success {
        affected: vec![relation.source, relation.target],
    })
}

fn apply_unrelate(
    state: &mut CommandState<'_>,
    source: EntityId,
    target: EntityId,
    relation_type: RelationType,
) -> DomainResult<ReceiptOutcome> {
    if !state.remove_relation_by_endpoint(source, target, relation_type)? {
        return Err(DomainError::new(
            DomainErrorCode::NotFound,
            "relation not found",
        ));
    }
    Ok(ReceiptOutcome::Success {
        affected: vec![source, target],
    })
}

fn apply_merge(
    state: &mut CommandState<'_>,
    source_ids: &[EntityId],
    result: &Memory,
    consolidate: bool,
) -> DomainResult<ReceiptOutcome> {
    for source_id in source_ids {
        if state.get_memory(*source_id)?.is_none() {
            return Err(DomainError::new(
                DomainErrorCode::NotFound,
                "source memory not found",
            ));
        }
    }
    if state.get_memory(result.id)?.is_some() {
        return Err(DomainError::new(
            DomainErrorCode::Validation,
            "result memory already exists",
        ));
    }
    if let Some(alias) = &result.external_alias
        && state.alias_exists(alias)?
    {
        return Err(DomainError::new(
            DomainErrorCode::DuplicateAlias,
            "alias already in use",
        ));
    }

    let now = state.now_millis;
    let mut affected = Vec::new();

    let mut result = result.clone();
    result.entity_revision = EntityRevision::new(result.entity_revision.as_u64() + 1);
    state.put_memory(&result)?;
    if let Some(alias) = &result.external_alias {
        state.put_alias(alias, result.id)?;
    }
    affected.push(result.id);

    if consolidate {
        // Frozen contract (consolidate=true): sources are KEPT live,
        // marked superseded via edges, and down-weighted — never archived.
        // Edges record first so validation sees live endpoints on both
        // sides; creating them after a lifecycle change would reject.
        for source_id in source_ids {
            let edge = ltmrs_domain::relation::Relation::consolidation_edge(
                result.id,
                *source_id,
                Instant::new(now),
            );
            apply_relate(state, &edge)?;
        }
        for source_id in source_ids {
            if let Some(mut source) = state.get_memory(*source_id)? {
                source.confidence = ltmrs_domain::memory::CONSOLIDATED_CONFIDENCE;
                source.entity_revision = source.entity_revision.next();
                state.put_memory(&source)?;
                // Confidence is a filter-relevant projection column: the
                // down-weight must re-publish like any absolute write.
                state.record_pending_projection(*source_id, now)?;
                affected.push(*source_id);
            }
        }
    } else {
        // Frozen contract (consolidate=false): sources are hard-deleted —
        // rows gone, edges severed, projection tombstoned.
        for source_id in source_ids {
            apply_forget(state, *source_id, ltmrs_domain::command::ForgetMode::Delete)?;
            affected.push(*source_id);
        }
    }

    // Merge write set: the result is new recallable content (pending job).
    // Deleted sources were tombstoned above; the projected set changed, so
    // any open build must be refreshed.
    state.record_pending_projection(result.id, now)?;
    state.mark_build_dirty()?;

    Ok(ReceiptOutcome::Success { affected })
}

fn apply_forget(
    state: &mut CommandState<'_>,
    id: EntityId,
    mode: ltmrs_domain::command::ForgetMode,
) -> DomainResult<ReceiptOutcome> {
    let mut memory = state
        .get_memory(id)?
        .ok_or_else(|| DomainError::new(DomainErrorCode::NotFound, "memory not found"))?;

    let now = Instant::new(state.now_millis);
    memory.lifecycle = match mode {
        ltmrs_domain::command::ForgetMode::Delete => MemoryLifecycle::Deleted { at: now },
        ltmrs_domain::command::ForgetMode::Invalidate => MemoryLifecycle::Invalidated { at: now },
        ltmrs_domain::command::ForgetMode::Archive => MemoryLifecycle::Archived { at: now },
    };
    memory.advance_eligibility();

    // Deletion effects (design §5.3 Forget/invalidate row):
    // - Adjacency: hard delete severs edges; invalidation/archival preserve them
    //   as history so a deleted memory's edges are not traversable.
    // - Evidence + guide links: preserved on the tombstone record for audit and
    //   explicit history reads; never silently discarded.
    // - Receipt history: receipts are stored in a separate keyspace and are
    //   never removed by forget — a deleted operation stays auditable.
    // - Pending projections: invalidated so a delayed embedding worker cannot
    //   resurrect or index a deleted/invalidated/archived memory.
    if matches!(mode, ltmrs_domain::command::ForgetMode::Delete) {
        state.remove_edges_involving(id)?;
    }
    state.invalidate_projection(id)?;

    state.put_memory(&memory)?;
    // The recallable set changed: any open build must be refreshed.
    state.mark_build_dirty()?;
    Ok(ReceiptOutcome::Success { affected: vec![id] })
}
