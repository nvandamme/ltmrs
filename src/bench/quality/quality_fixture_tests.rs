//! Quality fixture tests (moved verbatim from `quality.rs`).

use super::*;

/// Safety fixture through the lexical leg: the frozen safety cases seed
/// a real FTS table; the executable subset (everything except currency
/// judgment and cross-language matching, which need the engine leg)
/// must pass with zero leakage and zero no-answer false positives.
#[tokio::test]
async fn safety_fixture_passes_lexical_subset() {
    use crate::bench::experiment::search_row_for;
    use ltmrs_search::search::table::SearchTable;
    use std::collections::HashMap;
    let raw = std::fs::read_to_string(
        crate::bench::crate_root().join("experiments/quality/safety-cases.json"),
    )
    .unwrap();
    let corpus: Corpus = serde_json::from_str(&raw).unwrap();
    assert_eq!(corpus.version, CORPUS_VERSION);
    assert!(validate_corpus(&corpus).is_empty());
    let dir = tempfile::tempdir().unwrap();
    let table = SearchTable::open(dir.path().join("search").to_str().unwrap())
        .await
        .unwrap();
    let mut id_by_row: HashMap<String, String> = HashMap::new();
    let mut rows = Vec::new();
    for (index, doc) in corpus.documents.iter().enumerate() {
        let mut row = search_row_for(index as u64, &doc.text);
        row.project = Some(doc.project.clone());
        row.created_at_millis = doc.created_at_millis;
        id_by_row.insert(row.memory_id.as_uuid().to_string(), doc.id.clone());
        rows.push(row);
    }
    table.publish_rows(&rows).await.unwrap();
    table.create_fts_index().await.unwrap();
    // Currency judgment needs the engine's relational supersession
    // (engine leg); cross-language matching needs real embedding
    // semantics (real-E5 leg). The lexical leg decides neither.
    let mut executed = 0usize;
    let mut skipped_ids = Vec::new();
    let mut leakage_total = 0usize;
    for case in &corpus.cases {
        if matches!(case.kind, CaseKind::Obsolete | CaseKind::CrossLanguage) {
            skipped_ids.push(case.id.clone());
            continue;
        }
        executed += 1;
        // The canonical scope builds the predicate (same builder the
        // engine uses: project scope plus declared time bounds).
        let scope = ltmrs_domain::command::Scope {
            project: Some(case.project.clone()),
            after: case.after_millis,
            before: case.before_millis,
            ..Default::default()
        };
        let filter = ltmrs_search::retrieval::scope::EffectiveScope::resolve(&scope)
            .unwrap()
            .to_lance_filter();
        let hits = table
            .fts_query(&case.query, case.top_k, filter.as_deref())
            .await
            .unwrap();
        let ranked: Vec<String> = hits
            .iter()
            .map(|row| id_by_row[&row.memory_id.as_uuid().to_string()].clone())
            .collect();
        // Non-vacuous trap: the excluded doc must match unfiltered, so
        // its absence above proves the scope filter, not a dead query.
        if case.kind == CaseKind::ScopeTrap {
            let unfiltered = table
                .fts_query(&case.query, case.top_k, None)
                .await
                .unwrap();
            let unfiltered_ids: Vec<String> = unfiltered
                .iter()
                .map(|row| id_by_row[&row.memory_id.as_uuid().to_string()].clone())
                .collect();
            for excluded in &case.must_exclude {
                assert!(
                    unfiltered_ids.contains(excluded),
                    "case {}: trap dead, {excluded} matches nothing unfiltered",
                    case.id
                );
            }
        }
        let metrics = score_case(case, &ranked);
        leakage_total += metrics.leakage_count;
        assert_eq!(metrics.leakage_count, 0, "case {} leaked", case.id);
        if case.no_answer {
            assert!(
                !metrics.no_answer_fp,
                "case {} false-positive on no-answer",
                case.id
            );
        } else {
            assert!(
                metrics.must_include_covered,
                "case {} missed mandatory ids (ranked {ranked:?})",
                case.id
            );
        }
    }
    assert!(
        executed >= 6,
        "subset must stay substantial, ran {executed}"
    );
    assert_eq!(
        skipped_ids,
        vec!["obsolete-g".to_string(), "cross-g1".to_string()]
    );
    assert_eq!(leakage_total, 0);
}

