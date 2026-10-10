//! Scoring / mean-vector tests (moved verbatim from `engine.rs`).

use super::shaping::mean_vector;
use super::test_support::*;
use super::{Engine, RetrievalRequest};
use crate::search::row::SearchRow;
use ltmrs_domain::command::DomainCommand;
use ltmrs_domain::id::EntityId;
use ltmrs_domain::id::ModelFingerprint;

/// MMR diversity must see the whole document: the candidate vector is
/// the mean over all embedded chunks (lexical rows first), not the
/// first chunk alone.
#[test]
fn mean_vector_aggregates_all_chunks() {
    use ltmrs_domain::id::ChunkId;
    fn row(id: u64, chunk: u32, embedding: Option<Vec<f32>>) -> SearchRow {
        SearchRow {
            store_generation: ltmrs_domain::id::StoreGeneration::FIRST,
            memory_id: eid(id),
            document_revision: ltmrs_domain::id::DocumentRevision::new(1),
            model_fingerprint: ltmrs_domain::id::ModelFingerprint::new(1),
            chunk_id: ChunkId::new(chunk),
            chunker_version: "v1".to_string(),
            lexical_text: "t".to_string(),
            char_start: 0,
            char_end: 1,
            project: None,
            fragment_type: "fact".to_string(),
            created_at_millis: 0,
            confidence: 0.5,
            updated_at_millis: 0,
            embedding,
        }
    }
    // Two embedded chunks + one embedding-free row: mean over the two.
    let lexical = vec![
        row(1, 0, Some(vec![1.0, 0.0])),
        row(1, 1, Some(vec![0.0, 1.0])),
        row(1, 2, None),
    ];
    assert_eq!(
        mean_vector(&lexical, &[], eid(1), Some(ModelFingerprint::new(1))),
        Some(vec![0.5, 0.5])
    );
    // No lexical rows: fall back to dense rows.
    let dense = vec![row(1, 0, Some(vec![0.0, 4.0]))];
    assert_eq!(
        mean_vector(&[], &dense, eid(1), Some(ModelFingerprint::new(1))),
        Some(vec![0.0, 4.0])
    );
    // Nothing embedded anywhere: no vector (MMR zero-placeholder).
    assert_eq!(mean_vector(&[], &[], eid(1), None), None);
    let bare = vec![row(1, 0, None)];
    assert_eq!(mean_vector(&bare, &[], eid(1), None), None);
    // Other memories' rows never leak in.
    let mixed = vec![row(2, 0, Some(vec![9.0, 9.0]))];
    assert_eq!(mean_vector(&mixed, &[], eid(1), None), None);
}

/// Union across legs: disjoint chunks in lexical and dense average
/// together; a chunk present in both counts once.
#[test]
fn mean_vector_unions_legs_without_double_counting() {
    use ltmrs_domain::id::ChunkId;
    fn row_chunk(id: u64, chunk: u32, embedding: Option<Vec<f32>>) -> SearchRow {
        SearchRow {
            store_generation: ltmrs_domain::id::StoreGeneration::FIRST,
            memory_id: eid(id),
            document_revision: ltmrs_domain::id::DocumentRevision::new(1),
            model_fingerprint: ltmrs_domain::id::ModelFingerprint::new(1),
            chunk_id: ChunkId::new(chunk),
            chunker_version: "v1".to_string(),
            lexical_text: "t".to_string(),
            char_start: 0,
            char_end: 1,
            project: None,
            fragment_type: "fact".to_string(),
            created_at_millis: 0,
            confidence: 0.5,
            updated_at_millis: 0,
            embedding,
        }
    }
    let fp = Some(ModelFingerprint::new(1));
    // Chunk 0 lexical-only, chunk 1 dense-only: mean over both.
    let lexical = vec![row_chunk(1, 0, Some(vec![1.0, 0.0]))];
    let dense = vec![row_chunk(1, 1, Some(vec![0.0, 1.0]))];
    assert_eq!(
        mean_vector(&lexical, &dense, eid(1), fp),
        Some(vec![0.5, 0.5])
    );
    // Same chunk in both legs: counted once, not averaged with itself.
    let lexical = vec![row_chunk(1, 0, Some(vec![2.0, 0.0]))];
    let dense = vec![row_chunk(1, 0, Some(vec![2.0, 0.0]))];
    assert_eq!(
        mean_vector(&lexical, &dense, eid(1), fp),
        Some(vec![2.0, 0.0])
    );
}

