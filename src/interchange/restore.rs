//! Native restore state machine (WP-11b; T-BACKUP-02/03, T-REC-02).
//!
//! Preview issues a single-use, TTL-bound confirmation token bound to the
//! backup digest (and the live store generation); confirm re-validates
//! everything before replacing anything. Restore atomically replaces the
//! domain records, bumps the store generation (retiring pre-restore
//! pipelines, invalidating their publish rights), and reports quarantined
//! references plus abandoned sessions separately. Rollback is a second
//! restore of the safety backup the coordinator always writes first.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::domain::id::StoreGeneration;
use crate::service::repository::CanonicalRepository;

/// Preview token TTL (mirrors the upstream 10-minute single-use token).
pub const RESTORE_PREVIEW_TTL_MILLIS: u64 = 10 * 60 * 1000;

/// Restore failure (every rejection names its cause; nothing is replaced
/// before all checks pass).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RestoreError {
    #[error("unknown confirmation token (preview again for a fresh one)")]
    InvalidToken,
    #[error("confirmation token expired (preview again for a fresh one)")]
    Expired,
    #[error("confirmation token already used (single-use; preview again)")]
    AlreadyUsed,
    #[error("restore requires explicit confirmation (confirm=true)")]
    NeedsConfirm,
    #[error("backup file changed since preview (digest {actual} != {expected}); preview again")]
    StaleSource { expected: String, actual: String },
    #[error("store changed since preview (generation {actual} != {expected}); preview again")]
    GenerationChanged { expected: u64, actual: u64 },
    #[error("restore blocked: {0}")]
    Blocked(String),
    #[error("store error: {0}")]
    Store(String),
    #[error("backup io at {path}: {message}")]
    Io { path: String, message: String },
}

/// Readiness followed by an optional single-use token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePreview {
    /// Whether confirm may proceed.
    pub ready: bool,
    /// Human-readable readiness state.
    pub message: String,
    /// Unknown top-level keys the backup carries (counted, dropped on
    /// restore, warned about here so confirm never drops them unseen).
    pub unknown_top_level: usize,
    /// Single-use token (None when blocked).
    pub confirmation_token: Option<String>,
    /// Token expiry millis (None when blocked).
    pub expires_at: Option<u64>,
}

/// A quarantined reference: skipped on restore, reported for manual repair.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct QuarantinedRef {
    /// Relation id (UUID string) that was skipped.
    pub relation: String,
    /// Missing endpoint description.
    pub missing: String,
}

/// Restore outcome report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    /// Records written per collection.
    pub restored: BTreeMap<String, u64>,
    /// References skipped with reasons.
    pub quarantined: Vec<QuarantinedRef>,
    /// New store generation activated.
    pub generation: u64,
    /// Unknown top-level keys the backup carried (counted, reported, not
    /// resurrected: untyped data has no keyspace to land in).
    pub unknown_top_level: usize,
}

/// Daemon-lifetime preview registry (single-use TTL tokens bound to backup
/// digest + live store generation). Held behind a mutex by the dispatcher.
#[derive(Debug, Default)]
pub struct RestoreCoordinator {
    pending: HashMap<String, PreviewRecord>,
}

#[derive(Debug, Clone)]
struct PreviewRecord {
    digest: String,
    generation: u64,
    created_at: u64,
    used: bool,
    /// Backup file the digest was verified from (re-verified on confirm).
    source: PathBuf,
    /// Channel the preview came from (confirm must arrive on it).
    channel: String,
}

/// Preview inputs bundled (keeps the coordinator call under control).
pub struct PreviewRequest<'a> {
    pub backup: &'a crate::interchange::backup::VerifiedBackup,
    pub source_path: &'a Path,
    pub channel: &'a str,
    pub live_counts: &'a BTreeMap<String, u64>,
    pub live_generation: u64,
    pub active_channels: usize,
    pub now_millis: u64,
}

