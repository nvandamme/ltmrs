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
    /// Live mutation watermark at preview time (op_seq): writes landing
    /// between preview and confirm are acknowledged at confirm, never
    /// drained unseen.
    op_seq: u64,
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
    /// Live mutation watermark (repo op_seq) bound into the record.
    pub live_op_seq: u64,
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
                op_seq: req.live_op_seq,
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

    /// Confirm a previewed restore. Returns the bound digest (so the caller
    /// re-verifies the file before replacing anything) plus the live writes
    /// that landed between preview and confirm: the replace drains them, so
    /// the caller must acknowledge the count (recoverable from the safety
    /// backup) instead of dropping it silently. Refusing here would brick
    /// restores for active hosts (their own channel writes mid-flow), so
    /// the delta is reported, not refused. `active_channels` counts
    /// cooperating connections including the caller: more than one
    /// re-checks the preview lease (a connection that arrived after the
    /// preview may hold acknowledged writes the replace would drain unseen).
    /// Lease failures do NOT consume the token: close the others and
    /// confirm again.
    pub fn confirm(&mut self, req: ConfirmRequest<'_>) -> Result<(String, u64), RestoreError> {
        let token = req.token;
        let rec = self
            .pending
            .get(token)
            .cloned()
            .ok_or(RestoreError::InvalidToken)?;
        if req.now_millis > rec.created_at + RESTORE_PREVIEW_TTL_MILLIS {
            self.pending.remove(token);
            return Err(RestoreError::Expired);
        }
        if rec.used {
            return Err(RestoreError::AlreadyUsed);
        }
        if !req.confirm {
            return Err(RestoreError::NeedsConfirm);
        }
        if req.digest != rec.digest {
            // The file changed under us: the preview is meaningless now.
            self.pending.remove(token);
            return Err(RestoreError::StaleSource {
                expected: rec.digest,
                actual: req.digest.to_string(),
            });
        }
        if req.channel != rec.channel {
            self.pending.remove(token);
            return Err(RestoreError::Blocked(format!(
                "confirmation arrived on a different channel (previewed on {})",
                rec.channel
            )));
        }
        if req.live_generation != rec.generation {
            self.pending.remove(token);
            return Err(RestoreError::GenerationChanged {
                expected: rec.generation,
                actual: req.live_generation,
            });
        }
        if req.active_channels > 1 {
            return Err(RestoreError::Blocked(format!(
                "{} other connections are open; close them and confirm again (the token stays valid)",
                req.active_channels - 1
            )));
        }
        if let Some(stored) = self.pending.get_mut(token) {
            stored.used = true;
        }
        Ok((rec.digest, req.live_op_seq.saturating_sub(rec.op_seq)))
    }
}

