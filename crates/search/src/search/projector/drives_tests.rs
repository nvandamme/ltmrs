//! Projection drive / convergence tests (moved verbatim from `projector.rs`).

use super::test_support::*;
use super::{Embedder, FixedEmbedder, Projector, ProjectorOutcome};
use ltmrs_domain::command::{DomainCommand, ForgetMode};
use ltmrs_domain::id::{DocumentRevision, ModelFingerprint, StoreGeneration};

/// Table-measured convergence: every live recallable memory has at
/// least one row in the generation (vectors not required — lexical rows
/// count; deleted memories are excluded, not required).
#[tokio::test]
async fn verify_generation_converged_detects_missing() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "one", "first body");
    add(&repo, 2, "two", "second body");

    let mut p = projector(repo.clone(), table.clone());
    // Project only the first memory.
    let job = repo.projection_job(eid(1)).unwrap().unwrap();
    assert_eq!(
        p.process_job(&job).await.unwrap(),
        ProjectorOutcome::Published
    );
    assert!(
        !p.verify_generation_converged(StoreGeneration::FIRST)
            .await
            .unwrap(),
        "second memory has no rows yet"
    );
    // Converge the second memory: now the generation is complete.
    let job = repo.projection_job(eid(2)).unwrap().unwrap();
    assert_eq!(
        p.process_job(&job).await.unwrap(),
        ProjectorOutcome::Published
    );
    assert!(
        p.verify_generation_converged(StoreGeneration::FIRST)
            .await
            .unwrap()
    );
}

/// Convergence is scoped per model space: a projector for fingerprint
/// 2 must not report converged off fingerprint-1 rows (AD-04: never
/// mix vector spaces, including in watermarks).
#[tokio::test]
async fn verify_generation_is_fingerprint_scoped() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "one", "first body");

    let mut p1 = projector(repo.clone(), table.clone());
    p1.run_until_idle().await.unwrap();
    assert!(
        p1.verify_generation_converged(StoreGeneration::FIRST)
            .await
            .unwrap(),
        "own space converged"
    );
    let p2 = Projector::new(
        repo,
        table,
        Box::new(FixedEmbedder { dim: 384 }),
        ModelFingerprint::new(2),
        StoreGeneration::FIRST,
    );
    assert!(
        !p2.verify_generation_converged(StoreGeneration::FIRST)
            .await
            .unwrap(),
        "foreign-space rows must not count as converged"
    );
}

/// Convergence is scoped per generation and ignores deleted memories.
#[tokio::test]
async fn verify_generation_is_scoped_and_ignores_deleted() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "one", "first body");
    add(&repo, 2, "two", "second body");

    let mut p = projector(repo.clone(), table.clone());
    p.run_until_idle().await.unwrap();
    assert!(
        p.verify_generation_converged(StoreGeneration::FIRST)
            .await
            .unwrap()
    );
    // An empty generation with no rows is unconverged while live
    // memories exist.
    assert!(
        !p.verify_generation_converged(StoreGeneration::new(9))
            .await
            .unwrap()
    );
    // Deleting a converged memory keeps the generation converged: the
    // tombstone path removes its rows and it is no longer required.
    repo.apply(
        &ctx(3),
        &DomainCommand::Forget {
            id: eid(1),
            mode: ForgetMode::Delete,
        },
    )
    .unwrap();
    p.run_until_idle().await.unwrap();
    assert!(
        p.verify_generation_converged(StoreGeneration::FIRST)
            .await
            .unwrap()
    );
    assert_eq!(table.count_rows(None).await.unwrap(), 1);
}

/// Deterministic fake for the drive seam: constant vectors sized by
/// input length; `fail` makes every embed error.
struct VecFake {
    dim: usize,
    fail: bool,
}

impl Embedder for VecFake {
    fn embed(&mut self, text: &str) -> Result<Vec<f32>, String> {
        if self.fail {
            return Err("fake embedder stalled".into());
        }
        Ok(vec![text.len() as f32; self.dim])
    }
}

/// One-shot production drive: pending jobs project with the given
/// embedder under the E5 fingerprint, and jobs acknowledge.
#[tokio::test]
async fn project_pending_publishes_dense_rows() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "hello", "world");
    assert_eq!(
        Projector::project_pending(
            &repo,
            &table,
            Box::new(VecFake {
                dim: 384,
                fail: false
            }),
            10
        )
        .await
        .unwrap(),
        1
    );
    // Job acknowledged, and the row carries the E5 fingerprint + vector.
    assert!(!repo.has_pending_projection(eid(1)).unwrap());
    let rows = table.rows_where("embedding IS NOT NULL").await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].model_fingerprint,
        ltmrs_embeddings::e5_small::E5_SMALL_FINGERPRINT
    );
    assert_eq!(rows[0].embedding.as_ref().unwrap().len(), 384);
}