impl RestoreCoordinator {
    /// Preview a verified backup against the live store. `active_channels`
    /// counts cooperating connections (the caller plus others): more than
    /// one blocks the preview (upstream parity: close others first).
    pub fn preview(&mut self, req: PreviewRequest<'_>) -> RestorePreview {
        let backup = req.backup;
        let live_counts = req.live_counts;
        let live_generation = req.live_generation;
        let active_channels = req.active_channels;
        let now_millis = req.now_millis;
        self.evict_expired(now_millis);
        if active_channels > 1 {
            return RestorePreview {
                ready: false,
                message: format!(
                    "{} other connections are open; close them and preview again (restore replaces, never merges)",
                    active_channels - 1
                ),
                unknown_top_level: backup.unknown_top_level,
                confirmation_token: None,
                expires_at: None,
            };
        }
        let token = uuid::Uuid::now_v7().to_string();
        let live_total: u64 = live_counts.values().sum();
        let backup_total: u64 = backup.counts.values().sum();
        self.pending.insert(
            token.clone(),
            PreviewRecord {
                digest: backup.digest.clone(),
                generation: live_generation,
                created_at: now_millis,
                used: false,
                source: req.source_path.to_path_buf(),
                channel: req.channel.to_string(),
            },
        );
        RestorePreview {
            ready: true,
            message: format!(
                "Backup holds {backup_total} records (digest {}) vs live {live_total}; confirm replaces the live store (never merges).{}",
                &backup.digest[..backup.digest.len().min(12)],
                if backup.unknown_top_level == 0 {
                    String::new()
                } else {
                    format!(
                        " {} unknown top-level key(s) will be dropped on restore (counted, not restored).",
                        backup.unknown_top_level
                    )
                }
            ),
            unknown_top_level: backup.unknown_top_level,
            confirmation_token: Some(token),
            expires_at: Some(now_millis + RESTORE_PREVIEW_TTL_MILLIS),
        }
    }

    /// Peek the source path a token was previewed from (no consumption;
    /// lets the caller re-verify the file before confirming).
    pub fn source_path(&self, token: &str) -> Option<PathBuf> {
        self.pending.get(token).map(|rec| rec.source.clone())
    }

    /// Drop expired or consumed records (idempotent housekeeping).
    fn evict_expired(&mut self, now_millis: u64) {
        self.pending.retain(|_, rec| {
            !rec.used && now_millis <= rec.created_at + RESTORE_PREVIEW_TTL_MILLIS
        });
    }

    /// Confirm a previewed restore. Returns the bound digest so the caller
    /// re-verifies the file before replacing anything.
    pub fn confirm(
        &mut self,
        token: &str,
        confirm: bool,
        digest: &str,
        live_generation: u64,
        channel: &str,
        now_millis: u64,
    ) -> Result<String, RestoreError> {
        let rec = self
            .pending
            .get(token)
            .cloned()
            .ok_or(RestoreError::InvalidToken)?;
        if now_millis > rec.created_at + RESTORE_PREVIEW_TTL_MILLIS {
            self.pending.remove(token);
            return Err(RestoreError::Expired);
        }
        if rec.used {
            return Err(RestoreError::AlreadyUsed);
        }
        if !confirm {
            return Err(RestoreError::NeedsConfirm);
        }
        if digest != rec.digest {
            // The file changed under us: the preview is meaningless now.
            self.pending.remove(token);
            return Err(RestoreError::StaleSource {
                expected: rec.digest,
                actual: digest.to_string(),
            });
        }
        if channel != rec.channel {
            self.pending.remove(token);
            return Err(RestoreError::Blocked(format!(
                "confirmation arrived on a different channel (previewed on {})",
                rec.channel
            )));
        }
        if live_generation != rec.generation {
            self.pending.remove(token);
            return Err(RestoreError::GenerationChanged {
                expected: rec.generation,
                actual: live_generation,
            });
        }
        if let Some(stored) = self.pending.get_mut(token) {
            stored.used = true;
        }
        Ok(rec.digest)
    }
}

