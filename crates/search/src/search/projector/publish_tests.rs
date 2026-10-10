//! Projection publish-path tests (moved verbatim from `projector.rs`).

use super::test_support::*;
use super::{FixedEmbedder, Projector, ProjectorOutcome, StalledEmbedder};
use crate::search::row::SearchRow;
use crate::search::table::SearchTable;
use ltmrs_domain::command::{DomainCommand, ForgetMode};
use ltmrs_domain::id::{ChunkId, DocumentRevision, ModelFingerprint, StoreGeneration};

#[tokio::test]
async fn process_job_publishes_and_acknowledges() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "hello", "world");
    let job = repo.projection_job(eid(1)).unwrap().unwrap();

    let mut p = projector(repo.clone(), table.clone());
    assert_eq!(
        p.process_job(&job).await.unwrap(),
        ProjectorOutcome::Published
    );

    // Job acknowledged (no longer pending) and a row is in the projection.
    assert!(!repo.has_pending_projection(eid(1)).unwrap());
    let count = table.count_rows(None).await.unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn late_old_embedding_cannot_regress_published_revision() {
    let (repo, table, _guard) = env().await;

    // Add memory (canonical document_revision = 1).
    add(&repo, 1, "t", "f");
    // Update content → canonical document_revision = 2.
    let patch = ltmrs_domain::command::MemoryPatch {
        fragment: Some("new body".into()),
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

    let mut p = projector(repo.clone(), table.clone());

    // A stale worker holds an embedding rendered at document_revision=1.
    let stale_row = SearchRow {
        store_generation: StoreGeneration::FIRST,
        memory_id: eid(1),
        document_revision: DocumentRevision::new(1),
        model_fingerprint: ModelFingerprint::new(1),
        chunk_id: ChunkId::new(0),
        chunker_version: "single-chunk-v1".to_string(),
        lexical_text: "stale text".into(),
        char_start: 0,
        char_end: 10,
        project: None,
        fragment_type: "fact".into(),
        created_at_millis: 0,
        confidence: 0.5,
        updated_at_millis: 0,
        embedding: Some(vec![0.0; 384]),
    };

    // The guard must reject the stale row (canonical is now at rev 2).
    assert!(
        !p.publish_guarded(&stale_row).await.unwrap(),
        "a late old embedding must not be published over a newer revision"
    );
    let count = table.count_rows(None).await.unwrap();
    assert_eq!(count, 0, "no stale row may land in the projection");
}

#[tokio::test]
async fn publish_guarded_accepts_matching_revision() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "t", "f");

    let mut p = projector(repo.clone(), table.clone());

    // A row at the current canonical revision (1) is accepted.
    let ok_row = SearchRow {
        store_generation: StoreGeneration::FIRST,
        memory_id: eid(1),
        document_revision: DocumentRevision::new(1),
        model_fingerprint: ModelFingerprint::new(1),
        chunk_id: ChunkId::new(0),
        chunker_version: "single-chunk-v1".to_string(),
        lexical_text: "current text".into(),
        char_start: 0,
        char_end: 12,
        project: None,
        fragment_type: "fact".into(),
        created_at_millis: 0,
        confidence: 0.5,
        updated_at_millis: 0,
        embedding: Some(vec![1.0; 384]),
    };

    assert!(p.publish_guarded(&ok_row).await.unwrap());
    let count = table.count_rows(None).await.unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn deletion_propagates_to_projection() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "t", "f");

    let mut p = projector(repo.clone(), table.clone());
    // Publish a row first.
    let ok_row = SearchRow {
        store_generation: StoreGeneration::FIRST,
        memory_id: eid(1),
        document_revision: DocumentRevision::new(1),
        model_fingerprint: ModelFingerprint::new(1),
        chunk_id: ChunkId::new(0),
        chunker_version: "single-chunk-v1".to_string(),
        lexical_text: "x".into(),
        char_start: 0,
        char_end: 1,
        project: None,
        fragment_type: "fact".into(),
        created_at_millis: 0,
        confidence: 0.5,
        updated_at_millis: 0,
        embedding: Some(vec![1.0; 384]),
    };
    p.publish_guarded(&ok_row).await.unwrap();
    assert_eq!(table.count_rows(None).await.unwrap(), 1);

    // Forgetting the memory must remove its projected rows.
    repo.apply(
        &ctx(2),
        &DomainCommand::Forget {
            id: eid(1),
            mode: ForgetMode::Delete,
        },
    )
    .unwrap();
    p.propagate_deletion(eid(1)).await.unwrap();

    assert_eq!(
        table.count_rows(None).await.unwrap(),
        0,
        "deleted memory's rows must be removed from the projection"
    );
}

