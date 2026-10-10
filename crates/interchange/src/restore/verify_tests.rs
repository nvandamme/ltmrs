//! Restore verification/corruption tests (moved verbatim from `restore.rs`).

use super::test_support::*;
use super::verify::restore_verified;
use ltmrs_domain::export::CanonicalExport;

/// Unknown snapshot keys are counted (never dropped silently) and
/// reported through restore.
#[test]
fn unknown_keys_counted_and_reported() {
    use crate::backup::{MAX_BACKUP_BYTES, export_backup, verify_backup_file};
    let dir = tempfile::tempdir().unwrap();
    let repo = open_repo(&dir);
    let report = export_backup(&repo, dir.path(), "test", 1700000000000).unwrap();
    // Future-producer simulation: extra top-level snapshot key. The
    // manifest digest covers the snapshot bytes, so re-sign it.
    let mut v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report.path).unwrap()).unwrap();
    v["snapshot"]["future_collection"] = serde_json::json!([{"kept": true}]);
    let evolved_snap: CanonicalExport = serde_json::from_value(v["snapshot"].clone()).unwrap();
    let digest = evolved_snap.digest();
    v["manifest"]["digest"] = serde_json::Value::String(digest);
    let evolved = dir.path().join("evolved.ltmrs-backup");
    std::fs::write(&evolved, serde_json::to_vec(&v).unwrap()).unwrap();
    let verified = verify_backup_file(&evolved, MAX_BACKUP_BYTES).unwrap();
    assert_eq!(verified.unknown_top_level, 1);
    let out =
        restore_verified(&repo, &verified, ltmrs_domain::id::StoreGeneration::new(3)).unwrap();
    assert_eq!(out.unknown_top_level, 1);
}

/// Truncated files fail as corruption (never partial parses).
#[test]
fn truncated_file_is_corrupt() {
    use crate::backup::{MAX_BACKUP_BYTES, export_backup, verify_backup_file};
    let dir = tempfile::tempdir().unwrap();
    let repo = open_repo(&dir);
    let report = export_backup(&repo, dir.path(), "test", 1700000000000).unwrap();
    let raw = std::fs::read(&report.path).unwrap();
    for cut in [raw.len() / 2, raw.len() - 1] {
        let chopped = dir.path().join(format!("chopped-{cut}.ltmrs-backup"));
        std::fs::write(&chopped, &raw[..cut]).unwrap();
        let err = verify_backup_file(&chopped, MAX_BACKUP_BYTES).unwrap_err();
        assert!(
            matches!(err, crate::backup::BackupError::Corrupt(_)),
            "cut {cut}: got {err:?}"
        );
    }
}

/// Absurd nesting depth fails bounded (serde recursion guard surfaces
/// as corruption, never a stack overflow).
#[test]
fn depth_bomb_is_corrupt() {
    use crate::backup::MAX_BACKUP_BYTES;
    use crate::backup::verify_backup_file;
    let dir = tempfile::tempdir().unwrap();
    let mut nested = serde_json::json!({"format": "ltmrs-backup"});
    for _ in 0..300 {
        nested = serde_json::json!({"nest": nested});
    }
    let bomb = dir.path().join("bomb.ltmrs-backup");
    std::fs::write(&bomb, serde_json::to_vec(&nested).unwrap()).unwrap();
    let err = verify_backup_file(&bomb, MAX_BACKUP_BYTES).unwrap_err();
    assert!(
        matches!(err, crate::backup::BackupError::Corrupt(_)),
        "got: {err:?}"
    );
}

/// A manifest that lies about counts is corrupt even when the digest
/// matches (cross-checked against the snapshot it rides with).
#[test]
fn lying_manifest_is_corrupt() {
    use crate::backup::{MAX_BACKUP_BYTES, export_backup, verify_backup_file};
    let dir = tempfile::tempdir().unwrap();
    let repo = open_repo(&dir);
    let report = export_backup(&repo, dir.path(), "test", 1700000000000).unwrap();
    let mut v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report.path).unwrap()).unwrap();
    v["manifest"]["memories"] = serde_json::json!(999999);
    let lying = dir.path().join("lying.ltmrs-backup");
    std::fs::write(&lying, serde_json::to_vec(&v).unwrap()).unwrap();
    let err = verify_backup_file(&lying, MAX_BACKUP_BYTES).unwrap_err();
    assert!(
        matches!(err, crate::backup::BackupError::Corrupt(_)),
        "got: {err:?}"
    );
}

/// Export while concurrent writers append: the manifest always describes
/// exactly the snapshot it ships (self-consistent cut, never torn).
#[test]
fn export_during_writes_stays_consistent() {
    use crate::backup::{MAX_BACKUP_BYTES, export_backup, verify_backup_file};
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let repo = open_repo(&dir);
    let repo2 = Arc::clone(&repo);
    let writer = std::thread::spawn(move || {
        for i in 0..20u128 {
            let mut m = test_memory(1000 + i, &format!("Racer {i}"));
            // Retry on optimistic conflicts (concurrent writer discipline).
            for _ in 0..50 {
                if repo2.put_memory_direct(&m).is_ok() {
                    break;
                }
                m.entity_revision =
                    ltmrs_domain::id::EntityRevision::new(m.entity_revision.as_u64() + 1);
            }
        }
    });
    let mut digests = std::collections::BTreeSet::new();
    for _ in 0..5 {
        let rep = export_backup(&repo, dir.path(), "race", 1700000000000).unwrap();
        let verified = verify_backup_file(&rep.path, MAX_BACKUP_BYTES).unwrap();
        // Manifest digest always equals the shipped snapshot's digest.
        assert_eq!(verified.digest, verified.snapshot.digest());
        digests.insert(verified.digest.clone());
        let n = verified.counts["memories"];
        assert!(n <= 20, "bounded by the writer: {n}");
    }
    writer.join().unwrap();
    assert!(!digests.is_empty());
}
