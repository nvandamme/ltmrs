//! Isolated Magic Context SQLite importer (Slice C).
//!
//! This crate owns its own `rusqlite` edge and builds as a `cdylib` loaded
//! at runtime by the daemon. It never touches Fjall and is excluded from
//! the main workspace so `rusqlite` never enters the main dependency tree.
//!
//! MC schema facts (cortexkit/magic-context `storage-db.ts`):
//! - `LATEST_SUPPORTED_VERSION` fence value (upstream: 95 at inspection).
//! - Persisted version: `MAX(version)` from `schema_migrations`.
//! - Scope source: `session_projects(session_id, harness, project_path)`.
//! - Memories: `memories(project_path, category, content, normalized_hash,
//!   importance, scope, source_type, seen_count, ...)`.
//! - Embeddings: `memory_embeddings(memory_id, embedding BLOB, model_id)`.

use std::fmt;
use std::path::Path;

/// Highest Magic Context schema this importer reads.
/// Mirrors upstream `LATEST_SUPPORTED_VERSION` (95 at 2026-11-12 inspection).
pub const LATEST_SUPPORTED_VERSION: u64 = 95;

/// Import failure (IO, format, integrity — never silent, never a partial read).
#[derive(Debug)]
pub struct McImportError(String);

impl fmt::Display for McImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mc import failed: {}", self.0)
    }
}


impl std::error::Error for McImportError {}

pub mod map;
pub mod scope;

pub(crate) fn fail(m: &str) -> McImportError {
    McImportError(m.to_string())
}

/// Read the persisted MC schema version (`MAX(version)` from
/// `schema_migrations`; absent table means version 0, like upstream).
pub fn persisted_version(conn: &rusqlite::Connection) -> Result<u64, McImportError> {
    let has: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
            [],
            |r| r.get(0),
        )
        .map(|n: i64| n > 0)
        .map_err(|e| McImportError(format!("cannot inspect schema table: {e}")))?;
    if !has {
        return Ok(0);
    }
    let v: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |r| r.get(0),
    )
    .map_err(|e| McImportError(format!("cannot read schema version: {e}")))?;
Ok(v.max(0) as u64)
}

/// Fail closed on version skew: newer than supported refuses.
pub fn check_version(conn: &rusqlite::Connection) -> Result<u64, McImportError> {
    let v = persisted_version(conn)?;
    if v > LATEST_SUPPORTED_VERSION {
        return Err(McImportError(format!(
            "unsupported Magic Context schema v{v}; this importer reads up to v{LATEST_SUPPORTED_VERSION} (update ltmrs or the MC binary, then retry)"
        )));
    }
    Ok(v)
}

/// Open a private read-only copy of the MC database: copy to a tempfile,
/// open with read-only + query_only, assert the source is untouched.
/// Refuses WAL-mode sources (checkpoint first, like the lemma importer).
pub fn open_readonly_copy(src: &Path) -> Result<(tempfile::TempDir, rusqlite::Connection), McImportError> {
    let fail = |m: &str| McImportError(m.to_string());
    let meta_before = std::fs::metadata(src)
        .map_err(|e| fail(&format!("cannot stat {}: {e}", src.display())))?;
    if !meta_before.is_file() {
        return Err(fail("not a regular file"));
    }
    let dir = tempfile::tempdir().map_err(|e| fail(&format!("cannot make tempdir: {e}")))?;
    let dst = dir.path().join("mc-context.db");
    std::fs::copy(src, &dst).map_err(|e| fail(&format!("cannot copy source: {e}")))?;
    // Journal mode lives in bytes 18-19 of the SQLite header: 1=delete,
    // 2=WAL. A WAL-mode file refuses: un-checkpointed frames live outside
    // the copy, so reading it would silently drop recent rows.
    let head = std::fs::read(&dst).map_err(|e| fail(&format!("cannot read copy: {e}")))?;
    if head.len() >= 20 && head[18] == 2 && head[19] == 2 {
        return Err(fail(
            "source database is in WAL mode: checkpoint the live database and retry (copying a WAL file drops un-checkpointed rows)",
        ));
    }
    let conn = rusqlite::Connection::open_with_flags(
        &dst,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|e| fail(&format!("cannot open copy read-only: {e}")))?;
    conn.execute_batch("PRAGMA query_only=ON")
        .map_err(|e| fail(&format!("cannot enforce query_only: {e}")))?;
    check_version(&conn)?;
    let meta_after = std::fs::metadata(src)
        .map_err(|e| fail(&format!("cannot re-stat {}: {e}", src.display())))?;
    if meta_after.len() != meta_before.len() || meta_after.modified().ok() != meta_before.modified().ok() {
        return Err(fail("source changed during read"));
    }
    Ok((dir, conn))
}

