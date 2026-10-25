//! Interpreter concurrency tests (moved verbatim from `interpreter.rs`).

use super::ReferenceInterpreter;
use super::test_support::*;
use crate::command::{DomainCommand, DomainErrorCode, MemoryPatch, ReceiptOutcome};
use uuid::Uuid;

/// T-CONC-01 fixture: two writers at the same expected revision. Exactly
/// the permitted writer succeeds; the stale writer is rejected and its
/// intent is not silently reapplied.
#[test]
fn t_conc_01_single_writer_wins_at_same_revision() {
    let mut it = ReferenceInterpreter::new(1, 0);
    it.apply(
        &ctx(opid(1), "d1"),
        &DomainCommand::AddMemory {
            memory: mem(1, None),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let id = eid(1);
    let rev = it.memories.get(&id).unwrap().entity_revision;

    // Writer A changes content at the current revision => revision advances.
    let patch_a = MemoryPatch {
        title: Some("updated-a".into()),
        ..Default::default()
    };
    let a = it
        .apply(
            &ctx(opid(2), "da"),
            &DomainCommand::UpdateMemory {
                id,
                expected_revision: Some(rev),
                patch: patch_a,
            },
        )
        .unwrap();
    assert!(matches!(a.outcome, ReceiptOutcome::Success { .. }));

    // Writer B still holds the SAME (now stale) revision => must be rejected.
    let patch_b = MemoryPatch {
        title: Some("updated-b".into()),
        ..Default::default()
    };
    let b = it
        .apply(
            &ctx(opid(3), "db"),
            &DomainCommand::UpdateMemory {
                id,
                expected_revision: Some(rev),
                patch: patch_b,
            },
        )
        .unwrap_err();
    assert_eq!(b.code, DomainErrorCode::RevisionConflict);

    // The stale intent was NOT silently reapplied: revision advanced exactly once.
    assert_eq!(it.memories.get(&id).unwrap().entity_revision, rev.next());
}

/// T-CONC-02 fixture: N independent sessions are all retained; a contested
/// create-if-absent for the same alias has exactly one winner.
#[test]
fn t_conc_02_independent_sessions_contested_alias_one_winner() {
    let mut it = ReferenceInterpreter::new(1, 0);
    for i in 0..32u64 {
        let h = crate::id::SessionHandle::new(Uuid::from_u128(1000 + i as u128));
        let ch = crate::id::ChannelId::new(Uuid::from_u128(2000 + i as u128));
        it.register_session(h, ch, Some("task".into()), None);
    }
    assert_eq!(it.sessions.len(), 32);

    let winner = it
        .apply(
            &ctx(opid(100), "w"),
            &DomainCommand::AddMemory {
                memory: mem(1, Some("contested")),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    assert!(matches!(winner.outcome, ReceiptOutcome::Success { .. }));

    let loser = it
        .apply(
            &ctx(opid(101), "l"),
            &DomainCommand::AddMemory {
                memory: mem(2, Some("contested")),
                session: None,
                auto_link: None,
            },
        )
        .unwrap_err();
    assert_eq!(loser.code, DomainErrorCode::DuplicateAlias);
}
