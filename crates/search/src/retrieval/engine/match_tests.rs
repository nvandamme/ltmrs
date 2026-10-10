//! Text-matching robustness tests (moved verbatim from `engine.rs`).

use super::test_support::*;
use super::{Engine, QueryEmbedder, RetrievalRequest};
use crate::search::row::SearchRow;
use ltmrs_domain::command::DomainResult;
use ltmrs_domain::command::{DomainCommand, Scope};
use ltmrs_domain::id::{DocumentRevision, EntityId, ModelFingerprint, StoreGeneration};

/// T-SEARCH-02: French accents and mixed-case identifiers survive.
#[tokio::test]
async fn french_accents_and_mixed_case_survive() {
    let (repo, table, mut proj, _guard) = env().await;
    add(
        &repo,
        1,
        "config note",
        "comment configurer la persistance avec Fjall DB",
        None,
    );
    add(
        &repo,
        2,
        "env note",
        "set the DATABASE_URL environment variable correctly",
        None,
    );
    proj.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();

    // Query with French accents.
    let req = RetrievalRequest {
        query: "configurer la persistance".into(),
        model_fingerprint: None,
        ..Default::default()
    };
    let result = Engine::new(repo.clone(), table.clone())
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    assert!(
        result.results.iter().any(|r| r.memory.id == eid(1)),
        "French-accented query must find the matching memory"
    );

    // Query with mixed-case environment variable.
    let req = RetrievalRequest {
        query: "DATABASE_URL".into(),
        model_fingerprint: None,
        ..Default::default()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    assert!(
        result.results.iter().any(|r| r.memory.id == eid(2)),
        "mixed-case identifier must be found"
    );
}

/// T-SEARCH-02: underscores and paths are preserved.
#[tokio::test]
async fn underscores_and_paths_preserved() {
    let (repo, table, mut proj, _guard) = env().await;
    add(
        &repo,
        1,
        "path note",
        "the config lives at /etc/ltmrs/config.toml",
        None,
    );
    proj.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();

    let req = RetrievalRequest {
        query: "/etc/ltmrs/config.toml".into(),
        model_fingerprint: None,
        ..Default::default()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    assert!(
        result.results.iter().any(|r| r.memory.id == eid(1)),
        "path with underscores must be found"
    );
}

/// T-SCOPE-02: min_confidence is enforced via canonical backfill, not just
/// the first N hits.
#[tokio::test]
async fn min_confidence_backfilled_from_canonical() {
    let (repo, table, mut proj, _guard) = env().await;
    // Two memories with the same text but different confidence.
    let mut low = memory(eid(1), "note", "shared query terms here", None);
    low.confidence = 0.3;
    let mut high = memory(eid(2), "note", "shared query terms here", None);
    high.confidence = 0.9;
    repo.apply(
        &ctx(1),
        &DomainCommand::AddMemory {
            memory: low,
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    repo.apply(
        &ctx(2),
        &DomainCommand::AddMemory {
            memory: high,
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    proj.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();

    // Request with min_confidence 0.5: only the high-confidence memory is eligible.
    let req = RetrievalRequest {
        query: "shared query terms".into(),
        scope: Scope {
            min_confidence: Some(0.5),
            ..Default::default()
        },
        model_fingerprint: None,
        ..Default::default()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    let ids: Vec<EntityId> = result.results.iter().map(|r| r.memory.id).collect();
    assert!(
        ids.contains(&eid(2)),
        "high-confidence memory must be returned"
    );
    assert!(
        !ids.contains(&eid(1)),
        "low-confidence memory must be filtered by min_confidence"
    );
}

/// T-SCOPE-01: date filters apply consistently.
#[tokio::test]
async fn date_filters_apply_consistently() {
    let (repo, table, mut proj, _guard) = env().await;
    let mut old = memory(eid(1), "old", "query terms for old memory", None);
    old.created_at = ltmrs_domain::memory::Instant::new(100);
    let mut new = memory(eid(2), "new", "query terms for new memory", None);
    new.created_at = ltmrs_domain::memory::Instant::new(5000);
    repo.apply(
        &ctx(1),
        &DomainCommand::AddMemory {
            memory: old,
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    repo.apply(
        &ctx(2),
        &DomainCommand::AddMemory {
            memory: new,
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    proj.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();

    // after=1000: only the new memory is in scope.
    let req = RetrievalRequest {
        query: "query terms for".into(),
        scope: Scope {
            after: Some(1000),
            ..Default::default()
        },
        model_fingerprint: None,
        ..Default::default()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    let ids: Vec<EntityId> = result.results.iter().map(|r| r.memory.id).collect();
    assert!(ids.contains(&eid(2)));
    assert!(!ids.contains(&eid(1)));
}

/// T-RANK-04: the dense leg respects the no-answer similarity threshold.
/// A row with an orthogonal vector (similarity 0) must be filtered out by a
/// high threshold, so nearest-neighbor rank is never treated as truth.
#[tokio::test]
async fn dense_no_answer_threshold_filters_noise() {
    let (repo, table, _proj, _guard) = env().await;

    // Publish a row whose vector is orthogonal to the query vector.
    let mut row = SearchRow {
        store_generation: StoreGeneration::FIRST,
        memory_id: eid(1),
        document_revision: DocumentRevision::new(1),
        model_fingerprint: ModelFingerprint::new(1),
        chunk_id: ltmrs_domain::id::ChunkId::new(0),
        chunker_version: "single-chunk-v1".to_string(),
        lexical_text: "tokio runtime details".into(),
        char_start: 0,
        char_end: 23,
        project: None,
        fragment_type: "fact".into(),
        created_at_millis: 100,
        confidence: 0.5,
        updated_at_millis: 100,
        embedding: Some(vec![1.0f32; 384]),
    };
    // Ensure the canonical memory exists and is eligible.
    add(&repo, 1, "rust async", "tokio runtime details", None);
    row.document_revision = repo
        .get_memories(&[eid(1)])
        .unwrap()
        .first()
        .unwrap()
        .document_revision;
    table
        .publish_rows(std::slice::from_ref(&row))
        .await
        .unwrap();

    // Query vector orthogonal to [1;384]: similarity = 0.
    struct OrthoEmbedder;
    impl QueryEmbedder for OrthoEmbedder {
        fn embed_query<'a>(
            &'a self,
            _q: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = DomainResult<Vec<f32>>> + Send + 'a>>
        {
            // Orthogonal to [1;384]: sum of components is 0.
            Box::pin(async move {
                Ok((0..384)
                    .map(|i| if i < 192 { 1.0f32 } else { -1.0 })
                    .collect())
            })
        }
    }

    let req = RetrievalRequest {
        query: "zzqqxx unrelated".into(),
        min_similarity: Some(0.5),
        model_fingerprint: Some(ModelFingerprint::new(1)),
        ..Default::default()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &OrthoEmbedder)
        .await
        .unwrap();
    assert!(
        result.results.is_empty(),
        "orthogonal candidate (similarity 0) must be filtered by the threshold"
    );
    assert!(result.explanation.no_match);
}
/// AD-04: the dense leg never mixes vectors from different model spaces.
/// A row published under a different fingerprint is invisible to a dense
/// query for the requested fingerprint, even if its vector is a perfect
/// match.
#[tokio::test]
async fn dense_leg_respects_model_fingerprint_isolation() {
    let (repo, table, _proj, _guard) = env().await;
    add(
        &repo,
        1,
        "t1",
        "the quick brown fox jumps over the lazy dog",
        None,
    );

    // Publish a row under a DIFFERENT fingerprint whose vector is a perfect
    // match for the query vector (the "old" model space).
    let mut old_row = SearchRow {
        store_generation: StoreGeneration::FIRST,
        memory_id: eid(1),
        document_revision: DocumentRevision::new(1),
        model_fingerprint: ModelFingerprint::new(999),
        chunk_id: ltmrs_domain::id::ChunkId::new(0),
        chunker_version: "single-chunk-v1".to_string(),
        lexical_text: "the quick brown fox jumps over the lazy dog".into(),
        char_start: 0,
        char_end: 46,
        project: None,
        fragment_type: "fact".into(),
        created_at_millis: 100,
        confidence: 0.5,
        updated_at_millis: 100,
        embedding: Some(TestQueryEmbedder::hash_vec(
            "t1\nthe quick brown fox jumps over the lazy dog",
        )),
    };
    old_row.document_revision = repo
        .get_memories(&[eid(1)])
        .unwrap()
        .first()
        .unwrap()
        .document_revision;
    table
        .publish_rows(std::slice::from_ref(&old_row))
        .await
        .unwrap();

    // Query for fingerprint 1 (the "new" model space). The perfect-match
    // row under fingerprint 999 must NOT be returned.
    let req = RetrievalRequest {
        query: "the quick brown fox jumps over the lazy dog".into(),
        model_fingerprint: Some(ModelFingerprint::new(1)),
        ..Default::default()
    };
    let embedder = TestQueryEmbedder { prefix: "t1\n" };
    let result = Engine::new(repo, table)
        .retrieve(&req, &embedder)
        .await
        .unwrap();
    // No rows exist under fingerprint 1, so the dense leg returns nothing.
    assert!(
        result.results.is_empty(),
        "dense leg must not return rows from a different model fingerprint"
    );
}
