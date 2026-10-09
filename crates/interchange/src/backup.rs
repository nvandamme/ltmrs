//! Native logical backup (WP-11a; T-BACKUP-01 export leg).
//!
//! Format `ltmrs-backup` v1: a JSON envelope holding one coherent canonical
//! snapshot (`CanonicalExport` from a single read transaction) plus a manifest
//! (per-collection counts + snapshot digest). Files end `.ltmrs-backup`
//! (never `.lemma-backup`: a native archive is never relabeled) and publish
//! atomically (tmp + rename). `backup_create` verifies by re-reading.

use std::path::{Path, PathBuf};

use ltmrs_domain::export::CanonicalExport;
use ltmrs_service::repository::CanonicalRepository;

/// Native backup format marker (never `lemma-backup`).
pub const BACKUP_FORMAT: &str = "ltmrs-backup";
/// Native backup format version (single supported version; anything else is
/// rejected explicitly, never migrated silently).
pub const BACKUP_FORMAT_VERSION: u32 = 1;
/// Native backup file extension (never `.lemma-backup`).
pub const BACKUP_EXTENSION: &str = "ltmrs-backup";
/// Upper bound for a backup file (mirrors the upstream 128 MiB release cap;
/// enforced on read so a hostile file cannot exhaust memory).
pub const MAX_BACKUP_BYTES: u64 = 128 * 1024 * 1024;

/// Effective backup byte bound: `LTMRS_MAX_BACKUP_BYTES` overrides the
/// default when it parses as a positive u64 (100k-memory tiers need headroom
/// the hostile-input default denies); unset or garbage falls back silently
/// to `MAX_BACKUP_BYTES` — the safe direction, since a smaller bound only
/// refuses oversized backups.
pub fn backup_byte_limit() -> u64 {
    backup_byte_limit_from(std::env::var("LTMRS_MAX_BACKUP_BYTES").ok())
}

/// Pure precedence core (see `backup_byte_limit`).
pub fn backup_byte_limit_from(env: Option<String>) -> u64 {
    env.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(MAX_BACKUP_BYTES)
}

/// Backup failure (IO, format, integrity — never silent).
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("backup io at {path}: {message}")]
    Io { path: String, message: String },
    #[error("not an ltmrs backup (format marker: {found:?})")]
    Format { found: Option<String> },
    #[error(
        "legacy Lemma backup {format} v{version:?}: SQLite payloads require a C database engine, excluded by RQ-21; recorded as an unsupported conformance target, never silently converted"
    )]
    LegacyUnsupported {
        format: String,
        version: Option<u64>,
    },
    #[error("unsupported backup format version: {0} (supported: 1)")]
    Version(u32),
    #[error("backup exceeds size bound ({size} > {bound} bytes)")]
    TooLarge { size: u64, bound: u64 },
    #[error("backup digest mismatch: manifest declares {expected}, file holds {actual}")]
    DigestMismatch { expected: String, actual: String },
    #[error("backup snapshot is corrupt: {0}")]
    Corrupt(String),
    #[error("store error: {0}")]
    Store(String),
}

/// Successful export report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupReport {
    /// Final published path.
    pub path: PathBuf,
    /// Snapshot digest (stable across exports of identical content).
    pub digest: String,
    /// Manifest per-collection counts.
    pub counts: std::collections::BTreeMap<String, u64>,
}

/// Encoded backup envelope with its manifest outputs (alias: the tuple
/// trips `type_complexity` inline).
pub type EncodedBackup = (Vec<u8>, String, std::collections::BTreeMap<String, u64>);

/// A verified backup: the parsed envelope, ready for preview/restore.
#[derive(Debug)]
pub struct VerifiedBackup {
    /// Snapshot digest from the manifest (verified against the snapshot).
    pub digest: String,
    /// Store generation recorded at export.
    pub store_generation: u64,
    /// Per-collection counts from the manifest.
    pub counts: std::collections::BTreeMap<String, u64>,
    /// Unknown top-level keys captured (never dropped silently): snapshot
    /// extras via flatten plus unrecognized envelope keys. Counted here so
    /// restore can report them (WP-11c loss accounting).
    pub unknown_top_level: usize,
    /// The snapshot itself.
    pub snapshot: CanonicalExport,
}

