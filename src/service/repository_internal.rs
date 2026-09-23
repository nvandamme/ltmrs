//! Internal command application for the canonical repository.
//!
//! Mirrors the reference interpreter's state transitions, but reads and writes
//! through a Fjall write transaction so that precondition validation and the
//! resulting writes share one atomic snapshot.

use fjall::{OptimisticTxKeyspace, OptimisticWriteTx, Readable};

use crate::domain::command::{
    CommandContext, DomainCommand, DomainError, DomainErrorCode, DomainResult, MemoryPatch,
    ReceiptOutcome,
};
use crate::domain::graph::{GraphValidation, validate_new_edge};
use crate::domain::id::{EntityId, EntityRevision, ExternalAlias};
use crate::domain::memory::{Instant, Memory, MemoryLifecycle};
use crate::domain::projection::ProjectionJob;
use crate::domain::relation::{Relation, RelationType};

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
    feedback_events: &'a OptimisticTxKeyspace,
    /// Snapshot of the clock at command-application time, used to stamp
    /// projection jobs with their enqueue instant (for oldest-pending-age).
    now_millis: u64,
}

impl<'a> CommandState<'a> {
    pub(crate) fn new(
        tx: &'a mut OptimisticWriteTx,
        memories: &'a OptimisticTxKeyspace,
        relations: &'a OptimisticTxKeyspace,
        aliases: &'a OptimisticTxKeyspace,
        projections: &'a OptimisticTxKeyspace,
        feedback_events: &'a OptimisticTxKeyspace,
        now_millis: u64,
    ) -> Self {
        Self {
            tx,
            memories,
            relations,
            aliases,
            projections,
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
        DomainCommand::AddMemory { memory, .. } => apply_add_memory(state, memory),
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
        DomainCommand::Merge { source_ids, result } => apply_merge(state, source_ids, result),
        DomainCommand::Forget { id, mode } => apply_forget(state, *id, *mode),
        DomainCommand::Access {
            memory_ids,
            context,
        } => apply_access(state, memory_ids, context.as_deref()),
        // Session/guide commands are handled by their dedicated work packages;
        // the canonical gateway rejects them until those land.
        _ => Err(DomainError::new(
            DomainErrorCode::Validation,
            "command not yet supported by the canonical gateway",
        )),
    }
}

fn apply_add_memory(state: &mut CommandState<'_>, memory: &Memory) -> DomainResult<ReceiptOutcome> {
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
    Ok(ReceiptOutcome::Success {
        affected: vec![memory.id],
    })
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
    }

    state.put_memory(&memory)?;
    // Content mutations enqueue a newer desired-state job so the worker
    // re-projects at the new document revision.
    if content_changed {
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
    let event = crate::domain::session::FeedbackEvent {
        id: EntityId::new(uuid::Uuid::from_bytes(event_uuid_bytes)),
        memory_id,
        useful,
        timestamp: Instant::new(0),
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
            affected.push(*id);
        }
    }
    Ok(ReceiptOutcome::Success { affected })
}

fn apply_relate(state: &mut CommandState<'_>, relation: &Relation) -> DomainResult<ReceiptOutcome> {
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

    let now = Instant::new(0);
    let mut affected = Vec::new();
    for source_id in source_ids {
        if let Some(mut source) = state.get_memory(*source_id)? {
            source.lifecycle = MemoryLifecycle::Archived { at: now };
            source.advance_eligibility();
            state.put_memory(&source)?;
            affected.push(*source_id);
        }
    }

    let mut result = result.clone();
    result.entity_revision = EntityRevision::new(result.entity_revision.as_u64() + 1);
    state.put_memory(&result)?;
    affected.push(result.id);

    Ok(ReceiptOutcome::Success { affected })
}

fn apply_forget(
    state: &mut CommandState<'_>,
    id: EntityId,
    mode: crate::domain::command::ForgetMode,
) -> DomainResult<ReceiptOutcome> {
    let mut memory = state
        .get_memory(id)?
        .ok_or_else(|| DomainError::new(DomainErrorCode::NotFound, "memory not found"))?;

    let now = Instant::new(0);
    memory.lifecycle = match mode {
        crate::domain::command::ForgetMode::Delete => MemoryLifecycle::Deleted { at: now },
        crate::domain::command::ForgetMode::Invalidate => MemoryLifecycle::Invalidated { at: now },
        crate::domain::command::ForgetMode::Archive => MemoryLifecycle::Archived { at: now },
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
    if matches!(mode, crate::domain::command::ForgetMode::Delete) {
        state.remove_edges_involving(id)?;
    }
    state.invalidate_projection(id)?;

    state.put_memory(&memory)?;
    Ok(ReceiptOutcome::Success { affected: vec![id] })
}