/// Fingerprint scoping (AD-04): rows outside the requested model space
/// never enter the mean, even when they are the only embedded rows.
#[test]
fn mean_vector_filters_foreign_fingerprints() {
    use ltmrs_domain::id::ChunkId;
    fn row_fp(id: u64, fp: u64, embedding: Option<Vec<f32>>) -> SearchRow {
        SearchRow {
            store_generation: ltmrs_domain::id::StoreGeneration::FIRST,
            memory_id: eid(id),
            document_revision: ltmrs_domain::id::DocumentRevision::new(1),
            model_fingerprint: ltmrs_domain::id::ModelFingerprint::new(fp),
            chunk_id: ChunkId::new(0),
            chunker_version: "v1".to_string(),
            lexical_text: "t".to_string(),
            char_start: 0,
            char_end: 1,
            project: None,
            fragment_type: "fact".to_string(),
            created_at_millis: 0,
            confidence: 0.5,
            updated_at_millis: 0,
            embedding,
        }
    }
    let lexical = vec![row_fp(1, 2, Some(vec![9.0, 9.0]))];
    let dense = vec![row_fp(1, 1, Some(vec![1.0, 1.0]))];
    assert_eq!(
        mean_vector(&lexical, &dense, eid(1), Some(ModelFingerprint::new(1))),
        Some(vec![1.0, 1.0]),
        "foreign-fingerprint rows must not pollute the mean"
    );
    assert_eq!(
        mean_vector(&lexical, &[], eid(1), Some(ModelFingerprint::new(1))),
        None,
        "foreign-only rows yield no vector, not a mixed-space mean"
    );
}

/// Without a requested fingerprint the dominant space wins (most rows,
/// ties to the smallest): blue-green windows average coherently instead
/// of mixing vector spaces.
#[test]
fn mean_vector_without_fingerprint_uses_dominant_space() {
    use ltmrs_domain::id::ChunkId;
    fn row_fp_chunk(id: u64, fp: u64, chunk: u32, embedding: Option<Vec<f32>>) -> SearchRow {
        SearchRow {
            store_generation: ltmrs_domain::id::StoreGeneration::FIRST,
            memory_id: eid(id),
            document_revision: ltmrs_domain::id::DocumentRevision::new(1),
            model_fingerprint: ltmrs_domain::id::ModelFingerprint::new(fp),
            chunk_id: ChunkId::new(chunk),
            chunker_version: "v1".to_string(),
            lexical_text: "t".to_string(),
            char_start: 0,
            char_end: 1,
            project: None,
            fragment_type: "fact".to_string(),
            created_at_millis: 0,
            confidence: 0.5,
            updated_at_millis: 0,
            embedding,
        }
    }
    // Two fp-1 chunks beat one fp-2 chunk: mean over fp-1 only.
    let rows = vec![
        row_fp_chunk(1, 1, 0, Some(vec![1.0, 1.0])),
        row_fp_chunk(1, 1, 1, Some(vec![3.0, 3.0])),
        row_fp_chunk(1, 2, 2, Some(vec![9.0, 9.0])),
    ];
    assert_eq!(mean_vector(&rows, &[], eid(1), None), Some(vec![2.0, 2.0]));
    // Tie breaks to the smallest fingerprint, deterministically.
    let rows = vec![
        row_fp_chunk(1, 2, 0, Some(vec![8.0, 8.0])),
        row_fp_chunk(1, 1, 1, Some(vec![2.0, 2.0])),
    ];
    assert_eq!(mean_vector(&rows, &[], eid(1), None), Some(vec![2.0, 2.0]));
}

/// Election counts rows, mean dedupes chunks: fp-2 chunk 0 in both
/// legs (2 votes) beats fp-1 chunk 1 in one (1 vote) even though the
/// chunk race is tied 1-1. Row-vote semantics pinned (matches docs).
#[test]
fn mean_vector_election_counts_rows_not_chunks() {
    use ltmrs_domain::id::ChunkId;
    fn row_fp_chunk2(id: u64, fp: u64, chunk: u32, embedding: Option<Vec<f32>>) -> SearchRow {
        SearchRow {
            store_generation: ltmrs_domain::id::StoreGeneration::FIRST,
            memory_id: eid(id),
            document_revision: ltmrs_domain::id::DocumentRevision::new(1),
            model_fingerprint: ltmrs_domain::id::ModelFingerprint::new(fp),
            chunk_id: ChunkId::new(chunk),
            chunker_version: "v1".to_string(),
            lexical_text: "t".to_string(),
            char_start: 0,
            char_end: 1,
            project: None,
            fragment_type: "fact".to_string(),
            created_at_millis: 0,
            confidence: 0.5,
            updated_at_millis: 0,
            embedding,
        }
    }
    let lexical = vec![
        row_fp_chunk2(1, 2, 0, Some(vec![4.0, 4.0])),
        row_fp_chunk2(1, 1, 1, Some(vec![1.0, 1.0])),
    ];
    let dense = vec![row_fp_chunk2(1, 2, 0, Some(vec![4.0, 4.0]))];
    assert_eq!(
        mean_vector(&lexical, &dense, eid(1), None),
        Some(vec![4.0, 4.0]),
        "row votes (fp2 x2) beat chunk tie"
    );
}

