//! Interpreter registry/export (moved verbatim from `interpreter.rs`).

use super::ReferenceInterpreter;
use crate::export::CanonicalExport;
use crate::id::{ExternalAlias, SessionHandle};
use crate::memory::Instant;
use crate::session::{Session, SessionStatus};

impl ReferenceInterpreter {
    pub fn register_session(
        &mut self,
        handle: SessionHandle,
        channel_id: crate::id::ChannelId,
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
                technologies: Vec::new(),
                status: SessionStatus::Active,
                attempts: Vec::new(),
                outcome: None,
                final_approach: None,
                lessons: Vec::new(),
                initial_approach: None,
                guides_used: Vec::new(),
                memories_read: Vec::new(),
                memories_created: Vec::new(),
                refinement_attempts: 0,
                self_critique_count: 0,
                started_at: Instant::new(now),
                ended_at: None,
                is_virtual: false,
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
