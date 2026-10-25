//! Interpreter apply tests (moved verbatim from `interpreter.rs`).

use super::ReferenceInterpreter;
use super::test_support::*;
use crate::command::{DomainCommand, ForgetMode, MemoryPatch, ReceiptOutcome};
use crate::id::{EntityId, EntityRevision};
use crate::memory::Instant;
use uuid::Uuid;

#[test]
fn test_add_memory() {
    let mut interp = ReferenceInterpreter::new(42, 0);
    let ctx = test_ctx(1);
    let memory = test_memory(100);
    let cmd = DomainCommand::AddMemory {
        memory: memory.clone(),
        session: None,
        auto_link: None,
    };
    let receipt = interp.apply(&ctx, &cmd).unwrap();
    assert!(matches!(receipt.outcome, ReceiptOutcome::Success { .. }));
}

#[test]
fn test_idempotent_replay() {
    let mut interp = ReferenceInterpreter::new(42, 0);
    let ctx = test_ctx(1);
    let memory = test_memory(100);
    let cmd = DomainCommand::AddMemory {
        memory,
        session: None,
        auto_link: None,
    };
    let receipt1 = interp.apply(&ctx, &cmd.clone()).unwrap();
    let receipt2 = interp.apply(&ctx, &cmd).unwrap();
    assert_eq!(
        receipt1.operation_id, receipt2.operation_id,
        "idempotent replay returns same operation"
    );
}

#[test]
fn test_revision_conflict() {
    let mut interp = ReferenceInterpreter::new(42, 0);
    let ctx1 = test_ctx(1);
    let memory = test_memory(100);
    let cmd = DomainCommand::AddMemory {
        memory,
        session: None,
        auto_link: None,
    };
    let receipt = interp.apply(&ctx1, &cmd).unwrap();
    let affected = match receipt.outcome {
        ReceiptOutcome::Success { affected } => affected,
        _ => vec![],
    };
    let id = affected[0];

    let ctx2 = test_ctx(2);
    let stale_cmd = DomainCommand::UpdateMemory {
        id,
        expected_revision: Some(EntityRevision::new(999)),
        patch: MemoryPatch::default(),
    };
    let result = interp.apply(&ctx2, &stale_cmd);
    assert!(result.is_err());
}

