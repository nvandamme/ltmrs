//! Shared repository test helpers (moved verbatim from `repository.rs`).

use super::{AdmittedScope, CanonicalRepository};
use ltmrs_domain::command::{CommandContext, OperationScope};
use ltmrs_domain::guide::Guide;
use ltmrs_domain::id::{ChannelId, EntityId, OperationId, StoreGeneration};
use ltmrs_domain::memory::{FragmentType, Memory, MemoryLifecycle, MemorySource};
use ltmrs_domain::relation::{Relation, RelationType};
use uuid::Uuid;

pub(crate) fn eid(n: u64) -> EntityId {
    EntityId::new(Uuid::from_u128(n as u128))
}

pub(crate) fn memory(id: EntityId, title: &str) -> Memory {
    Memory {
        id,
        external_alias: None,
        title: title.to_string(),
        fragment: format!("frag-{title}"),
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
        entity_revision: ltmrs_domain::id::EntityRevision::new(1),
        document_revision: ltmrs_domain::id::DocumentRevision::new(1),
        eligibility_revision: ltmrs_domain::id::EligibilityRevision::new(1),
        created_at: ltmrs_domain::memory::Instant::new(1),
        updated_at: ltmrs_domain::memory::Instant::new(1),
        raw_created: None,
        unknown_fields: std::collections::BTreeMap::new(),
    }
}

pub(crate) fn ctx(op_num: u64, digest: &str) -> CommandContext {
    CommandContext {
        store_generation: StoreGeneration::FIRST,
        frontend_id: ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1)),
        channel_id: ltmrs_domain::id::ChannelId::new(Uuid::from_u128(2)),
        session: None,
        operation_id: OperationId::new(Uuid::from_u128(op_num as u128)),
        request_digest: digest.to_string(),
        deadline_millis: None,
        scope: Default::default(),
        retry_epoch: 1,
    }
}

pub(crate) fn ch(n: u64) -> ChannelId {
    ChannelId::new(Uuid::from_u128(n as u128))
}

/// Operation scope for frontend 1 / channel 2 / epoch 1 (the
/// `repo_with_ns` namespace): every direct-primitive test defaults here.
pub(crate) fn scope(op_num: u64, digest: &str) -> OperationScope {
    scope_in(1, 2, op_num, digest)
}

/// Admit a scope for continuation-path tests (validates + pins).
pub(crate) fn admit(repo: &CanonicalRepository, op_num: u64, digest: &str) -> AdmittedScope {
    repo.admit_scope(&scope(op_num, digest)).unwrap()
}

pub(crate) fn scope_in(epoch: u64, ch_n: u64, op_num: u64, digest: &str) -> OperationScope {
    OperationScope {
        store_generation: StoreGeneration::FIRST,
        frontend_id: ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1)),
        channel_id: ch(ch_n),
        retry_epoch: epoch,
        operation_id: OperationId::new(Uuid::from_u128(op_num as u128)),
        request_digest: digest.to_string(),
    }
}

/// Open a repo and issue a namespace for frontend 1 at epoch 1.
pub(crate) fn repo_with_ns() -> (CanonicalRepository, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    // Frozen clock at 1000 so namespace validity checks are deterministic
    // and consistent with the issue_namespace time below.
    let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
    let repo = CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), clock).unwrap();
    let ns = repo
        .issue_namespace(
            ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1)),
            ch(2),
            1000,
        )
        .unwrap();
    assert_eq!(ns.retry_epoch, 1, "first namespace is epoch 1");
    (repo, dir)
}

pub(crate) fn rel(id: EntityId, s: EntityId, t: EntityId, ty: RelationType) -> Relation {
    Relation::new(id, s, t, ty, None, ltmrs_domain::memory::Instant::new(1))
}
pub(crate) fn test_guide(name: &str) -> Guide {
    use ltmrs_domain::memory::Instant;
    ltmrs_domain::guide::Guide {
        name: name.into(),
        category: "dev-tool".into(),
        description: String::new(),
        contexts: vec![],
        learnings: vec![],
        usage_count: 0,
        last_used: None,
        success_count: 0,
        failure_count: 0,
        anti_patterns: vec![],
        pitfalls: vec![],
        depends_on: vec![],
        enables: vec![],
        source_memories: vec![],
        validated_by: vec![],
        superseded_by: None,
        deprecated: false,
        entity_revision: ltmrs_domain::id::EntityRevision::new(1),
        created_at: Instant::new(0),
        updated_at: Instant::new(0),
    }
}
