//! Session-operation tests (moved verbatim from `repository.rs`).

use super::test_support::*;
use super::{CanonicalRepository, SessionLinkField, ToolReplayStatus};
use ltmrs_domain::command::{DomainCommand, DomainErrorCode};
use ltmrs_domain::id::SessionHandle;
use ltmrs_domain::session::SessionOp;
use uuid::Uuid;

#[test]
fn first_tool_freeze_wins_over_later_duplicates() {
    let (repo, _dir) = repo_with_ns();
    let admitted = admit(&repo, 1, "d1");
    let scope = scope(1, "d1");
    // A receipt must exist first (freeze answers replays of executions).
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "x"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let first = ltmrs_domain::session::FrozenToolResponse {
        text: "first".to_string(),
        structured: Some(serde_json::json!({"n": 1})),
        is_error: false,
    };
    let second = ltmrs_domain::session::FrozenToolResponse {
        text: "second".to_string(),
        structured: Some(serde_json::json!({"n": 2})),
        is_error: false,
    };
    repo.freeze_tool_result(&admitted, &scope, &first).unwrap();
    // A duplicate in-flight execution must not overwrite the frozen
    // verbatim response.
    repo.freeze_tool_result(&admitted, &scope, &second).unwrap();
    match repo.check_tool_replay(&scope).unwrap() {
        ToolReplayStatus::Frozen(back) => {
            assert_eq!(back.text, "first");
            assert_eq!(back.structured, Some(serde_json::json!({"n": 1})));
        }
        other => panic!("expected frozen first response, got {other:?}"),
    }
}
/// P2-B: continuity-recall boosts apply exactly once per session-start
/// operation. The first claim applies and returns true; a second claim
/// (crash-window continuation) returns false without touching
/// confidence; a changed digest rejects.
#[test]
fn continuity_boost_claim_applies_once() {
    use ltmrs_domain::id::SessionHandle;
    use ltmrs_domain::session::{AttemptOutcome, SessionOp};
    let (repo, _dir) = repo_with_ns();
    let handle = SessionHandle::new(Uuid::from_u128(100));
    match repo
        .session_start_tx(
            &scope(101, "digest-start"),
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
    match repo
        .session_attempt_tx(
            &scope(102, "digest-attempt"),
            handle,
            "try X".to_string(),
            AttemptOutcome::Rejected,
            None,
            None,
            None,
            1000,
        )
        .unwrap()
    {
        SessionOp::Applied((_, 1)) => {}
        other => panic!("expected Applied seq 1, got {other:?}"),
    }
    // Lower first: fresh attempts start at the 1.0 ceiling where a
    // small positive delta clamps invisibly.
    repo.adjust_attempt(handle, 1, -0.5, 1000).unwrap();
    let targets = vec![(handle, 1)];
    assert!(
        repo.claim_continuity_boost(&admit(&repo, 101, "digest-start"), &targets, 0.015, 1000)
            .unwrap()
    );
    let after_first = repo.get_session(handle).unwrap().unwrap().attempts[0].confidence;
    assert!((after_first - 0.515).abs() < 1e-9, "got {after_first}");
    assert!(
        !repo
            .claim_continuity_boost(&admit(&repo, 101, "digest-start"), &targets, 0.015, 1000)
            .unwrap()
    );
    let after_second = repo.get_session(handle).unwrap().unwrap().attempts[0].confidence;
    assert_eq!(after_second, after_first, "second claim must not re-boost");
    let err = repo
        .claim_continuity_boost(&admit(&repo, 101, "DIFFERENT"), &targets, 0.015, 1000)
        .unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::KeyReuseDifferentInput
    );
}

/// Attempt-decay still runs (post-commit now): starting a second
/// session decays the first session's attempts by 0.002. Guards the
/// hotspot fix against silently dropping the policy.
#[test]
fn session_start_still_decays_prior_attempts() {
    use ltmrs_domain::id::SessionHandle;
    use ltmrs_domain::session::{AttemptOutcome, SessionOp};
    let (repo, _dir) = repo_with_ns();
    let h1 = SessionHandle::new(Uuid::from_u128(810));
    match repo
        .session_start_tx(
            &scope(810, "d-s1"),
            h1,
            None,
            None,
            vec![],
            None,
            None,
            1000,
        )
        .unwrap()
    {
        SessionOp::Applied(h) => assert_eq!(h, h1),
        other => panic!("expected Applied, got {other:?}"),
    }
    match repo
        .session_attempt_tx(
            &scope(811, "d-a1"),
            h1,
            "try X".to_string(),
            AttemptOutcome::Rejected,
            None,
            None,
            None,
            1000,
        )
        .unwrap()
    {
        SessionOp::Applied((_, 1)) => {}
        other => panic!("expected Applied seq 1, got {other:?}"),
    }
    let h2 = SessionHandle::new(Uuid::from_u128(812));
    match repo
        .session_start_tx(
            &scope(813, "d-s2"),
            h2,
            None,
            None,
            vec![],
            None,
            None,
            1000,
        )
        .unwrap()
    {
        SessionOp::Applied(h) => assert_eq!(h, h2),
        other => panic!("expected Applied, got {other:?}"),
    }
    let conf = repo.get_session(h1).unwrap().unwrap().attempts[0].confidence;
    assert!(
        (conf - 0.998).abs() < 1e-9,
        "prior attempt must decay 1.0 -> 0.998, got {conf}"
    );
}