/// Export one coherent snapshot of `repo` into `dir` (created when missing)
/// as `<prefix>-<millis>-<v7>.ltmrs-backup`, published atomically and
/// verified by re-read before returning. Everything — memories, guides,
/// canonical sessions, feedback, suggestions, generation — comes from ONE
/// Fjall read snapshot, so a concurrent `session_end` can never tear the
/// archive (an Active session paired with already-bumped guide counts).
pub fn export_backup(
    repo: &CanonicalRepository,
    dir: &Path,
    prefix: &str,
    created_at_millis: u64,
) -> Result<BackupReport, BackupError> {
    // One coherent cut (see below): the full domain export plus the live
    // generation come from a single read transaction.
    export_backup_with_limit(repo, dir, prefix, created_at_millis, MAX_BACKUP_BYTES)
}

/// `export_backup` with an explicit byte bound (see `backup_byte_limit`).
pub fn export_backup_with_limit(
    repo: &CanonicalRepository,
    dir: &Path,
    prefix: &str,
    created_at_millis: u64,
    limit: u64,
) -> Result<BackupReport, BackupError> {
    let io_err = |path: &Path, e: std::io::Error| BackupError::Io {
        path: path.to_string_lossy().into_owned(),
        message: e.to_string(),
    };
    let (export, generation) = repo
        .export_full_with_generation()
        .map_err(|e| BackupError::Store(e.message))?;
    let (bytes, _digest, _counts) =
        encode_backup_with_limit(&export, generation.as_u64(), created_at_millis, limit)?;
    std::fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
    let name = format!(
        "{prefix}-{created_at_millis}-{}.{}",
        uuid::Uuid::now_v7().as_simple(),
        BACKUP_EXTENSION
    );
    export_backup_to_with_limit(&dir.join(&name), &bytes, limit)
}

/// Publish already-encoded backup `bytes` at an exact path (safety backups
/// and tests): same atomic tmp+rename + re-read verification as exports.
/// Encode one backup envelope from a filled export (shared by fresh
/// exports and safety backups): manifest counts + snapshot digest + size
/// bound. Returns the bytes, the digest and the manifest counts.
pub fn encode_backup(
    export: &CanonicalExport,
    generation: u64,
    created_at_millis: u64,
) -> Result<EncodedBackup, BackupError> {
    encode_backup_with_limit(export, generation, created_at_millis, MAX_BACKUP_BYTES)
}

/// `encode_backup` with an explicit byte bound (see `backup_byte_limit`).
pub fn encode_backup_with_limit(
    export: &CanonicalExport,
    generation: u64,
    created_at_millis: u64,
    limit: u64,
) -> Result<EncodedBackup, BackupError> {
    let digest = export.digest();
    let count = |n: usize| n as u64;
    let mut counts = std::collections::BTreeMap::new();
    for (key, n) in [
        ("memories", export.memories.len()),
        ("relations", export.relations.len()),
        ("guides", export.guides.len()),
        ("sessions", export.sessions.len()),
        ("feedback", export.feedback.len()),
        ("suggestions", export.suggestions.len()),
        ("projects", export.projects.len()),
        ("archives", export.archives.len()),
        ("history", export.history.len()),
    ] {
        counts.insert(key.to_string(), count(n));
    }
    let envelope = serde_json::json!({
        "format": BACKUP_FORMAT,
        "format_version": BACKUP_FORMAT_VERSION,
        "ltmrs_version": env!("CARGO_PKG_VERSION"),
        "created_at": created_at_millis,
        "store_generation": generation,
        "manifest": {
            "memories": counts["memories"],
            "relations": counts["relations"],
            "guides": counts["guides"],
            "sessions": counts["sessions"],
            "feedback": counts["feedback"],
            "suggestions": counts["suggestions"],
            "projects": counts["projects"],
            "archives": counts["archives"],
            "history": counts["history"],
            "digest": digest,
        },
        "snapshot": export,
    });
    let bytes = serde_json::to_vec(&envelope)
        .map_err(|e| BackupError::Corrupt(format!("cannot encode snapshot: {e}")))?;
    if bytes.len() as u64 > limit {
        return Err(BackupError::TooLarge {
            size: bytes.len() as u64,
            bound: limit,
        });
    }
    Ok((bytes, digest, counts))
}

