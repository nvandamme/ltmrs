//! Namespace / admission / GC tests (moved verbatim from `repository.rs`).

use super::test_support::*;
use super::{CanonicalRepository, namespace_key};
use fjall::Readable;
use ltmrs_domain::command::{DomainCommand, DomainErrorCode};
use ltmrs_domain::id::StoreGeneration;
use uuid::Uuid;

#[test]
fn namespace_epochs_increment() {
    let dir = tempfile::tempdir().unwrap();
    let repo = CanonicalRepository::open(dir.path().to_str().unwrap()).unwrap();
    let fe = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1));
    let ns1 = repo.issue_namespace(fe, ch(2), 1000).unwrap();
    let ns2 = repo.issue_namespace(fe, ch(2), 2000).unwrap();
    assert_eq!(ns1.retry_epoch, 1);
    assert_eq!(ns2.retry_epoch, 2, "each issue increments the epoch");
    assert!(ns1.is_valid_at(1000));
    assert!(ns2.expires_at > ns2.issued_at);
}

#[test]
fn unknown_namespace_is_refused_as_stale() {
    let (repo, _dir) = repo_with_ns();
    // A ctx with a retry_epoch that was never issued must be refused.
    let mut c = ctx(1, "d1");
    c.retry_epoch = 999;
    let err = repo
        .apply(
            &c,
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "x"),
                session: None,
                auto_link: None,
            },
        )
        .unwrap_err();
    assert_eq!(err.code, DomainErrorCode::StaleReplay);
}

#[test]
fn corrupt_generation_meta_fails_closed_not_first() {
    let dir = tempfile::tempdir().unwrap();
    let repo = CanonicalRepository::open(dir.path().to_str().unwrap()).unwrap();
    assert_eq!(repo.store_generation().unwrap(), StoreGeneration::FIRST);
    // Torn metadata (present but short) must error, never degrade to
    // generation 1: generation identity is a fencing token.
    let meta = CanonicalRepository::keyspace(&repo.db, "meta").unwrap();
    let mut tx = repo.db.write_tx().unwrap();
    tx.insert(&meta, "store_generation", [1u8, 2, 3].as_slice());
    tx.commit().unwrap().unwrap();
    let err = repo.store_generation().unwrap_err();
    assert_eq!(err.code, DomainErrorCode::Validation);
    assert!(err.message.contains("corrupt"), "got: {}", err.message);
}

