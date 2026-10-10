//! Interpreter session/guide operations (moved verbatim from `interpreter.rs`).

use super::ReferenceInterpreter;
use crate::command::{DomainError, DomainErrorCode, DomainResult, ReceiptOutcome};
use crate::guide::Guide;
use crate::id::{EntityRevision, SessionHandle};
use crate::memory::Instant;
use crate::session::{SessionStatus, TaskOutcome};

impl ReferenceInterpreter {
    pub(crate) fn apply_end_session(
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

    pub(crate) fn apply_guide_practice(
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

    pub(crate) fn apply_guide_merge(
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

        for name in source_names {
            self.guides.remove(name);
        }
        self.guides.insert(result.name.clone(), result.clone());
        Ok(ReceiptOutcome::Success { affected: vec![] })
    }

    pub(crate) fn apply_guide_forget(&mut self, name: &str) -> DomainResult<ReceiptOutcome> {
        if !self.guides.contains_key(name) {
            return Err(DomainError::new(
                DomainErrorCode::NotFound,
                "guide not found",
            ));
        }
        self.guides.remove(name);
        Ok(ReceiptOutcome::Success { affected: vec![] })
    }
}