/// Advancing test clock (interior mutability so the repo's shared
/// Arc can move time forward between a primary commit and its
/// response finalization — exactly the TTL-boundary race).
struct AdvancingClock(std::sync::Mutex<u64>);

impl AdvancingClock {
    fn advance(&self, millis: u64) {
        *self.0.lock().unwrap() += millis;
    }
}

impl ltmrs_domain::clock::Clock for AdvancingClock {
    fn now_millis(&self) -> u64 {
        *self.0.lock().unwrap()
    }
}

/// P1 (admission contract): an operation admitted while its namespace
/// is live completes even if the namespace expires between primary
/// commit and response freeze — and GC cannot collect the pinned
/// namespace or its receipts mid-operation. Unpinned, the same
/// expiry collects normally.
#[test]
fn admitted_operation_survives_expiry_and_gc() {
    use ltmrs_domain::id::SessionHandle;
    use ltmrs_domain::session::{AttemptOutcome, SessionOp};
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(AdvancingClock(std::sync::Mutex::new(1000)));
    let repo = CanonicalRepository::open_with_clock(
        dir.path().to_str().unwrap(),
        Arc::clone(&clock) as Arc<dyn ltmrs_domain::clock::Clock + Send + Sync>,
    )
    .unwrap();
    let fe = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1));
    repo.issue_namespace(fe, ch(2), 1000).unwrap();
    let late = 1000 + crate::repository::DEFAULT_NAMESPACE_TTL_MILLIS + 1;
    // Primary effects commit while live.
    let handle = SessionHandle::new(Uuid::from_u128(830));
    match repo
        .session_start_tx(
            &scope(830, "d-s"),
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
    match repo
        .session_attempt_tx(
            &scope(831, "d-a"),
            handle,
            "try X".to_string(),
            AttemptOutcome::Rejected,
            None,
            None,
            None,
            1000,
        )
        .unwrap()
    {
        SessionOp::Applied((_, 1)) => {}
        other => panic!("expected Applied seq 1, got {other:?}"),
    }
    // Admit the attempt scope, then cross the TTL boundary.
    let admitted = repo.admit_scope(&scope(831, "d-a")).unwrap();
    clock.advance(late - 1000);
    // Response freeze under admission: no revalidation is possible,
    // so expiry cannot fail the executed attempt.
    repo.store_session_response(
        &admitted,
        &ltmrs_domain::session::FrozenToolResponse {
            text: "done".to_string(),
            structured: None,
            is_error: false,
        },
    )
    .unwrap();
    // GC at the boundary: pinned namespace + receipts survive
    // collection (resume still refuses: expiry refusal is not
    // collection — the receipts stay replayable under admission).
    let removed = repo.gc_expired(late).unwrap();
    assert!(
        repo.lookup_namespace(fe, 1).unwrap().is_some(),
        "pinned namespace must survive GC, removed={removed}"
    );
    assert!(
        repo.session_receipt(&admitted).unwrap().is_some(),
        "pinned receipt must survive GC"
    );
    // Unpinned, the same expiry collects normally.
    drop(admitted);
    repo.gc_expired(late).unwrap();
    assert!(
        repo.lookup_namespace(fe, 1).unwrap().is_none(),
        "unpinned expired namespace must collect"
    );
}

/// P1 (post-commit purity): a hard decay failure after the start
/// receipt committed must never turn the executed start into an
/// error. The persist fault is armed from the post-commit hook, so
/// it strikes decay's barrier — never the start's.
#[test]
fn session_start_applies_despite_post_commit_decay_failure() {
    use ltmrs_domain::id::SessionHandle;
    use ltmrs_domain::session::SessionOp;
    use std::sync::Arc;
    let (repo, _dir) = repo_with_ns();
    let repo = Arc::new(repo);
    let armer = Arc::clone(&repo);
    repo.set_commit_hook(Arc::new(move || {
        armer.fault_injector().set_persist_failures(1);
    }));
    let handle = SessionHandle::new(Uuid::from_u128(820));
    match repo
        .session_start_tx(
            &scope(820, "d-decay"),
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
        other => panic!("decay failure must not fail the start, got {other:?}"),
    }
    assert!(repo.get_session(handle).unwrap().is_some());
}

