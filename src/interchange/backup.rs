//! Native logical backup (WP-11a; T-BACKUP-01 export leg).
//!
//! Format `ltmrs-backup` v1: a JSON envelope holding one coherent canonical
//! snapshot (`CanonicalExport` from a single read transaction) plus a manifest
//! (per-collection counts + snapshot digest). Files end `.ltmrs-backup`
//! (never `.lemma-backup`: a native archive is never relabeled) and publish
//! atomically (tmp + rename). `backup_create` verifies by re-reading.

use std::path::{Path, PathBuf};

use crate::domain::export::CanonicalExport;
use crate::service::repository::CanonicalRepository;

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

/// Export one coherent snapshot of `repo` plus registry `sessions` into `dir`
/// (created when missing) as `<prefix>-<millis>-<v7>.ltmrs-backup`,
/// published atomically and verified by re-read before returning.
/// Sessions are registry-owned runtime state passed in by the caller
/// (the exec layer reads them from the dispatcher registry).
pub fn export_backup(
    repo: &CanonicalRepository,
    sessions: &[crate::domain::session::Session],
    dir: &Path,
    prefix: &str,
    created_at_millis: u64,
) -> Result<BackupReport, BackupError> {
    let io_err = |path: &Path, e: std::io::Error| BackupError::Io {
        path: path.to_string_lossy().into_owned(),
        message: e.to_string(),
    };
    // One coherent cut: the full domain export comes from a single read
    // transaction; registry sessions ride along verbatim.
    let (mut export, generation) = repo
        .export_full_with_generation()
        .map_err(|e| BackupError::Store(e.message))?;
    export.sessions = sessions.to_vec();
    let (bytes, _digest, _counts) = encode_backup(&export, generation.as_u64(), created_at_millis)?;
    std::fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
    let name = format!(
        "{prefix}-{created_at_millis}-{}.{}",
        uuid::Uuid::now_v7().as_simple(),
        BACKUP_EXTENSION
    );
    export_backup_to(&dir.join(&name), &bytes)
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
    if bytes.len() as u64 > MAX_BACKUP_BYTES {
        return Err(BackupError::TooLarge {
            size: bytes.len() as u64,
            bound: MAX_BACKUP_BYTES,
        });
    }
    Ok((bytes, digest, counts))
}

