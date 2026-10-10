//! Projection-queue tests (moved verbatim from `repository.rs`).

use super::CanonicalRepository;
use super::test_support::*;
use ltmrs_domain::command::{DomainCommand, ForgetMode};
use ltmrs_domain::id::EntityId;
use ltmrs_domain::memory::MemoryLifecycle;
use ltmrs_domain::relation::RelationType;
use uuid::Uuid;

/// Feedback changes confidence: the projection must refresh so the
/// source pre-filter reads the adjusted value, not the converged one.
#[test]
fn feedback_enqueues_projection_refresh() {
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "m1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "m1"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let seq_before = repo.projection_job(eid(1)).unwrap().unwrap().seq;
    let mut c = ctx(2, "fb");
    c.retry_epoch = 1;
    repo.apply(
        &c,
        &DomainCommand::Feedback {
            memory_id: eid(1),
            useful: false,
        },
    )
    .unwrap();
    let job = repo.projection_job(eid(1)).unwrap().unwrap();
    assert_eq!(
        job.seq,
        seq_before + 1,
        "feedback confidence change must refresh the projection"
    );
}

/// Read-side access bumps confidence (+0.015): same refresh rule —
/// micro-drift still flips threshold eligibility over time.
#[test]
fn access_enqueues_projection_refresh() {
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "m1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "m1"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let seq_before = repo.projection_job(eid(1)).unwrap().unwrap().seq;
    let mut c = ctx(2, "acc");
    c.retry_epoch = 1;
    repo.apply(
        &c,
        &DomainCommand::Access {
            memory_ids: vec![eid(1)],
            context: None,
        },
    )
    .unwrap();
    let job = repo.projection_job(eid(1)).unwrap().unwrap();
    assert_eq!(
        job.seq,
        seq_before + 1,
        "access confidence change must refresh the projection"
    );
}

/// Saturated clamps enqueue nothing on any hot path: at confidence
/// 1.0 a positive feedback/access/boost changes nothing, so no
/// refresh job is recorded.
#[test]
fn saturated_clamp_enqueues_nothing() {
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "m1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "m1"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let mut top = repo.get_memories(&[eid(1)]).unwrap().remove(0);
    top.confidence = 1.0;
    repo.put_memory_direct(&top).unwrap();
    let seq_before = repo.projection_job(eid(1)).unwrap().unwrap().seq;
    let mut c = ctx(2, "fb");
    c.retry_epoch = 1;
    repo.apply(
        &c,
        &DomainCommand::Feedback {
            memory_id: eid(1),
            useful: true,
        },
    )
    .unwrap();
    let mut c = ctx(3, "acc");
    c.retry_epoch = 1;
    repo.apply(
        &c,
        &DomainCommand::Access {
            memory_ids: vec![eid(1)],
            context: None,
        },
    )
    .unwrap();
    let mut c = ctx(4, "boost");
    c.retry_epoch = 1;
    repo.apply(
        &c,
        &DomainCommand::BoostConfidence {
            memory_ids: vec![eid(1)],
        },
    )
    .unwrap();
    assert_eq!(
        repo.projection_job(eid(1)).unwrap().unwrap().seq,
        seq_before,
        "saturated clamps must enqueue nothing on any path"
    );
}

