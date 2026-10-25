//! Apply / receipt / replay tests (moved verbatim from `repository.rs`).

use super::CanonicalRepository;
use super::FaultInjector;
use super::test_support::*;
use fjall::OptimisticTxDatabase;
use ltmrs_domain::command::{
    DomainCommand, DomainErrorCode, ForgetMode, MemoryPatch, ReceiptOutcome,
};
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

#[test]
fn disjoint_field_edits_at_same_revision_conflict_then_merge_without_loss() {
    // T-CONC-01 disjoint-field race: two writers hold the same expected
    // revision and patch different fields. Exactly one commits; the loser
    // must see RevisionConflict (never a silent last-writer-win), and a
    // re-read + re-patch lands both fields with no lost update.
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "base"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let rev = repo
        .get_memories(&[eid(1)])
        .unwrap()
        .pop()
        .unwrap()
        .entity_revision;
    let repo = std::sync::Arc::new(repo);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let title_patch = MemoryPatch {
        title: Some("retitled".to_string()),
        ..Default::default()
    };
    let conf_patch = MemoryPatch {
        confidence: Some(0.9),
        ..Default::default()
    };
    let mut handles = Vec::new();
    for (i, patch) in [title_patch, conf_patch].into_iter().enumerate() {
        let repo = std::sync::Arc::clone(&repo);
        let barrier = std::sync::Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            repo.apply(
                &ctx(i as u64 + 10, &format!("race{i}")),
                &DomainCommand::UpdateMemory {
                    id: eid(1),
                    expected_revision: Some(rev),
                    patch,
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
        "exactly one disjoint-field writer must commit, got {results:?}"
    );
    for r in results.iter().filter_map(|r| r.as_ref().err()) {
        assert_eq!(r.code, DomainErrorCode::RevisionConflict);
    }
    // Loser re-reads and re-patches: both fields land, nothing lost.
    let fresh = repo.get_memories(&[eid(1)]).unwrap().pop().unwrap();
    let missing_title = fresh.title != "retitled";
    let missing_conf = (fresh.confidence - 0.9).abs() > 1e-12;
    assert!(
        missing_title ^ missing_conf,
        "exactly one field applied, got title={} confidence={}",
        fresh.title,
        fresh.confidence
    );
    let repair = MemoryPatch {
        title: if missing_title {
            Some("retitled".to_string())
        } else {
            None
        },
        confidence: if missing_conf { Some(0.9) } else { None },
        ..Default::default()
    };
    repo.apply(
        &ctx(12, "repair"),
        &DomainCommand::UpdateMemory {
            id: eid(1),
            expected_revision: Some(fresh.entity_revision),
            patch: repair,
        },
    )
    .unwrap();
    let final_mem = repo.get_memories(&[eid(1)]).unwrap().pop().unwrap();
    assert_eq!(final_mem.title, "retitled");
    assert!((final_mem.confidence - 0.9).abs() < 1e-12);
    assert_ne!(final_mem.entity_revision, rev);
}

#[test]
fn same_field_edits_at_same_revision_exactly_one_wins() {
    // T-CONC-01 same-field race: two writers patch the same field at the
    // same expected revision. Exactly one commits (no silent
    // last-writer-win); the loser retries on the fresh revision.
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "base"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let rev = repo
        .get_memories(&[eid(1)])
        .unwrap()
        .pop()
        .unwrap()
        .entity_revision;
    let repo = std::sync::Arc::new(repo);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut handles = Vec::new();
    for (i, title) in ["first-writer", "second-writer"].into_iter().enumerate() {
        let repo = std::sync::Arc::clone(&repo);
        let barrier = std::sync::Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            repo.apply(
                &ctx(i as u64 + 10, &format!("race{i}")),
                &DomainCommand::UpdateMemory {
                    id: eid(1),
                    expected_revision: Some(rev),
                    patch: MemoryPatch {
                        title: Some(title.to_string()),
                        ..Default::default()
                    },
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
        "exactly one same-field writer must commit, got {results:?}"
    );
    for r in results.iter().filter_map(|r| r.as_ref().err()) {
        assert_eq!(r.code, DomainErrorCode::RevisionConflict);
    }
    let mid = repo.get_memories(&[eid(1)]).unwrap().pop().unwrap();
    assert!(mid.title == "first-writer" || mid.title == "second-writer");
    // Loser retries on the fresh revision: commits cleanly.
    let loser_title = if mid.title == "first-writer" {
        "second-writer"
    } else {
        "first-writer"
    };
    repo.apply(
        &ctx(12, "retry"),
        &DomainCommand::UpdateMemory {
            id: eid(1),
            expected_revision: Some(mid.entity_revision),
            patch: MemoryPatch {
                title: Some(loser_title.to_string()),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let final_mem = repo.get_memories(&[eid(1)]).unwrap().pop().unwrap();
    assert_eq!(final_mem.title, loser_title);
    assert_ne!(final_mem.entity_revision, rev);
}

#[test]
fn endpoint_delete_vs_edge_create_leaves_no_dangling_edge() {
    // T-GRAPH-01 race: endpoint deletion vs edge creation on the same
    // barrier. Whichever order commits, the final store must hold no edge
    // referencing the deleted endpoint and the endpoint must stay deleted
    // (delete severs; a late relate against a dead endpoint is refused).
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
    let repo_relate = std::sync::Arc::clone(&repo);
    let barrier_relate = std::sync::Arc::clone(&barrier);
    let relate_handle = std::thread::spawn(move || {
        barrier_relate.wait();
        repo_relate.apply(
            &ctx(100, "relate"),
            &DomainCommand::Relate {
                relation: rel(eid(100), eid(1), eid(2), RelationType::RelatedTo),
            },
        )
    });
    let repo_forget = std::sync::Arc::clone(&repo);
    let barrier_forget = std::sync::Arc::clone(&barrier);
    let forget_handle = std::thread::spawn(move || {
        barrier_forget.wait();
        repo_forget.apply(
            &ctx(101, "forget"),
            &DomainCommand::Forget {
                id: eid(1),
                mode: ForgetMode::Delete,
            },
        )
    });
    let relate_result = relate_handle.join().unwrap();
    let forget_result = forget_handle.join().unwrap();
    assert!(
        forget_result.is_ok(),
        "delete must always commit, got {forget_result:?}"
    );
    if let Err(e) = relate_result.as_ref() {
        // Delete-first order: the relate replays against the tombstone and
        // the domain validator rejects the dead endpoint as not found.
        assert_eq!(e.code, DomainErrorCode::NotFound);
    }
    // Invariants hold under either commit order: endpoint stays deleted,
    // no edge references it, forward/reverse views agree (empty here).
    let a = repo
        .get_memories(&[eid(1)])
        .unwrap()
        .pop()
        .expect("tombstone row must remain");
    assert!(!a.lifecycle.is_recallable(), "endpoint must stay deleted");
    let dangling: Vec<_> = repo
        .all_relations()
        .unwrap()
        .into_iter()
        .filter(|r| r.source == eid(1) || r.target == eid(1))
        .collect();
    assert!(
        dangling.is_empty(),
        "no edge may reference the deleted endpoint, got {dangling:?}"
    );
    assert!(repo.neighbors(eid(1)).unwrap().is_empty());
}

#[test]
fn concurrent_duplicate_edge_creation_keeps_uniqueness() {
    // T-GRAPH-01 concurrent duplicates: two operations create the same
    // logical edge under one barrier. Uniqueness must hold in the final
    // store: at most one commit may survive, and any loser must fail
    // loudly (DuplicateEdge), never silently double-record.
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
    for (i, rid) in [eid(100), eid(101)].into_iter().enumerate() {
        let repo = std::sync::Arc::clone(&repo);
        let barrier = std::sync::Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            repo.apply(
                &ctx(i as u64 + 200, &format!("dup{i}")),
                &DomainCommand::Relate {
                    relation: rel(rid, eid(1), eid(2), RelationType::RelatedTo),
                },
            )
        }));
    }
    let mut results = Vec::new();
    for h in handles {
        results.push(h.join().unwrap());
    }
    for r in results.iter().filter_map(|r| r.as_ref().err()) {
        assert_eq!(r.code, DomainErrorCode::DuplicateEdge);
    }
    let surviving: Vec<_> = repo
        .all_relations()
        .unwrap()
        .into_iter()
        .filter(|r| {
            r.source == eid(1) && r.target == eid(2) && r.relation_type == RelationType::RelatedTo
        })
        .collect();
    assert_eq!(
        surviving.len(),
        1,
        "uniqueness must hold: exactly one duplicate edge survives, got {surviving:?} ({results:?})"
    );
}

#[test]
fn concurrent_boosts_on_same_memory_both_apply() {
    // T-CONC-01 concurrent increments: two barrier-synced +0.02 boosts on
    // the same memory must both land (no lost increment) via the
    // transaction retry, with access counted twice.
    let (repo, _dir) = repo_with_ns();
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "boost-target"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let before = repo.get_memories(&[eid(1)]).unwrap().pop().unwrap();
    let repo = std::sync::Arc::new(repo);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut handles = Vec::new();
    for i in 0..2 {
        let repo = std::sync::Arc::clone(&repo);
        let barrier = std::sync::Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            repo.apply(
                &ctx(10 + i, &format!("boost{i}")),
                &DomainCommand::BoostConfidence {
                    memory_ids: vec![eid(1)],
                },
            )
        }));
    }
    let mut results = Vec::new();
    for h in handles {
        results.push(h.join().unwrap());
    }
    assert!(
        results.iter().all(|r| r.is_ok()),
        "both boosts must commit, got {results:?}"
    );
    let after = repo.get_memories(&[eid(1)]).unwrap().pop().unwrap();
    assert!(
        (after.confidence - (before.confidence + 0.04)).abs() < 1e-9,
        "both +0.02 increments must land, got {} from {}",
        after.confidence,
        before.confidence
    );
    assert_eq!(
        after.access_count,
        before.access_count + 2,
        "each boost records one access"
    );
}

#[test]
fn receipt_survives_migration_cycle_without_duplicate_effect() {
    // T-REC-02 retry across migration: a receipt recorded before a schema
    // migration replays after it with no duplicated canonical effect.
    use fjall::{KeyspaceCreateOptions, OptimisticTxDatabase};
    use ltmrs_domain::clock::FrozenClock;
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let clock: Arc<dyn ltmrs_domain::clock::Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
    let repo = CanonicalRepository::open_with_clock(path, Arc::clone(&clock)).unwrap();
    repo.issue_namespace(
        ltmrs_domain::id::FrontendId::new(uuid::Uuid::from_u128(1)),
        ch(2),
        1000,
    )
    .unwrap();
    let m = memory(eid(1), "migration-replay");
    let c = ctx(1, "mig1");
    let r1 = repo
        .apply(
            &c,
            &DomainCommand::AddMemory {
                memory: m,
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    drop(repo);
    // Downgrade the schema marker so reopening runs a real migration.
    {
        let db = OptimisticTxDatabase::builder(path).open().unwrap();
        let meta = db.keyspace("meta", KeyspaceCreateOptions::default).unwrap();
        let mut tx = db.write_tx().unwrap();
        tx.insert(&meta, "schema_version", 0u64.to_le_bytes());
        tx.commit().unwrap().unwrap();
    }
    let clock2: Arc<dyn ltmrs_domain::clock::Clock + Send + Sync> =
        Arc::new(FrozenClock::new(1000));
    let repo2 = CanonicalRepository::open_with_clock(path, clock2).unwrap();
    // The recorded receipt survived the migration ...
    let stored = repo2
        .lookup_receipt(
            StoreGeneration::FIRST,
            ltmrs_domain::id::FrontendId::new(uuid::Uuid::from_u128(1)),
            1,
            r1.operation_id,
        )
        .unwrap()
        .expect("receipt must survive migration");
    assert_eq!(stored.request_digest, "mig1");
    // ... and replaying the same operation replays it with no new effect.
    let m2 = memory(eid(1), "migration-replay");
    let r2 = repo2
        .apply(
            &c,
            &DomainCommand::AddMemory {
                memory: m2,
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    assert_eq!(r1.operation_id, r2.operation_id);
    assert_eq!(
        repo2
            .export_snapshot()
            .unwrap()
            .memories
            .iter()
            .filter(|m| m.title == "migration-replay")
            .count(),
        1,
        "replay after migration must not duplicate"
    );
}
