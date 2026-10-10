//! Restore behavior tests (moved verbatim from `restore.rs`).

use std::collections::BTreeMap;
use std::path::Path;

use super::test_support::*;
use super::verify::{restore_verified, safety_backup_path};
use super::{RESTORE_PREVIEW_TTL_MILLIS, RestoreCoordinator, RestoreError};
use ltmrs_domain::export::CanonicalExport;
use ltmrs_domain::id::{EntityId, StoreGeneration};
use ltmrs_domain::memory::Instant;

/// Export repo B (2 memories), verify, restore into empty repo A:
/// content moves, generation advances past A's.
#[test]
fn restore_replaces_atomically_with_generation_bump() {
    use crate::backup::{MAX_BACKUP_BYTES, export_backup, verify_backup_file};
    let dir_b = tempfile::tempdir().unwrap();
    let repo_b = open_repo(&dir_b);
    repo_b
        .put_memory_direct(&test_memory(1, "Beta One"))
        .unwrap();
    repo_b
        .put_memory_direct(&test_memory(2, "Beta Two"))
        .unwrap();
    let _gen_b = repo_b.store_generation().unwrap().as_u64();
    let report = export_backup(&repo_b, dir_b.path(), "test", 1700000000000).unwrap();
    let verified = verify_backup_file(&report.path, MAX_BACKUP_BYTES).unwrap();

    let dir_a = tempfile::tempdir().unwrap();
    let repo_a = open_repo(&dir_a);
    repo_a
        .put_memory_direct(&test_memory(9, "Alpha Stale"))
        .unwrap();
    let gen_a = repo_a.store_generation().unwrap().as_u64();

    let out = restore_verified(&repo_a, &verified, StoreGeneration::new(gen_a + 1)).unwrap();
    assert_eq!(out.restored["memories"], 2);
    assert_eq!(out.generation, gen_a + 1);
    // Stale content is gone (replace, not merge); source generation noted.
    let titles: Vec<String> = repo_a
        .export_full()
        .unwrap()
        .memories
        .iter()
        .map(|m| m.title.clone())
        .collect();
    assert!(
        !titles.iter().any(|t| t.contains("Stale")),
        "got: {titles:?}"
    );
    assert!(titles.iter().any(|t| t.contains("Beta One")));
    assert_eq!(repo_a.store_generation().unwrap().as_u64(), gen_a + 1);
}

/// Dangling relations are quarantined with reasons; valid records apply.
#[test]
fn restore_quarantines_dangling_relations() {
    use ltmrs_domain::relation::{Relation, RelationType};
    let dir = tempfile::tempdir().unwrap();
    let repo = open_repo(&dir);
    let m1 = test_memory(1, "Only Child");
    repo.put_memory_direct(&m1).unwrap();
    let rel = Relation::new(
        EntityId::new(uuid::Uuid::from_u128(77)),
        m1.id,
        EntityId::new(uuid::Uuid::from_u128(4242)),
        RelationType::Supports,
        None,
        Instant::new(1000),
    );
    let snapshot = CanonicalExport {
        memories: vec![m1],
        relations: vec![rel],
        ..Default::default()
    };
    let backup = crate::backup::VerifiedBackup {
        digest: snapshot.digest(),
        store_generation: 1,
        counts: BTreeMap::new(),
        snapshot,
        unknown_top_level: 0,
    };
    let out = restore_verified(&repo, &backup, StoreGeneration::new(7)).unwrap();
    assert_eq!(out.restored["memories"], 1);
    assert_eq!(out.restored.get("relations"), Some(&0));
    assert_eq!(out.quarantined.len(), 1);
    assert!(out.quarantined[0].missing.contains("4242") || !out.quarantined[0].missing.is_empty());
}

/// A relation dangling on BOTH ends must name both missing endpoints —
/// reporting only the source side silently drops half the diagnosis.
#[test]
fn restore_quarantine_names_both_missing_endpoints() {
    use ltmrs_domain::relation::{Relation, RelationType};
    let dir = tempfile::tempdir().unwrap();
    let repo = open_repo(&dir);
    // Empty store: neither endpoint exists.
    let source = EntityId::new(uuid::Uuid::from_u128(9001));
    let target = EntityId::new(uuid::Uuid::from_u128(9002));
    let rel = Relation::new(
        EntityId::new(uuid::Uuid::from_u128(77)),
        source,
        target,
        RelationType::Supports,
        None,
        Instant::new(1000),
    );
    let snapshot = CanonicalExport {
        memories: vec![],
        relations: vec![rel],
        ..Default::default()
    };
    let backup = crate::backup::VerifiedBackup {
        digest: snapshot.digest(),
        store_generation: 1,
        counts: BTreeMap::new(),
        snapshot,
        unknown_top_level: 0,
    };
    let out = restore_verified(&repo, &backup, StoreGeneration::new(2)).unwrap();
    assert_eq!(out.quarantined.len(), 1);
    let missing = &out.quarantined[0].missing;
    assert!(
        missing.contains(&source.as_uuid().to_string()),
        "must name the missing source, got: {missing}"
    );
    assert!(
        missing.contains(&target.as_uuid().to_string()),
        "must name the missing target, got: {missing}"
    );
}