#[test]
fn committed_operation_replays_after_namespace_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
    let repo =
        CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), clock.clone()).unwrap();
    let fe = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1));
    repo.issue_namespace(fe, ch(2), 1000).unwrap();
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "x"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    drop(repo);
    // Past the 24h TTL: the namespace is dead, but the durable receipt
    // must still replay instead of refusing as stale.
    let late = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(
        1000 + crate::repository::DEFAULT_NAMESPACE_TTL_MILLIS + 1,
    ));
    let repo2 = CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), late).unwrap();
    let replayed = repo2
        .apply(
            &ctx(1, "d1"),
            &DomainCommand::AddMemory {
                memory: memory(eid(1), "x"),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    assert_eq!(
        replayed.operation_id,
        ctx(1, "d1").operation_id,
        "expired epoch must replay the recorded receipt"
    );
    // …while genuinely fresh work under the dead epoch still refuses.
    let err = repo2
        .apply(
            &ctx(2, "d2"),
            &DomainCommand::AddMemory {
                memory: memory(eid(2), "y"),
                session: None,
                auto_link: None,
            },
        )
        .unwrap_err();
    assert_eq!(err.code, DomainErrorCode::StaleReplay);
}

/// The epoch counter cannot wrap: u64::MAX advances fail closed
/// instead of panicking (debug) or reusing epoch 0 (release).
#[test]
fn epoch_counter_overflow_fails_closed() {
    use ltmrs_domain::id::FrontendId;
    let (repo, _dir) = repo_with_ns();
    let fe = FrontendId::new(Uuid::from_u128(99));
    let mut tx = repo.db.write_tx().unwrap();
    tx.insert(&repo.namespaces, namespace_key(fe), u64::MAX.to_le_bytes());
    tx.commit().unwrap().unwrap();
    let err = repo.issue_namespace(fe, ch(2), 1000).unwrap_err();
    assert_eq!(err.code, DomainErrorCode::Validation);
}

/// Mutation watermark: every executed command advances op_seq
/// atomically with its receipt (restore preview/confirm binding).
/// Replays record nothing and advance nothing.
#[test]
fn op_seq_advances_per_execution_not_replay() {
    let (repo, _dir) = repo_with_ns();
    assert_eq!(repo.op_seq().unwrap(), 0);
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "m"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    assert_eq!(repo.op_seq().unwrap(), 1);
    repo.apply(
        &ctx(2, "d2"),
        &DomainCommand::AddMemory {
            memory: memory(eid(2), "m"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    assert_eq!(repo.op_seq().unwrap(), 2);
    // Same operation key + digest replays: no new execution, no advance.
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "m"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    assert_eq!(repo.op_seq().unwrap(), 2);
}

/// Direct-write primitives advance the watermark too: guide/distill
/// writes bypass the command bus, but a restore must still count them.
#[test]
fn op_seq_counts_direct_writes() {
    use ltmrs_domain::guide::Guide;
    use ltmrs_domain::memory::Instant;
    let (repo, _dir) = repo_with_ns();
    assert_eq!(repo.op_seq().unwrap(), 0);
    repo.put_guide(&Guide {
        name: "g".into(),
        category: "c".into(),
        description: String::new(),
        contexts: vec![],
        learnings: vec![],
        usage_count: 0,
        last_used: None,
        success_count: 0,
        failure_count: 0,
        anti_patterns: vec![],
        pitfalls: vec![],
        depends_on: vec![],
        enables: vec![],
        source_memories: vec![],
        validated_by: vec![],
        superseded_by: None,
        deprecated: false,
        entity_revision: ltmrs_domain::id::EntityRevision::new(1),
        created_at: Instant::new(0),
        updated_at: Instant::new(0),
    })
    .unwrap();
    assert_eq!(repo.op_seq().unwrap(), 1);
    // The watermark sums across writer keys (per-frontend + direct):
    // a command execution lands on top.
    repo.apply(
        &ctx(1, "d1"),
        &DomainCommand::AddMemory {
            memory: memory(eid(1), "m"),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    assert_eq!(repo.op_seq().unwrap(), 2);
}

/// Lookalike keys do not pollute the watermark: only `op_seq:`-prefixed
/// counters sum (tight prefix, no decoys).
#[test]
fn op_seq_ignores_lookalike_keys() {
    let (repo, _dir) = repo_with_ns();
    let mut tx = repo.db.write_tx().unwrap();
    tx.insert(&repo.namespaces, "op_seq_backup", 100u64.to_le_bytes());
    tx.insert(&repo.namespaces, "op_seqx", 100u64.to_le_bytes());
    tx.commit().unwrap().unwrap();
    assert_eq!(
        repo.op_seq().unwrap(),
        0,
        "decoy keys must not enter the sum"
    );
    // Empty suffix and nested colons: only exact `op_seq:`-prefixed
    // counters sum; anything else is ignored, never failed closed.
    let mut tx = repo.db.write_tx().unwrap();
    tx.insert(&repo.namespaces, "op_seq:", 5u64.to_le_bytes());
    tx.insert(&repo.namespaces, "op_seq:direct:extra", 7u64.to_le_bytes());
    tx.commit().unwrap().unwrap();
    assert_eq!(
        repo.op_seq().unwrap(),
        12,
        "op_seq:-prefixed counters sum regardless of suffix shape"
    );
}

/// Same-frontend parallel writes share one watermark key: disjoint
/// memories must all commit via the standard retry discipline (no
/// spurious exhaustion), each advancing the watermark exactly once.
/// Retry dynamics proven here; the transient typing of any residual
/// exhaustion is pinned by `exhaustion_reports_transient_contention`
/// (conflicts are too rare here to assert their code deterministically).
#[test]
fn concurrent_same_frontend_writes_all_commit() {
    use std::sync::{Arc, Barrier};
    let (repo, _dir) = repo_with_ns();
    let repo = Arc::new(repo);
    let start = Arc::new(Barrier::new(5));
    let mut handles = Vec::new();
    for t in 0..4u64 {
        let repo = Arc::clone(&repo);
        let start = Arc::clone(&start);
        handles.push(std::thread::spawn(move || {
            start.wait();
            let mut done = 0;
            for i in 0..10u64 {
                let n = t * 100 + i + 10;
                // Retry on conflicts (shared watermark key discipline);
                // exhaustion would be the bug.
                for _ in 0..20 {
                    let r = repo.apply(
                        &ctx(t * 1000 + i, &format!("c{t}-{i}")),
                        &DomainCommand::AddMemory {
                            memory: memory(eid(n), "x"),
                            session: None,
                            auto_link: None,
                        },
                    );
                    if r.is_ok() {
                        done += 1;
                        break;
                    }
                }
            }
            done
        }));
    }
    start.wait();
    let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(total, 40, "every parallel write must commit after retries");
    assert_eq!(repo.op_seq().unwrap(), 40);
    // Safety, not just liveness: every disjoint commit persisted.
    let live: std::collections::BTreeSet<u128> = repo
        .export_full()
        .unwrap()
        .memories
        .iter()
        .map(|m| m.id.as_uuid().as_u128())
        .collect();
    for t in 0..4u64 {
        for i in 0..10u64 {
            let n = t * 100 + i + 10;
            assert!(
                live.contains(&(n as u128)),
                "disjoint commit {n} lost despite Ok"
            );
        }
    }
}

/// SSI exhaustion is transient, never fatal: every write path that
/// runs out of conflict budget reports Contention (safe to retry)
/// instead of Validation (refuse). Pinned here; retry dynamics are
/// proven by the concurrent tests.
#[test]
fn exhaustion_reports_transient_contention() {
    let err = CanonicalRepository::exhausted_contention("probe op");
    assert_eq!(err.code, DomainErrorCode::Contention);
    assert!(
        err.message.contains("probe op"),
        "site message preserved, got: {err:?}"
    );
}

/// End-to-end exhaustion typing: hammered parallel applies on one
/// frontend collide on the shared watermark key; whatever conflicts
/// surface must be transient Contention, never fatal Validation.
/// (Conflicts are scheduled by the engine, so the kind assertion is
/// opportunistic — the constructor test pins the mapping
/// deterministically; all 200 commits succeeding proves liveness.)
#[test]
fn exhausted_apply_reports_transient_contention() {
    use std::sync::{Arc, Barrier};
    let (repo, _dir) = repo_with_ns();
    let repo = Arc::new(repo);
    let start = Arc::new(Barrier::new(9));
    let mut handles = Vec::new();
    for t in 0..8u64 {
        let repo = Arc::clone(&repo);
        let start = Arc::clone(&start);
        handles.push(std::thread::spawn(move || {
            start.wait();
            let mut done = 0;
            for i in 0..25u64 {
                let n = t * 100 + i + 10;
                for _ in 0..20 {
                    match repo.apply(
                        &ctx(t * 1000 + i, &format!("e{t}-{i}")),
                        &DomainCommand::AddMemory {
                            memory: memory(eid(n), "x"),
                            session: None,
                            auto_link: None,
                        },
                    ) {
                        Ok(_) => {
                            done += 1;
                            break;
                        }
                        Err(e) => assert_eq!(
                            e.code,
                            DomainErrorCode::Contention,
                            "contention must be typed transient, got {e:?}"
                        ),
                    }
                }
            }
            done
        }));
    }
    start.wait();
    let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    assert_eq!(total, 200, "every hammered write must commit after retries");
    assert_eq!(repo.op_seq().unwrap(), 200);
}
/// P1-B: resuming a live retry namespace returns the SAME epoch without
/// minting a new one, so a reconnected frontend keeps resolving its
/// pre-failure receipts. Unknown or expired namespaces refuse as stale
/// (the caller surfaces an unknown outcome, never a silent fresh epoch).
#[test]
fn resume_namespace_returns_same_epoch() {
    let (repo, _dir) = repo_with_ns();
    let fe = ltmrs_domain::id::FrontendId::new(uuid::Uuid::from_u128(1));
    // repo_with_ns issued epoch 1 at t=1000.
    let ns = repo.resume_namespace(fe, ch(2), 1, 1000).unwrap();
    assert_eq!(ns.retry_epoch, 1);
    // Resuming does not consume an epoch: the next issue still yields 2.
    let next = repo.issue_namespace(fe, ch(2), 1000).unwrap();
    assert_eq!(next.retry_epoch, 2);
    // Unknown epoch refuses.
    let err = repo.resume_namespace(fe, ch(2), 99, 1000).unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::StaleReplay
    );
    // Another frontend's epoch is not resumable here.
    let other = ltmrs_domain::id::FrontendId::new(uuid::Uuid::from_u128(2));
    let err = repo.resume_namespace(other, ch(2), 1, 1000).unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::StaleReplay
    );
}

/// P1-B: an expired namespace (past the 24h TTL) refuses resume.
#[test]
fn resume_namespace_rejects_expired_epoch() {
    let (repo, _dir) = repo_with_ns();
    let fe = ltmrs_domain::id::FrontendId::new(uuid::Uuid::from_u128(1));
    let expired_at = 1000 + crate::repository::DEFAULT_NAMESPACE_TTL_MILLIS + 1;
    let err = repo.resume_namespace(fe, ch(2), 1, expired_at).unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::StaleReplay
    );
    assert!(err.message.contains("expired"), "got: {err:?}");
}

/// NEW P1/P2 (channel isolation): a retry namespace belongs to the
/// (frontend, channel) pair that issued it. A sibling channel resuming
/// the same epoch is refused as stale — never silently adopted.
#[test]
fn resume_namespace_refuses_cross_channel() {
    let (repo, _dir) = repo_with_ns();
    let fe = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1));
    // Same channel resumes fine.
    let ns = repo.resume_namespace(fe, ch(2), 1, 1000).unwrap();
    assert_eq!(ns.retry_epoch, 1);
    assert_eq!(ns.channel_id, ch(2));
    // Sibling channel is refused.
    let err = repo.resume_namespace(fe, ch(3), 1, 1000).unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::StaleReplay
    );
    assert!(err.message.contains("another channel"), "got: {err:?}");
}

