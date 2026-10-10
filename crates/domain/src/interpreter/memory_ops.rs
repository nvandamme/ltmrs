//! Interpreter memory operations (moved verbatim from `interpreter.rs`).

use super::ReferenceInterpreter;
use crate::command::{
    DomainError, DomainErrorCode, DomainResult, ForgetMode, MemoryPatch, ReceiptOutcome,
};
use crate::graph::{GraphValidation, validate_new_edge};
use crate::id::{EntityId, EntityRevision};
use crate::memory::{Instant, Memory, MemoryLifecycle};
use crate::relation::Relation;
use crate::session::FeedbackEvent;

impl ReferenceInterpreter {
    pub(crate) fn apply_add_memory(
        &mut self,
        memory: &Memory,
        auto_link: Option<&Relation>,
    ) -> DomainResult<ReceiptOutcome> {
        if self.memories.contains_key(&memory.id) {
            return Err(DomainError::new(
                DomainErrorCode::Validation,
                "memory already exists",
            ));
        }
        if let Some(alias) = &memory.external_alias
            && self.aliases.contains_key(alias)
        {
            return Err(DomainError::new(
                DomainErrorCode::DuplicateAlias,
                "alias already in use",
            ));
        }

        let mut memory = memory.clone();
        memory.entity_revision = self.next_revision();
        self.memories.insert(memory.id, memory.clone());
        if let Some(alias) = &memory.external_alias {
            self.aliases.insert(alias.clone(), memory.id);
        }
        let mut affected = vec![memory.id];
        // Parity with the canonical gateway: the planned auto-link
        // records atomically; duplicates skip, other rejections fail.
        if let Some(rel) = auto_link {
            let live_memory_ids = |id: EntityId| -> bool {
                self.memories
                    .get(&id)
                    .map(|m| m.lifecycle.is_recallable())
                    .unwrap_or(false)
            };
            match validate_new_edge(rel, self.relations.iter(), live_memory_ids) {
                GraphValidation::Ok => {
                    self.relations.push(rel.clone());
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

    pub(crate) fn apply_update_memory(
        &mut self,
        id: EntityId,
        expected_revision: Option<EntityRevision>,
        patch: &MemoryPatch,
    ) -> DomainResult<ReceiptOutcome> {
        let memory = self
            .memories
            .get_mut(&id)
            .ok_or_else(|| DomainError::new(DomainErrorCode::NotFound, "memory not found"))?;

        // Parity with the canonical gateway (I3): lifecycle transitions
        // stay in Forget/Merge; direct updates to non-Live rows are rejected.
        if !matches!(memory.lifecycle, MemoryLifecycle::Live) {
            return Err(DomainError::new(
                DomainErrorCode::Validation,
                "memory is not live",
            ));
        }

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
            // Project is Lance-indexed: mirrors the canonical gateway, where
            // it counts as content for re-projection (parity).
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
            // Parity with the canonical gateway (I3): absolute-only writes
            // advance the entity revision so same-expected writers conflict.
            memory.entity_revision = memory.entity_revision.next();
        }

        Ok(ReceiptOutcome::Success { affected: vec![id] })
    }

    pub(crate) fn apply_feedback(
        &mut self,
        memory_id: EntityId,
        useful: bool,
    ) -> DomainResult<ReceiptOutcome> {
        let memory = self
            .memories
            .get_mut(&memory_id)
            .ok_or_else(|| DomainError::new(DomainErrorCode::NotFound, "memory not found"))?;

        // Upstream contract: positive = boostOnAccess (+0.015 conf, access
        // bump) + positive_feedback++; negative = recordNegativeHit (-0.02
        // conf, negative_hits++) + negative_feedback++.
        if useful {
            memory.positive_feedback += 1;
            memory.confidence = (memory.confidence + 0.015).min(1.0);
            memory.access_count += 1;
            memory.last_accessed_at = Some(Instant::new(self.clock.now_millis()));
        } else {
            memory.negative_feedback += 1;
            memory.negative_hits += 1;
            memory.confidence = (memory.confidence - 0.02).max(0.0);
        }

        self.feedback.push(FeedbackEvent {
            id: self.id_gen.next_entity_id(),
            memory_id,
            useful,
            timestamp: Instant::new(self.clock.now_millis()),
        });

        Ok(ReceiptOutcome::Success {
            affected: vec![memory_id],
        })
    }

    pub(crate) fn apply_relate(&mut self, relation: &Relation) -> DomainResult<ReceiptOutcome> {
        // Parity with the canonical gateway (I4): relation ids are bound
        // to their full input; reuse with any divergence fails.
        if let Some(existing) = self.relations.iter().find(|r| r.id == relation.id)
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
        let live_memory_ids = |id: EntityId| -> bool {
            self.memories
                .get(&id)
                .map(|m| m.lifecycle.is_recallable())
                .unwrap_or(false)
        };

        match validate_new_edge(relation, self.relations.iter(), live_memory_ids) {
            GraphValidation::Ok => {}
            GraphValidation::Reject(code) => {
                return Err(DomainError::new(code, "graph validation failed"));
            }
        }

        self.relations.push(relation.clone());
        Ok(ReceiptOutcome::Success {
            affected: vec![relation.source, relation.target],
        })
    }

    pub(crate) fn apply_unrelate(
        &mut self,
        source: EntityId,
        target: EntityId,
        relation_type: crate::relation::RelationType,
    ) -> DomainResult<ReceiptOutcome> {
        let before_len = self.relations.len();
        self.relations.retain(|r| {
            !(r.source == source && r.target == target && r.relation_type == relation_type)
        });
        if self.relations.len() == before_len {
            return Err(DomainError::new(
                DomainErrorCode::NotFound,
                "relation not found",
            ));
        }
        Ok(ReceiptOutcome::Success {
            affected: vec![source, target],
        })
    }

    pub(crate) fn apply_merge(
        &mut self,
        source_ids: &[EntityId],
        result: &Memory,
        consolidate: bool,
    ) -> DomainResult<ReceiptOutcome> {
        for source_id in source_ids {
            if !self.memories.contains_key(source_id) {
                return Err(DomainError::new(
                    DomainErrorCode::NotFound,
                    "source memory not found",
                ));
            }
        }
        if self.memories.contains_key(&result.id) {
            return Err(DomainError::new(
                DomainErrorCode::Validation,
                "result memory already exists",
            ));
        }
        // Parity with the canonical gateway (C8): merge enforces alias
        // uniqueness and registers the result alias.
        if let Some(alias) = &result.external_alias
            && self.aliases.contains_key(alias)
        {
            return Err(DomainError::new(
                DomainErrorCode::DuplicateAlias,
                "alias already in use",
            ));
        }

        let mut affected = Vec::new();

        let mut result = result.clone();
        result.entity_revision = self.next_revision();
        if let Some(alias) = &result.external_alias {
            self.aliases.insert(alias.clone(), result.id);
        }
        self.memories.insert(result.id, result.clone());
        affected.push(result.id);

        // Parity with the canonical gateway: consolidate=true keeps live
        // sources (down-weighted, supersession edges); consolidate=false
        // hard-deletes them (rows gone, edges severed).
        let now = Instant::new(self.clock.now_millis());
        if consolidate {
            // Edges record while both endpoints are live.
            for source_id in source_ids {
                let edge = Relation::consolidation_edge(result.id, *source_id, now);
                self.apply_relate(&edge)?;
            }
            for source_id in source_ids {
                if let Some(source) = self.memories.get_mut(source_id) {
                    source.confidence = crate::memory::CONSOLIDATED_CONFIDENCE;
                    affected.push(*source_id);
                }
            }
        } else {
            for source_id in source_ids {
                self.relations
                    .retain(|r| r.source != *source_id && r.target != *source_id);
                self.memories.remove(source_id);
                affected.push(*source_id);
            }
        }

        Ok(ReceiptOutcome::Success { affected })
    }

    pub(crate) fn apply_forget(
        &mut self,
        id: EntityId,
        mode: ForgetMode,
    ) -> DomainResult<ReceiptOutcome> {
        let memory = self
            .memories
            .get_mut(&id)
            .ok_or_else(|| DomainError::new(DomainErrorCode::NotFound, "memory not found"))?;

        let now = Instant::new(self.clock.now_millis());
        memory.lifecycle = match mode {
            ForgetMode::Delete => MemoryLifecycle::Deleted { at: now },
            ForgetMode::Invalidate => MemoryLifecycle::Invalidated { at: now },
            ForgetMode::Archive => MemoryLifecycle::Archived { at: now },
        };
        memory.advance_eligibility();

        Ok(ReceiptOutcome::Success { affected: vec![id] })
    }

    pub(crate) fn apply_access(
        &mut self,
        memory_ids: &[EntityId],
        context: Option<&str>,
    ) -> DomainResult<ReceiptOutcome> {
        let now = self.clock.now_millis();
        let mut affected = Vec::new();
        for id in memory_ids {
            if let Some(memory) = self.memories.get_mut(id) {
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
                affected.push(*id);
            }
        }
        Ok(ReceiptOutcome::Success { affected })
    }

    pub(crate) fn apply_boost_confidence(
        &mut self,
        memory_ids: &[EntityId],
    ) -> DomainResult<ReceiptOutcome> {
        let now = self.clock.now_millis();
        let mut affected = Vec::new();
        for id in memory_ids {
            if let Some(memory) = self.memories.get_mut(id) {
                memory.confidence = (memory.confidence + 0.02).min(1.0);
                memory.access_count += 1;
                memory.last_accessed_at = Some(Instant::new(now));
                affected.push(*id);
            }
        }
        Ok(ReceiptOutcome::Success { affected })
    }
}
