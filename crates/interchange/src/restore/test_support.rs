//! Restore test helpers (moved verbatim from `restore.rs`).

use std::sync::Arc;

use ltmrs_domain::id::EntityId;
use ltmrs_domain::memory::{FragmentType, Instant, Memory, MemoryLifecycle, MemorySource};
use ltmrs_service::repository::CanonicalRepository;

pub(crate) fn test_memory(id_num: u128, title: &str) -> Memory {
    use ltmrs_domain::id::{DocumentRevision, EligibilityRevision, EntityRevision};
    Memory {
        id: EntityId::new(uuid::Uuid::from_u128(id_num)),
        external_alias: None,
        title: title.into(),
        fragment: format!("{title} body text."),
        description: String::new(),
        fragment_type: FragmentType::Fact,
        project: None,
        source: MemorySource::Ai,
        confidence: 1.0,
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
        entity_revision: EntityRevision::new(1),
        document_revision: DocumentRevision::new(1),
        eligibility_revision: EligibilityRevision::new(1),
        created_at: Instant::new(1000),
        updated_at: Instant::new(1000),
        raw_created: None,
        unknown_fields: std::collections::BTreeMap::new(),
    }
}

pub(crate) fn open_repo(dir: &tempfile::TempDir) -> Arc<CanonicalRepository> {
    Arc::new(CanonicalRepository::open(dir.path().join("store").to_str().unwrap()).unwrap())
}

/// Gateway scaffolding: frozen clock + issued namespace so `apply`
/// writes receipts, aliases and projection jobs like production.
pub(crate) fn gateway_repo(dir: &tempfile::TempDir) -> Arc<CanonicalRepository> {
    use ltmrs_domain::clock::FrozenClock;
    let clock = std::sync::Arc::new(FrozenClock::new(1000));
    let repo =
        CanonicalRepository::open_with_clock(dir.path().join("store").to_str().unwrap(), clock)
            .unwrap();
    repo.issue_namespace(
        ltmrs_domain::id::FrontendId::new(uuid::Uuid::from_u128(1)),
        ltmrs_domain::id::ChannelId::new(uuid::Uuid::from_u128(2)),
        1000,
    )
    .unwrap();
    Arc::new(repo)
}

pub(crate) fn gateway_ctx(op_num: u64) -> ltmrs_domain::command::CommandContext {
    gateway_ctx_gen(op_num, ltmrs_domain::id::StoreGeneration::FIRST)
}

/// Context under an explicit generation: post-restore writes name the
/// live generation (like a re-handshaked client); the transaction
/// fence rejects retired generations.
pub(crate) fn gateway_ctx_gen(
    op_num: u64,
    generation: ltmrs_domain::id::StoreGeneration,
) -> ltmrs_domain::command::CommandContext {
    use ltmrs_domain::id::{ChannelId, FrontendId, OperationId};
    ltmrs_domain::command::CommandContext {
        store_generation: generation,
        frontend_id: FrontendId::new(uuid::Uuid::from_u128(1)),
        channel_id: ChannelId::new(uuid::Uuid::from_u128(2)),
        session: None,
        operation_id: OperationId::new(uuid::Uuid::from_u128(op_num as u128)),
        request_digest: format!("restore-test-{op_num}"),
        deadline_millis: None,
        scope: Default::default(),
        retry_epoch: 1,
    }
}

/// Operation scope for the session-channel tests below (channel 9,
/// second namespace): op ids derive deterministically from their
/// strings, so distinct test ops never share a key.
pub(crate) fn gateway_scope(op: &str, digest: &str) -> ltmrs_domain::command::OperationScope {
    ltmrs_domain::command::OperationScope {
        store_generation: ltmrs_domain::id::StoreGeneration::FIRST,
        frontend_id: ltmrs_domain::id::FrontendId::new(uuid::Uuid::from_u128(1)),
        channel_id: ltmrs_domain::id::ChannelId::new(uuid::Uuid::from_u128(9)),
        retry_epoch: 2,
        operation_id: ltmrs_domain::id::OperationId::new(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            op.as_bytes(),
        )),
        request_digest: digest.to_string(),
    }
}

/// Issue the channel-9 namespace the session tests run under.
pub(crate) fn issue_session_channel(repo: &CanonicalRepository) {
    repo.issue_namespace(
        ltmrs_domain::id::FrontendId::new(uuid::Uuid::from_u128(1)),
        ltmrs_domain::id::ChannelId::new(uuid::Uuid::from_u128(9)),
        1000,
    )
    .unwrap();
}

/// Post-restore scope: the drain resets the epoch counter, so the
/// re-issued channel-9 namespace is epoch 1 under generation 2 — a
/// different key from every pre-restore receipt by construction.
pub(crate) fn gateway_scope_post(op: &str, digest: &str) -> ltmrs_domain::command::OperationScope {
    let mut scope = gateway_scope(op, digest);
    scope.store_generation = ltmrs_domain::id::StoreGeneration::new(2);
    scope.retry_epoch = 1;
    scope
}
