//! MCP test helpers (moved verbatim from `mcp.rs`).

use ltmrs_domain::id::EntityId;
use ltmrs_domain::memory::Memory;
use uuid::Uuid;

pub(crate) fn mem(id: u64, project: Option<&str>, confidence: f64, title: &str) -> Memory {
    Memory {
        id: EntityId::new(Uuid::from_u128(id as u128)),
        external_alias: Some(ltmrs_domain::id::ExternalAlias::new(format!("m{id:012x}"))),
        title: title.to_string(),
        fragment: format!("frag-{title}"),
        description: String::new(),
        fragment_type: ltmrs_domain::memory::FragmentType::Fact,
        project: project.map(|p| p.to_string()),
        source: ltmrs_domain::memory::MemorySource::Ai,
        confidence,
        quality_score: None,
        lifecycle: ltmrs_domain::memory::MemoryLifecycle::Live,
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
