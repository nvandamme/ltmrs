//! Lemma backup importer loading (Wave B5c).
//!
//! The `ltmrs-lemma-import` cdylib owns the only `rusqlite` edge and is
//! loaded at runtime — never linked. The daemon binary therefore never
//! gains a build-time dependency on SQLite: a missing library is an
//! explicit error telling the operator how to build it.

use std::ffi::CStr;
use std::os::raw::{c_char, c_void};
use std::path::{Path, PathBuf};

/// Well-known cdylib file name (Linux-first per AD-07).
const LIB_FILE_NAME: &str = "libltmrs_lemma_import.so";
/// Cargo package name of the importer (for the build-hint error).
const IMPORTER_PACKAGE: &str = "ltmrs-lemma-import";
/// Environment override for the cdylib path.
pub const LIB_ENV_OVERRIDE: &str = "LTMRS_LEMMA_IMPORT_LIB";

/// Load failure (missing library, missing symbol — never silent).
#[derive(Debug)]
pub struct LemmaImportError(pub String);

impl std::fmt::Display for LemmaImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for LemmaImportError {}

/// `ltmrs_lemma_import_json` signature (mirrors the cdylib ABI exactly).
type ImportJsonFn = unsafe extern "C" fn(*const c_char) -> *mut c_char;
/// `ltmrs_lemma_free_string` signature (mirrors the cdylib ABI exactly).
type FreeStringFn = unsafe extern "C" fn(*mut c_char);

/// An opened importer library: the handle plus the two resolved symbols.
/// `unsafe` is confined to construction and the import call; the handle
/// closes on drop.
pub struct LemmaImportLib {
    handle: *mut c_void,
    import_json: ImportJsonFn,
    free_string: FreeStringFn,
}

// The handle is an opaque dlopen cookie: safe to move across threads, never
// dereferenced. Calls into the cdylib need no caller fence: preview and
// confirm imports each open the library, run a read-only staging pass over
// a private temp-file copy of the image, and close the handle on return,
// so concurrent calls cannot share mutable state.
unsafe impl Send for LemmaImportLib {}

impl Drop for LemmaImportLib {
    fn drop(&mut self) {
        // SAFETY: the handle came from `open_library` and is closed once.
        unsafe { close_library(self.handle) };
    }
}

/// Last dynamic-loader error for this thread (null means no error text).
///
/// # Safety
/// Calls `dlerror`, which returns either null or a valid thread-local string.
unsafe fn last_dl_error() -> String {
    let err = unsafe { libc::dlerror() };
    if err.is_null() {
        return "unknown dynamic loader error".to_string();
    }
    // SAFETY: `dlerror` guarantees a valid NUL-terminated thread-local string.
    unsafe { CStr::from_ptr(err) }
        .to_string_lossy()
        .into_owned()
}

