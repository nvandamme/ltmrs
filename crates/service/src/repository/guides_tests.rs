//! Guide-mutation tests (moved verbatim from `repository.rs`).

use super::test_support::*;
use ltmrs_domain::command::{DomainCommand, DomainErrorCode};
use ltmrs_domain::id::ModelFingerprint;
use uuid::Uuid;

/// Merge changes the recallable set (archived sources, new live result),
/// so it must enqueue projection work for both and dirty open builds —
/// otherwise a cutover could activate missing the result entirely.
#[test]
fn merge_enqueues_jobs_and_dirties_pipeline() {
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
    let next = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
    repo.apply(
        &ctx(3, "merge"),
        &DomainCommand::Merge {
            source_ids: vec![eid(1), eid(2)],
            result: memory(eid(3), "m3"),
            consolidate: false,
        },
    )
    .unwrap();
    // Result gets a pending job; archived sources get tombstone jobs.
    let result_job = repo.projection_job(eid(3)).unwrap().unwrap();
    assert!(!result_job.is_tombstone);
    for s in [eid(1), eid(2)] {
        let tomb = repo.projection_job(s).unwrap().unwrap();
        assert!(tomb.is_tombstone, "archived source needs a tombstone job");
    }
    // And the open build is dirty.
    assert!(repo.generation_record(next).unwrap().unwrap().build_dirty);
    assert!(repo.activate_generation(next).is_err());
}

/// P1 (consolidate graph): merge with consolidate=true creates
/// result→source Supersedes edges in the SAME transaction that
/// archives the sources — never as an ignored post-commit tail (the
/// sources are unrecallable by then, so a later Relate rejects).
#[test]
fn merge_consolidate_creates_supersession_edges_atomically() {
    use ltmrs_domain::relation::RelationType;
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
    repo.apply(
        &ctx(3, "merge"),
        &DomainCommand::Merge {
            source_ids: vec![eid(1), eid(2)],
            result: memory(eid(3), "m3"),
            consolidate: true,
        },
    )
    .unwrap();
    assert!(
        repo.get_memories(&[eid(3)])
            .unwrap()
            .remove(0)
            .lifecycle
            .is_recallable(),
        "result must stay live"
    );
    for s in [eid(1), eid(2)] {
        // Frozen contract (consolidate=true): sources are KEPT, marked
        // superseded via the edges below, and down-weighted — never
        // archived.
        let kept = repo.get_memories(&[s]).unwrap().remove(0);
        assert!(
            kept.lifecycle.is_recallable(),
            "consolidated sources stay recallable"
        );
        assert_eq!(
            kept.confidence, 0.05,
            "consolidated sources down-weight to 0.05"
        );
    }
    let edges: Vec<_> = repo
        .neighbors(eid(3))
        .unwrap()
        .into_iter()
        .filter(|r| r.relation_type == RelationType::Supersedes)
        .collect();
    assert_eq!(
        edges.len(),
        2,
        "exactly two supersession edges, got {edges:?}"
    );
    for e in &edges {
        assert_eq!(e.source, eid(3));
        assert!(e.target == eid(1) || e.target == eid(2));
    }
}

#[test]
fn merge_delete_removes_sources_and_severs_edges() {
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
    // A relation touching a source: hard delete must sever it.
    repo.apply(
        &ctx(10, "rel"),
        &DomainCommand::Relate {
            relation: ltmrs_domain::relation::Relation::new(
                ltmrs_domain::id::EntityId::new(Uuid::from_u128(90)),
                eid(1),
                eid(2),
                ltmrs_domain::relation::RelationType::RelatedTo,
                None,
                ltmrs_domain::memory::Instant::new(1000),
            ),
        },
    )
    .unwrap();
    repo.apply(
        &ctx(3, "merge"),
        &DomainCommand::Merge {
            source_ids: vec![eid(1), eid(2)],
            result: memory(eid(3), "m3"),
            consolidate: false,
        },
    )
    .unwrap();
    // Frozen contract (consolidate=false): sources are deleted —
    // ltmrs hard-delete tombstones (row preserved for audit, like
    // Forget{Delete}): not recallable, edges severed.
    for s in [eid(1), eid(2)] {
        let tomb = repo.get_memories(&[s]).unwrap().pop().unwrap();
        assert!(
            !tomb.lifecycle.is_recallable(),
            "deleted sources must not be recallable"
        );
    }
    assert!(
        repo.all_relations().unwrap().is_empty(),
        "hard delete severs edges involving deleted sources"
    );
    assert!(
        repo.get_memories(&[eid(3)])
            .unwrap()
            .remove(0)
            .lifecycle
            .is_recallable(),
        "result must stay live"
    );
}
/// Re-review R3: a merge planned against stale source revisions rejects
/// explicitly instead of discarding a concurrent update. Sources stay
/// intact and no result appears.
#[test]
fn merge_with_stale_source_revisions_conflicts() {
    let (repo, _dir) = repo_with_ns();
    repo.put_guide(&test_guide("alpha")).unwrap();
    repo.put_guide(&test_guide("beta")).unwrap();
    let rev_alpha = repo.get_guide("alpha").unwrap().unwrap().entity_revision;
    let rev_beta = repo.get_guide("beta").unwrap().unwrap().entity_revision;
    // Concurrent update AFTER planning (practice bumps the revision).
    repo.practice_guide_idempotent(
        &repo.admit_scope(&scope(900, "digest-1")).unwrap(),
        "alpha",
        "dev-tool",
        None,
        &[],
        &["new learning".to_string()],
        &[],
        None,
        1000,
    )
    .unwrap();
    let mut merged = test_guide("gamma");
    merged.usage_count = 2;
    let stale = vec![
        ("alpha".to_string(), rev_alpha),
        ("beta".to_string(), rev_beta),
    ];
    let err = repo
        .merge_guides_atomically(&["alpha".to_string(), "beta".to_string()], &stale, &merged)
        .unwrap_err();
    assert_eq!(err.code, DomainErrorCode::RevisionConflict);
    // Nothing published: sources intact with the concurrent update, no
    // half-merge, no result.
    let alpha = repo.get_guide("alpha").unwrap().unwrap();
    assert!(alpha.learnings.contains(&"new learning".to_string()));
    assert!(repo.get_guide("beta").unwrap().is_some());
    assert!(repo.get_guide("gamma").unwrap().is_none());
    // Fresh revisions commit cleanly.
    let fresh = vec![
        (
            "alpha".to_string(),
            repo.get_guide("alpha").unwrap().unwrap().entity_revision,
        ),
        (
            "beta".to_string(),
            repo.get_guide("beta").unwrap().unwrap().entity_revision,
        ),
    ];
    repo.merge_guides_atomically(&["alpha".to_string(), "beta".to_string()], &fresh, &merged)
        .unwrap();
    assert!(repo.get_guide("gamma").unwrap().is_some());
}