/// Preview readiness, single-use token, expiry and reuse rules.
#[test]
fn preview_token_lifecycle() {
    use crate::backup::VerifiedBackup;
    let backup = VerifiedBackup {
        digest: "abc".to_string(),
        store_generation: 1,
        counts: BTreeMap::from([("memories".to_string(), 2)]),
        snapshot: CanonicalExport::default(),
        unknown_top_level: 0,
    };
    let live = BTreeMap::from([("memories".to_string(), 1)]);
    let mut coord = RestoreCoordinator::default();
    // Blocked with cooperating connections (no token issued).
    let blocked = coord.preview(crate::restore::PreviewRequest {
        backup: &backup,
        source_path: Path::new("/tmp/x.ltmrs-backup"),
        channel: "ch-1",
        live_counts: &live,
        live_generation: 1,
        active_channels: 3,
        now_millis: 1000,
        live_op_seq: 0,
    });
    assert!(!blocked.ready);
    assert!(blocked.confirmation_token.is_none());
    assert!(blocked.message.contains("2 other connections"));
    // Ready with one channel: token bound with TTL.
    let ready = coord.preview(crate::restore::PreviewRequest {
        backup: &backup,
        source_path: Path::new("/tmp/x.ltmrs-backup"),
        channel: "ch-1",
        live_counts: &live,
        live_generation: 1,
        active_channels: 1,
        now_millis: 1000,
        live_op_seq: 0,
    });
    assert!(ready.ready);
    let token = ready.confirmation_token.clone().unwrap();
    assert_eq!(ready.expires_at, Some(1000 + RESTORE_PREVIEW_TTL_MILLIS));
    // Confirm=false asks explicitly; unknown token rejected.
    assert_eq!(
        coord.confirm(crate::restore::ConfirmRequest {
            token: "nope",
            confirm: true,
            digest: "abc",
            live_generation: 1,
            channel: "ch-1",
            active_channels: 1,
            now_millis: 1000,
            live_op_seq: 0,
        }),
        Err(RestoreError::InvalidToken)
    );
    assert_eq!(
        coord.confirm(crate::restore::ConfirmRequest {
            token: &token,
            confirm: false,
            digest: "abc",
            live_generation: 1,
            channel: "ch-1",
            active_channels: 1,
            now_millis: 1000,
            live_op_seq: 0,
        }),
        Err(RestoreError::NeedsConfirm)
    );
    // First confirm consumes; second is reuse.
    assert!(
        coord
            .confirm(crate::restore::ConfirmRequest {
                token: &token,
                confirm: true,
                digest: "abc",
                live_generation: 1,
                channel: "ch-1",
                active_channels: 1,
                now_millis: 1000,
                live_op_seq: 0,
            })
            .is_ok()
    );
    assert_eq!(
        coord.confirm(crate::restore::ConfirmRequest {
            token: &token,
            confirm: true,
            digest: "abc",
            live_generation: 1,
            channel: "ch-1",
            active_channels: 1,
            now_millis: 1000,
            live_op_seq: 0,
        }),
        Err(RestoreError::AlreadyUsed)
    );
}

/// Confirm binds the live data version: writes landing between
/// preview and confirm are counted (saturating) so the replace
/// acknowledges them instead of draining them unseen. Refusal would
/// brick restores for active hosts, so the delta is reported.
#[test]
fn confirm_reports_writes_since_preview() {
    use crate::backup::VerifiedBackup;
    let backup = VerifiedBackup {
        digest: "abc".to_string(),
        store_generation: 1,
        counts: BTreeMap::new(),
        snapshot: CanonicalExport::default(),
        unknown_top_level: 0,
    };
    let live = BTreeMap::from([("memories".to_string(), 1)]);
    let mut coord = RestoreCoordinator::default();
    let cycle = |coord: &mut RestoreCoordinator, preview_seq: u64, confirm_seq: u64| {
        let ready = coord.preview(crate::restore::PreviewRequest {
            backup: &backup,
            source_path: Path::new("/tmp/x.ltmrs-backup"),
            channel: "ch-1",
            live_counts: &live,
            live_generation: 1,
            active_channels: 1,
            now_millis: 1000,
            live_op_seq: preview_seq,
        });
        let token = ready.confirmation_token.clone().unwrap();
        coord
            .confirm(crate::restore::ConfirmRequest {
                token: &token,
                confirm: true,
                digest: "abc",
                live_generation: 1,
                channel: "ch-1",
                active_channels: 1,
                now_millis: 1000,
                live_op_seq: confirm_seq,
            })
            .unwrap()
    };
    // Quiet store: zero delta.
    let (digest, delta) = cycle(&mut coord, 5, 5);
    assert_eq!(digest, "abc");
    assert_eq!(delta, 0);
    // Two writes landed mid-window: counted, saturating.
    let (_, delta) = cycle(&mut coord, 5, 7);
    assert_eq!(delta, 2);
}

