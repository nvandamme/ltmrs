//! GC / expiry-collection tests (moved verbatim from `admission_tests.rs`).

use super::test_support::*;
use super::{CanonicalRepository, namespace_key};
use fjall::Readable;
use ltmrs_domain::command::{DomainCommand, DomainErrorCode};
use ltmrs_domain::id::{OperationId, StoreGeneration};
use uuid::Uuid;

#[test]
fn expired_namespace_receipts_are_gc_d() {
    let dir = tempfile::tempdir().unwrap();
    // Frozen clock at 1000 (within the namespace's validity window) so the
    // apply() below passes namespace validation deterministically.
    let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
    let repo = CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), clock).unwrap();
    let fe = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1));
    let ns = repo.issue_namespace(fe, ch(2), 1000).unwrap();

    // Apply a command under this namespace.
    let mut c = ctx(1, "d1");
    c.retry_epoch = ns.retry_epoch;
    repo.apply(
        &c,
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "x"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    assert!(
        repo.lookup_receipt(StoreGeneration::FIRST, fe, ns.retry_epoch, c.operation_id)
            .unwrap()
            .is_some()
    );

    // GC at a time beyond the namespace expiry removes the receipt.
    let removed = repo.gc_expired(ns.expires_at + 1).unwrap();
    assert!(removed >= 1, "expired receipt should be removed");
    assert!(
        repo.lookup_receipt(StoreGeneration::FIRST, fe, ns.retry_epoch, c.operation_id)
            .unwrap()
            .is_none()
    );
}

/// GC must remove EVERY receipt under an expired namespace: removing
/// while iterating the keyspace risks skipping entries (iterator
/// invalidation) and orphaning receipts no future GC re-triggers for.
#[test]
fn gc_expired_removes_all_receipts_under_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
    let repo = CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), clock).unwrap();
    let fe = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1));
    let ns = repo.issue_namespace(fe, ch(2), 1000).unwrap();

    let mut ops = Vec::new();
    for (i, op) in [(1u64, 10u64), (2, 11), (3, 12)] {
        let mut c = ctx(op, &format!("d{op}"));
        c.retry_epoch = ns.retry_epoch;
        repo.apply(
            &c,
            &DomainCommand::AddMemory {
                memory: memory(eid(i), "x"),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
        ops.push(c.operation_id);
    }

    let removed = repo.gc_expired(ns.expires_at + 1).unwrap();
    assert_eq!(
        removed, 3,
        "all three receipts must be removed, got {removed}"
    );
    for op in ops {
        assert!(
            repo.lookup_receipt(StoreGeneration::FIRST, fe, ns.retry_epoch, op)
                .unwrap()
                .is_none(),
            "receipt {op:?} orphaned by GC"
        );
    }
}
/// Contention contract: under same-frontend concurrent issuance every
/// conflicting attempt must report transient Contention (retryable),
/// never a fatal Validation — and every issue still succeeds on retry
/// with a distinct epoch.
#[test]
fn namespace_contention_is_transient_typed() {
    use ltmrs_domain::id::FrontendId;
    use std::sync::{Arc, Barrier};
    let (repo, _dir) = repo_with_ns();
    let repo = Arc::new(repo);
    let fe = FrontendId::new(Uuid::from_u128(99));
    let start = Arc::new(Barrier::new(9));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let repo = Arc::clone(&repo);
        let start = Arc::clone(&start);
        handles.push(std::thread::spawn(move || {
            start.wait();
            let mut epochs = Vec::new();
            for _ in 0..25 {
                for _ in 0..20 {
                    match repo.issue_namespace(fe, ch(2), 1000) {
                        Ok(ns) => {
                            epochs.push(ns.retry_epoch);
                            break;
                        }
                        Err(e) => assert_eq!(
                            e.code,
                            ltmrs_domain::command::DomainErrorCode::Contention,
                            "contention must be typed transient, got {e:?}"
                        ),
                    }
                }
            }
            epochs
        }));
    }
    start.wait();
    let mut all = Vec::new();
    for h in handles {
        all.extend(h.join().unwrap());
    }
    assert_eq!(all.len(), 200, "every issue must succeed after retries");
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), 200, "epochs must be distinct");
}