/// Receipt replay is channel-scoped: the same operation retried from a
/// sibling channel must neither replay nor re-execute — refused as
/// stale at the namespace gate.
#[test]
fn apply_refuses_cross_channel_replay() {
    let (repo, _dir) = repo_with_ns();
    let m = memory(eid(1), "ch-scoped");
    let mut ca = ctx(1, "cross-channel");
    ca.channel_id = ch(2);
    repo.apply(
        &ca,
        &DomainCommand::AddMemory {
            memory: m.clone(),
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    // Same op id + digest from a sibling channel: refused, never replayed.
    let mut cb = ctx(1, "cross-channel");
    cb.channel_id = ch(3);
    let err = repo
        .apply(
            &cb,
            &DomainCommand::AddMemory {
                memory: m,
                session: None,
                auto_link: None,
            },
        )
        .unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::StaleReplay
    );
    assert!(err.message.contains("another channel"), "got: {err:?}");
    // Exactly one effect: no re-execution slipped through.
    assert_eq!(repo.get_memories(&[eid(1)]).unwrap().len(), 1);
}

/// P2 (T-CONC-02): interactive watermarks shard per channel. After
/// writes from two channels of one frontend, each channel owns its
/// `op_seq:{frontend}:{channel}` counter and no shared key advances —
/// otherwise-disjoint agents never contend on one global sequence.
#[test]
fn op_seq_watermarks_shard_per_channel() {
    use ltmrs_domain::id::SessionHandle;
    use ltmrs_domain::session::SessionOp;
    let (repo, _dir) = repo_with_ns();
    let fe = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1));
    repo.issue_namespace(fe, ch(3), 1000).unwrap();
    let ha = SessionHandle::new(Uuid::from_u128(910));
    match repo
        .session_start_tx(&scope(910, "d-a"), ha, None, None, vec![], None, None, 1000)
        .unwrap()
    {
        SessionOp::Applied(h) => assert_eq!(h, ha),
        other => panic!("expected Applied, got {other:?}"),
    }
    let hb = SessionHandle::new(Uuid::from_u128(911));
    match repo
        .session_start_tx(
            &scope_in(2, 3, 911, "d-b"),
            hb,
            None,
            None,
            vec![],
            None,
            None,
            1000,
        )
        .unwrap()
    {
        SessionOp::Applied(h) => assert_eq!(h, hb),
        other => panic!("expected Applied, got {other:?}"),
    }
    let snapshot = repo.db.read_tx();
    let mut keys = Vec::new();
    for kv in snapshot.iter(&repo.namespaces) {
        let (k, _) = kv.into_inner().unwrap();
        let key_str = String::from_utf8_lossy(k.as_ref()).to_string();
        if key_str.starts_with("op_seq:") {
            keys.push(key_str);
        }
    }
    drop(snapshot);
    let fe_hex = fe.as_uuid().to_string();
    assert!(
        keys.iter()
            .any(|k| k == &format!("op_seq:{fe_hex}:{}", ch(2).as_uuid())),
        "channel-2 watermark missing, got {keys:?}"
    );
    assert!(
        keys.iter()
            .any(|k| k == &format!("op_seq:{fe_hex}:{}", ch(3).as_uuid())),
        "channel-3 watermark missing, got {keys:?}"
    );
    assert!(
        !keys.iter().any(|k| k == "op_seq:direct"),
        "global watermark must stay untouched, got {keys:?}"
    );
}