#[test]
fn test_supersession_cycle_rejected() {
    let mut interp = ReferenceInterpreter::new(42, 0);
    let m1 = test_memory(1);
    let m2 = test_memory(2);
    interp
        .apply(
            &test_ctx(1),
            &DomainCommand::AddMemory {
                memory: m1.clone(),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    interp
        .apply(
            &test_ctx(2),
            &DomainCommand::AddMemory {
                memory: m2.clone(),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();

    let rel1 = crate::relation::Relation::new(
        EntityId::new(Uuid::from_u128(100)),
        m1.id,
        m2.id,
        crate::relation::RelationType::Supersedes,
        None,
        Instant::new(0),
    );
    interp
        .apply(&test_ctx(3), &DomainCommand::Relate { relation: rel1 })
        .unwrap();

    let rel2 = crate::relation::Relation::new(
        EntityId::new(Uuid::from_u128(101)),
        m2.id,
        m1.id,
        crate::relation::RelationType::Supersedes,
        None,
        Instant::new(0),
    );
    let result = interp.apply(&test_ctx(4), &DomainCommand::Relate { relation: rel2 });
    assert!(result.is_err());
}

#[test]
fn test_merge_deletes_sources() {
    let mut interp = ReferenceInterpreter::new(42, 0);
    let m1 = test_memory(1);
    let m2 = test_memory(2);
    let m3 = test_memory(3);
    interp
        .apply(
            &test_ctx(1),
            &DomainCommand::AddMemory {
                memory: m1.clone(),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    interp
        .apply(
            &test_ctx(2),
            &DomainCommand::AddMemory {
                memory: m2.clone(),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();

    let cmd = DomainCommand::Merge {
        source_ids: vec![m1.id, m2.id],
        result: m3.clone(),
        consolidate: false,
    };
    let receipt = interp.apply(&test_ctx(3), &cmd).unwrap();
    let affected = match receipt.outcome {
        ReceiptOutcome::Success { affected } => affected,
        _ => vec![],
    };
    assert_eq!(affected.len(), 3);
}

#[test]
fn test_merge_consolidate_keeps_and_down_weights_sources() {
    let mut interp = ReferenceInterpreter::new(42, 0);
    let m1 = test_memory(1);
    let m2 = test_memory(2);
    let m3 = test_memory(3);
    for (n, m) in [(1, m1.clone()), (2, m2.clone())] {
        interp
            .apply(
                &test_ctx(n),
                &DomainCommand::AddMemory {
                    memory: m,
                    session: None,
                    auto_link: None,
                },
            )
            .unwrap();
    }
    let cmd = DomainCommand::Merge {
        source_ids: vec![m1.id, m2.id],
        result: m3.clone(),
        consolidate: true,
    };
    interp.apply(&test_ctx(3), &cmd).unwrap();
    for id in [m1.id, m2.id] {
        let source = interp.memories.get(&id).expect("source kept");
        assert!(
            source.lifecycle.is_recallable(),
            "consolidated sources stay live"
        );
        assert_eq!(
            source.confidence,
            crate::memory::CONSOLIDATED_CONFIDENCE,
            "consolidated sources down-weight"
        );
    }
    assert_eq!(
        interp
            .relations
            .iter()
            .filter(|r| r.relation_type.is_supersession())
            .count(),
        2,
        "consolidation records supersession edges"
    );
}

#[test]
fn test_feedback_updates_counters() {
    let mut interp = ReferenceInterpreter::new(42, 0);
    let m1 = test_memory(1);
    interp
        .apply(
            &test_ctx(1),
            &DomainCommand::AddMemory {
                memory: m1.clone(),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();

    interp
        .apply(
            &test_ctx(2),
            &DomainCommand::Feedback {
                memory_id: m1.id,
                useful: true,
            },
        )
        .unwrap();
    interp
        .apply(
            &test_ctx(3),
            &DomainCommand::Feedback {
                memory_id: m1.id,
                useful: false,
            },
        )
        .unwrap();

    assert_eq!(interp.feedback.len(), 2);
}

#[test]
fn test_forget_modes() {
    let mut interp = ReferenceInterpreter::new(42, 0);
    let m1 = test_memory(1);
    interp
        .apply(
            &test_ctx(1),
            &DomainCommand::AddMemory {
                memory: m1.clone(),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();

    interp
        .apply(
            &test_ctx(2),
            &DomainCommand::Forget {
                id: m1.id,
                mode: ForgetMode::Invalidate,
            },
        )
        .unwrap();

    let export = interp.export();
    let memory = export.memories.iter().find(|m| m.id == m1.id).unwrap();
    assert!(!memory.lifecycle.is_recallable());
}

#[test]
fn test_duplicate_memory_rejected() {
    let mut interp = ReferenceInterpreter::new(42, 0);
    let m1 = test_memory(1);
    interp
        .apply(
            &test_ctx(1),
            &DomainCommand::AddMemory {
                memory: m1.clone(),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();

    let result = interp.apply(
        &test_ctx(2),
        &DomainCommand::AddMemory {
            memory: m1,
            session: None,
            auto_link: None,
        },
    );
    assert!(result.is_err());
}

#[test]
fn test_key_reuse_different_input() {
    let mut interp = ReferenceInterpreter::new(42, 0);
    let ctx1 = test_ctx(1);
    let m1 = test_memory(1);
    interp
        .apply(
            &ctx1,
            &DomainCommand::AddMemory {
                memory: m1,
                session: None,
                auto_link: None,
            },
        )
        .unwrap();

    let mut ctx2 = test_ctx(1);
    ctx2.request_digest = "different-digest".to_string();
    let m2 = test_memory(2);
    let result = interp.apply(
        &ctx2,
        &DomainCommand::AddMemory {
            memory: m2,
            session: None,
            auto_link: None,
        },
    );
    assert!(result.is_err());
}

#[test]
fn test_export_digest_stable() {
    let mut interp = ReferenceInterpreter::new(42, 0);
    let m1 = test_memory(1);
    interp
        .apply(
            &test_ctx(1),
            &DomainCommand::AddMemory {
                memory: m1,
                session: None,
                auto_link: None,
            },
        )
        .unwrap();

    let export1 = interp.export();
    let digest1 = export1.digest();
    let export2 = interp.export();
    let digest2 = export2.digest();
    assert_eq!(digest1, digest2);
}