/// Deterministic hash embedder with the exact FixedEmbedder derivation
/// (query/passage spaces align without a model).
struct HashQueryEmbedder;

/// Embedding width shared by the hash query embedder and the passage
/// FixedEmbedder below: a dim change must move both together or
/// query/passage spaces silently misalign.
const BENCH_EMBED_DIM: usize = 384;

impl HashQueryEmbedder {
    fn hash_vec(text: &str) -> Vec<f32> {
        ltmrs_search::search::projector::hash_embed_vec(text, BENCH_EMBED_DIM)
    }
}

impl ltmrs_search::retrieval::engine::QueryEmbedder for HashQueryEmbedder {
    fn embed_query<'a>(
        &'a self,
        query: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = ltmrs_domain::command::DomainResult<Vec<f32>>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move { Ok(Self::hash_vec(query)) })
    }
}

/// Safety fixture through the full engine (fusion + scope + currency):
/// the same frozen cases run end-to-end with deterministic vectors, so
/// currency judgment and scope enforcement are exercised with no model.
/// Only cross-language matching needs real embedding semantics (E5 leg).
#[tokio::test]
async fn safety_fixture_passes_engine_leg() {
    use ltmrs_domain::command::{CommandContext, DomainCommand, Scope};
    use ltmrs_domain::id::{
        ChannelId, DocumentRevision, EligibilityRevision, EntityId, EntityRevision, FrontendId,
        ModelFingerprint, OperationId, StoreGeneration,
    };
    use ltmrs_domain::memory::{
        FragmentType, Instant as DomainInstant, Memory, MemoryLifecycle, MemorySource,
    };
    use ltmrs_domain::relation::{Relation, RelationType};
    use ltmrs_search::retrieval::engine::{Engine, RetrievalRequest};
    use ltmrs_search::search::projector::{FixedEmbedder, Projector};
    use ltmrs_search::search::table::SearchTable;
    use ltmrs_service::repository::CanonicalRepository;
    use std::sync::Arc;

    fn eid(n: u64) -> EntityId {
        EntityId::new(uuid::Uuid::from_u128(n as u128))
    }

    fn engine_memory(
        id: EntityId,
        title: &str,
        fragment: &str,
        project: Option<&str>,
        created_at_millis: u64,
    ) -> Memory {
        Memory {
            id,
            external_alias: None,
            title: title.to_string(),
            fragment: fragment.to_string(),
            description: String::new(),
            fragment_type: FragmentType::Fact,
            project: project.map(|s| s.to_string()),
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
            created_at: DomainInstant::new(created_at_millis),
            updated_at: DomainInstant::new(created_at_millis),
            raw_created: None,
            unknown_fields: std::collections::BTreeMap::new(),
        }
    }

    fn engine_ctx(op_num: u64) -> CommandContext {
        CommandContext {
            store_generation: StoreGeneration::FIRST,
            frontend_id: FrontendId::new(uuid::Uuid::from_u128(1)),
            channel_id: ChannelId::new(uuid::Uuid::from_u128(2)),
            session: None,
            operation_id: OperationId::new(uuid::Uuid::from_u128(u128::from(op_num))),
            request_digest: format!("d{op_num}"),
            deadline_millis: None,
            scope: Scope::default(),
            retry_epoch: 1,
        }
    }

    let raw = std::fs::read_to_string(
        crate::bench::crate_root().join("experiments/quality/safety-cases.json"),
    )
    .unwrap();
    let corpus: Corpus = serde_json::from_str(&raw).unwrap();
    assert!(validate_corpus(&corpus).is_empty());

    let dir = tempfile::tempdir().unwrap();
    let lance_dir = tempfile::tempdir().unwrap();
    // Frozen clock: retry namespaces carry a 24h TTL, so a real-clock
    // store outlives a fixed issuance instant before the first apply.
    let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
    let repo =
        CanonicalRepository::open_with_clock(dir.path().join("store").to_str().unwrap(), clock)
            .unwrap();
    repo.issue_namespace(
        FrontendId::new(uuid::Uuid::from_u128(1)),
        ChannelId::new(uuid::Uuid::from_u128(2)),
        1000,
    )
    .unwrap();
    let repo = Arc::new(repo);
    let table = SearchTable::open(lance_dir.path().to_str().unwrap())
        .await
        .unwrap();
    let mut projector = Projector::new(
        repo.clone(),
        table.clone(),
        Box::new(FixedEmbedder {
            dim: BENCH_EMBED_DIM,
        }),
        ModelFingerprint::new(1),
        StoreGeneration::FIRST,
    );
    // Seed order fixes the eid mapping (index -> eid).
    let mut id_by_uuid = std::collections::HashMap::new();
    for (index, doc) in corpus.documents.iter().enumerate() {
        let id = eid(index as u64);
        id_by_uuid.insert(id.as_uuid().to_string(), doc.id.clone());
        repo.apply(
            &engine_ctx(index as u64),
            &DomainCommand::AddMemory {
                memory: engine_memory(
                    id,
                    &doc.id,
                    &doc.text,
                    Some(&doc.project),
                    doc.created_at_millis,
                ),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    }
    projector.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();
    // The fixture's obsolete pair: g5 supersedes g4.
    let idx = |fid: &str| corpus.documents.iter().position(|d| d.id == fid).unwrap() as u64;
    repo.apply(
        &engine_ctx(9000),
        &DomainCommand::Relate {
            relation: Relation::new(
                eid(9000),
                eid(idx("g5")),
                eid(idx("g4")),
                RelationType::Supersedes,
                None,
                DomainInstant::new(1),
            ),
        },
    )
    .unwrap();

    let mut executed = 0usize;
    let mut skipped_ids = Vec::new();
    let mut leakage_total = 0usize;
    for case in &corpus.cases {
        // Two exclusions, both named:
        // - CrossLanguage: hash vectors are language-blind by
        //   construction; matching needs real embedding semantics.
        // - NoAnswer: the engine's no-answer rule is a calibrated
        //   similarity threshold, and thresholds calibrate on real
        //   similarity distributions (WP-12 task 9), not on hash noise.
        //   With no threshold the dense-hash nearest neighbor flows in.
        if matches!(case.kind, CaseKind::CrossLanguage | CaseKind::NoAnswer) {
            skipped_ids.push(case.id.clone());
            continue;
        }
        executed += 1;
        let req = RetrievalRequest {
            query: case.query.clone(),
            scope: Scope {
                project: Some(case.project.clone()),
                after: case.after_millis,
                before: case.before_millis,
                ..Default::default()
            },
            model_fingerprint: Some(ModelFingerprint::new(1)),
            // No threshold: hash similarities are uncalibrated by design
            // (see the NoAnswer exclusion above).
            min_similarity: None,
            result_limit: case.top_k,
            ..Default::default()
        };
        let result = Engine::new(repo.clone(), table.clone())
            .retrieve(&req, &HashQueryEmbedder)
            .await
            .unwrap();
        let ranked: Vec<String> = result
            .results
            .iter()
            .map(|r| id_by_uuid[&r.memory.id.as_uuid().to_string()].clone())
            .collect();
        let metrics = score_case(case, &ranked);
        leakage_total += metrics.leakage_count;
        assert_eq!(metrics.leakage_count, 0, "case {} leaked", case.id);
        // No-answer cases are excluded from this leg (see above), so
        // every executed case must cover its mandatory ids.
        assert!(
            metrics.must_include_covered,
            "case {} missed mandatory ids (ranked {ranked:?})",
            case.id
        );
        if !case.obsolete_ids.is_empty() {
            assert!(
                !metrics.obsolete_hit,
                "case {} presented superseded advice (ranked {ranked:?})",
                case.id
            );
            // Non-vacuous: the superseded doc must have been considered
            // (a candidate) and then excluded from the primary answer.
            for obsolete in &case.obsolete_ids {
                let position = corpus
                    .documents
                    .iter()
                    .position(|d| &d.id == obsolete)
                    .unwrap();
                let uuid = eid(position as u64).as_uuid().to_string();
                let considered = result
                    .explanation
                    .candidates
                    .keys()
                    .any(|id| id.as_uuid().to_string() == uuid);
                assert!(
                    considered,
                    "case {}: superseded doc {obsolete} was never a candidate (vacuous exclusion)",
                    case.id
                );
            }
        }
    }
    assert!(
        executed >= 7,
        "engine subset must stay substantial, ran {executed}"
    );
    assert_eq!(
        skipped_ids,
        vec![
            "noanswer-g".to_string(),
            "cross-g1".to_string(),
            "noanswer-h".to_string()
        ]
    );
    assert_eq!(leakage_total, 0);
}

/// Passage + query bridges: real E5 semantics behind both seams. One
/// shared adapter (single model load per test), roles kept separate.
struct E5SharedEmbedder {
    adapter: std::sync::Arc<std::sync::Mutex<ltmrs_embeddings::e5_small::E5SmallAdapter>>,
}

impl ltmrs_search::search::projector::Embedder for E5SharedEmbedder {
    fn embed(&mut self, text: &str) -> Result<Vec<f32>, String> {
        use ltmrs_embeddings::recipe::Role;
        self.adapter
            .lock()
            .expect("embedder alive")
            .embed(text, Role::Passage)
            .map(|seq| seq.vector)
            .map_err(|e| format!("{e:?}"))
    }

    /// Production chunking verbatim (same code path as the daemon
    /// projector): the cross-lang verdict covers multi-chunk documents.
    fn chunk_text(
        &self,
        title: &str,
        fragment: &str,
    ) -> Vec<ltmrs_search::search::projector::TextChunk> {
        let chunks = self
            .adapter
            .lock()
            .expect("embedder alive")
            .chunk_passage(title, fragment);
        ltmrs_search::search::backend::e5::e5_chunks_to_text_chunks(title, &chunks)
    }

    fn chunker_version(&self) -> String {
        ltmrs_search::search::backend::e5::E5_CHUNK_VERSION.to_string()
    }
}

impl ltmrs_search::retrieval::engine::QueryEmbedder for E5SharedEmbedder {
    fn embed_query<'a>(
        &'a self,
        query: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = ltmrs_domain::command::DomainResult<Vec<f32>>>
                + Send
                + 'a,
        >,
    > {
        use ltmrs_embeddings::recipe::Role;
        Box::pin(async move {
            self.adapter
                .lock()
                .expect("embedder alive")
                .embed(query, Role::Query)
                .map(|seq| seq.vector)
                .map_err(|e| {
                    ltmrs_domain::command::DomainError::new(
                        ltmrs_domain::command::DomainErrorCode::Validation,
                        format!("{e:?}"),
                    )
                })
        })
    }
}

/// Cross-language through real E5-small (the embedding engine near
/// WP-04 is the Candle E5 stack, not LanceDB — Lance is the vector
/// table). Skips cleanly when the pinned artifacts are absent (CI
/// without network), following the e5_small precedent.
#[tokio::test]
async fn safety_fixture_passes_real_e5_leg() {
    use ltmrs_domain::command::{CommandContext, DomainCommand, Scope};
    use ltmrs_domain::id::{
        ChannelId, DocumentRevision, EligibilityRevision, EntityId, EntityRevision, FrontendId,
        ModelFingerprint, OperationId, StoreGeneration,
    };
    use ltmrs_domain::memory::{
        FragmentType, Instant as DomainInstant, Memory, MemoryLifecycle, MemorySource,
    };
    use ltmrs_search::retrieval::engine::{Engine, RetrievalRequest};
    use ltmrs_search::search::projector::Projector;
    use ltmrs_search::search::table::SearchTable;
    use ltmrs_service::repository::CanonicalRepository;
    use std::sync::Arc;

    let artifacts_dir = crate::bench::crate_root().join("tmp/e5-artifacts");
    let artifacts = artifacts_dir.as_path();
    if !artifacts.join("model.safetensors").exists() {
        eprintln!("SKIP real-E5 leg: tmp/e5-artifacts absent (CI without network)");
        return;
    }
    let raw = std::fs::read_to_string(
        crate::bench::crate_root().join("experiments/quality/safety-cases.json"),
    )
    .unwrap();
    let corpus: Corpus = serde_json::from_str(&raw).unwrap();
    assert!(validate_corpus(&corpus).is_empty());

    let dir = tempfile::tempdir().unwrap();
    let lance_dir = tempfile::tempdir().unwrap();
    let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
    let repo =
        CanonicalRepository::open_with_clock(dir.path().join("store").to_str().unwrap(), clock)
            .unwrap();
    repo.issue_namespace(
        FrontendId::new(uuid::Uuid::from_u128(1)),
        ChannelId::new(uuid::Uuid::from_u128(2)),
        1000,
    )
    .unwrap();
    let repo = Arc::new(repo);
    let table = SearchTable::open(lance_dir.path().to_str().unwrap())
        .await
        .unwrap();
    let shared = std::sync::Arc::new(std::sync::Mutex::new(
        ltmrs_embeddings::e5_small::E5SmallAdapter::load_verified(artifacts)
            .expect("pinned artifacts load"),
    ));
    let mut projector = Projector::new(
        repo.clone(),
        table.clone(),
        Box::new(E5SharedEmbedder {
            adapter: shared.clone(),
        }),
        ModelFingerprint::new(1),
        StoreGeneration::FIRST,
    );
    let mut id_by_uuid = std::collections::HashMap::new();
    for (index, doc) in corpus.documents.iter().enumerate() {
        let id = EntityId::new(uuid::Uuid::from_u128(index as u128));
        id_by_uuid.insert(id.as_uuid().to_string(), doc.id.clone());
        repo.apply(
            &CommandContext {
                store_generation: StoreGeneration::FIRST,
                frontend_id: FrontendId::new(uuid::Uuid::from_u128(1)),
                channel_id: ChannelId::new(uuid::Uuid::from_u128(2)),
                session: None,
                operation_id: OperationId::new(uuid::Uuid::from_u128(index as u128)),
                request_digest: format!("e5-{index}"),
                deadline_millis: None,
                scope: Scope::default(),
                retry_epoch: 1,
            },
            &DomainCommand::AddMemory {
                memory: Memory {
                    id,
                    external_alias: None,
                    title: doc.id.clone(),
                    fragment: doc.text.clone(),
                    description: String::new(),
                    fragment_type: FragmentType::Fact,
                    project: Some(doc.project.clone()),
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
                    created_at: DomainInstant::new(doc.created_at_millis),
                    updated_at: DomainInstant::new(doc.created_at_millis),
                    raw_created: None,
                    unknown_fields: std::collections::BTreeMap::new(),
                },
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    }
    projector.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();

    let embedder = E5SharedEmbedder {
        adapter: shared.clone(),
    };
    // French query against English docs (plus an English anchor): real
    // cross-lingual similarity must surface the rose-pruning doc.
    for case_id in ["cross-g1", "exact-g1"] {
        let case = corpus.cases.iter().find(|c| c.id == case_id).unwrap();
        let req = RetrievalRequest {
            query: case.query.clone(),
            scope: Scope {
                project: Some(case.project.clone()),
                ..Default::default()
            },
            model_fingerprint: Some(ModelFingerprint::new(1)),
            min_similarity: None,
            result_limit: case.top_k,
            ..Default::default()
        };
        let result = Engine::new(repo.clone(), table.clone())
            .retrieve(&req, &embedder)
            .await
            .unwrap();
        let ranked: Vec<String> = result
            .results
            .iter()
            .map(|r| id_by_uuid[&r.memory.id.as_uuid().to_string()].clone())
            .collect();
        assert!(
            case.must_include.iter().all(|id| ranked.contains(id)),
            "case {case_id} missed mandatory ids (ranked {ranked:?})"
        );
    }
}

/// The harness must project through production chunking, not single-chunk
/// override: a long document yields multiple rows (greedy units).
#[tokio::test]
async fn e5_leg_uses_production_chunking() {
    use ltmrs_domain::command::{CommandContext, DomainCommand, Scope};
    use ltmrs_domain::id::{
        ChannelId, DocumentRevision, EligibilityRevision, EntityId, EntityRevision, FrontendId,
        ModelFingerprint, OperationId, StoreGeneration,
    };
    use ltmrs_domain::memory::{
        FragmentType, Instant as DomainInstant, Memory, MemoryLifecycle, MemorySource,
    };
    use ltmrs_search::search::projector::Projector;
    use ltmrs_search::search::table::SearchTable;
    use ltmrs_service::repository::CanonicalRepository;

    let artifacts_dir = crate::bench::crate_root().join("tmp/e5-artifacts");
    let artifacts = artifacts_dir.as_path();
    if !artifacts.join("model.safetensors").exists() {
        eprintln!("SKIP real-E5 leg: tmp/e5-artifacts absent (CI without network)");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let lance_dir = tempfile::tempdir().unwrap();
    let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
    let repo =
        CanonicalRepository::open_with_clock(dir.path().join("store").to_str().unwrap(), clock)
            .unwrap();
    repo.issue_namespace(
        FrontendId::new(uuid::Uuid::from_u128(1)),
        ChannelId::new(uuid::Uuid::from_u128(2)),
        1000,
    )
    .unwrap();
    let repo = std::sync::Arc::new(repo);
    let table = SearchTable::open(lance_dir.path().to_str().unwrap())
        .await
        .unwrap();
    let shared = std::sync::Arc::new(std::sync::Mutex::new(
        ltmrs_embeddings::e5_small::E5SmallAdapter::load_verified(artifacts)
            .expect("pinned artifacts load"),
    ));
    let mut projector = Projector::new(
        repo.clone(),
        table.clone(),
        Box::new(E5SharedEmbedder {
            adapter: shared.clone(),
        }),
        ModelFingerprint::new(1),
        StoreGeneration::FIRST,
    );
    let id = EntityId::new(uuid::Uuid::from_u128(77));
    repo.apply(
        &CommandContext {
            store_generation: StoreGeneration::FIRST,
            frontend_id: FrontendId::new(uuid::Uuid::from_u128(1)),
            channel_id: ChannelId::new(uuid::Uuid::from_u128(2)),
            session: None,
            operation_id: OperationId::new(uuid::Uuid::from_u128(3)),
            request_digest: "e5-chunk".to_string(),
            deadline_millis: None,
            scope: Scope::default(),
            retry_epoch: 1,
        },
        &DomainCommand::AddMemory {
            memory: Memory {
                id,
                external_alias: None,
                title: "Long doc".to_string(),
                fragment: "word ".repeat(600),
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
                created_at: DomainInstant::new(1000),
                updated_at: DomainInstant::new(1000),
                raw_created: None,
                unknown_fields: std::collections::BTreeMap::new(),
            },
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    projector.run_until_idle().await.unwrap();
    let rows = table
        .rows_where(&format!("memory_id = '{}'", id.as_uuid()))
        .await
        .unwrap();
    assert!(
        rows.len() > 1,
        "long document must project to multiple chunks, got {}",
        rows.len()
    );
}
