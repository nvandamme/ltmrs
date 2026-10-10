//! Durability-barrier tests (moved verbatim from `repository.rs`).

use super::test_support::*;
use ltmrs_domain::command::DomainCommand;
use ltmrs_domain::id::StoreGeneration;

/// No ACK without a barrier: an injected persist failure must fail the
/// command (not silently succeed with buffered-only data).
#[test]
fn ack_requires_durability_barrier() {
    let (repo, _dir) = repo_with_ns();
    repo.fault_injector().set_persist_failures(1);
    let err = repo
        .apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "m"),
                session: None,
                auto_link: None,
            },
        )
        .unwrap_err();
    assert!(
        err.message.contains("persist"),
        "barrier failure must fail the ACK, got: {err:?}"
    );
    // Counter consumed: the retry succeeds and the write is real.
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "m"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    assert_eq!(repo.get_memories(&[eid(1)]).unwrap().len(), 1);
}

/// The restore path honors the same barrier: an injected persist
/// failure fails the replace instead of reporting a durable cutover
/// on buffered-only data.
#[test]
fn restore_replace_requires_durability_barrier() {
    let (repo, _dir) = repo_with_ns();
    repo.fault_injector().set_persist_failures(1);
    let err = repo
        .restore_replace(&[], &[], &[], &[], &[], &[], StoreGeneration::new(2))
        .unwrap_err();
    assert!(
        err.message.contains("persist"),
        "barrier failure must fail the restore, got: {err:?}"
    );
}

/// Direct writes ACK through the same barrier.
#[test]
fn direct_write_requires_durability_barrier() {
    let (repo, _dir) = repo_with_ns();
    repo.fault_injector().set_persist_failures(1);
    let err = repo.put_memory_direct(&memory(eid(9), "x")).unwrap_err();
    assert!(
        err.message.contains("persist"),
        "barrier failure must fail the write, got: {err:?}"
    );
}