pub fn export_backup_to(final_path: &Path, bytes: &[u8]) -> Result<BackupReport, BackupError> {
    export_backup_to_with_limit(final_path, bytes, MAX_BACKUP_BYTES)
}

/// `export_backup_to` with an explicit byte bound (see `backup_byte_limit`).
/// Durability barrier: the staged file is created owner-private, flushed,
/// and closed before the rename makes it visible; the rename is then pinned
/// with a parent-dir flush (unix). Every step participates in the result —
/// a discarded sync error would acknowledge unflushed state as durable.
/// Windows opens directories un-openable, so the parent flush is unix-only
/// (NTFS flushes metadata at handle close; the rename itself is atomic).
pub fn export_backup_to_with_limit(
    final_path: &Path,
    bytes: &[u8],
    limit: u64,
) -> Result<BackupReport, BackupError> {
    let io_err = |path: &Path, e: std::io::Error| BackupError::Io {
        path: path.to_string_lossy().into_owned(),
        message: e.to_string(),
    };
    if let Some(parent) = final_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
    }
    let staged = final_path.with_extension(BACKUP_EXTENSION.to_string() + ".tmp");
    // Owner-private from creation (0600 on unix; the per-user profile ACL
    // on Windows): a backup holds the whole corpus and must never depend
    // on the process umask.
    #[cfg(unix)]
    let mut staged_file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&staged)
            .map_err(|e| io_err(&staged, e))?
    };
    #[cfg(not(unix))]
    let mut staged_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&staged)
        .map_err(|e| io_err(&staged, e))?;
    std::io::Write::write_all(&mut staged_file, bytes).map_err(|e| io_err(&staged, e))?;
    staged_file.sync_all().map_err(|e| io_err(&staged, e))?;
    drop(staged_file);
    std::fs::rename(&staged, final_path).map_err(|e| io_err(final_path, e))?;
    #[cfg(unix)]
    if let Some(parent) = final_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|e| io_err(parent, e))?;
    }
    // Verify by re-reading (a torn write fails here, never silently).
    let verified = verify_backup_file(final_path, limit)?;
    Ok(BackupReport {
        path: final_path.to_path_buf(),
        digest: verified.digest.clone(),
        counts: verified.counts,
    })
}

