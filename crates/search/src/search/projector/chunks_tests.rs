//! Multi-chunk projection tests (moved verbatim from `projector.rs`).

use super::test_support::*;
use super::{Projector, ProjectorOutcome, render_text};
use ltmrs_domain::command::DomainCommand;
use ltmrs_domain::id::{ModelFingerprint, StoreGeneration};

/// Projected rows carry the embedder's chunker version so a policy
/// change is attributable per row (never silently mixed).
#[tokio::test]
async fn projected_rows_carry_chunker_version() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "long", "alpha-half\nbeta-half");
    let job = repo.projection_job(eid(1)).unwrap().unwrap();
    let mut p = Projector::new(
        repo.clone(),
        table.clone(),
        Box::new(HalvingEmbedder {
            dim: 384,
            fail: false,
        }),
        ModelFingerprint::new(1),
        StoreGeneration::FIRST,
    );
    assert_eq!(
        p.process_job(&job).await.unwrap(),
        ProjectorOutcome::Published
    );
    let rows = table
        .rows_where("chunker_version = 'test-halving-v1'")
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "every chunk row carries the version");
    assert_eq!(table.count_rows(None).await.unwrap(), 2);
}

/// RQ-10: a long memory projects one row per chunk (not a single
/// first-window row). Each row carries its chunk id, evidence span and
/// vector; the job is acknowledged only when every chunk is embedded.
#[tokio::test]
async fn long_memory_projects_one_row_per_chunk() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "long", "alpha-half\nbeta-half");
    let job = repo.projection_job(eid(1)).unwrap().unwrap();

    let mut p = Projector::new(
        repo.clone(),
        table.clone(),
        Box::new(HalvingEmbedder {
            dim: 384,
            fail: false,
        }),
        ModelFingerprint::new(1),
        StoreGeneration::FIRST,
    );
    assert_eq!(
        p.process_job(&job).await.unwrap(),
        ProjectorOutcome::Published
    );
    assert!(!repo.has_pending_projection(eid(1)).unwrap());

    let rows = table.rows_where("embedding IS NOT NULL").await.unwrap();
    assert_eq!(rows.len(), 2, "one vector row per chunk expected");
    let mut chunk_ids: Vec<u32> = rows.iter().map(|r| r.chunk_id.as_u32()).collect();
    chunk_ids.sort_unstable();
    assert_eq!(chunk_ids, vec![0, 1]);
    // Evidence spans are disjoint and jointly cover the fragment body.
    let mut spans: Vec<(u64, u64)> = rows.iter().map(|r| (r.char_start, r.char_end)).collect();
    spans.sort_unstable();
    assert!(spans[0].1 <= spans[1].0, "chunk spans must not overlap");
    let title_len = "long".len() as u64;
    assert_eq!(
        spans[0].0,
        title_len + 1,
        "first span must start at the fragment body"
    );
    assert_eq!(
        spans[0].1, spans[1].0,
        "adjacent chunk spans must join with no dropped middle"
    );
    assert_eq!(
        spans[1].1,
        render_text("long", "alpha-half\nbeta-half").len() as u64,
        "last span must reach the end of the rendered text"
    );
    assert!(
        rows.iter().any(|r| r.lexical_text.contains("alpha-half")),
        "first-half content must be projected"
    );
    assert!(
        rows.iter().any(|r| r.lexical_text.contains("beta-half")),
        "tail content beyond the first window must be projected"
    );
}

