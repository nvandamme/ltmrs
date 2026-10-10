//! Apply / receipt / replay tests (moved verbatim from `repository.rs`).

use super::FaultInjector;
use super::test_support::*;
use fjall::OptimisticTxDatabase;
use ltmrs_domain::command::{DomainCommand, DomainErrorCode, ForgetMode, ReceiptOutcome};
use ltmrs_domain::id::{EntityId, StoreGeneration};
use ltmrs_domain::relation::RelationType;
use uuid::Uuid;

#[test]
fn apply_add_memory_stores_receipt_atomically() {
    let (repo, _dir) = repo_with_ns();
    let m = memory(eid(1), "hello");
    let r = repo
        .apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: m,
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    assert!(matches!(r.outcome, ReceiptOutcome::Success { .. }));
    // Receipt is durably recorded with the same operation key.
    let stored = repo
        .lookup_receipt(
            StoreGeneration::FIRST,
            r.frontend_id,
            r.retry_epoch,
            r.operation_id,
        )
        .unwrap()
        .unwrap();
    assert_eq!(stored.operation_id, r.operation_id);
    assert_eq!(stored.request_digest, "d1");
}

#[test]
fn idempotent_replay_returns_recorded_receipt() {
    let (repo, _dir) = repo_with_ns();
    let m = memory(eid(1), "hello");
    let c = ctx(1, "d1");
    let r1 = repo
        .apply(
            &c,
            &DomainCommand::AddMemory {
                memory: m.clone(),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    // Same key + same digest: replay returns the recorded result, no error.
    let r2 = repo
        .apply(
            &c,
            &DomainCommand::AddMemory {
                memory: m,
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    assert_eq!(r1.operation_id, r2.operation_id);
}

#[test]
fn key_reuse_with_different_input_is_error() {
    let (repo, _dir) = repo_with_ns();
    let m = memory(eid(1), "hello");
    let c1 = ctx(1, "d1");
    repo.apply(
        &c1,
        &DomainCommand::AddMemory {
            memory: m,
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    // Same operation key, different digest: must be rejected.
    let c2 = ctx(1, "DIFFERENT");
    let err = repo
        .apply(
            &c2,
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "x"),
                session: None,
                auto_link: None,
            },
        )
        .unwrap_err();
    assert_eq!(err.code, DomainErrorCode::KeyReuseDifferentInput);
}

#[test]
fn stale_revision_conflict_is_not_blindly_rebased() {
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
    // Update with a stale expected_revision must be rejected, not rebased.
    let patch = ltmrs_domain::command::MemoryPatch {
        title: Some("new".into()),
        ..Default::default()
    };
    let err = repo
        .apply(
            &ctx(2, "d2"),
            &DomainCommand::UpdateMemory {
                id: eid(1),
                expected_revision: Some(ltmrs_domain::id::EntityRevision::new(99)),
                patch,
            },
        )
        .unwrap_err();
    assert_eq!(err.code, DomainErrorCode::RevisionConflict);
}

#[test]
fn supersession_cycle_rejected_concurrency_safely() {
    let (repo, _dir) = repo_with_ns();
    for n in [1u64, 2, 3] {
        repo.apply(
            &ctx(n, &format!("m{n}")),
            &DomainCommand::AddMemory {
                memory: memory(eid(n), &format!("m{n}")),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    }
    // 1 -> 2 -> 3 supersession chain.
    repo.apply(
        &ctx(10, "e1"),
        &DomainCommand::Relate {
            relation: rel(eid(100), eid(1), eid(2), RelationType::Supersedes),
        },
    )
    .unwrap();
    repo.apply(
        &ctx(11, "e2"),
        &DomainCommand::Relate {
            relation: rel(eid(101), eid(2), eid(3), RelationType::Supersedes),
        },
    )
    .unwrap();
    // 3 -> 1 would form a cycle: must be rejected.
    let err = repo
        .apply(
            &ctx(12, "e3"),
            &DomainCommand::Relate {
                relation: rel(eid(102), eid(3), eid(1), RelationType::Supersedes),
            },
        )
        .unwrap_err();
    assert_eq!(err.code, DomainErrorCode::SupersessionCycle);
}

#[test]
fn concurrent_opposite_supersession_edges_keep_graph_acyclic() {
    // T-GRAPH-01 race: A→B ∥ B→A supersedes with a barrier so both
    // validate against the same snapshot. Exactly one must win; the
    // loser must observe SupersessionCycle (via SSI conflict + retry
    // or by seeing the winner's edge directly). Final graph is acyclic.
    let (repo, _dir) = repo_with_ns();
    for n in [1u64, 2] {
        repo.apply(
            &ctx(n, &format!("m{n}")),
            &DomainCommand::AddMemory {
                memory: memory(eid(n), &format!("m{n}")),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    }
    let repo = std::sync::Arc::new(repo);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut handles = Vec::new();
    for (i, (s, t, rid)) in [(eid(1), eid(2), eid(100)), (eid(2), eid(1), eid(101))]
        .into_iter()
        .enumerate()
    {
        let repo = std::sync::Arc::clone(&repo);
        let barrier = std::sync::Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            let c = ctx(i as u64 + 200, &format!("race{i}"));
            repo.apply(
                &c,
                &DomainCommand::Relate {
                    relation: rel(rid, s, t, RelationType::Supersedes),
                },
            )
        }));
    }
    let mut results = Vec::new();
    for h in handles {
        results.push(h.join().unwrap());
    }
    let winners = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(
        winners, 1,
        "exactly one opposite edge must win, got {results:?}"
    );
    for r in results.iter().filter_map(|r| r.as_ref().err()) {
        assert_eq!(r.code, DomainErrorCode::SupersessionCycle);
    }
    // Final graph holds a single supersession edge: still acyclic.
    // Note neighbors(id) returns edges where id is source OR target,
    // so one edge is visible from both endpoints: dedupe by relation id.
    let mut ids: Vec<EntityId> = repo
        .neighbors(eid(1))
        .unwrap()
        .into_iter()
        .chain(repo.neighbors(eid(2)).unwrap())
        .map(|r| r.id)
        .collect();
    ids.sort_by_key(|id| id.as_uuid());
    ids.dedup_by_key(|id| id.as_uuid());
    assert_eq!(ids.len(), 1, "one surviving supersession edge expected");
}
#[test]
fn forget_preserves_receipt_history() {
    let (repo, _dir) = repo_with_ns();
    let m = memory(eid(1), "hello");
    let add_receipt = repo
        .apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: m,
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    repo.apply(
        &ctx(2, "d2"),
        &DomainCommand::Forget {
            id: eid(1),
            mode: ForgetMode::Delete,
        },
    )
    .unwrap();

    // The add receipt survives the forget: audit history is never removed.
    assert!(
        repo.lookup_receipt(
            StoreGeneration::FIRST,
            add_receipt.frontend_id,
            add_receipt.retry_epoch,
            add_receipt.operation_id
        )
        .unwrap()
        .is_some(),
        "receipt history must survive deletion"
    );
}
#[test]
fn injected_unknown_outcome_leaves_store_consistent() {
    let (repo, _dir) = repo_with_ns();
    // Inject one unknown-outcome fault on the next commit.
    repo.fault_injector().set_commit_unknown_outcomes(1);

    // The apply should report an unknown outcome (no receipt recorded).
    let result = repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "hello"),
            session: None,
            auto_link: None,
        },
    );
    // The unknown outcome is resolved: no receipt exists, so it's an error.
    assert!(
        result.is_err(),
        "unknown outcome with no receipt must error"
    );

    // The store must be consistent: no partial write, no memory, no receipt.
    assert!(repo.get_memories(&[eid(1)]).unwrap().is_empty());
    let c = ctx(1, "d1");
    assert!(
        repo.lookup_receipt(
            c.store_generation,
            c.frontend_id,
            c.retry_epoch,
            c.operation_id
        )
        .unwrap()
        .is_none()
    );

    // A fresh apply of the same operation succeeds (the fault was consumed).
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "hello"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    assert_eq!(repo.get_memories(&[eid(1)]).unwrap().len(), 1);
}
#[test]
fn injected_migration_fault_fails_atomically() {
    use crate::migrations::{
        MigrationOutcome, MigrationPlan, MigrationRunner, MigrationSafetyRules,
    };
    use std::sync::Arc;

    let dir = tempfile::tempdir().unwrap();
    let db = OptimisticTxDatabase::builder(dir.path().to_str().unwrap())
        .open()
        .unwrap();

    // Inject a migration fault: the first migration step fails.
    let fi = Arc::new(FaultInjector::new());
    fi.set_migration_failures(1);
    let runner = MigrationRunner::new(
        MigrationPlan::default_plan(),
        MigrationSafetyRules::default(),
    )
    .with_fault_injector(fi);

    // The migration step fails atomically (no partial version stamp).
    let err = runner.assess(&db).unwrap_err();
    assert!(
        err.message.contains("injected migration fault"),
        "migration fault must fail the assess, got: {}",
        err.message
    );

    // The store is left in a recoverable state: no version stamped.
    // A fresh runner (no fault) can complete the migration.
    let runner2 = MigrationRunner::new(
        MigrationPlan::default_plan(),
        MigrationSafetyRules::default(),
    );
    let outcome2 = runner2.assess(&db).unwrap();
    assert!(matches!(outcome2, MigrationOutcome::Migrated { .. }));
}

/// Feedback survives a backup/restore round trip under a stable key
/// (no silent re-keying, no double-recording on replay).
#[test]
fn feedback_survives_restore_round_trip() {
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
    repo.apply(
        &ctx(2, "d2"),
        &DomainCommand::Feedback {
            memory_id: eid(1),
            useful: true,
        },
    )
    .unwrap();
    let before = repo.export_full().unwrap().feedback;
    assert_eq!(before.len(), 1);
    let snapshot = repo.export_full().unwrap();
    let marked = repo
        .restore_replace(
            &snapshot.memories,
            &snapshot.relations,
            &snapshot.guides,
            &snapshot.feedback,
            &snapshot.suggestions,
            &snapshot.sessions,
            StoreGeneration::new(2),
        )
        .unwrap();
    assert_eq!(marked, 0, "no live sessions to retire here");
    let after = repo.export_full().unwrap().feedback;
    assert_eq!(after, before, "feedback must round-trip identically");
}

/// P1 (bfe8844-review follow-up): the native backup must be one coherent
/// cut. Sessions live in Fjall now, so `export_full_with_generation`
/// must read them from the SAME snapshot as memories/guides/etc. — a
/// caller-side `all_sessions()` from a second snapshot can tear across
/// a concurrent `session_end` (Active session + already-bumped guide
/// counts that never coexisted).
#[test]
fn export_full_covers_canonical_sessions() {
    use ltmrs_domain::id::SessionHandle;
    use ltmrs_domain::session::SessionOp;
    let (repo, _dir) = repo_with_ns();
    let handle = SessionHandle::new(Uuid::from_u128(100));
    match repo
        .session_start_tx(
            &scope(100, "digest-export"),
            handle,
            None,
            None,
            vec![],
            None,
            None,
            1000,
        )
        .unwrap()
    {
        SessionOp::Applied(h) => assert_eq!(h, handle),
        other => panic!("expected Applied, got {other:?}"),
    }
    let (export, _) = repo.export_full_with_generation().unwrap();
    assert!(
        export.sessions.iter().any(|s| s.handle == handle),
        "export must carry live sessions from its own snapshot"
    );
}

/// P2-1 wake-up: a committed mutation fires the commit hook exactly once;
/// an idempotent replay fires nothing (no new work to project).
#[test]
fn commit_hook_fires_once_per_commit_not_replay() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (repo, _dir) = repo_with_ns();
    let fired = std::sync::Arc::new(AtomicUsize::new(0));
    let hook_fired = std::sync::Arc::clone(&fired);
    repo.set_commit_hook(std::sync::Arc::new(move || {
        hook_fired.fetch_add(1, Ordering::SeqCst);
    }));
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
    assert_eq!(fired.load(Ordering::SeqCst), 1);
    // Same operation + digest replays the receipt: no new commit, no fire.
    let m2 = memory(eid(1), "hello");
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: m2,
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    assert_eq!(
        fired.load(Ordering::SeqCst),
        1,
        "replay must not wake the projection worker"
    );
}