#[test]
fn direct_mutations_refuse_expired_namespace() {
    use ltmrs_domain::id::SessionHandle;
    use std::sync::Arc;
    // Namespace issued at t=1000; the repo clock already reads past
    // the TTL, so every direct mutation below meets an expired
    // namespace (validate_scope reads the clock, not the op stamp).
    let dir = tempfile::tempdir().unwrap();
    let expired_at = 1000 + crate::repository::DEFAULT_NAMESPACE_TTL_MILLIS + 1;
    let clock: Arc<dyn ltmrs_domain::clock::Clock + Send + Sync> =
        Arc::new(ltmrs_domain::clock::FrozenClock::new(expired_at));
    let repo = CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), clock).unwrap();
    let fe = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1));
    repo.issue_namespace(fe, ch(2), 1000).unwrap();
    let handle = SessionHandle::new(Uuid::from_u128(800));
    let err = repo
        .session_start_tx(
            &scope(800, "d-exp"),
            handle,
            None,
            None,
            vec![],
            None,
            None,
            expired_at,
        )
        .unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::StaleReplay
    );
    assert!(err.message.contains("expired"), "got: {err:?}");
    assert!(repo.get_session(handle).unwrap().is_none());
    // Admission-once: the expired envelope is rejected at the gate,
    // before any store call runs.
    let err = repo.admit_scope(&scope(801, "d-exp")).unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::StaleReplay
    );
}