pub fn export_backup_to(final_path: &Path, bytes: &[u8]) -> Result<BackupReport, BackupError> {
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
    std::fs::write(&staged, bytes).map_err(|e| io_err(&staged, e))?;
    // Best-effort durability before publication (the verify below re-reads).
    if let Ok(f) = std::fs::File::open(&staged) {
        let _ = f.sync_all();
    }
    std::fs::rename(&staged, final_path).map_err(|e| io_err(final_path, e))?;
    // Verify by re-reading (a torn write fails here, never silently).
    let verified = verify_backup_file(final_path, MAX_BACKUP_BYTES)?;
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
        if format == Some(crate::interchange::legacy::LEGACY_FORMAT) {
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

    use crate::compatibility::lemma::tool_args::{
        GuideCreateArgs, MemoryAddArgs, MemoryRelateArgs, ToolArgs,
    };
    use crate::daemon::dispatcher::Dispatcher;
    use crate::daemon::envelope::{
        DomainPayload, DomainRequest, HandshakeRequest, IpcEnvelope, IpcResult, PROTOCOL_VERSION,
    };
    use crate::domain::command::Scope;
    use crate::domain::id::{ChannelId, FrontendId, OperationId, StoreGeneration};

    fn ids() -> (FrontendId, ChannelId) {
        (
            FrontendId::new(uuid::Uuid::from_u128(1)),
            ChannelId::new(uuid::Uuid::from_u128(2)),
        )
    }

    fn setup() -> (
        tempfile::TempDir,
        Arc<crate::service::repository::CanonicalRepository>,
        Dispatcher,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Arc::new(
            crate::service::repository::CanonicalRepository::open(
                dir.path().join("store").to_str().unwrap(),
            )
            .unwrap(),
        );
        let clock: Arc<dyn crate::domain::clock::Clock + Send + Sync> =
            Arc::new(crate::domain::clock::SystemClock);
        let disp = Dispatcher::new(
            Arc::clone(&repo),
            crate::daemon::registry::FrontendRegistry::new(),
            clock,
        );
        // Handshake once (public boundary, like a real frontend) and issue
        // the retry namespace the dispatcher requires on every frame.
        let (fe, ch) = ids();
        let hs = disp
            .handle_handshake(&HandshakeRequest {
                protocol_version: PROTOCOL_VERSION,
                store_generation: StoreGeneration::FIRST,
                frontend_id: fe,
                channel_id: ch,
            })
            .unwrap();
        assert!(hs.retry_epoch >= 1);
        repo.issue_namespace(fe, 1000).unwrap();
        (dir, repo, disp)
    }

    fn call(disp: &Dispatcher, op: u128, tool: ToolArgs) -> serde_json::Value {
        let (fe, ch) = ids();
        let env = IpcEnvelope {
            protocol_version: PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id: fe,
            channel_id: ch,
            operation_id: OperationId::new(uuid::Uuid::from_u128(op)),
            session: None,
            retry_epoch: 1,
            deadline_millis: None,
            scope: Scope::default(),
            body: DomainRequest::ToolCall { tool },
        };
        match disp.handle(&env).unwrap().result {
            IpcResult::Success {
                payload:
                    DomainPayload::ToolResult {
                        text,
                        structured,
                        is_error,
                    },
                ..
            } => {
                assert!(!is_error, "soft tool failure: {text}");
                structured.unwrap_or(serde_json::Value::Null)
            }
            IpcResult::Error { message, .. } => panic!("tool call failed: {message}"),
            other => panic!("unexpected result: {other:?}"),
        }
    }

    fn seed_alpha_beta(disp: &Dispatcher) -> (String, String) {
        let a = call(
            disp,
            10,
            ToolArgs::MemoryAdd(MemoryAddArgs {
                fragment: "## Backup Alpha\n\n### Context\nFirst backup fixture fragment."
                    .to_string(),
                ..Default::default()
            }),
        );
        let b = call(
            disp,
            11,
            ToolArgs::MemoryAdd(MemoryAddArgs {
                fragment: "## Backup Beta\n\n### Context\nSecond backup fixture fragment."
                    .to_string(),
                ..Default::default()
            }),
        );
        let ida = a["id"].as_str().unwrap().to_string();
        let idb = b["id"].as_str().unwrap().to_string();
        call(
            disp,
            12,
            ToolArgs::MemoryRelate(MemoryRelateArgs {
                source_id: ida.clone(),
                target_id: idb.clone(),
                relation_type: "supports".to_string(),
                note: None,
            }),
        );
        // GuideCreate needs the full 5 fields (no Default).
        let (fe, ch) = ids();
        let env = IpcEnvelope {
            protocol_version: PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id: fe,
            channel_id: ch,
            operation_id: OperationId::new(uuid::Uuid::from_u128(13)),
            session: None,
            retry_epoch: 1,
            deadline_millis: None,
            scope: Scope::default(),
            body: DomainRequest::ToolCall {
                tool: ToolArgs::GuideCreate(GuideCreateArgs {
                    guide: "backup-guide".to_string(),
                    category: "test".to_string(),
                    description: "Backup fixture guide.".to_string(),
                    contexts: vec![],
                    learnings: vec![],
                }),
            },
        };
        match disp.handle(&env).unwrap().result {
            IpcResult::Success {
                payload: DomainPayload::ToolResult { text, is_error, .. },
                ..
            } => assert!(!is_error, "guide create failed: {text}"),
            other => panic!("unexpected guide result: {other:?}"),
        }
        (ida, idb)
    }

    /// Export writes a `.ltmrs-backup` file whose manifest counts and digest
    /// match the snapshot it carries.
    #[test]
    fn export_roundtrips_with_manifest() {
        let (dir, repo, disp) = setup();
        let (_a, _b) = seed_alpha_beta(&disp);
        // One live session rides the envelope (registry-owned state).
        let (fe, ch) = ids();
        let start_env = IpcEnvelope {
            protocol_version: PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id: fe,
            channel_id: ch,
            operation_id: OperationId::new(uuid::Uuid::from_u128(20)),
            session: None,
            retry_epoch: 1,
            deadline_millis: None,
            scope: Scope::default(),
            body: DomainRequest::ToolCall {
                tool: ToolArgs::SessionStart(
                    crate::compatibility::lemma::tool_args::SessionStartArgs {
                        task_type: "backup".to_string(),
                        technologies: vec![],
                        initial_approach: None,
                    },
                ),
            },
        };
        let _ = disp.handle(&start_env).unwrap();
        // One traced session rides along (canonical store). The virtual
        // session ensured by session-less calls stays routing-ephemeral in
        // the registry: it carries no durable knowledge worth backing up
        // (and would typically be idle-expired by restore time anyway).
        let sessions = disp.repo().all_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        let out = dir.path().join("backups");
        let report = export_backup(&repo, &sessions, &out, "test", 1700000000000).unwrap();
        assert_eq!(report.path.extension().unwrap(), BACKUP_EXTENSION);
        assert!(report.path.starts_with(&out), "dir created + used");
        let raw = std::fs::read(&report.path).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["format"], BACKUP_FORMAT);
        assert_eq!(v["format_version"], BACKUP_FORMAT_VERSION);
        assert_eq!(v["manifest"]["memories"], 2);
        assert_eq!(v["manifest"]["guides"], 1);
        // Only the traced session is backup cargo now; the virtual one
        // stays routing-ephemeral (see above).
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
        let (dir, repo, disp) = setup();
        let (_a, _b) = seed_alpha_beta(&disp);
        let out = dir.path().join("backups");
        let first = export_backup(&repo, &[], &out, "test", 1700000000000).unwrap();
        let second = export_backup(&repo, &[], &out, "test", 1700000000001).unwrap();
        assert_ne!(first.path, second.path, "unique filenames");
        assert_eq!(first.digest, second.digest, "stable snapshot digest");
    }

    /// Oversize files are refused before reading them fully.
    #[test]
    fn verify_rejects_oversize() {
        let (dir, repo, disp) = setup();
        let (_a, _b) = seed_alpha_beta(&disp);
        let report = export_backup(&repo, &[], dir.path(), "test", 1700000000000).unwrap();
        let err = verify_backup_file(&report.path, 10).unwrap_err();
        assert!(matches!(err, BackupError::TooLarge { .. }), "got: {err}");
    }

    /// A single flipped content byte breaks the digest (not the parse).
    #[test]
    fn verify_detects_corruption() {
        let (dir, repo, disp) = setup();
        let (_a, _b) = seed_alpha_beta(&disp);
        let report = export_backup(&repo, &[], dir.path(), "test", 1700000000000).unwrap();
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
}
