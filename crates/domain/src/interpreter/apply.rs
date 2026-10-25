//! Interpreter apply dispatch (moved verbatim from `interpreter.rs`).

use super::ReferenceInterpreter;
use crate::command::{
    CommandContext, CommandReceipt, DomainCommand, DomainError, DomainErrorCode, DomainResult,
};
use crate::id::{EntityRevision, OperationId, StoreGeneration};

impl ReferenceInterpreter {
    pub(crate) fn next_revision(&mut self) -> EntityRevision {
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
            DomainCommand::AddMemory {
                memory, auto_link, ..
            } => self.apply_add_memory(memory, auto_link.as_ref())?,
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
            DomainCommand::Merge {
                source_ids,
                result,
                consolidate,
            } => self.apply_merge(source_ids, result, *consolidate)?,
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
            DomainCommand::Access {
                memory_ids,
                context,
            } => self.apply_access(memory_ids, context.as_deref())?,
            DomainCommand::BoostConfidence { memory_ids } => {
                self.apply_boost_confidence(memory_ids)?
            }
        };

        Ok(CommandReceipt {
            operation_id: ctx.operation_id,
            store_generation: ctx.store_generation,
            frontend_id: ctx.frontend_id,
            channel_id: ctx.channel_id,
            request_digest: ctx.request_digest.clone(),
            outcome,
            retry_epoch: ctx.retry_epoch,
        })
    }
}