/// P1 T2 (channel isolation for direct receipts): channel A starts a
/// session with op X; channel B submitting the same op id + digest
/// under its own namespace must execute in its own scope — never
/// replay A's receipt, never bind B to A's session.
#[test]
fn direct_session_start_same_op_id_is_scoped_per_channel() {
    use ltmrs_domain::id::SessionHandle;
    use ltmrs_domain::session::SessionOp;
    let (repo, _dir) = repo_with_ns();
    let fe = ltmrs_domain::id::FrontendId::new(Uuid::from_u128(1));
    // Channel A (epoch 1) starts its session with op X.
    let ha = SessionHandle::new(Uuid::from_u128(901));
    match repo
        .session_start_tx(&scope(900, "d-x"), ha, None, None, vec![], None, None, 1000)
        .unwrap()
    {
        SessionOp::Applied(h) => assert_eq!(h, ha),
        other => panic!("expected Applied, got {other:?}"),
    }
    // Channel B (own namespace, epoch 2), same op id + digest.
    repo.issue_namespace(fe, ch(3), 1000).unwrap();
    let scope_b = scope_in(2, 3, 900, "d-x");
    let hb = SessionHandle::new(Uuid::from_u128(902));
    match repo
        .session_start_tx(&scope_b, hb, None, None, vec![], None, None, 1000)
        .unwrap()
    {
        SessionOp::Applied(h) => assert_eq!(h, hb),
        other => panic!("B must apply in its own scope, got {other:?}"),
    }
    // A's session is untouched; each receipt lives in its own scope.
    assert!(repo.get_session(ha).unwrap().is_some());
    assert!(repo.get_session(hb).unwrap().is_some());
    assert!(
        repo.session_receipt(&repo.admit_scope(&scope(900, "d-x")).unwrap())
            .unwrap()
            .is_some()
    );
    assert!(
        repo.session_receipt(&repo.admit_scope(&scope_b).unwrap())
            .unwrap()
            .is_some()
    );
}

/// Concurrent issuance for one frontend must yield distinct epochs
/// (shared retry namespace would confuse replays across channels).
#[test]
fn concurrent_namespace_issue_yields_distinct_epochs() {
    use std::sync::{Arc, Barrier};
    let (repo, _dir) = repo_with_ns();
    let repo = std::sync::Arc::new(repo);
    let fe = ltmrs_domain::id::FrontendId::new(uuid::Uuid::from_u128(99));
    let start = Arc::new(Barrier::new(9));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let repo = Arc::clone(&repo);
        let start = Arc::clone(&start);
        handles.push(std::thread::spawn(move || {
            start.wait();
            let mut epochs = Vec::new();
            for _ in 0..50 {
                // Retry on conflicts (concurrent issuance discipline);
                // duplicates are the bug, conflicts are not.
                for _ in 0..20 {
                    match repo.issue_namespace(fe, ch(2), 1000) {
                        Ok(ns) => {
                            epochs.push(ns.retry_epoch);
                            break;
                        }
                        Err(_) => continue,
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
    assert_eq!(all.len(), 400, "every issue must succeed after retries");
    all.sort_unstable();
    let distinct: Vec<u64> = {
        let mut d = all.clone();
        d.dedup();
        d
    };
    assert_eq!(
        distinct.len(),
        400,
        "epochs must be distinct (double-issue reuses an epoch)"
    );
}