/// A stalled embedder resolves nothing but still publishes lexical rows:
/// the job stays pending for the next pass (retry), never lost.
#[tokio::test]
async fn project_pending_leaves_semantic_retry_on_embed_failure() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "hello", "world");
    assert_eq!(
        Projector::project_pending(
            &repo,
            &table,
            Box::new(VecFake {
                dim: 384,
                fail: true
            }),
            10
        )
        .await
        .unwrap(),
        0
    );
    // Nothing acknowledged, but the lexical row is searchable.
    assert!(repo.has_pending_projection(eid(1)).unwrap());
    assert_eq!(table.count_rows(None).await.unwrap(), 1);
    assert!(
        table
            .rows_where("embedding IS NOT NULL")
            .await
            .unwrap()
            .is_empty()
    );
}

/// The drive honors a per-tick job cap (fairness §7.3): beyond the cap,
/// extra jobs stay pending for the next tick instead of one unbounded
/// bulk pass starving interactive recall.
#[tokio::test]
async fn project_pending_respects_job_cap() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "a", "1");
    add(&repo, 2, "b", "2");
    add(&repo, 3, "c", "3");
    assert_eq!(
        Projector::project_pending(
            &repo,
            &table,
            Box::new(VecFake {
                dim: 384,
                fail: false
            }),
            2
        )
        .await
        .unwrap(),
        2
    );
    assert_eq!(repo.projection_jobs().unwrap().len(), 1);
    // The remainder converges on the next drive.
    assert_eq!(
        Projector::project_pending(
            &repo,
            &table,
            Box::new(VecFake {
                dim: 384,
                fail: false
            }),
            2
        )
        .await
        .unwrap(),
        1
    );
    assert!(repo.projection_jobs().unwrap().is_empty());
}

/// Re-review P2-1: a pending job whose memory is absent (orphaned)
/// retires instead of being recounted as progress every pass. A capped
/// drive resolves it once; the next drive finds nothing — no
/// false-progress loop for the drain worker to spin on.
#[tokio::test]
async fn orphaned_pending_job_retires_instead_of_looping() {
    let (repo, table, _guard) = env().await;
    let ghost = eid(4242);
    repo.enqueue_projection_job(ghost, DocumentRevision::new(7), 1, false)
        .unwrap();
    assert_eq!(repo.projection_jobs().unwrap().len(), 1);
    let drive = |limit: usize| {
        Projector::project_pending(
            &repo,
            &table,
            Box::new(VecFake {
                dim: 384,
                fail: false,
            }),
            limit,
        )
    };
    assert_eq!(drive(100).await.unwrap(), 1);
    assert!(
        repo.projection_jobs().unwrap().is_empty(),
        "orphaned job must be retired, not left pending"
    );
    assert_eq!(drive(100).await.unwrap(), 0);
}

/// Idempotency: a second drive immediately after a converged one
/// resolves nothing (compare-and-clear actually cleared). A repeat
/// resolution here means acknowledgements are lost and any
/// long-running drive spins forever re-publishing.
#[tokio::test]
async fn project_pending_second_drive_resolves_nothing() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "hello", "world");
    add(&repo, 2, "foo", "bar");
    assert_eq!(
        Projector::project_pending(
            &repo,
            &table,
            Box::new(VecFake {
                dim: 384,
                fail: false
            }),
            10
        )
        .await
        .unwrap(),
        2
    );
    assert_eq!(
        Projector::project_pending(
            &repo,
            &table,
            Box::new(VecFake {
                dim: 384,
                fail: false
            }),
            10
        )
        .await
        .unwrap(),
        0,
        "converged state must stay converged"
    );
}

/// Batch default maps per-text results 1:1 in order, preserving errors.
#[test]
fn embed_texts_default_preserves_order_and_errors() {
    use crate::search::projector::Embedder as _;

    struct Flaky {
        calls: usize,
    }
    impl Embedder for Flaky {
        fn embed(&mut self, text: &str) -> Result<Vec<f32>, String> {
            self.calls += 1;
            if text == "bad" {
                return Err("stalled".into());
            }
            Ok(vec![text.len() as f32; 4])
        }
    }
    let mut fx = Flaky { calls: 0 };
    let out = fx.embed_texts(&["ok".to_string(), "bad".to_string(), "ok2".to_string()]);
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].as_ref().unwrap(), &vec![2.0; 4]);
    assert!(out[1].is_err());
    assert_eq!(out[2].as_ref().unwrap(), &vec![3.0; 4]);
    assert_eq!(fx.calls, 3);
}
