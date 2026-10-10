//! Projector test helpers (moved verbatim from `projector.rs`).

use std::sync::Arc;

use super::{Embedder, FixedEmbedder, Projector, TextChunk};
use crate::search::table::SearchTable;
use ltmrs_domain::command::DomainCommand;
use ltmrs_domain::id::{DocumentRevision, EntityId, FrontendId, ModelFingerprint, StoreGeneration};
use ltmrs_domain::memory::{FragmentType, Memory, MemoryLifecycle, MemorySource};
use uuid::Uuid;

pub(crate) fn eid(n: u64) -> EntityId {
    EntityId::new(Uuid::from_u128(n as u128))
}

pub(crate) fn memory(id: EntityId, title: &str, fragment: &str) -> Memory {
    Memory {
        id,
        external_alias: None,
        title: title.to_string(),
        fragment: fragment.to_string(),
        description: String::new(),
        fragment_type: FragmentType::Fact,
        project: Some("ltmrs".into()),
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

/// Open a repo with a namespace and wire up an in-memory Lance table. The
/// returned guard keeps both backing dirs alive for the test's lifetime.
pub(crate) async fn env() -> (
    Arc<ltmrs_service::repository::CanonicalRepository>,
    SearchTable,
    EnvGuard,
) {
    let dir = tempfile::tempdir().unwrap();
    let lance_dir = tempfile::tempdir().unwrap();

    let clock = Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
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

    (
        Arc::new(repo),
        table,
        EnvGuard {
            _dir: dir,
            _lance_dir: lance_dir,
        },
    )
}

pub(crate) struct EnvGuard {
    _dir: tempfile::TempDir,
    _lance_dir: tempfile::TempDir,
}

pub(crate) fn add(
    repo: &ltmrs_service::repository::CanonicalRepository,
    n: u64,
    title: &str,
    frag: &str,
) {
    repo.apply(
        &ctx(n),
        &DomainCommand::AddMemory {
            memory: memory(eid(n), title, frag),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
}

pub(crate) fn projector(
    repo: Arc<ltmrs_service::repository::CanonicalRepository>,
    table: SearchTable,
) -> Projector {
    Projector::new(
        repo,
        table,
        Box::new(FixedEmbedder { dim: 384 }),
        ModelFingerprint::new(1),
        StoreGeneration::FIRST,
    )
}

pub(crate) async fn table_with_vector_count(tbl: &SearchTable) -> u64 {
    tbl.count_rows(Some("embedding IS NOT NULL")).await.unwrap()
}

// ---- RQ-10 multi-chunk projection (WP-06 follow-up) ----

/// Shared halving policy for the chunk-aware test doubles below: split the
/// fragment at a line boundary when present, else at a char boundary near
/// the midpoint. Each unit carries the title prefix for lexical
/// searchability with offsets in rendered coordinates.
pub(crate) fn halving_chunks(title: &str, fragment: &str) -> Vec<TextChunk> {
    let base = title.len() + 1; // "title\n" rendered prefix
    let mid = fragment.find('\n').map(|i| i + 1).unwrap_or_else(|| {
        let mut m = fragment.len() / 2;
        while !fragment.is_char_boundary(m) {
            m -= 1;
        }
        m
    });
    let (first, second) = fragment.split_at(mid);
    vec![
        TextChunk {
            text: format!("{title}\n{first}"),
            char_start: base as u64,
            char_end: (base + first.len()) as u64,
        },
        TextChunk {
            text: format!("{title}\n{second}"),
            char_start: (base + first.len()) as u64,
            char_end: (base + fragment.len()) as u64,
        },
    ]
}

/// Test embedder with a chunk-aware policy: splits the fragment into two
/// halves (line boundary when present) and embeds each span separately.
/// `fail` models a stalled worker for the SemanticPending policy test.
pub(crate) struct HalvingEmbedder {
    pub(crate) dim: usize,
    pub(crate) fail: bool,
}

impl Embedder for HalvingEmbedder {
    fn embed(&mut self, text: &str) -> Result<Vec<f32>, String> {
        if self.fail {
            return Err("embedding service unavailable".into());
        }
        Ok((0..self.dim)
            .map(|i| {
                ((text.bytes().fold(0u64, |a, b| a.wrapping_add(b as u64)) >> (i % 64)) ^ i as u64)
                    as f32
                    / 1e9
            })
            .collect())
    }

    fn chunk_text(&self, title: &str, fragment: &str) -> Vec<TextChunk> {
        halving_chunks(title, fragment)
    }

    fn chunker_version(&self) -> String {
        "test-halving-v1".to_string()
    }
}

/// Test embedder whose second chunk always fails: pins the all-or-pending
/// policy for mixed partial embeddings (an `any`-instead-of-`all` ack
/// check must not pass this test). Shares the halving policy — and its
/// version — with HalvingEmbedder so attribution stays exact.
pub(crate) struct SecondChunkFailsEmbedder {
    pub(crate) dim: usize,
}

impl Embedder for SecondChunkFailsEmbedder {
    fn embed(&mut self, text: &str) -> Result<Vec<f32>, String> {
        if text.contains("beta-half") {
            return Err("tail chunk unavailable".into());
        }
        Ok(vec![0.5; self.dim])
    }

    fn chunk_text(&self, title: &str, fragment: &str) -> Vec<TextChunk> {
        halving_chunks(title, fragment)
    }

    fn chunker_version(&self) -> String {
        "test-halving-v1".to_string()
    }
}
