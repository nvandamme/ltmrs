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

#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
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
    /// Record a contract-visible read access (RQ-17): increments
    /// `access_count`, updates `last_accessed_at`, boosts confidence by
    /// 0.015 (capped at 1.0), and optionally adds a context tag.
    /// Persisted atomically before the read response reports success.
    Access {
        memory_ids: Vec<EntityId>,
        context: Option<String>,
    },
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// The frontend's retry namespace epoch. The daemon issues each frontend a
    /// retry namespace with a fixed expiry; the frontend retains its operation
    /// ID for IPC retries within that namespace. Reconnecting renews the
    /// channel but cannot extend an old retry namespace.
    pub retry_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommandReceipt {
    pub operation_id: OperationId,
    pub store_generation: StoreGeneration,
    pub frontend_id: FrontendId,
    pub channel_id: ChannelId,
    pub request_digest: String,
    pub outcome: ReceiptOutcome,
    /// The retry namespace epoch under which this operation was recorded.
    pub retry_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReceiptOutcome {
    Success { affected: Vec<EntityId> },
    Rejected { code: DomainErrorCode },
}

/// A daemon-issued retry namespace for an authenticated frontend.
///
/// Each namespace has a fixed expiry, separate from the renewable
/// channel/session binding. Receipts remain until the namespace expires.
/// Reconnecting can renew a channel but cannot extend an old retry namespace
/// or transplant its pending operations into a new one. Expired or
/// resurrected namespaces are refused as stale rather than silently converted
/// into new work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RetryNamespace {
    pub frontend_id: FrontendId,
    pub retry_epoch: u64,
    /// Issued timestamp in milliseconds.
    pub issued_at: u64,
    /// Fixed expiry in milliseconds (issued_at + namespace TTL).
    pub expires_at: u64,
}

impl RetryNamespace {
    /// Create a new namespace with the given TTL.
    pub fn new(frontend_id: FrontendId, retry_epoch: u64, issued_at: u64, ttl_millis: u64) -> Self {
        Self {
            frontend_id,
            retry_epoch,
            issued_at,
            expires_at: issued_at + ttl_millis,
        }
    }

    /// Whether the namespace is still valid at the given time.
    pub fn is_valid_at(&self, now_millis: u64) -> bool {
        now_millis < self.expires_at
    }
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

impl DomainErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            DomainErrorCode::NotFound => "not_found",
            DomainErrorCode::RevisionConflict => "revision_conflict",
            DomainErrorCode::DuplicateAlias => "duplicate_alias",
            DomainErrorCode::DuplicateEdge => "duplicate_edge",
            DomainErrorCode::SelfSupersession => "self_supersession",
            DomainErrorCode::SupersessionCycle => "supersession_cycle",
            DomainErrorCode::InvalidLifecycleTransition => "invalid_lifecycle_transition",
            DomainErrorCode::StaleReplay => "stale_replay",
            DomainErrorCode::KeyReuseDifferentInput => "key_reuse_different_input",
            DomainErrorCode::OutOfScope => "out_of_scope",
            DomainErrorCode::Validation => "validation",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "not_found" => DomainErrorCode::NotFound,
            "revision_conflict" => DomainErrorCode::RevisionConflict,
            "duplicate_alias" => DomainErrorCode::DuplicateAlias,
            "duplicate_edge" => DomainErrorCode::DuplicateEdge,
            "self_supersession" => DomainErrorCode::SelfSupersession,
            "supersession_cycle" => DomainErrorCode::SupersessionCycle,
            "invalid_lifecycle_transition" => DomainErrorCode::InvalidLifecycleTransition,
            "stale_replay" => DomainErrorCode::StaleReplay,
            "key_reuse_different_input" => DomainErrorCode::KeyReuseDifferentInput,
            "out_of_scope" => DomainErrorCode::OutOfScope,
            _ => DomainErrorCode::Validation,
        }
    }
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
