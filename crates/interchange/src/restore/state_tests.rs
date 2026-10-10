//! Restore state/pipeline/session tests (moved verbatim from `restore.rs`).

use std::collections::BTreeMap;

use std::sync::Arc;

use super::test_support::*;
use super::verify::restore_verified;
use ltmrs_domain::export::CanonicalExport;
use ltmrs_domain::id::EntityId;
use ltmrs_domain::memory::Memory;

/// Restoring retires the pre-restore pipeline: a staged worker from
/// before the restore can no longer publish (generation invalidation).
#[test]
fn restore_invalidates_old_pipelines() {
    use ltmrs_domain::id::ModelFingerprint;
    let dir = tempfile::tempdir().unwrap();
    let repo = open_repo(&dir);
    let staged = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
    assert!(
        repo.generation_under_construction(staged, ModelFingerprint::new(7))
            .unwrap()
    );
    let snapshot = CanonicalExport::default();
    let backup = crate::backup::VerifiedBackup {
        digest: snapshot.digest(),
        store_generation: 1,
        counts: BTreeMap::new(),
        snapshot,
        unknown_top_level: 0,
    };
    let live_gen = repo.store_generation().unwrap().as_u64();
    restore_verified(
        &repo,
        &backup,
        ltmrs_domain::id::StoreGeneration::new(live_gen + 1),
    )
    .unwrap();
    assert!(
        !repo
            .generation_under_construction(staged, ModelFingerprint::new(7))
            .unwrap(),
        "pre-restore pipeline must lose publish rights"
    );
}