/// Loss accounting reaches the caller before the destructive step: the
/// preview carries the backup's unknown-key count and warns about it,
/// so confirm never drops future keys sight unseen.
#[test]
fn preview_reports_unknown_keys() {
    use crate::backup::VerifiedBackup;
    let backup = VerifiedBackup {
        digest: "abc".to_string(),
        store_generation: 1,
        counts: BTreeMap::new(),
        snapshot: CanonicalExport::default(),
        unknown_top_level: 2,
    };
    let live = BTreeMap::new();
    let mut coord = RestoreCoordinator::default();
    let ready = coord.preview(crate::restore::PreviewRequest {
        backup: &backup,
        source_path: Path::new("/tmp/x.ltmrs-backup"),
        channel: "ch-1",
        live_counts: &live,
        live_generation: 1,
        active_channels: 1,
        now_millis: 1000,
        live_op_seq: 0,
    });
    assert!(ready.ready);
    assert_eq!(ready.unknown_top_level, 2);
    assert!(
        ready.message.contains("2 unknown"),
        "must warn, got: {}",
        ready.message
    );
    // Counted even when blocked (a property of the backup, not of
    // readiness); the warning itself rides the ready message.
    let blocked = coord.preview(crate::restore::PreviewRequest {
        backup: &backup,
        source_path: Path::new("/tmp/x.ltmrs-backup"),
        channel: "ch-1",
        live_counts: &live,
        live_generation: 1,
        active_channels: 3,
        now_millis: 1000,
        live_op_seq: 0,
    });
    assert!(!blocked.ready);
    assert_eq!(blocked.unknown_top_level, 2);
}

/// Changed file or live store since preview blocks the confirm.
#[test]
fn confirm_rejects_stale_source_and_generation() {
    use crate::backup::VerifiedBackup;
    let backup = VerifiedBackup {
        digest: "abc".to_string(),
        store_generation: 1,
        counts: BTreeMap::new(),
        snapshot: CanonicalExport::default(),
        unknown_top_level: 0,
    };
    let mut coord = RestoreCoordinator::default();
    let ready = coord.preview(crate::restore::PreviewRequest {
        backup: &backup,
        source_path: Path::new("/tmp/x.ltmrs-backup"),
        channel: "ch-1",
        live_counts: &BTreeMap::new(),
        live_generation: 1,
        active_channels: 1,
        now_millis: 1000,
        live_op_seq: 0,
    });
    let token = ready.confirmation_token.unwrap();
    assert_eq!(
        coord.confirm(crate::restore::ConfirmRequest {
            token: &token,
            confirm: true,
            digest: "CHANGED",
            live_generation: 1,
            channel: "ch-1",
            active_channels: 1,
            now_millis: 1000,
            live_op_seq: 0,
        }),
        Err(RestoreError::StaleSource {
            expected: "abc".to_string(),
            actual: "CHANGED".to_string(),
        })
    );
    // Stale source invalidates the token outright.
    assert_eq!(
        coord.confirm(crate::restore::ConfirmRequest {
            token: &token,
            confirm: true,
            digest: "abc",
            live_generation: 1,
            channel: "ch-1",
            active_channels: 1,
            now_millis: 1000,
            live_op_seq: 0,
        }),
        Err(RestoreError::InvalidToken)
    );
    let ready = coord.preview(crate::restore::PreviewRequest {
        backup: &backup,
        source_path: Path::new("/tmp/x.ltmrs-backup"),
        channel: "ch-1",
        live_counts: &BTreeMap::new(),
        live_generation: 1,
        active_channels: 1,
        now_millis: 1000,
        live_op_seq: 0,
    });
    let token = ready.confirmation_token.unwrap();
    assert_eq!(
        coord.confirm(crate::restore::ConfirmRequest {
            token: &token,
            confirm: true,
            digest: "abc",
            live_generation: 2,
            channel: "ch-1",
            active_channels: 1,
            now_millis: 1000,
            live_op_seq: 0,
        }),
        Err(RestoreError::GenerationChanged {
            expected: 1,
            actual: 2,
        })
    );
    // Expired tokens die even with matching state.
    let ready = coord.preview(crate::restore::PreviewRequest {
        backup: &backup,
        source_path: Path::new("/tmp/x.ltmrs-backup"),
        channel: "ch-1",
        live_counts: &BTreeMap::new(),
        live_generation: 1,
        active_channels: 1,
        now_millis: 1000,
        live_op_seq: 0,
    });
    let token = ready.confirmation_token.unwrap();
    assert_eq!(
        coord.confirm(crate::restore::ConfirmRequest {
            token: &token,
            confirm: true,
            digest: "abc",
            live_generation: 1,
            channel: "ch-1",
            active_channels: 1,
            now_millis: 1000 + RESTORE_PREVIEW_TTL_MILLIS + 1,
            live_op_seq: 0,
        }),
        Err(RestoreError::Expired)
    );
}

