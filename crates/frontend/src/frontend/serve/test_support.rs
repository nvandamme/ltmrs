//! Serve test helpers (moved verbatim from `serve.rs`).

use super::super::mcp::FrontendIdentity;
use ltmrs_domain::id::{ChannelId, FrontendId};
use uuid::Uuid;

pub(crate) fn test_identity() -> FrontendIdentity {
    FrontendIdentity::new(
        FrontendId::new(Uuid::from_u128(1)),
        ChannelId::new(Uuid::from_u128(2)),
    )
}

pub(crate) fn test_memory() -> ltmrs_domain::memory::Memory {
    use ltmrs_domain::id::{DocumentRevision, EligibilityRevision, EntityId, EntityRevision};
    use ltmrs_domain::memory::Instant;
    use ltmrs_domain::memory::{FragmentType, Memory, MemoryLifecycle, MemorySource};
    Memory {
        id: EntityId::new(Uuid::from_u128(42)),
        external_alias: None,
        title: "stdio-bridge".into(),
        fragment: "bridged write".into(),
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