/// Replace covers every durable keyspace, not just the five exported
/// collections: stale aliases must not block reuse, stale receipts must
/// not replay, namespaces must not survive, and projection jobs must be
/// re-enqueued for the restored memories (or search never converges).
#[test]
fn restore_replace_covers_all_keyspaces() {
    use ltmrs_domain::command::DomainCommand;
    use ltmrs_domain::id::ExternalAlias;
    let dir = tempfile::tempdir().unwrap();
    let repo = gateway_repo(&dir);
    // Live state through the gateway: aliased memory + receipt + job.
    let mut stale = test_memory(1, "Stale One");
    stale.external_alias = Some(ExternalAlias::new("old-alias"));
    repo.apply(
        &gateway_ctx(5),
        &DomainCommand::AddMemory {
            memory: stale,
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    assert_eq!(
        repo.resolve_id("old-alias").unwrap().as_uuid(),
        uuid::Uuid::from_u128(1)
    );
    assert!(
        repo.projection_job(EntityId::new(uuid::Uuid::from_u128(1)))
            .unwrap()
            .is_some()
    );

    // Backup carries a different memory with its own alias.
    let mut fresh = test_memory(2, "Fresh Two");
    fresh.external_alias = Some(ExternalAlias::new("new-alias"));
    let snapshot = CanonicalExport {
        memories: vec![fresh],
        ..Default::default()
    };
    let backup = crate::backup::VerifiedBackup {
        digest: snapshot.digest(),
        store_generation: 1,
        counts: BTreeMap::new(),
        snapshot,
        unknown_top_level: 0,
    };
    restore_verified(&repo, &backup, ltmrs_domain::id::StoreGeneration::new(2)).unwrap();

    // Data replaced.
    let titles: Vec<String> = repo
        .export_full()
        .unwrap()
        .memories
        .iter()
        .map(|m| m.title.clone())
        .collect();
    assert_eq!(titles, vec!["Fresh Two".to_string()]);
    // Aliases rebuilt: old gone, new resolvable.
    assert!(
        repo.resolve_id("old-alias").is_err(),
        "stale alias must not survive"
    );
    assert_eq!(
        repo.resolve_id("new-alias").unwrap().as_uuid(),
        uuid::Uuid::from_u128(2)
    );
    // Projection jobs re-enqueued for restored memories; the drained
    // memory has no job until it is written again.
    let job = repo
        .projection_job(EntityId::new(uuid::Uuid::from_u128(2)))
        .unwrap()
        .expect("restored memory needs a projection job");
    assert_eq!(job.desired_document_revision.as_u64(), 1);
    assert!(
        repo.projection_job(EntityId::new(uuid::Uuid::from_u128(1)))
            .unwrap()
            .is_none(),
        "stale jobs must not resurrect drained memories"
    );
    // Namespaces drained: the pre-restore epoch is unknown now.
    let err = repo
        .apply(
            &gateway_ctx(6),
            &DomainCommand::AddMemory {
                memory: test_memory(3, "Nope"),
                session: None,
                auto_link: None,
            },
        )
        .unwrap_err();
    assert!(
        err.message.contains("namespace"),
        "stale namespace must be refused, got: {err:?}"
    );
    // Receipts drained: replaying op 5 with new content executes fresh
    // instead of returning the stale receipt. A fresh namespace is
    // issued first (the pre-restore epoch was drained with the rest),
    // and the write names the live generation like a re-handshaked
    // client (the fence rejects the retired one).
    repo.issue_namespace(
        ltmrs_domain::id::FrontendId::new(uuid::Uuid::from_u128(1)),
        ltmrs_domain::id::ChannelId::new(uuid::Uuid::from_u128(2)),
        1000,
    )
    .unwrap();
    let mut changed = test_memory(1, "Changed Content");
    changed.external_alias = Some(ExternalAlias::new("old-alias"));
    repo.apply(
        &gateway_ctx_gen(5, ltmrs_domain::id::StoreGeneration::new(2)),
        &DomainCommand::AddMemory {
            memory: changed,
            session: None,
            auto_link: None,
        },
    )
    .unwrap();
    let titles: Vec<String> = repo
        .export_full()
        .unwrap()
        .memories
        .iter()
        .map(|m| m.title.clone())
        .collect();
    assert!(
        titles.contains(&"Changed Content".to_string()),
        "drained receipts must not replay stale results, got: {titles:?}"
    );
    // Single active generation at the new value.
    assert_eq!(repo.store_generation().unwrap().as_u64(), 2);
    let actives: Vec<_> = repo
        .list_generations()
        .unwrap()
        .into_iter()
        .filter(|r| matches!(r.status, ltmrs_domain::projection::GenerationStatus::Active))
        .collect();
    assert_eq!(actives.len(), 1, "exactly one Active generation");
    assert_eq!(actives[0].generation.as_u64(), 2);
}

/// P2-A: restoring an Active session must not resurrect an unowned live
/// execution context. Non-terminal sessions restore as Abandoned
/// (history preserved for analytics/continuity); terminal ones pass
/// through verbatim. The report counts the marked sessions.
#[test]
fn restore_marks_restored_active_sessions_abandoned() {
    use ltmrs_domain::id::SessionHandle;
    use ltmrs_domain::session::{SessionOp, SessionStatus, TaskOutcome};
    let dir = tempfile::tempdir().unwrap();
    let repo = gateway_repo(&dir);
    issue_session_channel(&repo);
    for (op, n) in [("op-a", 100u128), ("op-b", 200u128)] {
        let handle = SessionHandle::new(uuid::Uuid::from_u128(n));
        match repo
            .session_start_tx(
                &gateway_scope(op, "digest"),
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
    }
    // End B: terminal sessions must pass through untouched.
    let handle_b = SessionHandle::new(uuid::Uuid::from_u128(200));
    match repo
        .session_end_tx(
            &gateway_scope("op-end-b", "digest-end"),
            handle_b,
            TaskOutcome::Success,
            None,
            vec![],
            1000,
        )
        .unwrap()
    {
        SessionOp::Applied(_) => {}
        other => panic!("expected Applied end, got {other:?}"),
    }
    let snapshot = CanonicalExport {
        sessions: repo.all_sessions().unwrap(),
        ..Default::default()
    };
    assert_eq!(snapshot.sessions.len(), 2);
    let backup = crate::backup::VerifiedBackup {
        digest: snapshot.digest(),
        store_generation: 1,
        counts: BTreeMap::new(),
        snapshot,
        unknown_top_level: 0,
    };
    let report =
        restore_verified(&repo, &backup, ltmrs_domain::id::StoreGeneration::new(2)).unwrap();
    let handle_a = SessionHandle::new(uuid::Uuid::from_u128(100));
    let sessions = repo.all_sessions().unwrap();
    let restored_a = sessions
        .iter()
        .find(|s| s.handle == handle_a)
        .expect("A restored");
    assert_eq!(
        restored_a.status,
        SessionStatus::Abandoned,
        "restored Active session must be Abandoned, got {:?}",
        restored_a.status
    );
    assert!(
        restored_a.ended_at.is_some(),
        "abandoned restore needs an end timestamp"
    );
    let restored_b = sessions
        .iter()
        .find(|s| s.handle == handle_b)
        .expect("B restored");
    assert_eq!(
        restored_b.status,
        SessionStatus::Ended,
        "terminal sessions pass through, got {:?}",
        restored_b.status
    );
    assert_eq!(report.sessions_marked_abandoned, 1);
}

/// P1-1 (review of bfe8844): restore must follow the session migration
/// into Fjall. `restore_replace` drains memories/guides/receipts but
/// leaves `sessions`, `session_ops` and `guide_ops` behind, so old
/// sessions survive (merely abandoned) while the backup's sessions are
/// ignored, and pre-restore operation receipts can replay across the
/// generation cut. True backup restore: sessions come from the backup,
/// op receipts never cross a generation.
#[test]
fn restore_restores_backup_sessions_and_fences_op_receipts() {
    use ltmrs_domain::id::SessionHandle;
    use ltmrs_domain::session::SessionOp;
    let dir = tempfile::tempdir().unwrap();
    let repo = gateway_repo(&dir);
    issue_session_channel(&repo);
    // Live session A via session op X.
    let handle_a = SessionHandle::new(uuid::Uuid::from_u128(100));
    match repo
        .session_start_tx(
            &gateway_scope("op-X", "digest-X"),
            handle_a,
            None,
            None,
            vec![],
            None,
            None,
            1000,
        )
        .unwrap()
    {
        SessionOp::Applied(h) => assert_eq!(h, handle_a),
        other => panic!("expected Applied, got {other:?}"),
    }
    // Guide op Y: creates + practices guide "git" (usage 1).
    let guide = repo
        .practice_guide_idempotent(
            &repo
                .admit_scope(&gateway_scope("op-Y", "digest-Y"))
                .unwrap(),
            "git",
            "dev-tool",
            None,
            &[],
            &["learn it".to_string()],
            &[],
            Some(true),
            1000,
        )
        .unwrap();
    assert_eq!(guide.usage_count, 1);
    // Backup B carries a different session.
    let handle_b = SessionHandle::new(uuid::Uuid::from_u128(200));
    match repo
        .session_start_tx(
            &gateway_scope("op-B", "digest-B"),
            handle_b,
            None,
            None,
            vec![],
            None,
            None,
            1000,
        )
        .unwrap()
    {
        SessionOp::Applied(h) => assert_eq!(h, handle_b),
        other => panic!("expected Applied, got {other:?}"),
    }
    let session_b = repo
        .all_sessions()
        .unwrap()
        .into_iter()
        .find(|s| s.handle == handle_b)
        .expect("session B must be live before restore");
    let snapshot = CanonicalExport {
        sessions: vec![session_b],
        ..Default::default()
    };
    let backup = crate::backup::VerifiedBackup {
        digest: snapshot.digest(),
        store_generation: 1,
        counts: BTreeMap::new(),
        snapshot,
        unknown_top_level: 0,
    };
    restore_verified(&repo, &backup, ltmrs_domain::id::StoreGeneration::new(2)).unwrap();

    // Sessions come from the backup: A gone, B present.
    let handles: Vec<SessionHandle> = repo
        .all_sessions()
        .unwrap()
        .into_iter()
        .map(|s| s.handle)
        .collect();
    assert!(
        !handles.contains(&handle_a),
        "pre-restore session A must not survive, got: {handles:?}"
    );
    assert!(
        handles.contains(&handle_b),
        "backup session B must be restored, got: {handles:?}"
    );
    // Session op X fenced: retrying it must execute fresh, never replay
    // the pre-restore outcome for A. Fresh namespace post-restore
    // (the drain took the old one): epoch restarts at 1.
    issue_session_channel(&repo);
    let handle_c = SessionHandle::new(uuid::Uuid::from_u128(300));
    match repo
        .session_start_tx(
            &gateway_scope_post("op-X", "digest-X"),
            handle_c,
            None,
            None,
            vec![],
            None,
            None,
            1000,
        )
        .unwrap()
    {
        SessionOp::Applied(h) => assert_eq!(h, handle_c),
        SessionOp::Replayed(h) => panic!("stale session op X replayed {h:?} across restore"),
        SessionOp::Conflict => panic!("same digest must not conflict"),
    }
    // Guide op Y fenced: the guides keyspace was legitimately drained,
    // so retrying op Y must re-apply (guide persisted again), never
    // return a detached pre-restore recording.
    let guide = repo
        .practice_guide_idempotent(
            &repo
                .admit_scope(&gateway_scope_post("op-Y", "digest-Y"))
                .unwrap(),
            "git",
            "dev-tool",
            None,
            &[],
            &["learn it".to_string()],
            &[],
            Some(true),
            1000,
        )
        .unwrap();
    assert_eq!(guide.usage_count, 1);
    assert!(
        repo.get_guide("git").unwrap().is_some(),
        "op-Y retry must re-apply into the restored store, not replay a detached recording"
    );
}

/// A writer racing the replace can never tear it: with the barrier, a
/// racing write commits strictly before the drain (then drained) or
/// strictly after the commit (then present). A survivor whose commit
/// long predates the restore return tore the merge. The margin below is
/// load-bearing: a legitimate post-restore write can beat the t1 read
/// by microseconds (hot writer vs returning caller), so only survivors
/// older than the margin prove tearing; the fat backup keeps the replace
/// itself at millisecond scale, far above it.
#[test]
fn restore_racing_writer_never_tears() {
    let dir = tempfile::tempdir().unwrap();
    let repo = open_repo(&dir);
    // Fat backup: 500 memories widen the replace window.
    let memories: Vec<Memory> = (0..500u128)
        .map(|i| test_memory(1000 + i, &format!("Bulk {i}")))
        .collect();
    let snapshot = CanonicalExport {
        memories,
        ..Default::default()
    };
    let backup = crate::backup::VerifiedBackup {
        digest: snapshot.digest(),
        store_generation: 1,
        counts: BTreeMap::new(),
        snapshot,
        unknown_top_level: 0,
    };
    let repo2 = Arc::clone(&repo);
    let writer = std::thread::spawn(move || {
        let mut committed = Vec::new();
        for i in 0..300u128 {
            let m = test_memory(500_000 + i, &format!("Racer {i}"));
            if repo2.put_memory_direct(&m).is_ok() {
                committed.push((500_000 + i, std::time::Instant::now()));
            }
        }
        committed
    });
    restore_verified(&repo, &backup, ltmrs_domain::id::StoreGeneration::new(2)).unwrap();
    let t1 = std::time::Instant::now();
    let committed = writer.join().unwrap();
    assert_eq!(repo.store_generation().unwrap().as_u64(), 2);
    let live: std::collections::BTreeSet<u128> = repo
        .export_full()
        .unwrap()
        .memories
        .iter()
        .map(|m| m.id.as_uuid().as_u128())
        .collect();
    // All backup keys present (replace, not merge-with-loss).
    for i in 0..500u128 {
        assert!(live.contains(&(1000 + i)), "backup key {i} lost");
    }
    // Survivors older than the margin tore the merge; younger ones
    // (and post-t1 commits) are legitimate post-restore writes.
    let margin = std::time::Duration::from_millis(5);
    for (key, at) in &committed {
        if live.contains(key) {
            match t1.checked_duration_since(*at) {
                None => {}
                Some(age) => assert!(
                    age < margin,
                    "writer key {key} committed {age:?} before restore return yet survived: torn merge"
                ),
            }
        }
    }
}