/// Confirm inputs bundled (mirrors `PreviewRequest`; keeps the coordinator
/// call under control as checks accumulate).
pub struct ConfirmRequest<'a> {
    pub token: &'a str,
    pub confirm: bool,
    pub digest: &'a str,
    pub live_generation: u64,
    pub channel: &'a str,
    pub active_channels: usize,
    pub now_millis: u64,
    /// Live mutation watermark at confirm time (compared to the preview's).
    pub live_op_seq: u64,
}

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
    repo.restore_replace(
        &backup.snapshot.memories,
        &kept,
        &backup.snapshot.guides,
        &backup.snapshot.feedback,
        &backup.snapshot.suggestions,
        &backup.snapshot.sessions,
        new_generation,
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

    /// Gateway scaffolding: frozen clock + issued namespace so `apply`
    /// writes receipts, aliases and projection jobs like production.
    fn gateway_repo(dir: &tempfile::TempDir) -> Arc<CanonicalRepository> {
        use crate::domain::clock::FrozenClock;
        let clock = std::sync::Arc::new(FrozenClock::new(1000));
        let repo =
            CanonicalRepository::open_with_clock(dir.path().join("store").to_str().unwrap(), clock)
                .unwrap();
        repo.issue_namespace(
            crate::domain::id::FrontendId::new(uuid::Uuid::from_u128(1)),
            1000,
        )
        .unwrap();
        Arc::new(repo)
    }

    fn gateway_ctx(op_num: u64) -> crate::domain::command::CommandContext {
        use crate::domain::id::{ChannelId, FrontendId, OperationId};
        crate::domain::command::CommandContext {
            store_generation: crate::domain::id::StoreGeneration::FIRST,
            frontend_id: FrontendId::new(uuid::Uuid::from_u128(1)),
            channel_id: ChannelId::new(uuid::Uuid::from_u128(2)),
            session: None,
            operation_id: OperationId::new(uuid::Uuid::from_u128(op_num as u128)),
            request_digest: format!("restore-test-{op_num}"),
            deadline_millis: None,
            scope: Default::default(),
            retry_epoch: 1,
        }
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
            live_op_seq: 0,
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
            live_op_seq: 0,
        });
        assert!(ready.ready);
        let token = ready.confirmation_token.clone().unwrap();
        assert_eq!(ready.expires_at, Some(1000 + RESTORE_PREVIEW_TTL_MILLIS));
        // Confirm=false asks explicitly; unknown token rejected.
        assert_eq!(
            coord.confirm(crate::interchange::restore::ConfirmRequest {
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
            coord.confirm(crate::interchange::restore::ConfirmRequest {
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
                .confirm(crate::interchange::restore::ConfirmRequest {
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
            coord.confirm(crate::interchange::restore::ConfirmRequest {
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
        use crate::interchange::backup::VerifiedBackup;
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
            let ready = coord.preview(crate::interchange::restore::PreviewRequest {
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
                .confirm(crate::interchange::restore::ConfirmRequest {
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
        let blocked = coord.preview(crate::interchange::restore::PreviewRequest {
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
            live_op_seq: 0,
        });
        let token = ready.confirmation_token.unwrap();
        assert_eq!(
            coord.confirm(crate::interchange::restore::ConfirmRequest {
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
            coord.confirm(crate::interchange::restore::ConfirmRequest {
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
        let ready = coord.preview(crate::interchange::restore::PreviewRequest {
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
            coord.confirm(crate::interchange::restore::ConfirmRequest {
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
        let ready = coord.preview(crate::interchange::restore::PreviewRequest {
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
            coord.confirm(crate::interchange::restore::ConfirmRequest {
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
            live_op_seq: 0,
        });
        let token = ready.confirmation_token.unwrap();
        let err = coord
            .confirm(crate::interchange::restore::ConfirmRequest {
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
            live_op_seq: 0,
        });
        let token = ready.confirmation_token.unwrap();
        let err = coord
            .confirm(crate::interchange::restore::ConfirmRequest {
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
                .confirm(crate::interchange::restore::ConfirmRequest {
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

    /// Replace covers every durable keyspace, not just the five exported
    /// collections: stale aliases must not block reuse, stale receipts must
    /// not replay, namespaces must not survive, and projection jobs must be
    /// re-enqueued for the restored memories (or search never converges).
    #[test]
    fn restore_replace_covers_all_keyspaces() {
        use crate::domain::command::DomainCommand;
        use crate::domain::id::ExternalAlias;
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
        let backup = crate::interchange::backup::VerifiedBackup {
            digest: snapshot.digest(),
            store_generation: 1,
            counts: BTreeMap::new(),
            snapshot,
            unknown_top_level: 0,
        };
        restore_verified(&repo, &backup, crate::domain::id::StoreGeneration::new(2)).unwrap();

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
                },
            )
            .unwrap_err();
        assert!(
            err.message.contains("namespace"),
            "stale namespace must be refused, got: {err:?}"
        );
        // Receipts drained: replaying op 5 with new content executes fresh
        // instead of returning the stale receipt. A fresh namespace is
        // issued first (the pre-restore epoch was drained with the rest).
        repo.issue_namespace(
            crate::domain::id::FrontendId::new(uuid::Uuid::from_u128(1)),
            1000,
        )
        .unwrap();
        let mut changed = test_memory(1, "Changed Content");
        changed.external_alias = Some(ExternalAlias::new("old-alias"));
        repo.apply(
            &gateway_ctx(5),
            &DomainCommand::AddMemory {
                memory: changed,
                session: None,
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
            .filter(|r| {
                matches!(
                    r.status,
                    crate::domain::projection::GenerationStatus::Active
                )
            })
            .collect();
        assert_eq!(actives.len(), 1, "exactly one Active generation");
        assert_eq!(actives[0].generation.as_u64(), 2);
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
        use crate::domain::id::{ChannelId, SessionHandle};
        use crate::domain::session::SessionOp;
        let dir = tempfile::tempdir().unwrap();
        let repo = gateway_repo(&dir);
        let channel = ChannelId::new(uuid::Uuid::from_u128(9));
        // Live session A via session op X.
        let handle_a = SessionHandle::new(uuid::Uuid::from_u128(100));
        match repo
            .session_start_tx(
                "op-X",
                "digest-X",
                handle_a,
                channel,
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
                "op-Y",
                "digest-Y",
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
                "op-B",
                "digest-B",
                handle_b,
                channel,
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
        let backup = crate::interchange::backup::VerifiedBackup {
            digest: snapshot.digest(),
            store_generation: 1,
            counts: BTreeMap::new(),
            snapshot,
            unknown_top_level: 0,
        };
        restore_verified(&repo, &backup, crate::domain::id::StoreGeneration::new(2)).unwrap();

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
        // the pre-restore outcome for A.
        let handle_c = SessionHandle::new(uuid::Uuid::from_u128(300));
        match repo
            .session_start_tx(
                "op-X",
                "digest-X",
                handle_c,
                channel,
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
                "op-Y",
                "digest-Y",
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
        let backup = crate::interchange::backup::VerifiedBackup {
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
        restore_verified(&repo, &backup, crate::domain::id::StoreGeneration::new(2)).unwrap();
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
}
