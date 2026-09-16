//! Core domain command types: commands, context, receipts, errors, scope,
//! and snapshot tokens.

use std::collections::BTreeMap;

use crate::domain::clock::Clock;
use crate::domain::guide::Guide;
use crate::domain::id::{
    ChannelId, EntityId, EntityRevision, FrontendId, OperationId, SessionHandle, StoreGeneration,
};
use crate::domain::memory::{Evidence, FragmentType, Memory, MemoryLifecycle};
use crate::domain::relation::{Relation, RelationType};
use crate::domain::session::TaskOutcome;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Scope {
    pub project: Option<String>,
    pub fragment_types: Option<Vec<FragmentType>>,
    pub after: Option<u64>,
    pub before: Option<u64>,
    pub min_confidence: Option<f64>,
    pub all_projects: bool,
}

impl Scope {
    pub fn includes_project(&self, record_project: Option<&str>) -> bool {
        if self.all_projects {
            return true;
        }
        match (&self.project, record_project) {
            (Some(_), None) | (None, None) => true,
            (Some(p), Some(rp)) => p == rp,
            (None, Some(_)) => false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum DomainCommand {
    AddMemory {
        memory: Memory,
        session: Option<SessionHandle>,
    },
    UpdateMemory {
        id: EntityId,
        expected_revision: Option<EntityRevision>,
        patch: MemoryPatch,
    },
    Feedback {
        memory_id: EntityId,
        useful: bool,
    },
    Relate {
        relation: Relation,
    },
    Unrelate {
        source: EntityId,
        target: EntityId,
        relation_type: RelationType,
    },
    Merge {
        source_ids: Vec<EntityId>,
        result: Memory,
    },
    Forget {
        id: EntityId,
        mode: ForgetMode,
    },
    EndSession {
        session: SessionHandle,
        outcome: TaskOutcome,
        final_approach: Option<String>,
        lessons: Vec<String>,
    },
    GuidePractice {
        guide: String,
        category: String,
        contexts: Vec<String>,
        learnings: Vec<String>,
        outcome: Option<bool>,
    },
    GuideMerge {
        source_names: Vec<String>,
        result: Guide,
    },
    GuideForget {
        name: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct MemoryPatch {
    pub title: Option<String>,
    pub fragment: Option<String>,
    pub description: Option<String>,
    pub fragment_type: Option<FragmentType>,
    pub project: Option<Option<String>>,
    pub confidence: Option<f64>,
    pub quality_score: Option<Option<f64>>,
    pub tags: Option<Vec<String>>,
    pub evidence: Option<Vec<Evidence>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgetMode {
    Delete,
    Invalidate,
    Archive,
}

impl ForgetMode {
    pub fn lifecycle(&self) -> MemoryLifecycle {
        match self {
            ForgetMode::Delete => MemoryLifecycle::Deleted {
                at: crate::domain::memory::Instant(0),
            },
            ForgetMode::Invalidate => MemoryLifecycle::Invalidated {
                at: crate::domain::memory::Instant(0),
            },
            ForgetMode::Archive => MemoryLifecycle::Archived {
                at: crate::domain::memory::Instant(0),
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct CommandContext {
    pub store_generation: StoreGeneration,
    pub frontend_id: FrontendId,
    pub channel_id: ChannelId,
    pub session: Option<SessionHandle>,
    pub operation_id: OperationId,
    pub request_digest: String,
    pub deadline_millis: Option<u64>,
    pub scope: Scope,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommandReceipt {
    pub operation_id: OperationId,
    pub store_generation: StoreGeneration,
    pub frontend_id: FrontendId,
    pub channel_id: ChannelId,
    pub request_digest: String,
    pub outcome: ReceiptOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReceiptOutcome {
    Success { affected: Vec<EntityId> },
    Rejected { code: DomainErrorCode },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotToken(u64);

impl SnapshotToken {
    pub fn new(v: u64) -> Self {
        Self(v)
    }
    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DomainErrorCode {
    NotFound,
    RevisionConflict,
    DuplicateAlias,
    DuplicateEdge,
    SelfSupersession,
    SupersessionCycle,
    InvalidLifecycleTransition,
    StaleReplay,
    KeyReuseDifferentInput,
    OutOfScope,
    Validation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainError {
    pub code: DomainErrorCode,
    pub message: String,
}

impl DomainError {
    pub fn new(code: DomainErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

pub type DomainResult<T> = Result<T, DomainError>;

pub type ClockRef = dyn Clock;

pub type ReceiptLedger = BTreeMap<(StoreGeneration, OperationId), CommandReceipt>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::memory::MemorySource;

    #[test]
    fn scope_inheritance() {
        let scope = Scope {
            project: Some("app".into()),
            ..Default::default()
        };
        assert!(scope.includes_project(Some("app")));
        assert!(scope.includes_project(None));
        assert!(!scope.includes_project(Some("other")));

        let global_only = Scope::default();
        assert!(global_only.includes_project(None));
        assert!(!global_only.includes_project(Some("app")));

        let all = Scope {
            all_projects: true,
            ..Default::default()
        };
        assert!(all.includes_project(Some("anything")));
    }

    #[test]
    fn forget_modes_are_distinct() {
        assert_eq!(
            ForgetMode::Delete.lifecycle(),
            MemoryLifecycle::Deleted {
                at: crate::domain::memory::Instant(0)
            }
        );
        assert_ne!(
            ForgetMode::Invalidate.lifecycle(),
            ForgetMode::Archive.lifecycle()
        );
        assert_eq!(MemorySource::parse("ai"), Some(MemorySource::Ai));
    }
}