/// Confidence pre-filters at the source: a low-confidence row that
/// ranks top lexically is excluded before candidate-limit truncation
/// can crowd out eligible rows. (Post-filtering alone cannot save this:
/// with limit 1 the top hit is dropped after truncation, leaving
/// nothing — the eligible row never enters the pool. Dense disabled
/// here to isolate the lexical pre-filter; both legs share the predicate.
/// No ANN index exists today, so both legs exact-scan with the filter
/// applied before top-k: adding an ANN index requires a dense-leg
/// crowding variant of this test.)
#[tokio::test]
async fn confidence_prefilters_at_source() {
    let (repo, table, mut proj, _guard) = env().await;
    add(&repo, 1, "alpha beta", &"alpha beta ".repeat(10), None);
    add(&repo, 2, "alpha beta", "alpha beta delta", None);
    // Demote the lexical winner below the filter floor.
    {
        let mut low = repo.get_memories(&[eid(1)]).unwrap().remove(0);
        low.confidence = 0.1;
        repo.put_memory_direct(&low).unwrap();
        let mut high = repo.get_memories(&[eid(2)]).unwrap().remove(0);
        high.confidence = 0.9;
        repo.put_memory_direct(&high).unwrap();
    }
    proj.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();

    let mut req = RetrievalRequest {
        query: "alpha beta".into(),
        candidate_limit: 1,
        model_fingerprint: None,
        ..base_req()
    };
    req.scope.min_confidence = Some(0.5);
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    let ids: Vec<EntityId> = result.results.iter().map(|r| r.memory.id).collect();
    assert_eq!(
        ids,
        vec![eid(2)],
        "pre-filter must admit the eligible row, got {ids:?}"
    );
}

/// Post-convergence confidence drift heals through the worker: a
/// negative-feedback demotion (0.5 -> 0.48) enqueues a refresh, so the
/// next projection carries the lowered confidence and the source
/// pre-filter excludes the row — no silent stale-high reads.
#[tokio::test]
async fn feedback_drift_refreshes_projection() {
    let (repo, table, mut proj, _guard) = env().await;
    add(&repo, 1, "alpha beta", &"alpha beta ".repeat(10), None);
    add(&repo, 2, "alpha beta", "alpha beta delta", None);
    // Converge first: the rows publish at 0.5. The demotion below
    // lands post-convergence, so only a refresh heals the projection
    // (a pre-convergence write would publish fresh trivially).
    proj.run_until_idle().await.unwrap();
    // Demote the lexical winner below the filter floor via feedback
    // (not a direct write): this must enqueue a projection refresh.
    repo.apply(
        &ctx(50),
        &DomainCommand::Feedback {
            memory_id: eid(1),
            useful: false,
        },
    )
    .unwrap();
    proj.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();

    let mut req = RetrievalRequest {
        query: "alpha beta".into(),
        candidate_limit: 1,
        model_fingerprint: None,
        ..base_req()
    };
    req.scope.min_confidence = Some(0.5);
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    let ids: Vec<EntityId> = result.results.iter().map(|r| r.memory.id).collect();
    assert_eq!(
        ids,
        vec![eid(2)],
        "refreshed projection must exclude the demoted row, got {ids:?}"
    );
}

/// Task 10: finite-score validation. Every returned candidate's score
/// components must be finite and within their documented bounds.
#[tokio::test]
async fn all_scores_finite_and_bounded() {
    let (repo, table, mut proj, _guard) = env().await;
    add(&repo, 1, "rust async", "tokio runtime details", None);
    add(
        &repo,
        2,
        "python asyncio",
        "asyncio event loop details",
        None,
    );
    proj.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();

    let req = RetrievalRequest {
        query: "async runtime event loop".into(),
        ..base_req()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();

    for cand in result.explanation.candidates.values() {
        let s = &cand.scores;
        assert!(s.rrf_normalized.is_finite());
        assert!(s.graph.is_finite());
        assert!(s.priority.is_finite());
        assert!(s.native_score.is_finite());
        assert!(s.legacy_reference.is_finite());
        assert!((0.0..=1.0).contains(&s.rrf_normalized));
        assert!((0.0..=1.0).contains(&s.graph));
        assert!((0.0..=1.0).contains(&s.priority));
        assert!((0.0..=1.0).contains(&s.native_score));
    }
}