#[tokio::test]
async fn process_job_returns_tombstoned_for_deleted_memory() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "t", "f");

    // A job exists for the live memory.
    let job = repo.projection_job(eid(1)).unwrap().unwrap();

    // Forget it before projecting: rows must be removed and outcome is Tombstoned.
    repo.apply(
        &ctx(2),
        &DomainCommand::Forget {
            id: eid(1),
            mode: ForgetMode::Delete,
        },
    )
    .unwrap();

    let mut p = projector(repo.clone(), table.clone());
    // The pre-forget job object is superseded by the forget's tombstone
    // job: it must report stale (never clear newer work), not claim a
    // tombstone it did not publish.
    assert_eq!(
        p.process_job(&job).await.unwrap(),
        ProjectorOutcome::StaleRevision
    );
    // The worker always drives the CURRENT job: the tombstone retires,
    // rows are gone, and nothing stays pending.
    let current = repo.projection_job(eid(1)).unwrap().unwrap();
    assert_eq!(
        p.process_job(&current).await.unwrap(),
        ProjectorOutcome::Tombstoned
    );
    let count = table.count_rows(None).await.unwrap();
    assert_eq!(count, 0);
    assert!(repo.projection_jobs().unwrap().is_empty());
}

/// Task 6: events are retryable wakeups, not an ordered commit log. A job
/// whose desired revision is already stale (a newer mutation superseded it)
/// must be left pending, never force-published — so processing order cannot
/// corrupt the projection.
#[tokio::test]
async fn out_of_order_stale_job_is_left_pending_not_published() {
    let (repo, table, _guard) = env().await;

    // Add memory → canonical document_revision 1, job seq=1 desired_rev=1.
    add(&repo, 1, "t", "f");
    let stale_job = repo.projection_job(eid(1)).unwrap().unwrap();
    assert_eq!(stale_job.seq, 1);

    // A concurrent update advances canonical to rev 2 and the job to seq=2.
    let patch = ltmrs_domain::command::MemoryPatch {
        fragment: Some("v2".into()),
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

    // A delayed worker replays the OLD job (seq=1) out of order. It must be
    // refused as stale — not published and not acknowledged.
    let mut p = projector(repo.clone(), table.clone());
    assert_eq!(
        p.process_job(&stale_job).await.unwrap(),
        ProjectorOutcome::StaleRevision,
        "a late/out-of-order job must be left pending"
    );

    // Work is still pending (the newer seq=2 job remains), and nothing stale landed.
    assert!(repo.has_pending_projection(eid(1)).unwrap());
    let count = table.count_rows(None).await.unwrap();
    assert_eq!(count, 0);

    // Processing the CURRENT job now converges to exactly one row at rev 2.
    let current_job = repo.projection_job(eid(1)).unwrap().unwrap();
    assert_eq!(current_job.seq, 2);
    assert_eq!(
        p.process_job(&current_job).await.unwrap(),
        ProjectorOutcome::Published
    );
    let count = table.count_rows(None).await.unwrap();
    assert_eq!(count, 1, "the projection converges to the latest revision");
}

/// Task 7: a rebuild must never resurrect a deleted generation. Deleted and
/// live memories coexist canonically; the rebuild projects only the live one
/// and removes any rows for the deleted one.
#[tokio::test]
async fn rebuild_does_not_resurrect_deleted_memory() {
    let (repo, table, _guard) = env().await;

    // Two memories: 1 will be deleted, 2 stays live.
    add(&repo, 1, "doomed", "gone");
    add(&repo, 2, "keeper", "stays");

    let mut p = projector(repo.clone(), table.clone());
    // First publish both while live (simulating prior state).
    for n in [1u64, 2] {
        if let Some(job) = repo.projection_job(eid(n)).unwrap() {
            let _ = p.process_job(&job).await.unwrap();
        }
    }
    assert_eq!(table.count_rows(None).await.unwrap(), 2);

    // Delete memory 1 canonically.
    repo.apply(
        &ctx(3),
        &DomainCommand::Forget {
            id: eid(1),
            mode: ForgetMode::Delete,
        },
    )
    .unwrap();

    // Rebuild from canonical state: only the live memory is projected and the
    // deleted one's rows are removed.
    let published = p.rebuild().await.unwrap();
    assert_eq!(published, 1, "only recallable memories may be rebuilt");

    // The deleted memory cannot be resurrected by the rebuild.
    let pred = format!("memory_id = '{}'", eid(1).as_uuid());
    assert_eq!(table.count_rows(Some(&pred)).await.unwrap(), 0);
    let total = table.count_rows(None).await.unwrap();
    assert_eq!(total, 1, "deleted generation must not reappear");
}

/// Task 8: blue-green model/index generation. A new fingerprint builds its own
/// rows without touching the old generation's; publication is per-fingerprint,
/// so readers of the old space keep working until they drain.
#[tokio::test]
async fn blue_green_generations_are_isolated_by_fingerprint() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "t", "f");

    // Green projector with a NEW model fingerprint.
    let green_fp = ModelFingerprint::new(2);
    let mut p_green = Projector::new(
        repo.clone(),
        table.clone(),
        Box::new(FixedEmbedder { dim: 384 }),
        green_fp,
        StoreGeneration::FIRST,
    );

    // Build the new generation's rows.
    if let Some(job) = repo.projection_job(eid(1)).unwrap() {
        assert_eq!(
            p_green.process_job(&job).await.unwrap(),
            ProjectorOutcome::Published
        );
    }

    // The row is tagged with the NEW fingerprint only — the old space (fp=1)
    // has nothing, so a reader pinned to the old model sees no false hits.
    let pred_new = format!("model_fingerprint = {}", green_fp.as_u64());
    assert_eq!(table.count_rows(Some(&pred_new)).await.unwrap(), 1);

    let pred_old = "model_fingerprint = 1";
    assert_eq!(
        table.count_rows(Some(pred_old)).await.unwrap(),
        0,
        "a new model must not write into the old vector space"
    );
}