/// Confirm on a different channel than preview is blocked.
#[test]
fn confirm_rejects_foreign_channel() {
    use crate::backup::VerifiedBackup;
    let backup = VerifiedBackup {
        digest: "abc".to_string(),
        store_generation: 1,
        counts: BTreeMap::new(),
        snapshot: CanonicalExport::default(),
        unknown_top_level: 0,
    };
    let mut coord = RestoreCoordinator::default();
    let ready = coord.preview(crate::restore::PreviewRequest {
        backup: &backup,
        source_path: Path::new("/tmp/x.ltmrs-backup"),
        channel: "ch-1",
        live_counts: &BTreeMap::new(),
        live_generation: 1,
        active_channels: 1,
        now_millis: 1000,
        live_op_seq: 0,
    });
    let token = ready.confirmation_token.unwrap();
    let err = coord
        .confirm(crate::restore::ConfirmRequest {
            token: &token,
            confirm: true,
            digest: "abc",
            live_generation: 1,
            channel: "ch-2",
            active_channels: 1,
            now_millis: 1000,
            live_op_seq: 0,
        })
        .unwrap_err();
    assert!(matches!(err, RestoreError::Blocked(_)), "got: {err:?}");
}

/// A connection that arrived after the preview re-checks the lease at
/// confirm: the token survives so the caller retries after closing the
/// other connection (its acknowledged writes must not be drained
/// unseen).
#[test]
fn confirm_rechecks_preview_lease() {
    use crate::backup::VerifiedBackup;
    let backup = VerifiedBackup {
        digest: "abc".to_string(),
        store_generation: 1,
        counts: BTreeMap::new(),
        snapshot: CanonicalExport::default(),
        unknown_top_level: 0,
    };
    let mut coord = RestoreCoordinator::default();
    let ready = coord.preview(crate::restore::PreviewRequest {
        backup: &backup,
        source_path: Path::new("/tmp/x.ltmrs-backup"),
        channel: "ch-1",
        live_counts: &BTreeMap::new(),
        live_generation: 1,
        active_channels: 1,
        now_millis: 1000,
        live_op_seq: 0,
    });
    let token = ready.confirmation_token.unwrap();
    let err = coord
        .confirm(crate::restore::ConfirmRequest {
            token: &token,
            confirm: true,
            digest: "abc",
            live_generation: 1,
            channel: "ch-1",
            active_channels: 3,
            now_millis: 1000,
            live_op_seq: 0,
        })
        .unwrap_err();
    assert!(
        matches!(err, RestoreError::Blocked(_)),
        "cooperating connections must block confirm, got: {err:?}"
    );
    // Token unconsumed: closing the others unblocks the same confirm.
    assert!(
        coord
            .confirm(crate::restore::ConfirmRequest {
                token: &token,
                confirm: true,
                digest: "abc",
                live_generation: 1,
                channel: "ch-1",
                active_channels: 1,
                now_millis: 1000,
                live_op_seq: 0,
            })
            .is_ok()
    );
}

/// Safety path lives next to the source backup with a distinct name.
#[test]
fn safety_backup_path_shape() {
    let src = Path::new("/tmp/backups/test-123.ltmrs-backup");
    let safety = safety_backup_path(src, 456);
    assert!(safety.to_string_lossy().contains("safety"));
    assert_ne!(safety, src);
    assert_eq!(safety.extension().unwrap(), crate::backup::BACKUP_EXTENSION);
}