/// Open a library path (or the current process when `None`).
///
/// # Safety
/// `path` must be either `None` (current process handle) or a live
/// NUL-terminated path string. The caller owns the returned handle.
unsafe fn open_library(path: Option<&std::ffi::CString>) -> Result<*mut c_void, LemmaImportError> {
    // Clear any stale error so a later `dlerror` is attributable.
    unsafe { libc::dlerror() };
    let raw = path.map(|p| p.as_ptr()).unwrap_or(std::ptr::null());
    // SAFETY: `raw` is either null (current process) or a caller-live path;
    // flags are the strictest link mode (bind now, keep private).
    let handle = unsafe { libc::dlopen(raw, libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        // SAFETY: error text is read immediately after the failing call.
        Err(LemmaImportError(unsafe { last_dl_error() }))
    } else {
        Ok(handle)
    }
}

/// Resolve a symbol from an open handle.
///
/// # Safety
/// The handle must be live (from `open_library`, not yet closed) and `name`
/// a valid `CStr`.
unsafe fn resolve_symbol(
    handle: *mut c_void,
    name: &CStr,
) -> Result<*mut c_void, LemmaImportError> {
    // SAFETY: handle is live per the caller contract.
    unsafe { libc::dlerror() };
    let sym = unsafe { libc::dlsym(handle, name.as_ptr()) };
    if sym.is_null() {
        // SAFETY: error text is read immediately after the failing call.
        let detail = unsafe { last_dl_error() };
        Err(LemmaImportError(format!(
            "symbol {} not found: {detail}",
            name.to_string_lossy()
        )))
    } else {
        Ok(sym)
    }
}

/// Close a handle from `open_library`.
///
/// # Safety
/// The handle must be live and closed exactly once.
unsafe fn close_library(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    // SAFETY: live handle per the caller contract; the return code carries
    // no action (a failed close leaves nothing to clean up).
    unsafe { libc::dlclose(handle) };
}

/// Candidate cdylib locations: the env override first, then next to the
/// daemon executable, then the well-known build outputs. Pure in the
/// override so tests never mutate process state.
pub fn candidate_paths(env_override: Option<PathBuf>, exe_dir: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = env_override.filter(|v| !v.as_os_str().is_empty()) {
        paths.push(path);
    }
    paths.push(exe_dir.join(LIB_FILE_NAME));
    for profile in ["debug", "release"] {
        paths.push(
            cwd.join("crates/lemma-import/target")
                .join(profile)
                .join(LIB_FILE_NAME),
        );
    }
    paths
}

/// Open the first loadable candidate, resolving both ABI symbols. Every
/// failure names what was tried and how to build the library — never a
/// silent fallback, never a build-time dependency.
pub fn open_first(candidates: &[PathBuf]) -> Result<LemmaImportLib, LemmaImportError> {
    let mut tried = Vec::new();
    for path in candidates {
        let c_path = match std::ffi::CString::new(path.to_string_lossy().as_bytes()) {
            Ok(p) => p,
            Err(_) => {
                tried.push(format!("{} (invalid path)", path.display()));
                continue;
            }
        };
        // SAFETY: `c_path` is live for the call; the handle is closed on
        // every path below (success moves it into `LemmaImportLib`, failure
        // closes it before trying the next candidate).
        let handle = match unsafe { open_library(Some(&c_path)) } {
            Ok(h) => h,
            Err(e) => {
                tried.push(format!("{} ({e})", path.display()));
                continue;
            }
        };
        // SAFETY: the handle is live (just opened, not yet closed).
        let import_json = match unsafe { resolve_symbol(handle, c"ltmrs_lemma_import_json") } {
            // SAFETY: the cdylib exports this symbol with the `ImportJsonFn`
            // signature (verified by the importer's ABI tests + .so smoke).
            Ok(sym) => unsafe { std::mem::transmute::<*mut c_void, ImportJsonFn>(sym) },
            Err(e) => {
                unsafe { close_library(handle) };
                tried.push(format!("{} ({e})", path.display()));
                continue;
            }
        };
        // SAFETY: same handle and provenance argument as above.
        let free_string = match unsafe { resolve_symbol(handle, c"ltmrs_lemma_free_string") } {
            // SAFETY: same provenance argument as above.
            Ok(sym) => unsafe { std::mem::transmute::<*mut c_void, FreeStringFn>(sym) },
            Err(e) => {
                unsafe { close_library(handle) };
                tried.push(format!("{} ({e})", path.display()));
                continue;
            }
        };
        return Ok(LemmaImportLib {
            handle,
            import_json,
            free_string,
        });
    }
    Err(LemmaImportError(format!(
        "Lemma import library not found (tried: {}). Build it with `cargo build -p {IMPORTER_PACKAGE}` or set {LIB_ENV_OVERRIDE} to the built .so path.",
        tried.join("; ")
    )))
}

/// Default candidates from the live process layout (exe dir + cwd).
pub fn default_candidates() -> Vec<PathBuf> {
    let env_override = std::env::var_os(LIB_ENV_OVERRIDE).map(PathBuf::from);
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_default();
    let cwd = std::env::current_dir().unwrap_or_default();
    candidate_paths(env_override, &exe_dir, &cwd)
}

impl LemmaImportLib {
    /// Open with the default candidates.
    pub fn open_default() -> Result<Self, LemmaImportError> {
        open_first(&default_candidates())
    }
    /// Import a `.lemma-backup` file through the cdylib into staging JSON.
    /// The panic boundary is caught here as well (belt over the cdylib's
    /// own containment): a failure is an error, never unwound.
    pub fn import_json(&self, backup_path: &Path) -> Result<String, LemmaImportError> {
        let c_path = std::ffi::CString::new(backup_path.to_string_lossy().as_bytes())
            .map_err(|_| LemmaImportError("invalid backup path".to_string()))?;
        let raw = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: `c_path` is live; the returned pointer is freed below
            // with the matching free function exactly once.
            unsafe { (self.import_json)(c_path.as_ptr()) }
        }))
        .map_err(|_| {
            LemmaImportError("lemma importer panicked (caught at call boundary)".to_string())
        })?;
        if raw.is_null() {
            return Err(LemmaImportError("lemma importer returned null".to_string()));
        }
        // SAFETY: non-null heap string from the cdylib, freed exactly once.
        let text = unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned();
        unsafe { (self.free_string)(raw) };
        Ok(text)
    }
}

/// One counted importer caveat (mirrors the cdylib loss schema).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StagingLoss {
    pub kind: String,
    pub count: u64,
    pub detail: String,
}