/// Floor clamp likewise: at confidence 0.0 a negative feedback
/// changes nothing, so no refresh job is recorded.
#[test]
fn floor_clamp_enqueues_nothing() {
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "m1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "m1"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let mut low = repo.get_memories(&[eid(1)]).unwrap().remove(0);
    low.confidence = 0.0;
    repo.put_memory_direct(&low).unwrap();
    let seq_before = repo.projection_job(eid(1)).unwrap().unwrap().seq;
    let mut c = ctx(2, "fb");
    c.retry_epoch = 1;
    repo.apply(
        &c,
        &DomainCommand::Feedback {
            memory_id: eid(1),
            useful: false,
        },
    )
    .unwrap();
    assert_eq!(
        repo.projection_job(eid(1)).unwrap().unwrap().seq,
        seq_before,
        "floor clamp must enqueue nothing"
    );
}
#[test]
fn if_absent_enqueue_never_clobbers_a_fresh_job() {
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "x"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let rev = repo
        .get_memories(&[eid(1)])
        .unwrap()
        .remove(0)
        .document_revision;
    // No job pending (fresh add records one — acknowledge it first).
    let job = repo
        .projection_job(eid(1))
        .unwrap()
        .expect("add records a job");
    assert!(repo.acknowledge_projection(eid(1), job.seq).unwrap());
    // Backfill path: enqueues exactly once, then holds.
    assert!(
        repo.enqueue_projection_job_if_absent(eid(1), rev, 1)
            .unwrap()
    );
    assert!(
        !repo
            .enqueue_projection_job_if_absent(eid(1), rev, 1)
            .unwrap()
    );
    let kept = repo.projection_job(eid(1)).unwrap().expect("job kept");
    assert_eq!(kept.seq, 1, "second call must not advance the seq");
    assert_eq!(kept.desired_document_revision, rev);
}