/// A projector with the wrong fingerprint cannot advance a staged build:
/// staged publication is bound to the recorded build fingerprint so
/// vector spaces never mix silently.
#[tokio::test]
async fn staged_build_rejects_wrong_fingerprint() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "t", "f");
    let gen2 = repo.stage_generation(ModelFingerprint::new(7)).unwrap();

    let mut p_wrong = Projector::new(
        repo.clone(),
        table.clone(),
        Box::new(FixedEmbedder { dim: 384 }),
        ModelFingerprint::new(8),
        gen2,
    );
    let job = repo.projection_job(eid(1)).unwrap().unwrap();
    assert_eq!(
        p_wrong.process_job(&job).await.unwrap(),
        ProjectorOutcome::StaleRevision,
        "wrong-fingerprint rows must be refused during a staged build"
    );
    assert_eq!(table.count_rows(None).await.unwrap(), 0);
}

/// Task 6: replaying an already-acknowledged job is idempotent — a retried
/// wakeup cannot double-publish or resurrect cleared work.
#[tokio::test]
async fn replayed_acknowledged_job_is_idempotent() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "t", "f");
    let job = repo.projection_job(eid(1)).unwrap().unwrap();

    let mut p = projector(repo.clone(), table.clone());
    assert_eq!(
        p.process_job(&job).await.unwrap(),
        ProjectorOutcome::Published
    );
    assert!(!repo.has_pending_projection(eid(1)).unwrap());

    // The durable job is gone; a retried wakeup for the same seq finds no work.
    let outcome = p.process_job(&job).await.unwrap();
    // Re-processing an already-cleared, still-current memory republishes
    // idempotently (same revision) — never a duplicate row.
    assert_eq!(outcome, ProjectorOutcome::Published);
    let count = table.count_rows(None).await.unwrap();
    assert_eq!(count, 1, "replaying a wakeup must not create duplicates");
}

// ---- Plan acceptance tests (plans/03 §T-PROJ / T-SEARCH) ----