/// Parsed staging: the snapshot plus its verified manifest shell.
#[derive(Debug)]
pub struct ParsedStaging {
    pub snapshot: ltmrs_domain::export::CanonicalExport,
    pub verified: ltmrs_interchange::backup::VerifiedBackup,
    pub loss: Vec<StagingLoss>,
    pub lemma_version: String,
    pub schema_version: u64,
}

/// Parse cdylib staging JSON into the interchange snapshot shape.
/// Trust-but-verify: the declared digest must equal the snapshot digest, so
/// a tampered or mismatched staging can never become a backup.
pub fn parse_staging(text: &str) -> Result<ParsedStaging, LemmaImportError> {
    let fail = |m: &str| LemmaImportError(m.to_string());
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| fail(&format!("invalid staging JSON: {e}")))?;
    if v.get("ok") != Some(&serde_json::Value::Bool(true)) {
        let detail = v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("importer reported failure");
        return Err(fail(&format!("lemma import failed: {detail}")));
    }
    let snapshot: ltmrs_domain::export::CanonicalExport = serde_json::from_value(
        v.get("snapshot")
            .cloned()
            .ok_or_else(|| fail("staging is missing its snapshot"))?,
    )
    .map_err(|e| fail(&format!("staging snapshot is not interchange-shaped: {e}")))?;
    let declared = v
        .get("digest")
        .and_then(|d| d.as_str())
        .ok_or_else(|| fail("staging is missing its digest"))?;
    let actual = snapshot.digest();
    if actual != declared {
        return Err(fail(&format!(
            "staging digest mismatch (declared {declared}, snapshot holds {actual})"
        )));
    }
    let counts: std::collections::BTreeMap<String, u64> = v
        .get("counts")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| fail(&format!("staging counts are malformed: {e}")))?
        .unwrap_or_default();
    let loss: Vec<StagingLoss> = v
        .get("loss")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| fail(&format!("staging loss report is malformed: {e}")))?
        .unwrap_or_default();
    let verified = ltmrs_interchange::backup::VerifiedBackup {
        digest: actual,
        store_generation: 0,
        counts,
        snapshot,
        // Total counted loss items (per-kind counts summed), not the number
        // of distinct kinds.
        unknown_top_level: loss.iter().map(|l| l.count as usize).sum(),
    };
    let snapshot_ref = &verified.snapshot;
    Ok(ParsedStaging {
        snapshot: snapshot_ref.clone(),
        verified,
        loss,
        lemma_version: v
            .get("lemma_version")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown")
            .to_string(),
        schema_version: v
            .get("schema_version")
            .and_then(|n| n.as_u64())
            .unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing library fails loudly with the exact build command.
    #[test]
    fn missing_library_error_names_build_command() {
        let err = match open_first(&[PathBuf::from("/nonexistent/libltmrs_lemma_import.so")]) {
            Err(e) => e,
            Ok(_) => panic!("must not open a nonexistent library"),
        };
        let text = err.to_string();
        assert!(
            text.contains("cargo build -p ltmrs-lemma-import"),
            "must tell how to build it, got: {text}"
        );
        assert!(
            text.contains(LIB_ENV_OVERRIDE),
            "must name the override, got: {text}"
        );
    }

    /// The dlopen wrapper mechanics work end to end (open self, resolve a
    /// ubiquitous symbol, close) without any importer artifact.
    #[test]
    fn dlopen_self_and_resolve_libc_symbol() {
        let lib = unsafe { open_library(None) }.unwrap();
        let sym = unsafe { resolve_symbol(lib, c"printf") }.unwrap();
        assert!(!sym.is_null());
        unsafe { close_library(lib) };
    }

    /// A missing symbol fails loudly instead of calling a null pointer.
    #[test]
    fn missing_symbol_is_loud() {
        let lib = unsafe { open_library(None) }.unwrap();
        let err = match unsafe { resolve_symbol(lib, c"ltmrs_no_such_symbol_xyz") } {
            Err(e) => e,
            Ok(_) => panic!("must not resolve a missing symbol"),
        };
        unsafe { close_library(lib) };
        assert!(
            err.to_string().contains("ltmrs_no_such_symbol_xyz"),
            "must name the symbol, got: {err}"
        );
    }

    /// The env override wins over the well-known build outputs.
    #[test]
    fn env_override_resolves_first() {
        let paths = candidate_paths(
            Some(PathBuf::from("/override/libltmrs_lemma_import.so")),
            Path::new("/exe"),
            Path::new("/cwd"),
        );
        assert_eq!(
            paths.first().unwrap(),
            &PathBuf::from("/override/libltmrs_lemma_import.so")
        );
        assert!(
            paths
                .iter()
                .any(|p| p.ends_with("target/debug/libltmrs_lemma_import.so"))
        );
    }

    /// Hand-written staging shaped like the cdylib output (hermetic: no .so).
    fn sample_memory() -> serde_json::Value {
        serde_json::json!({
            "id": "11111111-1111-1111-1111-111111111111",
            "external_alias": "m1",
            "title": "Staged One",
            "fragment": "## Staged One",
            "description": "",
            "fragment_type": "Fact",
            "project": "app",
            "project_id": null,
            "source": "Ai",
            "confidence": 0.8,
            "quality_score": null,
            "lifecycle": "Live",
            "tags": [],
            "associated_with": [],
            "relations": [],
            "parent_id": null,
            "child_ids": [],
            "session_id": null,
            "task_type": null,
            "related_guides": [],
            "evidence": [],
            "access_count": 0,
            "last_accessed_at": null,
            "positive_feedback": 0,
            "negative_feedback": 0,
            "negative_hits": 0,
            "refinement_count": 0,
            "distill_candidate": false,
            "entity_revision": 1,
            "document_revision": 1,
            "eligibility_revision": 1,
            "created_at": 1000,
            "updated_at": 1000,
            "raw_created": null,
            "unknown_fields": {}
        })
    }

    /// Hand-written staging shaped like the cdylib output (hermetic: no .so).
    fn sample_staging() -> serde_json::Value {
        serde_json::json!({
            "ok": true,
            "lemma_version": "0.21.0",
            "schema_version": 8,
            "digest": "RECOMPUTED",
            "counts": {"memories": 1, "relations": 0, "guides": 0, "sessions": 0, "feedback": 0, "suggestions": 0, "projects": 0, "archives": 0, "history": 0},
            "snapshot": {
                "memories": [sample_memory()],
                "relations": [],
                "guides": [],
                "sessions": [],
                "feedback": [],
                "suggestions": [],
                "projects": [],
                "archives": [],
                "history": []
            },
            "loss": [{"kind": "folded_guide_manual", "count": 2, "detail": "guide=\"g\""}]
        })
    }

    /// An error envelope from the cdylib never becomes a backup.
    #[test]
    fn staging_rejects_error_envelope() {
        let text = serde_json::json!({"ok": false, "error": "boom"}).to_string();
        let err = parse_staging(&text).unwrap_err();
        assert!(err.to_string().contains("boom"), "got: {err}");
    }

    /// A digest that does not match the snapshot is refused (trust-but-verify).
    #[test]
    fn staging_rejects_digest_mismatch() {
        let mut staging = sample_staging();
        // Fix the digest placeholder to something wrong.
        staging["digest"] = serde_json::json!("deadbeef");
        let err = parse_staging(&staging.to_string()).unwrap_err();
        assert!(err.to_string().contains("digest"), "got: {err}");
    }

    /// Hand-written staging parses with counts, loss and a verified digest.
    #[test]
    fn staging_parses_with_loss() {
        let mut staging = sample_staging();
        let snapshot: ltmrs_domain::export::CanonicalExport =
            serde_json::from_value(staging["snapshot"].clone()).unwrap();
        staging["digest"] = serde_json::json!(snapshot.digest());
        let parsed = parse_staging(&staging.to_string()).unwrap();
        assert_eq!(parsed.snapshot.memories.len(), 1);
        assert_eq!(parsed.loss.len(), 1);
        assert_eq!(parsed.verified.counts["memories"], 1);
        // unknown_top_level counts loss items, not distinct kinds.
        assert_eq!(parsed.verified.unknown_top_level, 2);
    }

    /// Staging applies through the fenced restore path (replace, not merge).
    #[test]
    fn staging_applies_via_restore() {
        use ltmrs_domain::id::StoreGeneration;
        let mut staging = sample_staging();
        let snapshot: ltmrs_domain::export::CanonicalExport =
            serde_json::from_value(staging["snapshot"].clone()).unwrap();
        staging["digest"] = serde_json::json!(snapshot.digest());
        let parsed = parse_staging(&staging.to_string()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let repo = ltmrs_service::repository::CanonicalRepository::open(
            dir.path().join("store").to_str().unwrap(),
        )
        .unwrap();
        let out = ltmrs_interchange::restore::restore_verified(
            &repo,
            &parsed.verified,
            StoreGeneration::new(2),
        )
        .unwrap();
        assert_eq!(out.restored["memories"], 1);
        let titles: Vec<String> = repo
            .export_full()
            .unwrap()
            .memories
            .iter()
            .map(|m| m.title.clone())
            .collect();
        assert_eq!(titles, vec!["Staged One".to_string()]);
        // The imported memory keeps its external alias for id resolution.
        assert_eq!(
            repo.resolve_id("m1").unwrap().as_uuid().to_string(),
            "11111111-1111-1111-1111-111111111111"
        );
    }
}
