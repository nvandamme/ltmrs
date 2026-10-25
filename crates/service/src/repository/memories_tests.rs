//! Memory-write tests (moved verbatim from `repository.rs`).

use super::test_support::*;
use ltmrs_domain::command::{DomainCommand, ForgetMode};
use ltmrs_domain::id::{EntityId, ModelFingerprint};
use ltmrs_domain::relation::{Relation, RelationType};

/// Project-only updates change indexed rows (project column), so they
/// enqueue work and dirty builds exactly like content changes.
#[test]
fn project_only_update_enqueues_and_dirties() {
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
    let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
    repo.note_generation_progress(next, 1).unwrap();
    let seq_before = repo.projection_job(eid(1)).unwrap().unwrap().seq;
    repo.apply(
        &ctx(2, "proj"),
        &DomainCommand::UpdateMemory {
            id: eid(1),
            expected_revision: None,
            patch: ltmrs_domain::command::MemoryPatch {
                project: Some(Some("elsewhere".to_string())),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let job = repo.projection_job(eid(1)).unwrap().unwrap();
    assert!(
        job.seq > seq_before,
        "project change must enqueue a newer job"
    );
    assert!(repo.generation_record(next).unwrap().unwrap().build_dirty);
}

/// Confidence-only updates refresh the projection: confidence is a
/// filter-relevant projection column (source pre-filter), so a changed
/// confidence must re-publish — otherwise post-convergence drift
/// silently breaks eligibility. No content changed, so no build-dirty.
#[test]
fn confidence_only_update_enqueues_refresh() {
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
    let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
    let seq_before = repo.projection_job(eid(1)).unwrap().unwrap().seq;
    repo.apply(
        &ctx(2, "conf"),
        &DomainCommand::UpdateMemory {
            id: eid(1),
            expected_revision: None,
            patch: ltmrs_domain::command::MemoryPatch {
                confidence: Some(0.9),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let job = repo.projection_job(eid(1)).unwrap().unwrap();
    assert_eq!(
        job.seq,
        seq_before + 1,
        "confidence change must refresh the projection"
    );
    assert!(!repo.generation_record(next).unwrap().unwrap().build_dirty);
    // Identical absolute value: no drift, no job.
    let seq_after = repo.projection_job(eid(1)).unwrap().unwrap().seq;
    repo.apply(
        &ctx(3, "conf2"),
        &DomainCommand::UpdateMemory {
            id: eid(1),
            expected_revision: None,
            patch: ltmrs_domain::command::MemoryPatch {
                confidence: Some(0.9),
                ..Default::default()
            },
        },
    )
    .unwrap();
    assert_eq!(
        repo.projection_job(eid(1)).unwrap().unwrap().seq,
        seq_after,
        "identical confidence must enqueue nothing"
    );
}
#[test]
fn forget_transitions_lifecycle() {
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
        &DomainCommand::Forget {
            id: eid(1),
            mode: ForgetMode::Invalidate,
        },
    )
    .unwrap();
    let m = repo.get_memories(&[eid(1)]).unwrap().pop().unwrap();
    assert!(!m.lifecycle.is_recallable());
}

#[test]
fn hard_delete_severs_adjacency() {
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "a"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    repo.apply(
        &ctx(2, "d2"),
        &DomainCommand::AddMemory {
            memory: memory(eid(2), "b"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    repo.apply(
        &ctx(3, "e1"),
        &DomainCommand::Relate {
            relation: rel(eid(100), eid(1), eid(2), RelationType::Supports),
        },
    )
    .unwrap();
    assert_eq!(repo.neighbors(eid(1)).unwrap().len(), 1);

    // Hard delete memory 1: its edge must be removed.
    repo.apply(
        &ctx(4, "d3"),
        &DomainCommand::Forget {
            id: eid(1),
            mode: ForgetMode::Delete,
        },
    )
    .unwrap();
    assert_eq!(repo.neighbors(eid(1)).unwrap().len(), 0);
}

#[test]
fn one_winner_for_concurrent_absent_key() {
    let (repo, _dir) = repo_with_ns();
    let repo = std::sync::Arc::new(repo);

    const N: usize = 8;
    let mut handles = Vec::new();
    for i in 0..N {
        let repo = std::sync::Arc::clone(&repo);
        handles.push(std::thread::spawn(move || {
            // Distinct operation ids, same target memory: contested uniqueness.
            let c = ctx(i as u64 + 100, &format!("d{i}"));
            repo.apply(
                &c,
                &DomainCommand::AddMemory {
                    memory: memory(eid(1), "T"),
                    session: None,
                    auto_link: None,
                },
            )
        }));
    }
    let mut results = Vec::new();
    for h in handles {
        results.push(h.join().unwrap());
    }
    let winners = results.iter().filter(|r| r.is_ok()).count();
    let rows = repo.get_memories(&[eid(1)]).unwrap().len();
    eprintln!(
        "T-CONC-02: winners={winners} rows={rows} (one-winner requires winners==1 && rows==1)"
    );
    assert_eq!(winners, 1, "exactly one winner expected");
    assert_eq!(rows, 1, "exactly one row expected");
}
/// Merged results register aliases like added memories do: a duplicate
/// alias fails, a fresh alias resolves.
#[test]
fn merge_registers_alias_with_uniqueness() {
    use ltmrs_domain::id::ExternalAlias;
    let (repo, _dir) = repo_with_ns();
    let mut first = memory(eid(1), "first");
    first.external_alias = Some(ExternalAlias::new("taken"));
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: first,
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    // Duplicate alias on merge fails instead of shadowing.
    let mut dup = memory(eid(10), "merged-dup");
    dup.external_alias = Some(ExternalAlias::new("taken"));
    let err = repo
        .apply(
            &ctx(2, "d2"),
            &DomainCommand::Merge {
                source_ids: vec![eid(1)],
                result: dup,
                consolidate: false,
            },
        )
        .unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::DuplicateAlias
    );
    // Fresh alias registers and resolves.
    let mut fresh = memory(eid(11), "merged-fresh");
    fresh.external_alias = Some(ExternalAlias::new("fresh-alias"));
    repo.apply(
        &ctx(3, "d3"),
        &DomainCommand::Merge {
            source_ids: vec![eid(1)],
            result: fresh,
            consolidate: false,
        },
    )
    .unwrap();
    assert_eq!(repo.resolve_id("fresh-alias").unwrap(), eid(11));
}

/// Absolute-only writes still advance the entity revision (concurrent
/// same-expected writers conflict instead of last-writer-winning), and
/// non-live memories refuse content writes.
#[test]
fn absolute_writes_advance_revision_and_respect_lifecycle() {
    use ltmrs_domain::command::MemoryPatch;
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "m"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let rev = repo.get_memories(&[eid(1)]).unwrap()[0].entity_revision;
    repo.apply(
        &ctx(2, "d2"),
        &DomainCommand::UpdateMemory {
            id: eid(1),
            expected_revision: Some(rev),
            patch: MemoryPatch {
                confidence: Some(0.9),
                ..Default::default()
            },
        },
    )
    .unwrap();
    // Same expected revision twice: the second write conflicts.
    let err = repo
        .apply(
            &ctx(3, "d3"),
            &DomainCommand::UpdateMemory {
                id: eid(1),
                expected_revision: Some(rev),
                patch: MemoryPatch {
                    confidence: Some(0.1),
                    ..Default::default()
                },
            },
        )
        .unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::RevisionConflict
    );
    // Archived memories refuse content writes.
    repo.apply(
        &ctx(4, "d4"),
        &DomainCommand::Forget {
            id: eid(1),
            mode: ForgetMode::Archive,
        },
    )
    .unwrap();
    let err = repo
        .apply(
            &ctx(5, "d5"),
            &DomainCommand::UpdateMemory {
                id: eid(1),
                expected_revision: None,
                patch: MemoryPatch {
                    confidence: Some(0.2),
                    ..Default::default()
                },
            },
        )
        .unwrap_err();
    assert_eq!(err.code, ltmrs_domain::command::DomainErrorCode::Validation);
}

/// Relation ids are bound to their endpoints: reuse with different
/// endpoints fails instead of silently overwriting.
#[test]
fn relation_id_reuse_with_different_endpoints_fails() {
    use ltmrs_domain::memory::Instant;
    let (repo, _dir) = repo_with_ns();
    for (n, title) in [(1u64, "a"), (2, "b"), (3, "c")] {
        repo.apply(
            &ctx(n, &format!("d{n}")),
            &DomainCommand::AddMemory {
                memory: memory(eid(n), title),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    }
    let rel = Relation::new(
        EntityId::new(uuid::Uuid::from_u128(100)),
        eid(1),
        eid(2),
        RelationType::Supports,
        None,
        Instant::new(1000),
    );
    repo.apply(&ctx(10, "d10"), &DomainCommand::Relate { relation: rel })
        .unwrap();
    let moved = Relation::new(
        EntityId::new(uuid::Uuid::from_u128(100)),
        eid(1),
        eid(3),
        RelationType::Supports,
        None,
        Instant::new(1000),
    );
    let err = repo
        .apply(&ctx(11, "d11"), &DomainCommand::Relate { relation: moved })
        .unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::KeyReuseDifferentInput
    );
}

/// Relation ids are bound to their full input: reuse with a different
/// note fails instead of silently overwriting the annotation.
#[test]
fn relation_id_reuse_with_different_note_fails() {
    use ltmrs_domain::memory::Instant;
    let (repo, _dir) = repo_with_ns();
    for (n, title) in [(1u64, "a"), (2, "b")] {
        repo.apply(
            &ctx(n, &format!("d{n}")),
            &DomainCommand::AddMemory {
                memory: memory(eid(n), title),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    }
    let rel = Relation::new(
        EntityId::new(uuid::Uuid::from_u128(100)),
        eid(1),
        eid(2),
        RelationType::Supports,
        None,
        Instant::new(1000),
    );
    repo.apply(&ctx(10, "d10"), &DomainCommand::Relate { relation: rel })
        .unwrap();
    // Same endpoints, different note: reject (an exact duplicate falls
    // through to DuplicateEdge — unchanged pre-existing behavior).
    let noted = Relation::new(
        EntityId::new(uuid::Uuid::from_u128(100)),
        eid(1),
        eid(2),
        RelationType::Supports,
        Some("changed".to_string()),
        Instant::new(1000),
    );
    let err = repo
        .apply(&ctx(12, "d12"), &DomainCommand::Relate { relation: noted })
        .unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::KeyReuseDifferentInput
    );
}