/// A corrupt epoch counter fails closed (epoch reuse would confuse
/// replays across channels): short/long payloads at the epoch key are
/// Validation, never silently reset to epoch 1.
#[test]
fn corrupt_epoch_counter_fails_closed() {
    use ltmrs_domain::id::FrontendId;
    let (repo, _dir) = repo_with_ns();
    let fe = FrontendId::new(Uuid::from_u128(99));
    let mut tx = repo.db.write_tx().unwrap();
    tx.insert(&repo.namespaces, namespace_key(fe), [0xFFu8; 3]);
    tx.commit().unwrap().unwrap();
    let err = repo.issue_namespace(fe, ch(2), 1000).unwrap_err();
    assert_eq!(err.code, DomainErrorCode::Validation);
    assert!(
        err.message.contains("corrupt namespace epoch counter"),
        "must name the corruption, got: {err:?}"
    );
}

/// Undecodable namespace records are healed by GC: every reader fails
/// closed on them already, so removal changes no observable outcome
/// but stops the entry leaking forever past every GC pass. Their
/// orphaned receipts go too (no trigger could ever match them);
/// other frontends' receipts are untouched. Malformed watermark keys
/// heal the same way so op_seq un-bricks.
#[test]
fn gc_removes_corrupt_namespace_records() {
    use ltmrs_domain::id::FrontendId;
    let (repo, _dir) = repo_with_ns();
    let fe = FrontendId::new(Uuid::from_u128(1));
    // Two receipted commands under fe1/epoch1.
    for (n, op) in [(1u64, 10u64), (2, 11)] {
        let mut c = ctx(op, &format!("d{op}"));
        c.retry_epoch = 1;
        repo.apply(
            &c,
            &DomainCommand::AddMemory {
                memory: memory(eid(n), "x"),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    }
    // Corrupt fe1's namespace record + the watermark, and plant a
    // foreign-frontend receipt row (raw bytes: GC must not touch it).
    let mut tx = repo.db.write_tx().unwrap();
    let bad_ns = format!("ns:{}:1", fe.as_uuid());
    tx.insert(&repo.namespaces, &bad_ns, [0xFFu8; 5]);
    tx.insert(&repo.namespaces, "op_seq:direct", [0xFFu8; 3]);
    let foreign_key = format!(
        "1:ffffffff-ffff-ffff-ffff-ffffffffffff:9:{}",
        Uuid::from_u128(77)
    );
    tx.insert(&repo.receipts, &foreign_key, [0xFFu8; 1]);
    tx.commit().unwrap().unwrap();

    repo.gc_expired(1000).unwrap();

    let snapshot = repo.db.read_tx();
    let get = |ks: &_, k: &str| snapshot.get(ks, k).unwrap().is_some();
    assert!(!get(&repo.namespaces, &bad_ns), "corrupt ns healed");
    assert!(!get(&repo.namespaces, "op_seq:direct"), "watermark healed");
    assert!(
        get(&repo.receipts, &foreign_key),
        "foreign receipts untouched"
    );
    drop(snapshot);
    // fe1's orphaned receipts went with the corrupt record.
    for op in [10u64, 11] {
        assert!(
            repo.lookup_receipt(
                StoreGeneration::FIRST,
                fe,
                1,
                OperationId::new(Uuid::from_u128(op as u128))
            )
            .unwrap()
            .is_none(),
            "orphaned receipt must not leak"
        );
    }
    // Watermark reads again: fe1's two executions.
    assert_eq!(repo.op_seq().unwrap(), 2);
}

/// Corrupt healing is epoch-scoped, not frontend-scoped: a corrupt
/// epoch-1 record must not take down live epoch-2 receipts (RQ-06
/// replay for the live epoch keeps working).
#[test]
fn gc_corrupt_healing_preserves_live_epochs() {
    use ltmrs_domain::id::FrontendId;
    let (repo, _dir) = repo_with_ns();
    let fe = FrontendId::new(Uuid::from_u128(1));
    let mut c = ctx(10, "d10");
    c.retry_epoch = 1;
    repo.apply(
        &c,
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "x"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    // Second epoch, live: must survive the healing below.
    let ns2 = repo.issue_namespace(fe, ch(2), 1000).unwrap();
    assert_eq!(ns2.retry_epoch, 2);
    let mut c2 = ctx(11, "d11");
    c2.retry_epoch = 2;
    repo.apply(
        &c2,
        &DomainCommand::AddMemory {
            memory: memory(eid(2), "x"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    // Corrupt epoch 1's record only.
    let mut tx = repo.db.write_tx().unwrap();
    tx.insert(
        &repo.namespaces,
        format!("ns:{}:1", fe.as_uuid()),
        [0xFFu8; 5],
    );
    tx.commit().unwrap().unwrap();

    repo.gc_expired(1000).unwrap();

    assert!(
        repo.lookup_receipt(
            StoreGeneration::FIRST,
            fe,
            1,
            OperationId::new(Uuid::from_u128(10))
        )
        .unwrap()
        .is_none(),
        "corrupt epoch's receipt goes with it"
    );
    assert!(
        repo.lookup_receipt(
            StoreGeneration::FIRST,
            fe,
            2,
            OperationId::new(Uuid::from_u128(11))
        )
        .unwrap()
        .is_some(),
        "live epoch's receipt must survive healing"
    );
    assert!(
        repo.lookup_namespace(fe, 2).unwrap().is_some(),
        "live epoch's namespace must survive healing"
    );
}

#[test]
fn gc_does_not_delete_other_frontends_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
    let repo = CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), clock).unwrap();
    // Two frontends, each at epoch 1 (same retry_epoch value), but issued
    // at different times so A expires before B.
    let fe_a = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1));
    let fe_b = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(2));
    let ns_a = repo.issue_namespace(fe_a, ch(2), 1000).unwrap();
    let ns_b = repo.issue_namespace(fe_b, ch(2), 2000).unwrap();
    assert_eq!(
        ns_a.retry_epoch, ns_b.retry_epoch,
        "precondition: shared epoch"
    );

    // Apply one command under each frontend's namespace.
    let mut c_a = ctx(1, "d1");
    c_a.retry_epoch = ns_a.retry_epoch;
    repo.apply(
        &c_a,
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "a"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let mut c_b = ctx(2, "d2");
    c_b.frontend_id = fe_b;
    c_b.retry_epoch = ns_b.retry_epoch;
    repo.apply(
        &c_b,
        &DomainCommand::AddMemory {
            memory: memory(eid(2), "b"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();

    // Both receipts present.
    assert!(
        repo.lookup_receipt(
            StoreGeneration::FIRST,
            fe_a,
            ns_a.retry_epoch,
            c_a.operation_id
        )
        .unwrap()
        .is_some()
    );
    assert!(
        repo.lookup_receipt(
            StoreGeneration::FIRST,
            fe_b,
            ns_b.retry_epoch,
            c_b.operation_id
        )
        .unwrap()
        .is_some()
    );

    // GC at A's expiry: A is expired, B (issued 1000ms later) is still
    // valid. Both share retry_epoch=1, so without the frontend scoping fix
    // B's receipt would be wrongly deleted.
    let removed = repo.gc_expired(ns_a.expires_at + 1).unwrap();
    assert!(removed >= 1, "A's expired receipt should be removed");

    // Frontend B's receipt must NOT have been deleted (different frontend).
    assert!(
        repo.lookup_receipt(
            StoreGeneration::FIRST,
            fe_b,
            ns_b.retry_epoch,
            c_b.operation_id
        )
        .unwrap()
        .is_some(),
        "GC of A's namespace must not delete B's receipts"
    );
}