/// T-PROJ-01: commit events in an order different from their UUID
/// timestamps; kill between canonical commit, projection commit and
/// acknowledgement. No late event is skipped; replay is idempotent; a newer
/// desired revision stays pending.
#[tokio::test]
async fn t_proj_01_kill_between_commit_and_ack_keeps_newer_work_pending() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "v1", "f");

    // Canonical commit is durable: a job exists. Simulate a crash BEFORE the
    // projector reads it — on restart the same job is still pending.
    let mut p = projector(repo.clone(), table.clone());
    let job_v1 = repo
        .projection_job(eid(1))
        .unwrap()
        .expect("job survives restart");

    // A late update commits while the old worker is still "in flight". The
    // old worker's compare-and-clear must be refused (seq advanced).
    let patch = ltmrs_domain::command::MemoryPatch {
        fragment: Some("v2 body".into()),
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
    let job_v2 = repo
        .projection_job(eid(1))
        .unwrap()
        .expect("update enqueues a newer job");
    assert!(job_v2.seq > job_v1.seq);
    assert_eq!(job_v2.desired_document_revision, DocumentRevision::new(2));

    // The stale worker's acknowledgement is refused: the newer revision stays
    // pending and no trace remains.
    assert!(!repo.acknowledge_projection(eid(1), job_v1.seq).unwrap());
    let still_pending = repo
        .projection_job(eid(1))
        .unwrap()
        .expect("newer work survives");
    assert_eq!(still_pending, job_v2);

    // Processing the newer job publishes exactly once; replaying it is
    // idempotent (same revision republishes, never duplicates).
    assert_eq!(
        p.process_job(&job_v2).await.unwrap(),
        ProjectorOutcome::Published
    );
    assert!(!repo.has_pending_projection(eid(1)).unwrap());
    let outcome = p.process_job(&job_v2).await.unwrap();
    assert_eq!(outcome, ProjectorOutcome::Published);
    assert_eq!(table.count_rows(None).await.unwrap(), 1);
}