/// Re-review R3: rename/forget of a missing guide fail without
/// publishing anything (no half-rename, no half-forget).
#[test]
fn rename_forget_missing_guide_fail_cleanly() {
    let (repo, _dir) = repo_with_ns();
    repo.put_guide(&test_guide("solo")).unwrap();
    let mut renamed = test_guide("renamed");
    renamed.usage_count = 5;
    let err = repo
        .rename_guide_atomically(
            "missing",
            ltmrs_domain::id::EntityRevision::new(1),
            &renamed,
        )
        .unwrap_err();
    assert_eq!(err.code, DomainErrorCode::NotFound);
    assert!(repo.get_guide("solo").unwrap().is_some());
    assert!(repo.get_guide("renamed").unwrap().is_none());
    assert!(!repo.forget_guide_atomically("missing").unwrap());
    assert!(repo.get_guide("solo").unwrap().is_some());
}

/// Re-review P1-2: an ordinary guide write through the checked path
/// rejects a stale revision instead of overwriting; create-if-absent
/// refuses to clobber an existing guide.
#[test]
fn checked_guide_write_rejects_stale_revision() {
    let (repo, _dir) = repo_with_ns();
    repo.put_guide(&test_guide("g")).unwrap();
    let rev = repo.get_guide("g").unwrap().unwrap().entity_revision;
    // Concurrent writer bumps the revision (practice path).
    repo.practice_guide_idempotent(
        &repo.admit_scope(&scope(901, "d1")).unwrap(),
        "g",
        "dev-tool",
        None,
        &[],
        &[],
        &[],
        None,
        1,
    )
    .unwrap();
    let mut stale = repo.get_guide("g").unwrap().unwrap();
    // Simulate the stale plan: revision captured before the practice.
    let err = repo.put_guide_checked(Some(rev), &stale).unwrap_err();
    assert_eq!(err.code, DomainErrorCode::RevisionConflict);
    // Fresh revision commits.
    stale = repo.get_guide("g").unwrap().unwrap();
    let rev2 = stale.entity_revision;
    stale.description = "updated".into();
    repo.put_guide_checked(Some(rev2), &stale).unwrap();
    assert_eq!(repo.get_guide("g").unwrap().unwrap().description, "updated");
    // Create-if-absent refuses to overwrite.
    let err = repo.put_guide_checked(None, &test_guide("g")).unwrap_err();
    assert_eq!(err.code, DomainErrorCode::Validation);
}

/// Re-review P1-1: a durability-barrier failure fails the ack, and a
/// replay while the barrier still fails fails too — a visible receipt
/// never fabricates durable success. Once the barrier works, the replay
/// resolves to the recorded outcome without recounting.
#[test]
fn practice_replay_without_durability_fails() {
    let (repo, _dir) = repo_with_ns();
    let practice = || {
        repo.practice_guide_idempotent(
            &repo.admit_scope(&scope(902, "digest-p")).unwrap(),
            "git",
            "dev-tool",
            None,
            &[],
            &["learn it".to_string()],
            &[],
            Some(true),
            1000,
        )
    };
    // First execution: barrier fails → error, no ack. (The mutation +
    // receipt committed in-tx; only durability is unestablished.)
    repo.fault_injector().set_persist_failures(1);
    let err = practice().unwrap_err();
    assert!(
        err.message.contains("persist"),
        "barrier failure must fail loudly, got: {err:?}"
    );
    // Retry with the barrier STILL failing: must fail again, never flip
    // the in-tx receipt into a successful ack.
    repo.fault_injector().set_persist_failures(1);
    let err = practice().unwrap_err();
    assert!(
        err.message.contains("persist"),
        "replay without durability must fail loudly, got: {err:?}"
    );
    // Barrier healthy: replay resolves to the recorded outcome, counted
    // exactly once across all three attempts.
    let guide = practice().unwrap();
    assert_eq!(guide.usage_count, 1);
    assert_eq!(guide.success_count, 1);
    assert_eq!(repo.get_guide("git").unwrap().unwrap().usage_count, 1);
}