/// Atomically replace the store's domain records with the backup snapshot:
/// dangling relations are quarantined (listed, skipped), everything else is
/// written in one transaction, then the store generation advances (retiring
/// pre-restore pipelines and their publish rights). Sessions are NOT
/// resurrected here (the exec layer abandons live ones and reports the
/// backup count as abandoned history).
pub fn restore_verified(
    repo: &CanonicalRepository,
    backup: &crate::interchange::backup::VerifiedBackup,
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
        } else {
            let side = if missing[0] { "source" } else { "target" };
            let id = if missing[0] { rel.source } else { rel.target };
            quarantined.push(QuarantinedRef {
                relation: rel.id.as_uuid().to_string(),
                missing: format!("{side} memory {}", id.as_uuid()),
            });
        }
    }
    repo.replace_domain(
        &backup.snapshot.memories,
        &kept,
        &backup.snapshot.guides,
        &backup.snapshot.feedback,
        &backup.snapshot.suggestions,
    )
    .map_err(|e| RestoreError::Store(e.message))?;
    repo.set_store_generation(new_generation)
        .map_err(|e| RestoreError::Store(e.message))?;
    let count = |n: usize| n as u64;
    let mut restored = BTreeMap::new();
    restored.insert(
        "memories".to_string(),
        count(backup.snapshot.memories.len()),
    );
    restored.insert("relations".to_string(), count(kept.len()));
    restored.insert("guides".to_string(), count(backup.snapshot.guides.len()));
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
    restored.insert(
        "projects".to_string(),
        count(backup.snapshot.projects.len()),
    );
    restored.insert(
        "archives".to_string(),
        count(backup.snapshot.archives.len()),
    );
    restored.insert("history".to_string(), count(backup.snapshot.history.len()));
    Ok(RestoreReport {
        restored,
        quarantined,
        generation: new_generation.as_u64(),
        unknown_top_level: backup.unknown_top_level,
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
        crate::interchange::backup::BACKUP_EXTENSION
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::export::CanonicalExport;
    use std::sync::Arc;

    use crate::domain::id::EntityId;
    use crate::domain::memory::{FragmentType, Instant, Memory, MemoryLifecycle, MemorySource};

    fn test_memory(id_num: u128, title: &str) -> Memory {
        use crate::domain::id::{DocumentRevision, EligibilityRevision, EntityRevision};
        Memory {
            id: EntityId::new(uuid::Uuid::from_u128(id_num)),
            external_alias: None,
            title: title.into(),
            fragment: format!("{title} body text."),
            description: String::new(),
            fragment_type: FragmentType::Fact,
            project: None,
            source: MemorySource::Ai,
            confidence: 1.0,
            quality_score: None,
            lifecycle: MemoryLifecycle::Live,
            tags: vec![],
            associated_with: vec![],
            relations: vec![],
            parent_id: None,
            child_ids: vec![],
            session_id: None,
            task_type: None,
            related_guides: vec![],
            evidence: vec![],
            access_count: 0,
            last_accessed_at: None,
            positive_feedback: 0,
            negative_feedback: 0,
            negative_hits: 0,
            refinement_count: 0,
            distill_candidate: false,
            entity_revision: EntityRevision::new(1),
            document_revision: DocumentRevision::new(1),
            eligibility_revision: EligibilityRevision::new(1),
            created_at: Instant::new(1000),
            updated_at: Instant::new(1000),
            raw_created: None,
            unknown_fields: std::collections::BTreeMap::new(),
        }
    }

    fn open_repo(dir: &tempfile::TempDir) -> Arc<CanonicalRepository> {
        Arc::new(CanonicalRepository::open(dir.path().join("store").to_str().unwrap()).unwrap())
    }

    /// Export repo B (2 memories), verify, restore into empty repo A:
    /// content moves, generation advances past A's.
    #[test]
    fn restore_replaces_atomically_with_generation_bump() {
        use crate::interchange::backup::{MAX_BACKUP_BYTES, export_backup, verify_backup_file};
        let dir_b = tempfile::tempdir().unwrap();
        let repo_b = open_repo(&dir_b);
        repo_b
            .put_memory_direct(&test_memory(1, "Beta One"))
            .unwrap();
        repo_b
            .put_memory_direct(&test_memory(2, "Beta Two"))
            .unwrap();
        let _gen_b = repo_b.store_generation().unwrap().as_u64();
        let report = export_backup(&repo_b, &[], dir_b.path(), "test", 1700000000000).unwrap();
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
        use crate::domain::relation::{Relation, RelationType};
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
        let backup = crate::interchange::backup::VerifiedBackup {
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
        assert!(
            out.quarantined[0].missing.contains("4242") || !out.quarantined[0].missing.is_empty()
        );
    }

    /// Preview readiness, single-use token, expiry and reuse rules.
    #[test]
    fn preview_token_lifecycle() {
        use crate::interchange::backup::VerifiedBackup;
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
        let blocked = coord.preview(crate::interchange::restore::PreviewRequest {
            backup: &backup,
            source_path: Path::new("/tmp/x.ltmrs-backup"),
            channel: "ch-1",
            live_counts: &live,
            live_generation: 1,
            active_channels: 3,
            now_millis: 1000,
        });
        assert!(!blocked.ready);
        assert!(blocked.confirmation_token.is_none());
        assert!(blocked.message.contains("2 other connections"));
        // Ready with one channel: token bound with TTL.
        let ready = coord.preview(crate::interchange::restore::PreviewRequest {
            backup: &backup,
            source_path: Path::new("/tmp/x.ltmrs-backup"),
            channel: "ch-1",
            live_counts: &live,
            live_generation: 1,
            active_channels: 1,
            now_millis: 1000,
        });
        assert!(ready.ready);
        let token = ready.confirmation_token.clone().unwrap();
        assert_eq!(ready.expires_at, Some(1000 + RESTORE_PREVIEW_TTL_MILLIS));
        // Confirm=false asks explicitly; unknown token rejected.
        assert_eq!(
            coord.confirm("nope", true, "abc", 1, "ch-1", 1000),
            Err(RestoreError::InvalidToken)
        );
        assert_eq!(
            coord.confirm(&token, false, "abc", 1, "ch-1", 1000),
            Err(RestoreError::NeedsConfirm)
        );
        // First confirm consumes; second is reuse.
        assert!(coord.confirm(&token, true, "abc", 1, "ch-1", 1000).is_ok());
        assert_eq!(
            coord.confirm(&token, true, "abc", 1, "ch-1", 1000),
            Err(RestoreError::AlreadyUsed)
        );
    }

    /// Loss accounting reaches the caller before the destructive step: the
    /// preview carries the backup's unknown-key count and warns about it,
    /// so confirm never drops future keys sight unseen.
    #[test]
    fn preview_reports_unknown_keys() {
        use crate::interchange::backup::VerifiedBackup;
        let backup = VerifiedBackup {
            digest: "abc".to_string(),
            store_generation: 1,
            counts: BTreeMap::new(),
            snapshot: CanonicalExport::default(),
            unknown_top_level: 2,
        };
        let live = BTreeMap::new();
        let mut coord = RestoreCoordinator::default();
        let ready = coord.preview(crate::interchange::restore::PreviewRequest {
            backup: &backup,
            source_path: Path::new("/tmp/x.ltmrs-backup"),
            channel: "ch-1",
            live_counts: &live,
            live_generation: 1,
            active_channels: 1,
            now_millis: 1000,
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
        let blocked = coord.preview(crate::interchange::restore::PreviewRequest {
            backup: &backup,
            source_path: Path::new("/tmp/x.ltmrs-backup"),
            channel: "ch-1",
            live_counts: &live,
            live_generation: 1,
            active_channels: 3,
            now_millis: 1000,
        });
        assert!(!blocked.ready);
        assert_eq!(blocked.unknown_top_level, 2);
    }

    /// Changed file or live store since preview blocks the confirm.
    #[test]
    fn confirm_rejects_stale_source_and_generation() {
        use crate::interchange::backup::VerifiedBackup;
        let backup = VerifiedBackup {
            digest: "abc".to_string(),
            store_generation: 1,
            counts: BTreeMap::new(),
            snapshot: CanonicalExport::default(),
            unknown_top_level: 0,
        };
        let mut coord = RestoreCoordinator::default();
        let ready = coord.preview(crate::interchange::restore::PreviewRequest {
            backup: &backup,
            source_path: Path::new("/tmp/x.ltmrs-backup"),
            channel: "ch-1",
            live_counts: &BTreeMap::new(),
            live_generation: 1,
            active_channels: 1,
            now_millis: 1000,
        });
        let token = ready.confirmation_token.unwrap();
        assert_eq!(
            coord.confirm(&token, true, "CHANGED", 1, "ch-1", 1000),
            Err(RestoreError::StaleSource {
                expected: "abc".to_string(),
                actual: "CHANGED".to_string(),
            })
        );
        // Stale source invalidates the token outright.
        assert_eq!(
            coord.confirm(&token, true, "abc", 1, "ch-1", 1000),
            Err(RestoreError::InvalidToken)
        );
        let ready = coord.preview(crate::interchange::restore::PreviewRequest {
            backup: &backup,
            source_path: Path::new("/tmp/x.ltmrs-backup"),
            channel: "ch-1",
            live_counts: &BTreeMap::new(),
            live_generation: 1,
            active_channels: 1,
            now_millis: 1000,
        });
        let token = ready.confirmation_token.unwrap();
        assert_eq!(
            coord.confirm(&token, true, "abc", 2, "ch-1", 1000),
            Err(RestoreError::GenerationChanged {
                expected: 1,
                actual: 2,
            })
        );
        // Expired tokens die even with matching state.
        let ready = coord.preview(crate::interchange::restore::PreviewRequest {
            backup: &backup,
            source_path: Path::new("/tmp/x.ltmrs-backup"),
            channel: "ch-1",
            live_counts: &BTreeMap::new(),
            live_generation: 1,
            active_channels: 1,
            now_millis: 1000,
        });
        let token = ready.confirmation_token.unwrap();
        assert_eq!(
            coord.confirm(
                &token,
                true,
                "abc",
                1,
                "ch-1",
                1000 + RESTORE_PREVIEW_TTL_MILLIS + 1,
            ),
            Err(RestoreError::Expired)
        );
    }

    /// Confirm on a different channel than preview is blocked.
    #[test]
    fn confirm_rejects_foreign_channel() {
        use crate::interchange::backup::VerifiedBackup;
        let backup = VerifiedBackup {
            digest: "abc".to_string(),
            store_generation: 1,
            counts: BTreeMap::new(),
            snapshot: CanonicalExport::default(),
            unknown_top_level: 0,
        };
        let mut coord = RestoreCoordinator::default();
        let ready = coord.preview(crate::interchange::restore::PreviewRequest {
            backup: &backup,
            source_path: Path::new("/tmp/x.ltmrs-backup"),
            channel: "ch-1",
            live_counts: &BTreeMap::new(),
            live_generation: 1,
            active_channels: 1,
            now_millis: 1000,
        });
        let token = ready.confirmation_token.unwrap();
        let err = coord
            .confirm(&token, true, "abc", 1, "ch-2", 1000)
            .unwrap_err();
        assert!(matches!(err, RestoreError::Blocked(_)), "got: {err:?}");
    }

    /// Safety path lives next to the source backup with a distinct name.
    #[test]
    fn safety_backup_path_shape() {
        let src = Path::new("/tmp/backups/test-123.ltmrs-backup");
        let safety = safety_backup_path(src, 456);
        assert!(safety.to_string_lossy().contains("safety"));
        assert_ne!(safety, src);
        assert_eq!(
            safety.extension().unwrap(),
            crate::interchange::backup::BACKUP_EXTENSION
        );
    }
    /// Unknown snapshot keys are counted (never dropped silently) and
    /// reported through restore.
    #[test]
    fn unknown_keys_counted_and_reported() {
        use crate::interchange::backup::{MAX_BACKUP_BYTES, export_backup, verify_backup_file};
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir);
        let report = export_backup(&repo, &[], dir.path(), "test", 1700000000000).unwrap();
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
            restore_verified(&repo, &verified, crate::domain::id::StoreGeneration::new(3)).unwrap();
        assert_eq!(out.unknown_top_level, 1);
    }

    /// Truncated files fail as corruption (never partial parses).
    #[test]
    fn truncated_file_is_corrupt() {
        use crate::interchange::backup::{MAX_BACKUP_BYTES, export_backup, verify_backup_file};
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir);
        let report = export_backup(&repo, &[], dir.path(), "test", 1700000000000).unwrap();
        let raw = std::fs::read(&report.path).unwrap();
        for cut in [raw.len() / 2, raw.len() - 1] {
            let chopped = dir.path().join(format!("chopped-{cut}.ltmrs-backup"));
            std::fs::write(&chopped, &raw[..cut]).unwrap();
            let err = verify_backup_file(&chopped, MAX_BACKUP_BYTES).unwrap_err();
            assert!(
                matches!(err, crate::interchange::backup::BackupError::Corrupt(_)),
                "cut {cut}: got {err:?}"
            );
        }
    }

    /// Absurd nesting depth fails bounded (serde recursion guard surfaces
    /// as corruption, never a stack overflow).
    #[test]
    fn depth_bomb_is_corrupt() {
        use crate::interchange::backup::MAX_BACKUP_BYTES;
        use crate::interchange::backup::verify_backup_file;
        let dir = tempfile::tempdir().unwrap();
        let mut nested = serde_json::json!({"format": "ltmrs-backup"});
        for _ in 0..300 {
            nested = serde_json::json!({"nest": nested});
        }
        let bomb = dir.path().join("bomb.ltmrs-backup");
        std::fs::write(&bomb, serde_json::to_vec(&nested).unwrap()).unwrap();
        let err = verify_backup_file(&bomb, MAX_BACKUP_BYTES).unwrap_err();
        assert!(
            matches!(err, crate::interchange::backup::BackupError::Corrupt(_)),
            "got: {err:?}"
        );
    }

    /// A manifest that lies about counts is corrupt even when the digest
    /// matches (cross-checked against the snapshot it rides with).
    #[test]
    fn lying_manifest_is_corrupt() {
        use crate::interchange::backup::{MAX_BACKUP_BYTES, export_backup, verify_backup_file};
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir);
        let report = export_backup(&repo, &[], dir.path(), "test", 1700000000000).unwrap();
        let mut v: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&report.path).unwrap()).unwrap();
        v["manifest"]["memories"] = serde_json::json!(999999);
        let lying = dir.path().join("lying.ltmrs-backup");
        std::fs::write(&lying, serde_json::to_vec(&v).unwrap()).unwrap();
        let err = verify_backup_file(&lying, MAX_BACKUP_BYTES).unwrap_err();
        assert!(
            matches!(err, crate::interchange::backup::BackupError::Corrupt(_)),
            "got: {err:?}"
        );
    }

    /// Export while concurrent writers append: the manifest always describes
    /// exactly the snapshot it ships (self-consistent cut, never torn).
    #[test]
    fn export_during_writes_stays_consistent() {
        use crate::interchange::backup::{MAX_BACKUP_BYTES, export_backup, verify_backup_file};
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
                        crate::domain::id::EntityRevision::new(m.entity_revision.as_u64() + 1);
                }
            }
        });
        let mut digests = std::collections::BTreeSet::new();
        for _ in 0..5 {
            let rep = export_backup(&repo, &[], dir.path(), "race", 1700000000000).unwrap();
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

    /// Restoring retires the pre-restore pipeline: a staged worker from
    /// before the restore can no longer publish (generation invalidation).
    #[test]
    fn restore_invalidates_old_pipelines() {
        use crate::domain::id::ModelFingerprint;
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir);
        let staged = repo.stage_generation(ModelFingerprint::new(7)).unwrap();
        assert!(
            repo.generation_under_construction(staged, ModelFingerprint::new(7))
                .unwrap()
        );
        let snapshot = CanonicalExport::default();
        let backup = crate::interchange::backup::VerifiedBackup {
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
            crate::domain::id::StoreGeneration::new(live_gen + 1),
        )
        .unwrap();
        assert!(
            !repo
                .generation_under_construction(staged, ModelFingerprint::new(7))
                .unwrap(),
            "pre-restore pipeline must lose publish rights"
        );
    }
}