/// T-PROJ-02: delay an old embedding, update the memory, then restore to a
/// new store generation. Old jobs cannot overwrite or resurrect current
/// state; generation/fingerprint guards reject stale publication.
#[tokio::test]
async fn t_proj_02_stale_publication_rejected_after_generation_change() {
    let (repo, table, _guard) = env().await;
    add(&repo, 1, "t", "f");

    // A projector pinned to the FIRST generation publishes normally.
    let mut p_old_gen = Projector::new(
        repo.clone(),
        table.clone(),
        Box::new(FixedEmbedder { dim: 384 }),
        ModelFingerprint::new(1),
        StoreGeneration::FIRST,
    );
    if let Some(job) = repo.projection_job(eid(1)).unwrap() {
        assert_eq!(
            p_old_gen.process_job(&job).await.unwrap(),
            ProjectorOutcome::Published
        );
    }
    assert_eq!(table.count_rows(None).await.unwrap(), 1);

    // Update the memory: a new pending job carries the current revision.
    let patch = ltmrs_domain::command::MemoryPatch {
        fragment: Some("updated".into()),
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

    // A restore switches the store to a new generation. The old-generation
    // projector must now be refused: its rows are stale by construction.
    repo.set_store_generation(StoreGeneration::new(2)).unwrap();

    let job = repo
        .projection_job(eid(1))
        .unwrap()
        .expect("update enqueues a job");
    assert_eq!(
        p_old_gen.process_job(&job).await.unwrap(),
        ProjectorOutcome::StaleRevision,
        "an old-generation projector must be refused after a restore"
    );

    // No stale publication landed: still only the original rev-1 row.
    let gen1 = table.rows_where("store_generation = 1").await.unwrap();
    assert_eq!(gen1.len(), 1);
    assert_eq!(gen1[0].document_revision, DocumentRevision::new(1));

    // A new-generation projector publishes the current state instead.
    let mut p_new_gen = Projector::new(
        repo.clone(),
        table.clone(),
        Box::new(FixedEmbedder { dim: 384 }),
        ModelFingerprint::new(2),
        StoreGeneration::new(2),
    );
    assert_eq!(
        p_new_gen.process_job(&job).await.unwrap(),
        ProjectorOutcome::Published
    );

    // The new-generation row carries the current revision.
    let gen2 = table.rows_where("store_generation = 2").await.unwrap();
    assert_eq!(gen2.len(), 1);
    assert_eq!(gen2[0].document_revision, DocumentRevision::new(2));
}

/// T-PROJ-03: add/update/delete and immediately read by ID, lexically and
/// semantically; stall the embedder; reopen cached Lance readers. Direct
/// read-your-writes holds; lexical/semantic readiness is accurate; a stalled
/// embedder cannot prevent direct reads or lexical indexing.
#[tokio::test]
async fn t_proj_03_stalled_embedder_keeps_lexical_and_direct_reads() {
    let (repo, table, _guard) = env().await;

    // Add memory: canonical durability is immediate (direct read-your-writes).
    add(&repo, 1, "unique-term", "body");
    assert_eq!(repo.get_memories(&[eid(1)]).unwrap().len(), 1);

    // Stalled embedder: the worker still publishes the LEXICAL row and keeps
    // semantic work pending.
    let mut p_stalled = Projector::new(
        repo.clone(),
        table.clone(),
        Box::new(StalledEmbedder),
        ModelFingerprint::new(1),
        StoreGeneration::FIRST,
    );
    let resolved = p_stalled.run_until_idle().await.unwrap();
    assert_eq!(
        resolved, 0,
        "no job is fully resolvable while the embedder stalls"
    );

    // Lexical indexing works: one row exists with a NULL vector.
    assert_eq!(table.count_rows(None).await.unwrap(), 1);
    let null_vec = table.count_rows(Some("embedding IS NULL")).await.unwrap();
    assert_eq!(
        null_vec, 1,
        "a stalled embedder must not block lexical rows"
    );

    // Readiness is accurate: work is still pending (semantic phase).
    assert!(repo.has_pending_projection(eid(1)).unwrap());
    assert!(repo.projection_lag().unwrap() >= 1);

    // Update content: canonical read-your-writes holds immediately.
    let patch = ltmrs_domain::command::MemoryPatch {
        fragment: Some("updated body".into()),
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
    let rec = repo.get_memories(&[eid(1)]).unwrap().pop().unwrap();
    assert_eq!(rec.fragment, "updated body");

    // A working embedder now completes the semantic phase for the CURRENT
    // revision only (the stale lexical row is superseded and removed).
    let mut p_working = Projector::new(
        repo.clone(),
        table.clone(),
        Box::new(FixedEmbedder { dim: 384 }),
        ModelFingerprint::new(1),
        StoreGeneration::FIRST,
    );
    p_working.run_until_idle().await.unwrap();

    // Exactly one row at the latest revision with a vector; no stale text.
    assert_eq!(table.count_rows(None).await.unwrap(), 1);
    let vec_rows = table.rows_where("embedding IS NOT NULL").await.unwrap();
    assert_eq!(vec_rows.len(), 1);
    assert!(vec_rows[0].lexical_text.contains("updated body"));

    // Delete: rows propagate away; direct reads still observe the tombstone.
    repo.apply(
        &ctx(3),
        &DomainCommand::Forget {
            id: eid(1),
            mode: ForgetMode::Delete,
        },
    )
    .unwrap();
    p_working.run_until_idle().await.unwrap();
    assert_eq!(table.count_rows(None).await.unwrap(), 0);

    // A reopened reader observes the converged (empty) state.
    let mut fresh = table.clone();
    fresh.refresh().await.unwrap();
    assert_eq!(fresh.count_rows(None).await.unwrap(), 0);
}

/// T-SEARCH-01: empty database/query, FTS not yet built, null/missing
/// vectors, and optimization concurrent with queries. Clear valid outcomes;
/// no crash or false completeness.
#[tokio::test]
async fn t_search_01_empty_db_and_concurrent_optimization() {
    let dir = tempfile::tempdir().unwrap();
    let tbl = SearchTable::open(dir.path().to_str().unwrap())
        .await
        .unwrap();

    // Empty database: FTS query and predicate queries return clean empties.
    assert!(
        tbl.fts_query("anything", 10, None)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(tbl.count_rows(None).await.unwrap(), 0);
    assert!(
        tbl.rows_where("lexical_text LIKE '%x%'")
            .await
            .unwrap()
            .is_empty()
    );

    // Null-vector rows: semantic completeness must not be claimed.
    let only_text = SearchRow {
        store_generation: StoreGeneration::FIRST,
        memory_id: eid(1),
        document_revision: DocumentRevision::new(0),
        model_fingerprint: ModelFingerprint::new(1),
        chunk_id: ChunkId::new(0),
        chunker_version: "single-chunk-v1".to_string(),
        lexical_text: "only text".into(),
        char_start: 0,
        char_end: 9,
        project: None,
        fragment_type: "fact".into(),
        created_at_millis: 0,
        confidence: 0.5,
        updated_at_millis: 0,
        embedding: None,
    };
    tbl.publish_rows(std::slice::from_ref(&only_text))
        .await
        .unwrap();
    let with_vectors = table_with_vector_count(&tbl).await;
    assert_eq!(with_vectors, 0);

    // Optimization concurrent with queries: both must succeed without crash.
    let tbl_opt = tbl.clone();
    let (opt_result, query_ok) = tokio::join!(tbl_opt.optimize(), tbl.count_rows(None));
    assert!(
        query_ok.is_ok(),
        "queries must work while optimization runs"
    );
    opt_result.unwrap();

    // Data intact after concurrent optimization.
    assert_eq!(tbl.count_rows(None).await.unwrap(), 1);
}
