//! Sequential reference interpreter: the oracle for concurrency histories.
//!
//! This is NOT a production backend candidate. It provides deterministic
//! semantics for testing concurrent operations against a serial reference.

use std::collections::HashMap;

use crate::domain::clock::{DeterministicIdGen, FrozenClock};
use crate::domain::command::{
    CommandContext, CommandReceipt, DomainCommand, DomainError, DomainErrorCode, DomainResult,
    ForgetMode, MemoryPatch, ReceiptLedger, ReceiptOutcome,
};
use crate::domain::export::CanonicalExport;
use crate::domain::graph::{GraphValidation, validate_new_edge};
use crate::domain::guide::Guide;
use crate::domain::id::{
    EntityId, EntityRevision, ExternalAlias, OperationId, SessionHandle, StoreGeneration,
};
use crate::domain::memory::{Instant, Memory, MemoryLifecycle};
use crate::domain::relation::Relation;
use crate::domain::session::{FeedbackEvent, Session, SessionStatus, TaskOutcome};

pub struct ReferenceInterpreter {
    pub store_generation: StoreGeneration,
    memories: HashMap<EntityId, Memory>,
    aliases: HashMap<ExternalAlias, EntityId>,
    guides: HashMap<String, Guide>,
    sessions: HashMap<SessionHandle, Session>,
    relations: Vec<Relation>,
    feedback: Vec<FeedbackEvent>,
    suggestions: Vec<crate::domain::session::Suggestion>,
    receipts: ReceiptLedger,
    id_gen: DeterministicIdGen,
    clock: FrozenClock,
    revision_counter: u64,
}

impl ReferenceInterpreter {
    pub fn new(seed: u128, start_millis: u64) -> Self {
        Self {
            store_generation: StoreGeneration::FIRST,
            memories: HashMap::new(),
            aliases: HashMap::new(),
            guides: HashMap::new(),
            sessions: HashMap::new(),
            relations: Vec::new(),
            feedback: Vec::new(),
            suggestions: Vec::new(),
            receipts: ReceiptLedger::new(),
            id_gen: DeterministicIdGen::new(seed),
            clock: FrozenClock::new(start_millis),
            revision_counter: 0,
        }
    }

    fn next_revision(&mut self) -> EntityRevision {
        self.revision_counter += 1;
        EntityRevision::new(self.revision_counter)
    }

    pub fn lookup_receipt(
        &self,
        generation: StoreGeneration,
        op: OperationId,
    ) -> Option<&CommandReceipt> {
        self.receipts.get(&(generation, op))
    }

    pub fn apply(
        &mut self,
        ctx: &CommandContext,
        cmd: &DomainCommand,
    ) -> DomainResult<CommandReceipt> {
        let key = (ctx.store_generation, ctx.operation_id);
        if let Some(existing) = self.receipts.get(&key) {
            if existing.request_digest == ctx.request_digest {
                return Ok(existing.clone());
            }
            return Err(DomainError::new(
                DomainErrorCode::KeyReuseDifferentInput,
                "operation key reused with different input",
            ));
        }

        let receipt = self.apply_inner(ctx, cmd)?;
        self.receipts.insert(key, receipt.clone());
        Ok(receipt)
    }

    fn apply_inner(
        &mut self,
        ctx: &CommandContext,
        cmd: &DomainCommand,
    ) -> DomainResult<CommandReceipt> {
        let outcome = match cmd {
            DomainCommand::AddMemory { memory, .. } => self.apply_add_memory(memory)?,
            DomainCommand::UpdateMemory {
                id,
                expected_revision,
                patch,
            } => self.apply_update_memory(*id, *expected_revision, patch)?,
            DomainCommand::Feedback { memory_id, useful } => {
                self.apply_feedback(*memory_id, *useful)?
            }
            DomainCommand::Relate { relation } => self.apply_relate(relation)?,
            DomainCommand::Unrelate {
                source,
                target,
                relation_type,
            } => self.apply_unrelate(*source, *target, *relation_type)?,
            DomainCommand::Merge { source_ids, result } => self.apply_merge(source_ids, result)?,
            DomainCommand::Forget { id, mode } => self.apply_forget(*id, *mode)?,
            DomainCommand::EndSession {
                session,
                outcome,
                final_approach,
                lessons,
            } => self.apply_end_session(session, outcome, final_approach, lessons)?,
            DomainCommand::GuidePractice {
                guide,
                category,
                contexts,
                learnings,
                outcome,
            } => self.apply_guide_practice(guide, category, contexts, learnings, *outcome)?,
            DomainCommand::GuideMerge {
                source_names,
                result,
            } => self.apply_guide_merge(source_names, result)?,
            DomainCommand::GuideForget { name } => self.apply_guide_forget(name)?,
        };

        Ok(CommandReceipt {
            operation_id: ctx.operation_id,
            store_generation: ctx.store_generation,
            frontend_id: ctx.frontend_id,
            channel_id: ctx.channel_id,
            request_digest: ctx.request_digest.clone(),
            outcome,
        })
    }

