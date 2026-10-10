//! Interpreter session/guide tests (moved verbatim from `interpreter.rs`).

use super::ReferenceInterpreter;
use super::test_support::*;
use crate::command::{DomainCommand, ReceiptOutcome};
use crate::id::{ChannelId, ExternalAlias, SessionHandle};
use crate::session::TaskOutcome;
use uuid::Uuid;

#[test]
fn test_session_lifecycle() {
    let mut interp = ReferenceInterpreter::new(42, 0);
    let handle = SessionHandle::new(Uuid::from_u128(1000));
    interp.register_session(
        handle,
        ChannelId::new(Uuid::from_u128(2)),
        Some("debugging".to_string()),
        None,
    );

    let ctx = test_ctx(1);
    let cmd = DomainCommand::EndSession {
        session: handle,
        outcome: TaskOutcome::Success,
        final_approach: Some("fixed the bug".to_string()),
        lessons: vec!["always check logs".to_string()],
    };
    let receipt = interp.apply(&ctx, &cmd).unwrap();
    assert!(matches!(receipt.outcome, ReceiptOutcome::Success { .. }));
}

#[test]
fn test_guide_practice() {
    let mut interp = ReferenceInterpreter::new(42, 0);
    let ctx = test_ctx(1);
    let cmd = DomainCommand::GuidePractice {
        guide: "react".to_string(),
        category: "web-frontend".to_string(),
        contexts: vec!["hooks".to_string()],
        learnings: vec!["useCallback prevents re-renders".to_string()],
        outcome: Some(true),
    };
    let receipt = interp.apply(&ctx, &cmd).unwrap();
    assert!(matches!(receipt.outcome, ReceiptOutcome::Success { .. }));
}

#[test]
fn test_alias_allocation_collision() {
    let mut interp = ReferenceInterpreter::new(42, 0);
    let mut m1 = test_memory(1);
    m1.external_alias = Some(ExternalAlias::new("abc123"));
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

    let alias = interp.allocate_alias("abc123");
    assert_ne!(alias.as_str(), "abc123");
}
