//! Server test helpers (moved verbatim from `server.rs`).

use std::sync::Arc;

use ltmrs_domain::clock::{Clock, FrozenClock};
use ltmrs_domain::id::{
    ChannelId, DocumentRevision, EligibilityRevision, EntityId, EntityRevision, FrontendId,
    OperationId, StoreGeneration,
};
use ltmrs_domain::memory::{FragmentType, Instant, Memory, MemoryLifecycle, MemorySource};
use uuid::Uuid;

/// Test dispatcher with explicit quotas (bypasses Daemon::start, which
/// always uses the configured limits).
pub(crate) fn test_dispatcher_with_quotas(
    dir: &tempfile::TempDir,
    quotas: std::sync::Arc<crate::limits::QuotaTracker>,
) -> (
    std::sync::Arc<crate::dispatcher::Dispatcher>,
    std::sync::Arc<crate::limits::QuotaTracker>,
) {
    use crate::dispatcher::Dispatcher;
    use crate::registry::FrontendRegistry;
    use ltmrs_service::repository::CanonicalRepository;

    let clock: Arc<dyn Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
    let repo = Arc::new(
        CanonicalRepository::open_with_clock(
            dir.path().join("store").to_str().unwrap(),
            Arc::clone(&clock),
        )
        .unwrap(),
    );
    repo.issue_namespace(
        FrontendId::new(Uuid::from_u128(1)),
        ChannelId::new(Uuid::from_u128(2)),
        1000,
    )
    .unwrap();
    (
        Arc::new(Dispatcher::new(repo, FrontendRegistry::new(), clock)),
        quotas,
    )
}

pub(crate) fn handshake_as(fe_n: u64) -> crate::envelope::HandshakeRequest {
    crate::envelope::HandshakeRequest {
        protocol_version: crate::envelope::PROTOCOL_VERSION,
        store_generation: StoreGeneration::FIRST,
        frontend_id: FrontendId::new(Uuid::from_u128(fe_n as u128)),
        channel_id: ChannelId::new(Uuid::from_u128(2)),
        resume_retry_epoch: None,
    }
}

pub(crate) fn fe(n: u64) -> FrontendId {
    FrontendId::new(Uuid::from_u128(n as u128))
}
pub(crate) fn ch(n: u64) -> ChannelId {
    ChannelId::new(Uuid::from_u128(n as u128))
}
pub(crate) fn op(n: u64) -> OperationId {
    OperationId::new(Uuid::from_u128(n as u128))
}

pub(crate) fn test_memory(id_num: u64) -> Memory {
    Memory {
        id: EntityId::new(Uuid::from_u128(id_num as u128)),
        external_alias: None,
        title: format!("mem-{id_num}"),
        fragment: format!("frag-{id_num}"),
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
        entity_revision: EntityRevision::new(1),
        document_revision: DocumentRevision::new(1),
        eligibility_revision: EligibilityRevision::new(1),
        created_at: Instant::new(1),
        updated_at: Instant::new(1),
        raw_created: None,
        unknown_fields: std::collections::BTreeMap::new(),
    }
}