    fn apply_add_memory(&mut self, memory: &Memory) -> DomainResult<ReceiptOutcome> {
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
        Ok(ReceiptOutcome::Success {
            affected: vec![memory.id],
        })
    }

    fn apply_update_memory(
        &mut self,
        id: EntityId,
        expected_revision: Option<EntityRevision>,
        patch: &MemoryPatch,
    ) -> DomainResult<ReceiptOutcome> {
        let memory = self
            .memories
            .get_mut(&id)
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

        Ok(ReceiptOutcome::Success { affected: vec![id] })
    }

    fn apply_feedback(
        &mut self,
        memory_id: EntityId,
        useful: bool,
    ) -> DomainResult<ReceiptOutcome> {
        let memory = self
            .memories
            .get_mut(&memory_id)
            .ok_or_else(|| DomainError::new(DomainErrorCode::NotFound, "memory not found"))?;

        if useful {
            memory.positive_feedback += 1;
            memory.confidence = (memory.confidence + 0.01).min(1.0);
        } else {
            memory.negative_feedback += 1;
            memory.confidence = (memory.confidence - 0.05).max(0.0);
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

    fn apply_relate(&mut self, relation: &Relation) -> DomainResult<ReceiptOutcome> {
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

    fn apply_unrelate(
        &mut self,
        source: EntityId,
        target: EntityId,
        relation_type: crate::domain::relation::RelationType,
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

    fn apply_merge(
        &mut self,
        source_ids: &[EntityId],
        result: &Memory,
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

        let mut affected = Vec::new();
        for source_id in source_ids {
            if let Some(source) = self.memories.get_mut(source_id) {
                source.lifecycle = MemoryLifecycle::Archived {
                    at: Instant::new(self.clock.now_millis()),
                };
                source.advance_eligibility();
                affected.push(*source_id);
            }
        }

        let mut result = result.clone();
        result.entity_revision = self.next_revision();
        self.memories.insert(result.id, result.clone());
        affected.push(result.id);

        Ok(ReceiptOutcome::Success { affected })
    }

    fn apply_forget(&mut self, id: EntityId, mode: ForgetMode) -> DomainResult<ReceiptOutcome> {
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

    fn apply_end_session(
        &mut self,
        session: &SessionHandle,
        outcome: &TaskOutcome,
        final_approach: &Option<String>,
        lessons: &[String],
    ) -> DomainResult<ReceiptOutcome> {
        let session = self
            .sessions
            .get_mut(session)
            .ok_or_else(|| DomainError::new(DomainErrorCode::NotFound, "session not found"))?;

        if !session.can_end() {
            return Err(DomainError::new(
                DomainErrorCode::InvalidLifecycleTransition,
                "session already ended",
            ));
        }

        session.status = SessionStatus::Ended;
        session.outcome = Some(*outcome);
        session.final_approach = final_approach.clone();
        session.lessons = lessons.to_vec();
        session.ended_at = Some(Instant::new(self.clock.now_millis()));

        Ok(ReceiptOutcome::Success { affected: vec![] })
    }

    fn apply_guide_practice(
        &mut self,
        guide: &str,
        category: &str,
        contexts: &[String],
        learnings: &[String],
        outcome: Option<bool>,
    ) -> DomainResult<ReceiptOutcome> {
        let now = self.clock.now_millis();
        let entry = self
            .guides
            .entry(guide.to_string())
            .or_insert_with(|| Guide {
                name: guide.to_string(),
                category: category.to_string(),
                description: String::new(),
                contexts: Vec::new(),
                learnings: Vec::new(),
                usage_count: 0,
                last_used: None,
                success_count: 0,
                failure_count: 0,
                anti_patterns: Vec::new(),
                pitfalls: Vec::new(),
                depends_on: Vec::new(),
                enables: Vec::new(),
                source_memories: Vec::new(),
                validated_by: Vec::new(),
                superseded_by: None,
                deprecated: false,
                entity_revision: EntityRevision::new(1),
                created_at: Instant::new(now),
                updated_at: Instant::new(now),
            });

        entry.usage_count += 1;
        entry.last_used = Some(Instant::new(now));
        if let Some(success) = outcome {
            if success {
                entry.success_count += 1;
            } else {
                entry.failure_count += 1;
            }
        }
        for ctx in contexts {
            if !entry.contexts.contains(ctx) {
                entry.contexts.push(ctx.clone());
            }
        }
        for learning in learnings {
            if !entry.learnings.contains(learning) {
                entry.learnings.push(learning.clone());
            }
        }
        entry.updated_at = Instant::new(now);
        entry.entity_revision = entry.entity_revision.next();

        Ok(ReceiptOutcome::Success { affected: vec![] })
    }

    fn apply_guide_merge(
        &mut self,
        source_names: &[String],
        result: &Guide,
    ) -> DomainResult<ReceiptOutcome> {
        for name in source_names {
            if !self.guides.contains_key(name) {
                return Err(DomainError::new(
                    DomainErrorCode::NotFound,
                    "guide not found",
                ));
            }
        }
        if self.guides.contains_key(&result.name) {
            return Err(DomainError::new(
                DomainErrorCode::Validation,
                "guide already exists",
            ));
        }

        self.guides.insert(result.name.clone(), result.clone());
        Ok(ReceiptOutcome::Success { affected: vec![] })
    }

    fn apply_guide_forget(&mut self, name: &str) -> DomainResult<ReceiptOutcome> {
        if !self.guides.contains_key(name) {
            return Err(DomainError::new(
                DomainErrorCode::NotFound,
                "guide not found",
            ));
        }
        self.guides.remove(name);
        Ok(ReceiptOutcome::Success { affected: vec![] })
    }

    pub fn register_session(
        &mut self,
        handle: SessionHandle,
        channel_id: crate::domain::id::ChannelId,
        task_type: Option<String>,
        project: Option<String>,
    ) {
        let now = self.clock.now_millis();
        self.sessions.insert(
            handle,
            Session {
                handle,
                channel_id,
                project,
                task_type,
                status: SessionStatus::Active,
                attempts: Vec::new(),
                outcome: None,
                final_approach: None,
                lessons: Vec::new(),
                started_at: Instant::new(now),
                ended_at: None,
            },
        );
    }

    pub fn allocate_alias(&mut self, preferred: &str) -> ExternalAlias {
        let mut candidate = preferred.to_string();
        let mut counter = 1;
        while self
            .aliases
            .contains_key(&ExternalAlias::new(candidate.clone()))
        {
            counter += 1;
            candidate = format!("{preferred}-{counter}");
        }
        ExternalAlias::new(candidate)
    }

    pub fn export(&self) -> CanonicalExport {
        let mut export = CanonicalExport {
            memories: self.memories.values().cloned().collect(),
            relations: self.relations.clone(),
            guides: self.guides.values().cloned().collect(),
            sessions: self.sessions.values().cloned().collect(),
            feedback: self.feedback.clone(),
            suggestions: self.suggestions.clone(),
            projects: vec![],
            archives: vec![],
            history: vec![],
            unknown_fields: std::collections::BTreeMap::new(),
        };
        export.normalize();
        export
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::command::{CommandContext, Scope};
    use crate::domain::id::{ChannelId, EntityRevision, FrontendId};
    use crate::domain::memory::{FragmentType, MemorySource};
    use uuid::Uuid;

    fn test_memory(id_num: u64) -> Memory {
        Memory {
            id: EntityId::new(Uuid::from_u128(id_num as u128)),
            external_alias: None,
            title: format!("Memory {id_num}"),
            fragment: format!("Fragment {id_num}"),
            description: String::new(),
            fragment_type: FragmentType::Fact,
            project: None,
            source: MemorySource::Ai,
            confidence: 0.5,
            quality_score: None,
            lifecycle: MemoryLifecycle::Live,
            tags: vec![],
            associated_with: vec![],
            relations: vec![],
            parent_id: None,
            child_ids: vec![],
            session_id: None,
            task_type: None,
            related_guides: vec![],
            evidence: vec![],
            access_count: 0,
            last_accessed_at: None,
            positive_feedback: 0,
            negative_feedback: 0,
            negative_hits: 0,
            refinement_count: 0,
            distill_candidate: false,
            entity_revision: EntityRevision::new(0),
            document_revision: crate::domain::id::DocumentRevision::new(1),
            eligibility_revision: crate::domain::id::EligibilityRevision::new(1),
            created_at: Instant::new(0),
            updated_at: Instant::new(0),
            raw_created: None,
            unknown_fields: std::collections::BTreeMap::new(),
        }
    }

    fn test_ctx(op_num: u64) -> CommandContext {
        CommandContext {
            store_generation: StoreGeneration::FIRST,
            frontend_id: FrontendId::new(Uuid::from_u128(1)),
            channel_id: ChannelId::new(Uuid::from_u128(2)),
            session: None,
            operation_id: OperationId::new(Uuid::from_u128(op_num as u128)),
            request_digest: format!("digest-{op_num}"),
            deadline_millis: None,
            scope: Scope::default(),
        }
    }

    #[test]
    fn test_add_memory() {
        let mut interp = ReferenceInterpreter::new(42, 0);
        let ctx = test_ctx(1);
        let memory = test_memory(100);
        let cmd = DomainCommand::AddMemory {
            memory: memory.clone(),
            session: None,
        };
        let receipt = interp.apply(&ctx, &cmd).unwrap();
        assert!(matches!(receipt.outcome, ReceiptOutcome::Success { .. }));
    }

    #[test]
    fn test_idempotent_replay() {
        let mut interp = ReferenceInterpreter::new(42, 0);
        let ctx = test_ctx(1);
        let memory = test_memory(100);
        let cmd = DomainCommand::AddMemory {
            memory,
            session: None,
        };
        let receipt1 = interp.apply(&ctx, &cmd.clone()).unwrap();
        let receipt2 = interp.apply(&ctx, &cmd).unwrap();
        assert_eq!(
            receipt1.operation_id, receipt2.operation_id,
            "idempotent replay returns same operation"
        );
    }

    #[test]
    fn test_revision_conflict() {
        let mut interp = ReferenceInterpreter::new(42, 0);
        let ctx1 = test_ctx(1);
        let memory = test_memory(100);
        let cmd = DomainCommand::AddMemory {
            memory,
            session: None,
        };
        let receipt = interp.apply(&ctx1, &cmd).unwrap();
        let affected = match receipt.outcome {
            ReceiptOutcome::Success { affected } => affected,
            _ => vec![],
        };
        let id = affected[0];

        let ctx2 = test_ctx(2);
        let stale_cmd = DomainCommand::UpdateMemory {
            id,
            expected_revision: Some(EntityRevision::new(999)),
            patch: MemoryPatch::default(),
        };
        let result = interp.apply(&ctx2, &stale_cmd);
        assert!(result.is_err());
    }

    #[test]
    fn test_supersession_cycle_rejected() {
        let mut interp = ReferenceInterpreter::new(42, 0);
        let m1 = test_memory(1);
        let m2 = test_memory(2);
        interp
            .apply(
                &test_ctx(1),
                &DomainCommand::AddMemory {
                    memory: m1.clone(),
                    session: None,
                },
            )
            .unwrap();
        interp
            .apply(
                &test_ctx(2),
                &DomainCommand::AddMemory {
                    memory: m2.clone(),
                    session: None,
                },
            )
            .unwrap();

        let rel1 = crate::domain::relation::Relation::new(
            EntityId::new(Uuid::from_u128(100)),
            m1.id,
            m2.id,
            crate::domain::relation::RelationType::Supersedes,
            None,
            Instant::new(0),
        );
        interp
            .apply(&test_ctx(3), &DomainCommand::Relate { relation: rel1 })
            .unwrap();

        let rel2 = crate::domain::relation::Relation::new(
            EntityId::new(Uuid::from_u128(101)),
            m2.id,
            m1.id,
            crate::domain::relation::RelationType::Supersedes,
            None,
            Instant::new(0),
        );
        let result = interp.apply(&test_ctx(4), &DomainCommand::Relate { relation: rel2 });
        assert!(result.is_err());
    }

    #[test]
    fn test_merge_archives_sources() {
        let mut interp = ReferenceInterpreter::new(42, 0);
        let m1 = test_memory(1);
        let m2 = test_memory(2);
        let m3 = test_memory(3);
        interp
            .apply(
                &test_ctx(1),
                &DomainCommand::AddMemory {
                    memory: m1.clone(),
                    session: None,
                },
            )
            .unwrap();
        interp
            .apply(
                &test_ctx(2),
                &DomainCommand::AddMemory {
                    memory: m2.clone(),
                    session: None,
                },
            )
            .unwrap();

        let cmd = DomainCommand::Merge {
            source_ids: vec![m1.id, m2.id],
            result: m3.clone(),
        };
        let receipt = interp.apply(&test_ctx(3), &cmd).unwrap();
        let affected = match receipt.outcome {
            ReceiptOutcome::Success { affected } => affected,
            _ => vec![],
        };
        assert_eq!(affected.len(), 3);
    }

    #[test]
    fn test_feedback_updates_counters() {
        let mut interp = ReferenceInterpreter::new(42, 0);
        let m1 = test_memory(1);
        interp
            .apply(
                &test_ctx(1),
                &DomainCommand::AddMemory {
                    memory: m1.clone(),
                    session: None,
                },
            )
            .unwrap();

        interp
            .apply(
                &test_ctx(2),
                &DomainCommand::Feedback {
                    memory_id: m1.id,
                    useful: true,
                },
            )
            .unwrap();
        interp
            .apply(
                &test_ctx(3),
                &DomainCommand::Feedback {
                    memory_id: m1.id,
                    useful: false,
                },
            )
            .unwrap();

        assert_eq!(interp.feedback.len(), 2);
    }

    #[test]
    fn test_forget_modes() {
        let mut interp = ReferenceInterpreter::new(42, 0);
        let m1 = test_memory(1);
        interp
            .apply(
                &test_ctx(1),
                &DomainCommand::AddMemory {
                    memory: m1.clone(),
                    session: None,
                },
            )
            .unwrap();

        interp
            .apply(
                &test_ctx(2),
                &DomainCommand::Forget {
                    id: m1.id,
                    mode: ForgetMode::Invalidate,
                },
            )
            .unwrap();

        let export = interp.export();
        let memory = export.memories.iter().find(|m| m.id == m1.id).unwrap();
        assert!(!memory.lifecycle.is_recallable());
    }

    #[test]
    fn test_duplicate_memory_rejected() {
        let mut interp = ReferenceInterpreter::new(42, 0);
        let m1 = test_memory(1);
        interp
            .apply(
                &test_ctx(1),
                &DomainCommand::AddMemory {
                    memory: m1.clone(),
                    session: None,
                },
            )
            .unwrap();

        let result = interp.apply(
            &test_ctx(2),
            &DomainCommand::AddMemory {
                memory: m1,
                session: None,
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_key_reuse_different_input() {
        let mut interp = ReferenceInterpreter::new(42, 0);
        let ctx1 = test_ctx(1);
        let m1 = test_memory(1);
        interp
            .apply(
                &ctx1,
                &DomainCommand::AddMemory {
                    memory: m1,
                    session: None,
                },
            )
            .unwrap();

        let mut ctx2 = test_ctx(1);
        ctx2.request_digest = "different-digest".to_string();
        let m2 = test_memory(2);
        let result = interp.apply(
            &ctx2,
            &DomainCommand::AddMemory {
                memory: m2,
                session: None,
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_export_digest_stable() {
        let mut interp = ReferenceInterpreter::new(42, 0);
        let m1 = test_memory(1);
        interp
            .apply(
                &test_ctx(1),
                &DomainCommand::AddMemory {
                    memory: m1,
                    session: None,
                },
            )
            .unwrap();

        let export1 = interp.export();
        let digest1 = export1.digest();
        let export2 = interp.export();
        let digest2 = export2.digest();
        assert_eq!(digest1, digest2);
    }

    #[test]
    fn test_session_lifecycle() {
        let mut interp = ReferenceInterpreter::new(42, 0);
        let handle = SessionHandle::new(Uuid::from_u128(1000));
        interp.register_session(
            handle,
            ChannelId::new(Uuid::from_u128(2)),
            Some("debugging".to_string()),
            None,
        );

        let ctx = test_ctx(1);
        let cmd = DomainCommand::EndSession {
            session: handle,
            outcome: TaskOutcome::Success,
            final_approach: Some("fixed the bug".to_string()),
            lessons: vec!["always check logs".to_string()],
        };
        let receipt = interp.apply(&ctx, &cmd).unwrap();
        assert!(matches!(receipt.outcome, ReceiptOutcome::Success { .. }));
    }

    #[test]
    fn test_guide_practice() {
        let mut interp = ReferenceInterpreter::new(42, 0);
        let ctx = test_ctx(1);
        let cmd = DomainCommand::GuidePractice {
            guide: "react".to_string(),
            category: "web-frontend".to_string(),
            contexts: vec!["hooks".to_string()],
            learnings: vec!["useCallback prevents re-renders".to_string()],
            outcome: Some(true),
        };
        let receipt = interp.apply(&ctx, &cmd).unwrap();
        assert!(matches!(receipt.outcome, ReceiptOutcome::Success { .. }));
    }

    #[test]
    fn test_alias_allocation_collision() {
        let mut interp = ReferenceInterpreter::new(42, 0);
        let mut m1 = test_memory(1);
        m1.external_alias = Some(ExternalAlias::new("abc123"));
        interp
            .apply(
                &test_ctx(1),
                &DomainCommand::AddMemory {
                    memory: m1,
                    session: None,
                },
            )
            .unwrap();

        let alias = interp.allocate_alias("abc123");
        assert_ne!(alias.as_str(), "abc123");
    }
}
