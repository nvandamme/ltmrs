//! Verified restore application and safety backups (moved verbatim from `restore.rs`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{QuarantinedRef, RestoreError, RestoreReport};
use ltmrs_domain::id::StoreGeneration;
use ltmrs_service::repository::CanonicalRepository;

/// Atomically replace the store's domain records with the backup snapshot:
/// dangling relations are quarantined (listed, skipped); the store flips in
/// ONE durable transaction covering data, canonical sessions, both
/// operational receipt logs and the generation switch (see
/// `restore_replace`), so a crash lands on the old store or the new one,
/// never a mixture. Session history restores from the backup (it is
/// knowledge for continuity recall and analytics); channel bindings/leases
/// and virtual live sessions stay registry-side and are never restored.
pub fn restore_verified(
    repo: &CanonicalRepository,
    backup: &crate::backup::VerifiedBackup,
    new_generation: StoreGeneration,
) -> Result<RestoreReport, RestoreError> {
    let guard = repo.restore_write_guard();
    restore_verified_guarded(repo, &guard, backup, new_generation)
}

/// Verified restore under an already-held restore fence (see
/// `CanonicalRepository::restore_write_guard`): the exec restore flow
/// holds the fence across confirm → safety snapshot → this call, so no
/// acknowledged write can land between the safety backup and the
/// replace. Must not be called without holding the fence.
pub fn restore_verified_guarded(
    repo: &CanonicalRepository,
    guard: &std::sync::RwLockWriteGuard<'_, ()>,
    backup: &crate::backup::VerifiedBackup,
    new_generation: StoreGeneration,
) -> Result<RestoreReport, RestoreError> {
    use std::collections::BTreeSet;
    let present: BTreeSet<String> = backup
        .snapshot
        .memories
        .iter()
        .map(|m| m.id.as_uuid().to_string())
        .collect();
    let mut kept = Vec::new();
    let mut quarantined = Vec::new();
    for rel in &backup.snapshot.relations {
        let missing = [
            !present.contains(&rel.source.as_uuid().to_string()),
            !present.contains(&rel.target.as_uuid().to_string()),
        ];
        if missing == [false, false] {
            kept.push(rel.clone());
        } else if missing == [true, true] {
            quarantined.push(QuarantinedRef {
                relation: rel.id.as_uuid().to_string(),
                missing: format!(
                    "source memory {} and target memory {}",
                    rel.source.as_uuid(),
                    rel.target.as_uuid()
                ),
            });
        } else {
            let side = if missing[0] { "source" } else { "target" };
            let id = if missing[0] { rel.source } else { rel.target };
            quarantined.push(QuarantinedRef {
                relation: rel.id.as_uuid().to_string(),
                missing: format!("{side} memory {}", id.as_uuid()),
            });
        }
    }
    let sessions_marked_abandoned = repo
        .restore_replace_guarded(
            &backup.snapshot.memories,
            &kept,
            &backup.snapshot.guides,
            &backup.snapshot.feedback,
            &backup.snapshot.suggestions,
            &backup.snapshot.sessions,
            new_generation,
            guard,
        )
        .map_err(|e| RestoreError::Store(e.message))?;
    let count = |n: usize| n as u64;
    let mut restored = BTreeMap::new();
    restored.insert(
        "memories".to_string(),
        count(backup.snapshot.memories.len()),
    );
    restored.insert("relations".to_string(), count(kept.len()));
    restored.insert("guides".to_string(), count(backup.snapshot.guides.len()));
    // Replace semantics write exactly these six categories. Sessions
    // restore from the backup (continuity/analytics knowledge); projects,
    // archives and history have no storage keyspace. Reporting intake
    // counts for the latter would claim restores that never happened, so
    // these report 0 with the reason documented, not the snapshot lengths.
    restored.insert(
        "sessions".to_string(),
        count(backup.snapshot.sessions.len()),
    );
    restored.insert(
        "feedback".to_string(),
        count(backup.snapshot.feedback.len()),
    );
    restored.insert(
        "suggestions".to_string(),
        count(backup.snapshot.suggestions.len()),
    );
    restored.insert("projects".to_string(), 0);
    restored.insert("archives".to_string(), 0);
    restored.insert("history".to_string(), 0);
    Ok(RestoreReport {
        restored,
        quarantined,
        generation: new_generation.as_u64(),
        unknown_top_level: backup.unknown_top_level,
        sessions_marked_abandoned,
    })
}

/// Safety-backup path next to `backup_path` (rollback source on failure).
pub fn safety_backup_path(backup_path: &Path, created_at_millis: u64) -> PathBuf {
    let parent = backup_path.parent().unwrap_or_else(|| Path::new("."));
    let stem = backup_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "backup".to_string());
    parent.join(format!(
        "safety-{created_at_millis}-{stem}.{}",
        crate::backup::BACKUP_EXTENSION
    ))
}
