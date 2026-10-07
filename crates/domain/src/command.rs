//! Core domain command types: commands, context, receipts, errors, scope,
//! and snapshot tokens.

use std::collections::BTreeMap;

use crate::clock::Clock;
use crate::guide::Guide;
use crate::id::{
    ChannelId, EntityId, EntityRevision, FrontendId, OperationId, SessionHandle, StoreGeneration,
};
use crate::memory::{Evidence, FragmentType, Memory, MemoryLifecycle};
use crate::relation::{Relation, RelationType};
use crate::session::TaskOutcome;

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
        /// When true, the merge records result→source Supersedes edges in
        /// the same transaction (consolidated supersession): the sources
        /// archive in that transaction, so the edges cannot be created
        /// afterwards (archived endpoints fail edge validation).
        consolidate: bool,
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
    /// Boost confidence for pre-loaded memories (upstream boostConfidence):
    /// increments `access_count`, updates `last_accessed_at`, boosts
    /// confidence by 0.02 (capped at 1.0). No context tag.
    BoostConfidence {
        memory_ids: Vec<EntityId>,
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
                at: crate::memory::Instant(0),
            },
            ForgetMode::Invalidate => MemoryLifecycle::Invalidated {
                at: crate::memory::Instant(0),
            },
            ForgetMode::Archive => MemoryLifecycle::Archived {
                at: crate::memory::Instant(0),
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

/// The identity of one mutating tool operation (RQ-06): every direct
/// guide/session/suggestion primitive takes this instead of a bare
/// `(operation_id, digest)`, so namespace validation, receipt scoping
/// and watermark sharding cannot be forgotten per call site. Derivable
/// from an [`crate::id`] envelope plus its request digest.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OperationScope {
    pub store_generation: StoreGeneration,
    pub frontend_id: FrontendId,
    pub channel_id: ChannelId,
    pub retry_epoch: u64,
    pub operation_id: OperationId,
    pub request_digest: String,
}

impl OperationScope {
    /// The receipt key for this operation, shaped exactly like canonical
    /// command receipts: `generation:frontend:epoch:operation`. Two
    /// channels (or generations) never share a key, so a cross-channel
    /// retry executes in its own scope instead of replaying another
    /// channel's receipt.
    pub fn op_key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.store_generation.as_u64(),
            self.frontend_id.as_uuid(),
            self.retry_epoch,
            self.operation_id.as_uuid()
        )
    }
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

/// A daemon-issued retry namespace for one authenticated channel.
///
/// Each namespace has a fixed expiry, separate from the renewable
/// channel/session binding. Receipts remain until the namespace expires.
/// Reconnecting can renew a channel but cannot extend an old retry namespace
/// or transplant its pending operations into a new one — and a sibling
/// channel can never resume or replay it. Expired, unknown, or
/// cross-channel namespaces are refused as stale rather than silently
/// converted into new work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RetryNamespace {
    pub frontend_id: FrontendId,
    /// The channel this namespace was issued to: the agent/session
    /// isolation boundary. Resume, validation and receipt replay all
    /// require the caller's channel to match.
    pub channel_id: ChannelId,
    pub retry_epoch: u64,
    /// Issued timestamp in milliseconds.
    pub issued_at: u64,
    /// Fixed expiry in milliseconds (issued_at + namespace TTL).
    pub expires_at: u64,
}

impl RetryNamespace {
    /// Create a new namespace with the given TTL.
    pub fn new(
        frontend_id: FrontendId,
        channel_id: ChannelId,
        retry_epoch: u64,
        issued_at: u64,
        ttl_millis: u64,
    ) -> Self {
        Self {
            frontend_id,
            channel_id,
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
    StaleGeneration,
    /// Transient write contention (SSI conflict budget exhausted): safe to
    /// retry, never a validation of the request itself. Kept distinct from
    /// Validation so callers (handshake) can retry instead of refusing.
    Contention,
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
            DomainErrorCode::StaleGeneration => "stale_generation",
            DomainErrorCode::Contention => "contention",
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
            "stale_generation" => DomainErrorCode::StaleGeneration,
            "contention" => DomainErrorCode::Contention,
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
    use crate::memory::MemorySource;

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
                at: crate::memory::Instant(0)
            }
        );
        assert_ne!(
            ForgetMode::Invalidate.lifecycle(),
            ForgetMode::Archive.lifecycle()
        );
        assert_eq!(MemorySource::parse("ai"), Some(MemorySource::Ai));
    }
}