/// Read, bound-check, parse and digest-verify a backup file. Reads the file
/// read-only and never touches the store (safe on hostile input).
pub fn verify_backup_file(path: &Path, max_bytes: u64) -> Result<VerifiedBackup, BackupError> {
    let io_err = |e: std::io::Error| BackupError::Io {
        path: path.to_string_lossy().into_owned(),
        message: e.to_string(),
    };
    // Bound first: metadata alone decides, so a hostile file is never read fully.
    let size = std::fs::metadata(path).map_err(io_err)?.len();
    if size > max_bytes {
        return Err(BackupError::TooLarge {
            size,
            bound: max_bytes,
        });
    }
    let bytes = std::fs::read(path).map_err(io_err)?;
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| BackupError::Corrupt(e.to_string()))?;
    let format = v.get("format").and_then(|f| f.as_str());
    if format != Some(BACKUP_FORMAT) {
        if format == Some(crate::legacy::LEGACY_FORMAT) {
            return Err(BackupError::LegacyUnsupported {
                format: format.unwrap().to_string(),
                version: v.get("format_version").and_then(|f| f.as_u64()),
            });
        }
        return Err(BackupError::Format {
            found: format.map(|f| f.to_string()),
        });
    }
    let version = v
        .get("format_version")
        .and_then(|f| f.as_u64())
        .ok_or_else(|| BackupError::Corrupt("missing format_version".to_string()))?;
    if version != BACKUP_FORMAT_VERSION as u64 {
        return Err(BackupError::Version(
            u32::try_from(version).unwrap_or(u32::MAX),
        ));
    }
    let manifest = v
        .get("manifest")
        .ok_or_else(|| BackupError::Corrupt("missing manifest".to_string()))?;
    let expected = manifest
        .get("digest")
        .and_then(|d| d.as_str())
        .ok_or_else(|| BackupError::Corrupt("missing manifest.digest".to_string()))?;
    let snapshot_value = v
        .get("snapshot")
        .ok_or_else(|| BackupError::Corrupt("missing snapshot".to_string()))?;
    let snapshot: CanonicalExport = serde_json::from_value(snapshot_value.clone())
        .map_err(|e| BackupError::Corrupt(e.to_string()))?;
    let actual = snapshot.digest();
    if actual != expected {
        return Err(BackupError::DigestMismatch {
            expected: expected.to_string(),
            actual,
        });
    }
    // Manifest counts must describe the snapshot they ride with.
    let mut counts = std::collections::BTreeMap::new();
    for (key, len) in [
        ("memories", snapshot.memories.len()),
        ("relations", snapshot.relations.len()),
        ("guides", snapshot.guides.len()),
        ("sessions", snapshot.sessions.len()),
        ("feedback", snapshot.feedback.len()),
        ("suggestions", snapshot.suggestions.len()),
        ("projects", snapshot.projects.len()),
        ("archives", snapshot.archives.len()),
        ("history", snapshot.history.len()),
    ] {
        let declared = manifest
            .get(key)
            .and_then(|n| n.as_u64())
            .ok_or_else(|| BackupError::Corrupt(format!("missing manifest.{key}")))?;
        if declared != len as u64 {
            return Err(BackupError::Corrupt(format!(
                "manifest count mismatch for {key}: declares {declared}, holds {len}"
            )));
        }
        counts.insert(key.to_string(), declared);
    }
    let store_generation = v
        .get("store_generation")
        .and_then(|g| g.as_u64())
        .unwrap_or(0);
    // Loss accounting: unknown snapshot keys (flatten-captured) plus any
    // unrecognized envelope keys are counted, never dropped silently.
    let mut unknown_top_level = snapshot.unknown_fields.len();
    if let Some(obj) = v.as_object() {
        unknown_top_level += obj
            .keys()
            .filter(|k| {
                ![
                    "format",
                    "format_version",
                    "ltmrs_version",
                    "created_at",
                    "store_generation",
                    "manifest",
                    "snapshot",
                ]
                .contains(&k.as_str())
            })
            .count();
    }
    Ok(VerifiedBackup {
        digest: expected.to_string(),
        store_generation,
        counts,
        snapshot,
        unknown_top_level,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use ltmrs_domain::clock::FrozenClock;
    use ltmrs_domain::command::{CommandContext, DomainCommand, Scope};
    use ltmrs_domain::id::{
        ChannelId, EntityId, FrontendId, OperationId, SessionHandle, StoreGeneration,
    };
    use ltmrs_domain::memory::{FragmentType, Instant, Memory, MemoryLifecycle, MemorySource};
    use ltmrs_domain::relation::{Relation, RelationType};
    use ltmrs_domain::session::SessionOp;

    fn ids() -> (FrontendId, ChannelId) {
        (
            FrontendId::new(uuid::Uuid::from_u128(1)),
            ChannelId::new(uuid::Uuid::from_u128(2)),
        )
    }

    /// Operation scope for the fixture namespace (frontend 1 / channel 2 /
    /// epoch 1): op ids derive deterministically from their strings.
    fn fixture_scope(op: &str, digest: &str) -> ltmrs_domain::command::OperationScope {
        ltmrs_domain::command::OperationScope {
            store_generation: ltmrs_domain::id::StoreGeneration::FIRST,
            frontend_id: ltmrs_domain::id::FrontendId::new(uuid::Uuid::from_u128(1)),
            channel_id: ltmrs_domain::id::ChannelId::new(uuid::Uuid::from_u128(2)),
            retry_epoch: 1,
            operation_id: ltmrs_domain::id::OperationId::new(uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_URL,
                op.as_bytes(),
            )),
            request_digest: digest.to_string(),
        }
    }

    /// Repo-only fixture setup (no dispatcher): frozen clock plus an issued
    /// namespace so `apply` writes receipts like production (replicates the
    /// `repo_with_ns` pattern locally; test code is never imported across
    /// crates).
    fn setup() -> (
        tempfile::TempDir,
        Arc<ltmrs_service::repository::CanonicalRepository>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(FrozenClock::new(1000));
        let repo = Arc::new(
            ltmrs_service::repository::CanonicalRepository::open_with_clock(
                dir.path().join("store").to_str().unwrap(),
                clock,
            )
            .unwrap(),
        );
        let (fe, ch) = ids();
        let ns = repo.issue_namespace(fe, ch, 1000).unwrap();
        assert_eq!(ns.retry_epoch, 1, "first namespace is epoch 1");
        (dir, repo)
    }

    fn ctx(op_num: u64) -> CommandContext {
        let (fe, ch) = ids();
        CommandContext {
            store_generation: StoreGeneration::FIRST,
            frontend_id: fe,
            channel_id: ch,
            session: None,
            operation_id: OperationId::new(uuid::Uuid::from_u128(op_num as u128)),
            request_digest: format!("backup-test-{op_num}"),
            deadline_millis: None,
            scope: Scope::default(),
            retry_epoch: 1,
        }
    }

    fn fixture_memory(id_num: u128, title: &str, fragment: &str) -> Memory {
        use ltmrs_domain::id::{DocumentRevision, EligibilityRevision, EntityRevision};
        Memory {
            id: EntityId::new(uuid::Uuid::from_u128(id_num)),
            external_alias: None,
            title: title.into(),
            fragment: fragment.into(),
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

    /// Seed two memories, the explicit supports edge, the add-time auto-link
    /// stand-in (the dispatcher linked Beta→Alpha `RelatedTo` on topic
    /// overlap; reproduced here as an explicit edge) and one guide — all via
    /// `repo` calls, no dispatcher. Every manifest assertion below is
    /// unchanged (memories 2, guides 1, relations 2).
    fn seed_alpha_beta(repo: &ltmrs_service::repository::CanonicalRepository) {
        let alpha = fixture_memory(
            101,
            "Backup Alpha",
            "## Backup Alpha\n\n### Context\nFirst backup fixture fragment.",
        );
        let beta = fixture_memory(
            102,
            "Backup Beta",
            "## Backup Beta\n\n### Context\nSecond backup fixture fragment.",
        );
        repo.apply(
            &ctx(10),
            &DomainCommand::AddMemory {
                memory: alpha,
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
        repo.apply(
            &ctx(11),
            &DomainCommand::AddMemory {
                memory: beta,
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
        let ida = EntityId::new(uuid::Uuid::from_u128(101));
        let idb = EntityId::new(uuid::Uuid::from_u128(102));
        repo.apply(
            &ctx(12),
            &DomainCommand::Relate {
                relation: Relation::new(
                    EntityId::new(uuid::Uuid::from_u128(201)),
                    ida,
                    idb,
                    RelationType::Supports,
                    None,
                    Instant::new(1000),
                ),
            },
        )
        .unwrap();
        repo.apply(
            &ctx(13),
            &DomainCommand::Relate {
                relation: Relation::new(
                    EntityId::new(uuid::Uuid::from_u128(202)),
                    idb,
                    ida,
                    RelationType::RelatedTo,
                    Some("Auto-linked: topic overlap".to_string()),
                    Instant::new(1000),
                ),
            },
        )
        .unwrap();
        repo.practice_guide_idempotent(
            &repo
                .admit_scope(&fixture_scope("op-14", "digest-14"))
                .unwrap(),
            "backup-guide",
            "test",
            Some("Backup fixture guide."),
            &[],
            &[],
            &[],
            None,
            1000,
        )
        .unwrap();
    }

    /// Start one traced session on the repo (was a `SessionStart` tool call
    /// through the dispatcher; the canonical session it created is the single
    /// unit of session backup cargo).
    fn start_traced_session(repo: &ltmrs_service::repository::CanonicalRepository) {
        let handle = SessionHandle::new(uuid::Uuid::from_u128(300));
        match repo
            .session_start_tx(
                &fixture_scope("op-20", "digest-20"),
                handle,
                None,
                Some("backup".to_string()),
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

    /// Export writes a `.ltmrs-backup` file whose manifest counts and digest
    /// match the snapshot it carries.
    #[test]
    fn export_roundtrips_with_manifest() {
        let (dir, repo) = setup();
        seed_alpha_beta(&repo);
        // One live session rides the envelope (canonical session history).
        start_traced_session(&repo);
        // One traced session rides along (canonical store, now read from
        // the same snapshot as everything else). Repo-only seeding creates
        // no routing-ephemeral sessions at all: session-less `apply` calls
        // carry no session attribution worth backing up.
        assert_eq!(repo.all_sessions().unwrap().len(), 1);
        let out = dir.path().join("backups");
        let report = export_backup(&repo, &out, "test", 1700000000000).unwrap();
        assert_eq!(report.path.extension().unwrap(), BACKUP_EXTENSION);
        assert!(report.path.starts_with(&out), "dir created + used");
        let raw = std::fs::read(&report.path).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["format"], BACKUP_FORMAT);
        assert_eq!(v["format_version"], BACKUP_FORMAT_VERSION);
        assert_eq!(v["manifest"]["memories"], 2);
        assert_eq!(v["manifest"]["guides"], 1);
        // Only the traced session is backup cargo (repo-only seeding
        // creates no routing-ephemeral sessions).
        assert_eq!(v["manifest"]["sessions"], 1);
        // Both the explicit edge and the add-time auto-link are captured.
        assert_eq!(v["manifest"]["relations"], 2);
        let snapshot: CanonicalExport = serde_json::from_value(v["snapshot"].clone()).unwrap();
        assert_eq!(v["manifest"]["digest"], snapshot.digest());
        assert_eq!(report.digest, snapshot.digest());
    }

    /// Same content, different timestamp: the snapshot digest is stable
    /// (idempotent re-export where promised).
    #[test]
    fn export_snapshot_digest_stable() {
        let (dir, repo) = setup();
        seed_alpha_beta(&repo);
        let out = dir.path().join("backups");
        let first = export_backup(&repo, &out, "test", 1700000000000).unwrap();
        let second = export_backup(&repo, &out, "test", 1700000000001).unwrap();
        assert_ne!(first.path, second.path, "unique filenames");
        assert_eq!(first.digest, second.digest, "stable snapshot digest");
    }

    /// Oversize files are refused before reading them fully.
    #[test]
    fn verify_rejects_oversize() {
        let (dir, repo) = setup();
        seed_alpha_beta(&repo);
        let report = export_backup(&repo, dir.path(), "test", 1700000000000).unwrap();
        let err = verify_backup_file(&report.path, 10).unwrap_err();
        assert!(matches!(err, BackupError::TooLarge { .. }), "got: {err}");
    }

    /// A single flipped content byte breaks the digest (not the parse).
    #[test]
    fn verify_detects_corruption() {
        let (dir, repo) = setup();
        seed_alpha_beta(&repo);
        let report = export_backup(&repo, dir.path(), "test", 1700000000000).unwrap();
        let mut raw = std::fs::read(&report.path).unwrap();
        let needle = b"Backup Alpha";
        let pos = raw
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("fixture text present");
        raw[pos + 7] = b'X';
        let tampered = dir.path().join("tampered.ltmrs-backup");
        std::fs::write(&tampered, &raw).unwrap();
        let err = verify_backup_file(&tampered, MAX_BACKUP_BYTES).unwrap_err();
        assert!(
            matches!(err, BackupError::DigestMismatch { .. }),
            "digest must catch it, got: {err}"
        );
    }

    /// Foreign formats and versions fail explicitly (never migrated silently).
    #[test]
    fn verify_rejects_foreign_format() {
        let dir = tempfile::tempdir().unwrap();
        let foreign = dir.path().join("legacy.lemma-backup");
        std::fs::write(
            &foreign,
            r#"{"format":"lemma-backup","format_version":1,"database_sha256":"abc","database":"e30="}"#,
        )
        .unwrap();
        let err = verify_backup_file(&foreign, MAX_BACKUP_BYTES).unwrap_err();
        // Legacy envelopes take the dedicated refusal (see legacy.rs), not
        // the generic format error.
        assert!(
            matches!(err, BackupError::LegacyUnsupported { .. }),
            "got: {err}"
        );
        let future = dir.path().join("future.ltmrs-backup");
        std::fs::write(
            &future,
            r#"{"format":"ltmrs-backup","format_version":99,"manifest":{},"snapshot":{}}"#,
        )
        .unwrap();
        let err = verify_backup_file(&future, MAX_BACKUP_BYTES).unwrap_err();
        assert!(matches!(err, BackupError::Version(99)), "got: {err}");
    }

    /// Published backups are owner-private from creation (never umask-dependent).
    #[cfg(unix)]
    #[test]
    fn export_sets_owner_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, repo) = setup();
        seed_alpha_beta(&repo);
        let report = export_backup(&repo, dir.path(), "test", 1700000000000).unwrap();
        let mode = std::fs::metadata(&report.path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "backup must be owner-private, got {mode:o}"
        );
    }

    /// Fail-closed publish (restore safety): when the destination cannot
    /// even be created, NO artifact (final or staged) is left behind and
    /// the error names the failing path — a failed safety backup must
    /// never look like a usable rollback source.
    #[test]
    fn publish_through_file_parent_is_refused_without_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let final_path = blocker.join("safety.ltmrs-backup");
        let staged = final_path.with_extension(BACKUP_EXTENSION.to_string() + ".tmp");
        let err = export_backup_to_with_limit(&final_path, b"{}", MAX_BACKUP_BYTES).unwrap_err();
        assert!(
            err.to_string().contains("blocker"),
            "error must name the failing path, got: {err}"
        );
        assert!(!final_path.exists(), "no final artifact on failure");
        assert!(!staged.exists(), "no staged artifact on failure");
    }

    /// The byte bound is configurable: explicit positive values win, unset /
    /// empty / garbage / zero fall back to the hostile-input default.
    #[test]
    fn byte_limit_override_is_honored() {
        assert_eq!(backup_byte_limit_from(Some("1048576".to_string())), 1048576);
        assert_eq!(backup_byte_limit_from(None), MAX_BACKUP_BYTES);
        assert_eq!(
            backup_byte_limit_from(Some(String::new())),
            MAX_BACKUP_BYTES
        );
        assert_eq!(
            backup_byte_limit_from(Some("  ".to_string())),
            MAX_BACKUP_BYTES
        );
        assert_eq!(
            backup_byte_limit_from(Some("forever".to_string())),
            MAX_BACKUP_BYTES
        );
        assert_eq!(
            backup_byte_limit_from(Some("0".to_string())),
            MAX_BACKUP_BYTES
        );
    }

    /// Encode honors a custom bound end to end (encode + publish + verify).
    #[test]
    fn custom_limit_roundtrips_and_rejects() {
        let (dir, repo) = setup();
        seed_alpha_beta(&repo);
        let export = repo.export_full().unwrap();
        let err = encode_backup_with_limit(&export, 1, 1700000000000, 10).unwrap_err();
        assert!(
            matches!(err, BackupError::TooLarge { bound: 10, .. }),
            "got: {err}"
        );
        let report =
            export_backup_with_limit(&repo, dir.path(), "test", 1700000000000, MAX_BACKUP_BYTES)
                .unwrap();
        verify_backup_file(&report.path, MAX_BACKUP_BYTES).unwrap();
    }
}
