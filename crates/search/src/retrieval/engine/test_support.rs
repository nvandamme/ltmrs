//! Engine test helpers (moved verbatim from `engine.rs`).

use super::{QueryEmbedder, RetrievalRequest};
use crate::search::projector::{FixedEmbedder, Projector};
use crate::search::table::SearchTable;
use ltmrs_domain::command::{DomainCommand, DomainResult};
use ltmrs_domain::command::{DomainError, DomainErrorCode};
use ltmrs_domain::id::{DocumentRevision, EntityId, FrontendId, ModelFingerprint, StoreGeneration};
use ltmrs_domain::memory::{FragmentType, Memory, MemoryLifecycle, MemorySource};
use uuid::Uuid;

pub(crate) fn eid(n: u64) -> EntityId {
    EntityId::new(Uuid::from_u128(n as u128))
}

pub(crate) fn memory(id: EntityId, title: &str, fragment: &str, project: Option<&str>) -> Memory {
    Memory {
        id,
        external_alias: None,
        title: title.to_string(),
        fragment: fragment.to_string(),
        description: String::new(),
        fragment_type: FragmentType::Fact,
        project: project.map(|s| s.to_string()),
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
        document_revision: DocumentRevision::new(1),
        eligibility_revision: ltmrs_domain::id::EligibilityRevision::new(1),
        created_at: ltmrs_domain::memory::Instant::new(100),
        updated_at: ltmrs_domain::memory::Instant::new(100),
        raw_created: None,
        unknown_fields: std::collections::BTreeMap::new(),
    }
}

pub(crate) fn ctx(op_num: u64) -> ltmrs_domain::command::CommandContext {
    ltmrs_domain::command::CommandContext {
        store_generation: StoreGeneration::FIRST,
        frontend_id: FrontendId::new(Uuid::from_u128(1)),
        channel_id: ltmrs_domain::id::ChannelId::new(Uuid::from_u128(2)),
        session: None,
        operation_id: ltmrs_domain::id::OperationId::new(Uuid::from_u128(op_num as u128)),
        request_digest: format!("d{op_num}"),
        deadline_millis: None,
        scope: Default::default(),
        retry_epoch: 1,
    }
}

/// A deterministic query embedder: the SAME derivation as FixedEmbedder,
/// with an optional prefix so a prefixed query matches a document exactly
/// (mirroring the E5 query/passage prefix asymmetry).
pub(crate) struct TestQueryEmbedder {
    pub(crate) prefix: &'static str,
}

impl TestQueryEmbedder {
    pub(crate) fn hash_vec(text: &str) -> Vec<f32> {
        crate::search::projector::hash_embed_vec(text, 384)
    }
}

impl QueryEmbedder for TestQueryEmbedder {
    fn embed_query<'a>(
        &'a self,
        query: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = DomainResult<Vec<f32>>> + Send + 'a>>
    {
        let text = format!("{}{}", self.prefix, query);
        Box::pin(async move { Ok(Self::hash_vec(&text)) })
    }
}

/// An embedder under backpressure: every query fails (overloaded worker).
pub(crate) struct FailingEmbedder;

impl QueryEmbedder for FailingEmbedder {
    fn embed_query<'a>(
        &'a self,
        query: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = DomainResult<Vec<f32>>> + Send + 'a>>
    {
        let _ = query;
        Box::pin(async move {
            Err(DomainError::new(
                DomainErrorCode::Validation,
                "embedder overloaded",
            ))
        })
    }
}

pub(crate) async fn env() -> (
    std::sync::Arc<ltmrs_service::repository::CanonicalRepository>,
    SearchTable,
    Projector,
    (tempfile::TempDir, tempfile::TempDir),
) {
    let dir = tempfile::tempdir().unwrap();
    let lance_dir = tempfile::tempdir().unwrap();

    let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
    let repo_path = dir.path().to_str().unwrap();
    let repo =
        ltmrs_service::repository::CanonicalRepository::open_with_clock(repo_path, clock).unwrap();
    let fe = FrontendId::new(Uuid::from_u128(1));
    let ns = repo
        .issue_namespace(
            fe,
            ltmrs_domain::id::ChannelId::new(Uuid::from_u128(2)),
            1000,
        )
        .unwrap();
    assert_eq!(ns.retry_epoch, 1);

    let uri = lance_dir.path().to_str().unwrap().to_string();
    let table = SearchTable::open(&uri).await.unwrap();
    let repo_arc = std::sync::Arc::new(repo);
    let projector = Projector::new(
        repo_arc.clone(),
        table.clone(),
        Box::new(FixedEmbedder { dim: 384 }),
        ModelFingerprint::new(1),
        StoreGeneration::FIRST,
    );
    (repo_arc, table, projector, (dir, lance_dir))
}

pub(crate) fn add(
    repo: &ltmrs_service::repository::CanonicalRepository,
    n: u64,
    title: &str,
    frag: &str,
    project: Option<&str>,
) {
    repo.apply(
        &ctx(n),
        &DomainCommand::AddMemory {
            memory: memory(eid(n), title, frag, project),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
}

pub(crate) fn base_req() -> RetrievalRequest {
    RetrievalRequest {
        model_fingerprint: Some(ModelFingerprint::new(1)),
        ..Default::default()
    }
}
