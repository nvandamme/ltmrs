//! Interpreter oracle-conformance tests (moved verbatim from `interpreter.rs`).

use super::ReferenceInterpreter;
use super::test_support::*;
use crate::command::{DomainCommand, DomainErrorCode};
use crate::id::EntityRevision;

/// Oracle parity (gateway I3): absolute-only writes advance the entity
/// revision, so same-expected writers conflict instead of last-winning.
#[test]
fn oracle_absolute_writes_advance_revision() {
    use crate::command::MemoryPatch;
    let mut it = ReferenceInterpreter::new(1, 0);
    it.apply(
        &ctx(opid(1), "add"),
        &DomainCommand::AddMemory {
            memory: mem(100, None),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let rev1 = it.memories.get(&eid(100)).unwrap().entity_revision;
    it.apply(
        &ctx(opid(2), "abs"),
        &DomainCommand::UpdateMemory {
            id: eid(100),
            expected_revision: None,
            patch: MemoryPatch {
                confidence: Some(0.9),
                ..Default::default()
            },
        },
    )
    .unwrap();
    let rev2 = it.memories.get(&eid(100)).unwrap().entity_revision;
    assert_ne!(rev1, rev2, "absolute-only write must advance revision");
    // A writer holding the old revision must now conflict.
    let err = it
        .apply(
            &ctx(opid(3), "stale"),
            &DomainCommand::UpdateMemory {
                id: eid(100),
                expected_revision: Some(rev1),
                patch: MemoryPatch {
                    confidence: Some(0.1),
                    ..Default::default()
                },
            },
        )
        .unwrap_err();
    assert_eq!(err.code, DomainErrorCode::RevisionConflict);
}

/// Oracle parity (gateway I3): updates to non-Live rows are rejected.
#[test]
fn oracle_update_rejects_non_live() {
    use crate::command::{ForgetMode, MemoryPatch};
    let mut it = ReferenceInterpreter::new(1, 0);
    it.apply(
        &ctx(opid(1), "add"),
        &DomainCommand::AddMemory {
            memory: mem(100, None),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    it.apply(
        &ctx(opid(2), "forget"),
        &DomainCommand::Forget {
            id: eid(100),
            mode: ForgetMode::Archive,
        },
    )
    .unwrap();
    let err = it
        .apply(
            &ctx(opid(3), "upd"),
            &DomainCommand::UpdateMemory {
                id: eid(100),
                expected_revision: None,
                patch: MemoryPatch {
                    confidence: Some(0.9),
                    ..Default::default()
                },
            },
        )
        .unwrap_err();
    assert_eq!(err.code, DomainErrorCode::Validation);
}

/// Oracle parity (gateway I4): relation id reuse with different
/// endpoints fails instead of silently overwriting.
#[test]
fn oracle_relate_id_reuse_with_different_endpoints_fails() {
    use crate::memory::Instant;
    use crate::relation::{Relation, RelationType};
    let mut it = ReferenceInterpreter::new(1, 0);
    for (i, op) in [(100, 1), (101, 2), (102, 3)].iter() {
        it.apply(
            &ctx(opid(*op as u64), &format!("add{i}")),
            &DomainCommand::AddMemory {
                memory: mem(*i, None),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    }
    it.apply(
        &ctx(opid(10), "rel"),
        &DomainCommand::Relate {
            relation: Relation::new(
                eid(900),
                eid(100),
                eid(101),
                RelationType::RelatedTo,
                None,
                Instant::new(1),
            ),
        },
    )
    .unwrap();
    let err = it
        .apply(
            &ctx(opid(11), "rel2"),
            &DomainCommand::Relate {
                relation: Relation::new(
                    eid(900),
                    eid(100),
                    eid(102),
                    RelationType::RelatedTo,
                    None,
                    Instant::new(1),
                ),
            },
        )
        .unwrap_err();
    assert_eq!(err.code, DomainErrorCode::KeyReuseDifferentInput);
    // Same endpoints, different note: also rejected.
    let err = it
        .apply(
            &ctx(opid(12), "rel3"),
            &DomainCommand::Relate {
                relation: Relation::new(
                    eid(900),
                    eid(100),
                    eid(101),
                    RelationType::RelatedTo,
                    Some("changed".to_string()),
                    Instant::new(1),
                ),
            },
        )
        .unwrap_err();
    assert_eq!(err.code, DomainErrorCode::KeyReuseDifferentInput);
}

/// Oracle parity (gateway C8): merge enforces alias uniqueness and
/// registers the result alias.
#[test]
fn oracle_merge_registers_alias_with_uniqueness() {
    let mut it = ReferenceInterpreter::new(1, 0);
    it.apply(
        &ctx(opid(1), "add"),
        &DomainCommand::AddMemory {
            memory: mem(100, Some("taken")),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    // Merge claiming a taken alias fails.
    let mut claimed = mem(200, Some("taken"));
    claimed.entity_revision = EntityRevision::new(0);
    let err = it
        .apply(
            &ctx(opid(2), "merge"),
            &DomainCommand::Merge {
                source_ids: vec![eid(100)],
                result: claimed,
                consolidate: false,
            },
        )
        .unwrap_err();
    assert_eq!(err.code, DomainErrorCode::DuplicateAlias);
    // Merge with a fresh alias registers it: a later add collides.
    let fresh = mem(201, Some("fresh"));
    it.apply(
        &ctx(opid(3), "merge2"),
        &DomainCommand::Merge {
            source_ids: vec![eid(100)],
            result: fresh,
            consolidate: false,
        },
    )
    .unwrap();
    let err = it
        .apply(
            &ctx(opid(4), "add2"),
            &DomainCommand::AddMemory {
                memory: mem(202, Some("fresh")),
                session: None,
                auto_link: None,
            },
        )
        .unwrap_err();
    assert_eq!(err.code, DomainErrorCode::DuplicateAlias);
}