// ---------------------------------------------------------------------------
// C ABI: the daemon/CLI loads this cdylib at runtime and calls this symbol.
// `unsafe` is confined to this boundary: argument validation, UTF-8
// restoration and panic containment. The returned string is heap-owned and
// freed exactly once with `ltmrs_mc_free_string`.
// ---------------------------------------------------------------------------

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

fn error_json(message: &str) -> String {
    serde_json::json!({"ok": false, "error": message}).to_string()
}

/// Stage an MC database file into loss-accounted JSON (read-only).
///
/// # Safety
/// `path` must be non-null and point to a valid NUL-terminated C string for
/// the duration of the call. The returned pointer is non-null, heap-owned and
/// must be freed exactly once with `ltmrs_mc_free_string`. Never panics
/// across the boundary: all failures become `{"ok":false}` JSON.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ltmrs_mc_import_json(path: *const c_char) -> *mut c_char {
    let out = std::panic::catch_unwind(|| {
        if path.is_null() {
            return error_json("mc import failed: null path");
        }
        // SAFETY: upheld by the caller contract above.
        let path_str = match unsafe { CStr::from_ptr(path) }.to_str() {
            Ok(s) => s,
            Err(_) => return error_json("mc import failed: path is not valid UTF-8"),
        };
        match stage_to_json(std::path::Path::new(path_str)) {
            Ok(staging) => staging,
            Err(e) => error_json(&e.to_string()),
        }
    })
    .unwrap_or_else(|_| error_json("mc importer panicked (caught at call boundary)"));
    match CString::new(out) {
        Ok(c) => c.into_raw(),
        Err(_) => CString::new(error_json("mc import failed: interior NUL in output"))
            .unwrap()
            .into_raw(),
    }
}

/// Free a string returned by [`ltmrs_mc_import_json`] (exactly once).
///
/// # Safety
/// `ptr` must be a non-null pointer previously returned by
/// `ltmrs_mc_import_json` and not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ltmrs_mc_free_string(ptr: *mut c_char) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: upheld by the caller contract above.
    let _ = unsafe { CString::from_raw(ptr) };
}

/// Stage JSON shape (mirrors the lemma importer loss schema loosely).
fn stage_to_json(path: &std::path::Path) -> Result<String, McImportError> {
    let (_dir, conn) = open_readonly_copy(path)?;
    check_version(&conn)?;
    let rows = crate::map::read_memories(&conn)?;
    let scopes = crate::scope::load_session_scopes(&conn)?;
    let mut memories = Vec::new();
    let mut loss: Vec<serde_json::Value> = Vec::new();
    let mut digest_src = String::new();
    for (m, emb) in &rows {
        let mapped = crate::map::map_memory(m, emb.clone())?;
        digest_src.push_str(&m.normalized_hash);
        digest_src.push(';');
        for l in &mapped.loss {
            loss.push(serde_json::json!({
                "kind": "mc_unmapped_column",
                "field": l.field,
                "mc_id": l.mc_id,
                "reason": l.reason,
            }));
        }
        memories.push(serde_json::json!({
            "mc_id": m.id,
            "title": mapped.memory.title,
            "fragment": mapped.memory.fragment,
            "alias": mapped.memory.external_alias.map(|a| a.as_str().to_string()),
            "tags": mapped.memory.tags,
            "has_embedding": mapped.embedding.is_some(),
        }));
    }
    let digest = {
        use sha2::Digest;
        sha2::Sha256::digest(digest_src.as_bytes()).iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    let scope_list: Vec<serde_json::Value> = scopes
        .iter()
        .map(|((s, h), sc)| {
            serde_json::json!({
                "session": s, "harness": h,
                "project_path": sc.project_path,
                "git_root": sc.git_root.display().to_string(),
            })
        })
        .collect();
    Ok(serde_json::json!({
        "ok": true,
        "digest": digest,
        "memory_count": rows.len(),
        "memories": memories,
        "loss": loss,
        "scopes": scope_list,
    })
    .to_string())
}
