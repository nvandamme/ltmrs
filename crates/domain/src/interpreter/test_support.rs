//! Interpreter test helpers (moved verbatim from `interpreter.rs`).

use crate::command::{CommandContext, Scope};
use crate::id::ExternalAlias;
use crate::id::{ChannelId, EntityId, EntityRevision, FrontendId, OperationId, StoreGeneration};
use crate::memory::{FragmentType, Instant, Memory, MemoryLifecycle, MemorySource};
use uuid::Uuid;

pub(crate) fn eid(n: u64) -> EntityId {
    EntityId::new(Uuid::from_u128(n as u128))
}

pub(crate) fn opid(n: u64) -> OperationId {
    OperationId::new(Uuid::from_u128((10_000 + n) as u128))
}

pub(crate) fn ctx(op: OperationId, digest: &str) -> CommandContext {
    CommandContext {
        store_generation: StoreGeneration::FIRST,
        frontend_id: FrontendId::new(Uuid::from_u128(1)),
        channel_id: ChannelId::new(Uuid::from_u128(2)),
        session: None,
        operation_id: op,
        request_digest: digest.to_string(),
        deadline_millis: None,
        scope: Scope::default(),
        retry_epoch: 1,
    }
}

pub(crate) fn mem(id: u64, alias: Option<&str>) -> Memory {
    Memory {
        id: eid(id),
        external_alias: alias.map(ExternalAlias::new),
        title: format!("t{id}"),
        fragment: format!("f{id}"),
        description: String::new(),
        fragment_type: FragmentType::Fact,
        project: None,
        source: MemorySource::Ai,
        confidence: 0.5,
        quality_score: None,
        lifecycle: MemoryLifecycle::Live,
        tags: Vec::new(),
        associated_with: Vec::new(),
        relations: Vec::new(),
        parent_id: None,
        child_ids: Vec::new(),
        session_id: None,
        task_type: None,
        related_guides: Vec::new(),
        evidence: Vec::new(),
        access_count: 0,
        last_accessed_at: None,
        positive_feedback: 0,
        negative_feedback: 0,
        negative_hits: 0,
        refinement_count: 0,
        distill_candidate: false,
        entity_revision: EntityRevision::new(0),
        document_revision: crate::id::DocumentRevision::new(0),
        eligibility_revision: crate::id::EligibilityRevision::new(0),
        created_at: Instant(0),
        updated_at: Instant(0),
        raw_created: None,
        unknown_fields: std::collections::BTreeMap::new(),
    }
}

pub(crate) fn test_memory(id_num: u64) -> Memory {
    Memory {
        id: EntityId::new(Uuid::from_u128(id_num as u128)),
        external_alias: None,
        title: format!("Memory {id_num}"),
        fragment: format!("Fragment {id_num}"),
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
        entity_revision: EntityRevision::new(0),
        document_revision: crate::id::DocumentRevision::new(1),
        eligibility_revision: crate::id::EligibilityRevision::new(1),
        created_at: Instant::new(0),
        updated_at: Instant::new(0),
        raw_created: None,
        unknown_fields: std::collections::BTreeMap::new(),
    }
}

pub(crate) fn test_ctx(op_num: u64) -> CommandContext {
    CommandContext {
        store_generation: StoreGeneration::FIRST,
        frontend_id: FrontendId::new(Uuid::from_u128(1)),
        channel_id: ChannelId::new(Uuid::from_u128(2)),
        session: None,
        operation_id: OperationId::new(Uuid::from_u128(op_num as u128)),
        request_digest: format!("digest-{op_num}"),
        deadline_millis: None,
        scope: Scope::default(),
        retry_epoch: 1,
    }
}