#[test]
fn content_update_advances_updated_at() {
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "x"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    repo.apply(
        &ctx(2, "d2"),
        &DomainCommand::UpdateMemory {
            id: eid(1),
            expected_revision: None,
            patch: ltmrs_domain::command::MemoryPatch {
                fragment: Some("new body".to_string()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let updated = repo.get_memories(&[eid(1)]).unwrap().remove(0);
    // FrozenClock(1000) in repo_with_ns: the mutation stamps real time.
    assert_eq!(updated.updated_at.as_millis(), 1000);
}
#[test]
fn add_memory_records_pending_projection() {
    let (repo, _dir) = repo_with_ns();
    let m = memory(eid(1), "hello");
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: m,
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    assert!(
        repo.has_pending_projection(eid(1)).unwrap(),
        "add memory must atomically record a pending projection"
    );
}

#[test]
fn hard_delete_invalidates_projection_and_severs_edges() {
    let (repo, _dir) = repo_with_ns();
    let a = memory(eid(1), "a");
    let b = memory(eid(2), "b");
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: a,
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    repo.apply(
        &ctx(2, "d2"),
        &DomainCommand::AddMemory {
            memory: b,
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    // Link a -> b.
    let rel = rel(eid(3), eid(1), eid(2), RelationType::Supersedes);
    repo.apply(&ctx(3, "d3"), &DomainCommand::Relate { relation: rel })
        .unwrap();
    assert_eq!(repo.neighbors(eid(1)).unwrap().len(), 1);
    assert!(repo.has_pending_projection(eid(1)).unwrap());

    // Hard delete a.
    repo.apply(
        &ctx(4, "d4"),
        &DomainCommand::Forget {
            id: eid(1),
            mode: ForgetMode::Delete,
        },
    )
    .unwrap();

    // Projection invalidated via a durable tombstone job (the worker will
    // remove rows); edges severed; canonical tombstone preserved.
    let job = repo
        .projection_job(eid(1))
        .unwrap()
        .expect("tombstone job recorded");
    assert!(
        job.is_tombstone,
        "hard delete must record a tombstone projection job"
    );
    assert_eq!(
        repo.neighbors(eid(1)).unwrap().len(),
        0,
        "hard delete must sever adjacency"
    );
    let tombstone = repo.get_memories(&[eid(1)]).unwrap().pop().unwrap();
    assert!(matches!(
        tombstone.lifecycle,
        MemoryLifecycle::Deleted { .. }
    ));
}

#[test]
fn invalidate_preserves_edges_and_invalidates_projection() {
    let (repo, _dir) = repo_with_ns();
    let a = memory(eid(1), "a");
    let b = memory(eid(2), "b");
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: a,
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    repo.apply(
        &ctx(2, "d2"),
        &DomainCommand::AddMemory {
            memory: b,
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let rel = rel(eid(3), eid(1), eid(2), RelationType::Supersedes);
    repo.apply(&ctx(3, "d3"), &DomainCommand::Relate { relation: rel })
        .unwrap();

    repo.apply(
        &ctx(4, "d4"),
        &DomainCommand::Forget {
            id: eid(1),
            mode: ForgetMode::Invalidate,
        },
    )
    .unwrap();

    // Invalidation preserves edges as history; a tombstone job is recorded.
    let job = repo
        .projection_job(eid(1))
        .unwrap()
        .expect("tombstone job recorded");
    assert!(
        job.is_tombstone,
        "invalidation must record a tombstone projection job"
    );
    assert_eq!(
        repo.neighbors(eid(1)).unwrap().len(),
        1,
        "invalidation preserves edges as history"
    );
    let rec = repo.get_memories(&[eid(1)]).unwrap().pop().unwrap();
    assert!(matches!(rec.lifecycle, MemoryLifecycle::Invalidated { .. }));
}
// ---- WP-05 task 4: durable desired-state jobs + compare-and-clear ----

#[test]
fn add_memory_records_versioned_projection_job() {
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "hello"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();

    let job = repo.projection_job(eid(1)).unwrap().expect("job recorded");
    assert_eq!(job.memory_id, eid(1));
    // The helper view still reports pending work.
    assert!(repo.has_pending_projection(eid(1)).unwrap());
}

#[test]
fn acknowledge_clears_matching_seq_only() {
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "hello"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let job = repo.projection_job(eid(1)).unwrap().unwrap();

    // Wrong seq (a stale worker) must NOT clear the work.
    assert!(
        !repo.acknowledge_projection(eid(1), job.seq + 1).unwrap(),
        "stale acknowledgement must leave work pending"
    );
    assert!(repo.has_pending_projection(eid(1)).unwrap());

    // The correct seq clears exactly once.
    assert!(repo.acknowledge_projection(eid(1), job.seq).unwrap());
    assert!(!repo.has_pending_projection(eid(1)).unwrap());
}

#[test]
fn content_update_advances_desired_revision_and_seq() {
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "hello"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let job1 = repo.projection_job(eid(1)).unwrap().unwrap();

    // A content-changing update re-enqueues work at the new document
    // revision with a higher seq.
    let patch = ltmrs_domain::command::MemoryPatch {
        fragment: Some("updated body".into()),
        ..Default::default()
    };
    repo.apply(
        &ctx(2, "d2"),
        &DomainCommand::UpdateMemory {
            id: eid(1),
            expected_revision: None,
            patch,
        },
    )
    .unwrap();

    let job2 = repo.projection_job(eid(1)).unwrap().unwrap();
    assert!(job2.seq > job1.seq, "seq must advance monotonically");
    assert!(
        job2.desired_document_revision.as_u64() > job1.desired_document_revision.as_u64(),
        "desired revision must track the canonical document revision"
    );

    // A delayed worker holding the OLD seq cannot clear the newer work.
    assert!(!repo.acknowledge_projection(eid(1), job1.seq).unwrap());
    assert!(repo.has_pending_projection(eid(1)).unwrap());
}

#[test]
fn forget_writes_tombstone_job_and_stale_worker_cannot_ack() {
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "hello"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let job = repo.projection_job(eid(1)).unwrap().unwrap();

    repo.apply(
        &ctx(2, "d2"),
        &DomainCommand::Forget {
            id: eid(1),
            mode: ForgetMode::Delete,
        },
    )
    .unwrap();

    // The forget atomically records a tombstone job at a higher seq — the
    // worker will remove rows; it is NOT silently dropped.
    let tomb = repo
        .projection_job(eid(1))
        .unwrap()
        .expect("tombstone job recorded");
    assert!(tomb.is_tombstone);
    assert!(tomb.seq > job.seq);

    // A delayed worker with the old seq must not clear it.
    assert!(
        !repo.acknowledge_projection(eid(1), job.seq).unwrap(),
        "forget must make stale acknowledgements fail"
    );
}

#[test]
fn projection_jobs_lists_all_pending() {
    let (repo, _dir) = repo_with_ns();
    for n in [1u64, 2, 3] {
        repo.apply(
            &ctx(n, &format!("d{n}")),
            &DomainCommand::AddMemory {
                memory: memory(eid(n), &format!("m{n}")),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    }

    let jobs = repo.projection_jobs().unwrap();
    assert_eq!(jobs.len(), 3);
    let ids: Vec<EntityId> = jobs.iter().map(|j| j.memory_id).collect();
    for n in [1u64, 2, 3] {
        assert!(ids.contains(&eid(n)));
    }

    // Acknowledge one; it drops out of the list.
    let target = repo.projection_job(eid(2)).unwrap().unwrap();
    repo.acknowledge_projection(eid(2), target.seq).unwrap();
    assert_eq!(repo.projection_jobs().unwrap().len(), 2);
}

#[test]
fn jobs_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
    {
        let repo =
            CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), clock).unwrap();
        let fe = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1));
        let ns = repo.issue_namespace(fe, ch(2), 1000).unwrap();
        let mut c = ctx(1, "d1");
        c.retry_epoch = ns.retry_epoch;
        repo.apply(
            &c,
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "hello"),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    }

    // Reopen: the unacknowledged job must still be pending (crash-safe).
    let repo = CanonicalRepository::open(dir.path().to_str().unwrap()).unwrap();
    assert!(repo.projection_job(eid(1)).unwrap().is_some());
}

// ---- WP-05 task 9: readiness and lag metrics ----

#[test]
fn projection_lag_reflects_pending_work() {
    let (repo, _dir) = repo_with_ns();
    assert_eq!(repo.projection_lag().unwrap(), 0);

    for n in [1u64, 2] {
        repo.apply(
            &ctx(n, &format!("d{n}")),
            &DomainCommand::AddMemory {
                memory: memory(eid(n), "m"),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    }
    assert_eq!(repo.projection_lag().unwrap(), 2);

    let j = repo.projection_job(eid(1)).unwrap().unwrap();
    repo.acknowledge_projection(eid(1), j.seq).unwrap();
    assert_eq!(repo.projection_lag().unwrap(), 1);
}

#[test]
fn oldest_pending_age_is_measured_from_enqueue() {
    let (repo, _dir) = repo_with_ns();
    // Frozen clock at 1000; the job is stamped with enqueued_at_millis=1000.
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "m"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();

    // At the same instant, age is 0.
    assert_eq!(repo.oldest_pending_age_millis(1000).unwrap(), Some(0));
    // 500ms later, age is 500.
    assert_eq!(repo.oldest_pending_age_millis(1500).unwrap(), Some(500));

    // Acknowledging clears it: no pending work means None.
    let j = repo.projection_job(eid(1)).unwrap().unwrap();
    repo.acknowledge_projection(eid(1), j.seq).unwrap();
    assert_eq!(repo.oldest_pending_age_millis(2000).unwrap(), None);
}

#[test]
fn oldest_pending_uses_the_minimum_enqueue_time() {
    let (repo, _dir) = repo_with_ns();
    // Two jobs enqueued at the same frozen instant; both age identically.
    for n in [1u64, 2] {
        repo.apply(
            &ctx(n, &format!("d{n}")),
            &DomainCommand::AddMemory {
                memory: memory(eid(n), "m"),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    }
    assert_eq!(repo.oldest_pending_age_millis(1200).unwrap(), Some(200));

    // Clear the older one; age now derives from the remaining job.
    let j = repo.projection_job(eid(1)).unwrap().unwrap();
    repo.acknowledge_projection(eid(1), j.seq).unwrap();
    assert_eq!(repo.oldest_pending_age_millis(1200).unwrap(), Some(200));
}