/// P1 (tool atomicity): the session-link continuation stage fails
/// loudly on a durability-barrier failure — multi-effect tools must
/// propagate this, never freeze success over it.
#[test]
fn session_link_barrier_failure_errors() {
    use ltmrs_domain::id::SessionHandle;
    let (repo, _dir) = repo_with_ns();
    let handle = SessionHandle::new(Uuid::from_u128(700));
    repo.session_start_tx(
        &scope(700, "d-link"),
        handle,
        None,
        None,
        vec![],
        None,
        None,
        1000,
    )
    .unwrap();
    let admitted = admit(&repo, 701, "d-link-use");
    repo.fault_injector().set_persist_failures(1);
    let err = repo
        .track_session_link(
            &admitted,
            handle,
            SessionLinkField::MemoryCreated,
            &["m1".to_string()],
        )
        .unwrap_err();
    assert_eq!(err.code, DomainErrorCode::Validation);
}

/// Re-review R5: a session_end guide effect applies exactly once per
/// operation — retry resumes via the marker without double-counting,
/// and a missing guide is a skip (forget wins), not an error.
#[test]
fn session_guide_effect_applies_once_per_operation() {
    let (repo, _dir) = repo_with_ns();
    repo.put_guide(&test_guide("git")).unwrap();
    assert!(
        repo.apply_session_guide_effect(&scope(920, "digest-1"), "git", true, 1000)
            .unwrap()
    );
    assert_eq!(repo.get_guide("git").unwrap().unwrap().success_count, 1);
    // Same operation again: marker hit, no recount.
    assert!(
        !repo
            .apply_session_guide_effect(&scope(920, "digest-1"), "git", true, 1000)
            .unwrap()
    );
    assert_eq!(repo.get_guide("git").unwrap().unwrap().success_count, 1);
    // Same operation with changed arguments after a partial effect:
    // reject, never complete a mixed outcome (re-review P1-3).
    let err = repo
        .apply_session_guide_effect(&scope_in(1, 2, 920, "digest-CHANGED"), "git", false, 1000)
        .unwrap_err();
    assert_eq!(err.code, DomainErrorCode::KeyReuseDifferentInput);
    let g = repo.get_guide("git").unwrap().unwrap();
    assert_eq!((g.success_count, g.failure_count), (1, 0));
    // Same guide, different operation: applies (independent outcome).
    assert!(
        repo.apply_session_guide_effect(&scope(921, "digest-2"), "git", false, 1000)
            .unwrap()
    );
    let g = repo.get_guide("git").unwrap().unwrap();
    assert_eq!((g.success_count, g.failure_count), (1, 1));
    // Missing guide: skip, no marker (a later retry re-checks).
    assert!(
        !repo
            .apply_session_guide_effect(&scope(922, "digest-3"), "gone", true, 1000)
            .unwrap()
    );
    assert!(
        !repo
            .apply_session_guide_effect(&scope(922, "digest-3"), "gone", true, 1000)
            .unwrap()
    );
}

/// First freeze wins: two in-flight executions of the same operation
/// that both pass the unfrozen check must not let the second silently
/// replace the first — callers would receive divergent "verbatim"
/// responses for one operation id.
#[test]
fn session_response_freeze_keeps_first() {
    use ltmrs_domain::session::FrozenToolResponse;
    let (repo, _dir) = repo_with_ns();
    let fe = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1));
    repo.issue_namespace(fe, ch(9), 1000).unwrap();
    let handle = SessionHandle::new(Uuid::from_u128(100));
    match repo
        .session_start_tx(
            &scope_in(2, 9, 800, "digest-freeze"),
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
    let first = FrozenToolResponse {
        text: "first".to_string(),
        structured: None,
        is_error: false,
    };
    let second = FrozenToolResponse {
        text: "second".to_string(),
        structured: None,
        is_error: false,
    };
    let fscope = scope_in(2, 9, 800, "digest-freeze");
    let admitted = repo.admit_scope(&fscope).unwrap();
    repo.store_session_response(&admitted, &first).unwrap();
    repo.store_session_response(&admitted, &second).unwrap();
    let stored = repo
        .session_receipt(&admitted)
        .unwrap()
        .expect("receipt must exist");
    assert_eq!(
        stored.response.as_ref().map(|r| r.text.as_str()),
        Some("first"),
        "second freeze must not replace the first"
    );
}