/// A chunked rebuild supersedes as a unit: after a content update the new
/// revision has exactly the new chunk set and no row of the old revision
/// survives (exercises table.rs group cleanup with multi-chunk sets).
#[tokio::test]
async fn chunked_rebuild_supersedes_as_unit() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "long", "alpha-half\nbeta-half");

    let mut p = Projector::new(
        repo.clone(),
        table.clone(),
        Box::new(HalvingEmbedder {
            dim: 384,
            fail: false,
        }),
        ModelFingerprint::new(1),
        StoreGeneration::FIRST,
    );
    assert_eq!(p.rebuild().await.unwrap(), 1);
    assert_eq!(table.count_rows(None).await.unwrap(), 2);

    // Update content: canonical document_revision advances to 2.
    let patch = ltmrs_domain::command::MemoryPatch {
        fragment: Some("gamma-half\ndelta-half".into()),
        ..Default::default()
    };
    repo.apply(
        &ctx(2),
        &DomainCommand::UpdateMemory {
            id: eid(1),
            expected_revision: None,
            patch,
        },
    )
    .unwrap();
    assert_eq!(p.rebuild().await.unwrap(), 1);

    let rev2 = table.rows_where("document_revision = 2").await.unwrap();
    assert_eq!(rev2.len(), 2, "new revision must carry the full chunk set");
    let rev1 = table.rows_where("document_revision = 1").await.unwrap();
    assert_eq!(rev1.len(), 0, "no old-revision chunk may survive");
    assert_eq!(table.count_rows(None).await.unwrap(), 2);
}

/// All-or-pending chunk policy: when any chunk embedding fails, every
/// chunk still gets its lexical row now, the job stays pending, and a
/// recovered worker converges all chunks to vectors without duplicates.
#[tokio::test]
async fn stalled_chunking_embedder_leaves_all_chunks_lexical_and_pending() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "long", "alpha-half\nbeta-half");
    let job = repo.projection_job(eid(1)).unwrap().unwrap();

    let mut p_stalled = Projector::new(
        repo.clone(),
        table.clone(),
        Box::new(HalvingEmbedder {
            dim: 384,
            fail: true,
        }),
        ModelFingerprint::new(1),
        StoreGeneration::FIRST,
    );
    assert_eq!(
        p_stalled.process_job(&job).await.unwrap(),
        ProjectorOutcome::SemanticPending
    );
    // Both chunks indexed lexically; no vector claimed.
    assert_eq!(table.count_rows(None).await.unwrap(), 2);
    assert_eq!(
        table.count_rows(Some("embedding IS NULL")).await.unwrap(),
        2
    );
    assert!(repo.has_pending_projection(eid(1)).unwrap());

    // A recovered worker completes every chunk; replay is idempotent.
    let mut p_working = Projector::new(
        repo.clone(),
        table.clone(),
        Box::new(HalvingEmbedder {
            dim: 384,
            fail: false,
        }),
        ModelFingerprint::new(1),
        StoreGeneration::FIRST,
    );
    p_working.run_until_idle().await.unwrap();
    assert!(!repo.has_pending_projection(eid(1)).unwrap());
    assert_eq!(table.count_rows(None).await.unwrap(), 2);
    assert_eq!(
        table
            .count_rows(Some("embedding IS NOT NULL"))
            .await
            .unwrap(),
        2
    );
}

/// Mixed partial embeddings stay pending: when only one chunk has a
/// vector, the job must NOT be acknowledged (an `any`-instead-of-`all`
/// ack check must fail this test). Both chunks stay lexically indexed.
#[tokio::test]
async fn mixed_partial_embedding_stays_pending() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "long", "alpha-half\nbeta-half");
    let job = repo.projection_job(eid(1)).unwrap().unwrap();

    let mut p = Projector::new(
        repo.clone(),
        table.clone(),
        Box::new(SecondChunkFailsEmbedder { dim: 384 }),
        ModelFingerprint::new(1),
        StoreGeneration::FIRST,
    );
    assert_eq!(
        p.process_job(&job).await.unwrap(),
        ProjectorOutcome::SemanticPending,
        "one missing chunk vector must keep the job pending"
    );
    assert_eq!(table.count_rows(None).await.unwrap(), 2);
    assert_eq!(
        table
            .count_rows(Some("embedding IS NOT NULL"))
            .await
            .unwrap(),
        1,
        "only the successful chunk may carry a vector"
    );
    assert!(repo.has_pending_projection(eid(1)).unwrap());
}
