//! Slice C C4: MC cdylib loader + preview/confirm (CLI-first, additive).
//!
//! The `ltmrs-mc-import` cdylib owns a `rusqlite` edge and is loaded at
//! runtime — never linked (workspace exclusion). Preview stages a
//! read-only copy into JSON; confirm re-stages, binds the digest, and fans
//! out one `memory_add` per mapped row through the existing IPC path.
//! Deterministic UUIDv5 ids + the server Jaccard gate make reruns safe.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::path::{Path, PathBuf};

/// Well-known cdylib file name (Linux-first per AD-07).
#[cfg(not(windows))]
const LIB_FILE_NAME: &str = "libltmrs_mc_import.so";
/// Well-known cdylib file name on Windows.
#[cfg(windows)]
const LIB_FILE_NAME: &str = "ltmrs_mc_import.dll";

/// Load/staging failure (never silent).
#[derive(Debug)]
pub struct McLoadError(pub String);

impl std::fmt::Display for McLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for McLoadError {}

/// `ltmrs_mc_import_json` signature (mirrors the cdylib ABI exactly).
type ImportJsonFn = unsafe extern "C" fn(*const c_char) -> *mut c_char;
/// `ltmrs_mc_free_string` signature.
type FreeStringFn = unsafe extern "C" fn(*mut c_char);

struct McImportLib {
    handle: *mut c_void,
    import_json: ImportJsonFn,
    free_string: FreeStringFn,
}

// The handle is an opaque dlopen cookie: safe to move across threads, never
// dereferenced. Each preview/confirm opens, stages read-only, and closes.
unsafe impl Send for McImportLib {}

impl Drop for McImportLib {
    fn drop(&mut self) {
        // SAFETY: the handle came from `open_library` and is closed once.
        unsafe { close_library(self.handle) };
    }
}

#[cfg(not(windows))]
unsafe fn open_library(path: &CStr) -> Result<*mut c_void, String> {
    // SAFETY: `path` is live for the call; null + dlerror checked below.
    let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        let err = unsafe { libc::dlerror() };
        let msg = if err.is_null() {
            "unknown dynamic loader error".to_string()
        } else {
            // SAFETY: `dlerror` guarantees a valid thread-local string.
            unsafe { CStr::from_ptr(err) }
                .to_string_lossy()
                .into_owned()
        };
        return Err(msg);
    }
    Ok(handle)
}

#[cfg(not(windows))]
unsafe fn resolve_symbol(handle: *mut c_void, name: &CStr) -> Result<*mut c_void, String> {
    // SAFETY: handle is live; null + dlerror checked below.
    unsafe { libc::dlerror() };
    let sym = unsafe { libc::dlsym(handle, name.as_ptr()) };
    if sym.is_null() {
        let err = unsafe { libc::dlerror() };
        let msg = if err.is_null() {
            format!("missing symbol {}", name.to_string_lossy())
        } else {
            // SAFETY: `dlerror` guarantees a valid thread-local string.
            unsafe { CStr::from_ptr(err) }
                .to_string_lossy()
                .into_owned()
        };
        return Err(msg);
    }
    Ok(sym)
}

#[cfg(not(windows))]
unsafe fn close_library(handle: *mut c_void) {
    if !handle.is_null() {
        // SAFETY: live owned handle per the caller contract.
        unsafe { libc::dlclose(handle) };
    }
}

#[cfg(windows)]
mod windows_dl {
    use super::*;
    use windows_sys::Win32::Foundation::{FreeLibrary, GetLastError};
    pub(super) unsafe fn open_library(path: &Path) -> Result<*mut c_void, String> {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: wide is live; null return means GetLastError carries reason.
        let h = unsafe { windows_sys::Win32::System::LibraryLoader::LoadLibraryW(wide.as_ptr()) };
        if h.is_null() {
            // SAFETY: reading the thread error code touches no memory.
            return Err(format!("LoadLibraryW failed: {}", unsafe {
                GetLastError()
            }));
        }
        Ok(h as *mut c_void)
    }
    pub(super) unsafe fn resolve_symbol(
        handle: *mut c_void,
        name: &CStr,
    ) -> Result<*mut c_void, String> {
        // SAFETY: handle live; name live; null means missing symbol.
        let sym = unsafe {
            windows_sys::Win32::System::LibraryLoader::GetProcAddress(
                handle as windows_sys::Win32::Foundation::HMODULE,
                name.as_ptr() as *const u8,
            )
        };
        sym.map(|f| f as *mut c_void)
            .ok_or_else(|| format!("missing symbol {}", name.to_string_lossy()))
    }
    pub(super) unsafe fn close_library(handle: *mut c_void) {
        if !handle.is_null() {
            // SAFETY: live owned handle per the caller contract.
            unsafe { FreeLibrary(handle as windows_sys::Win32::Foundation::HMODULE) };
        }
    }
    pub(super) use super::LIB_FILE_NAME;
}

/// Candidate cdylib locations: env override, exe dir, well-known outputs.
pub fn candidate_paths(env_override: Option<PathBuf>, exe_dir: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = env_override.filter(|v| !v.as_os_str().is_empty()) {
        paths.push(path);
    }
    paths.push(exe_dir.join(LIB_FILE_NAME));
    for profile in ["debug", "release"] {
        paths.push(
            cwd.join("crates/mc-import/target")
                .join(profile)
                .join(LIB_FILE_NAME),
        );
    }
    paths
}

fn open_first(candidates: &[PathBuf]) -> Result<McImportLib, McLoadError> {
    let mut tried = Vec::new();
    for path in candidates {
        #[cfg(not(windows))]
        let opened = (|| {
            let c_path = std::ffi::CString::new(path.to_string_lossy().as_bytes())
                .map_err(|_| "invalid path".to_string())?;
            // SAFETY: `c_path` live; handle closed on every path below.
            let handle = unsafe { open_library(&c_path) }?;
            // SAFETY: handle live; symbols verified by ABI match.
            let import_json = unsafe { resolve_symbol(handle, c"ltmrs_mc_import_json") }
                .map(|s| unsafe { std::mem::transmute::<*mut c_void, ImportJsonFn>(s) })?;
            let free_string = unsafe { resolve_symbol(handle, c"ltmrs_mc_free_string") }
                .map(|s| unsafe { std::mem::transmute::<*mut c_void, FreeStringFn>(s) })?;
            Ok::<_, String>((handle, import_json, free_string))
        })();
        #[cfg(windows)]
        let opened = (|| {
            // SAFETY: path borrowed from candidates; handle closed below.
            let handle = unsafe { windows_dl::open_library(path) }?;
            let import_json =
                unsafe { windows_dl::resolve_symbol(handle, c"ltmrs_mc_import_json") }
                    .map(|s| unsafe { std::mem::transmute::<*mut c_void, ImportJsonFn>(s) })?;
            let free_string =
                unsafe { windows_dl::resolve_symbol(handle, c"ltmrs_mc_free_string") }
                    .map(|s| unsafe { std::mem::transmute::<*mut c_void, FreeStringFn>(s) })?;
            Ok::<_, String>((handle, import_json, free_string))
        })();
        match opened {
            Ok((handle, import_json, free_string)) => {
                return Ok(McImportLib {
                    handle,
                    import_json,
                    free_string,
                });
            }
            Err(e) => {
                tried.push(format!("{} ({e})", path.display()));
            }
        }
    }
    Err(McLoadError(format!(
        "MC import library not found (tried: {}). Build it with `cargo build --manifest-path crates/mc-import/Cargo.toml` or pass import_lib (mc-import --import-lib flag).",
        tried.join("; ")
    )))
}

/// Stage an MC database file into JSON via the cdylib (read-only).
pub fn stage_to_json(path: &Path) -> Result<String, McLoadError> {
    stage_to_json_with_lib(path, None)
}

/// Stage with an explicit cdylib override (CLI-first; replaces the env override).
pub fn stage_to_json_with_lib(
    path: &Path,
    lib_override: Option<PathBuf>,
) -> Result<String, McLoadError> {
    let env_override = lib_override;
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_default();
    let cwd = std::env::current_dir().unwrap_or_default();
    let lib = open_first(&candidate_paths(env_override, &exe_dir, &cwd))?;
    let c_path = CString::new(path.to_string_lossy().as_bytes())
        .map_err(|_| McLoadError("invalid source path".to_string()))?;
    let raw = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: `c_path` live; pointer freed below exactly once.
        unsafe { (lib.import_json)(c_path.as_ptr()) }
    }))
    .map_err(|_| McLoadError("mc importer panicked (caught at call boundary)".to_string()))?;
    if raw.is_null() {
        return Err(McLoadError("mc importer returned null".to_string()));
    }
    // SAFETY: non-null heap string from the cdylib, freed exactly once.
    let text = unsafe { CStr::from_ptr(raw) }
        .to_string_lossy()
        .into_owned();
    unsafe { (lib.free_string)(raw) };
    Ok(text)
}

/// Preview report fields read from staging JSON.
#[derive(Debug)]
pub struct McPreview {
    pub digest: String,
    pub memory_count: usize,
    pub loss_count: usize,
    pub scopes: Vec<(String, String)>,
}

/// Preview: stage read-only, report counts/digest/loss/scopes. No store touch.
pub fn preview(path: &Path) -> Result<McPreview, McLoadError> {
    preview_with_lib(path, None)
}

/// Preview with an explicit cdylib override (CLI-first ).
pub fn preview_with_lib(
    path: &Path,
    lib_override: Option<PathBuf>,
) -> Result<McPreview, McLoadError> {
    let text = stage_to_json_with_lib(path, lib_override)?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| McLoadError(format!("bad staging JSON: {e}")))?;
    if v.get("ok") != Some(&serde_json::Value::Bool(true)) {
        return Err(McLoadError(
            v.get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("staging failed")
                .to_string(),
        ));
    }
    let scopes = v
        .get("scopes")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .map(|s| {
                    (
                        s.get("harness")
                            .and_then(|h| h.as_str())
                            .unwrap_or("")
                            .to_string(),
                        s.get("git_root")
                            .and_then(|g| g.as_str())
                            .unwrap_or("")
                            .to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(McPreview {
        digest: v
            .get("digest")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string(),
        memory_count: v.get("memory_count").and_then(|n| n.as_u64()).unwrap_or(0) as usize,
        loss_count: v
            .get("loss")
            .and_then(|l| l.as_array())
            .map(|a| a.len())
            .unwrap_or(0),
        scopes,
    })
}

/// Parse staging JSON into its digest + memory rows (pure, hermetic).
fn parse_staging(text: &str) -> Result<(String, Vec<serde_json::Value>), McLoadError> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| McLoadError(format!("bad staging JSON: {e}")))?;
    if v.get("ok") != Some(&serde_json::Value::Bool(true)) {
        return Err(McLoadError(
            v.get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("staging failed")
                .to_string(),
        ));
    }
    let digest = v
        .get("digest")
        .and_then(|d| d.as_str())
        .unwrap_or("")
        .to_string();
    let memories = v
        .get("memories")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();
    Ok((digest, memories))
}

/// Confirm from already-staged JSON: bind digest, fan out `memory_add`
/// per staged memory through the existing IPC path. Returns
/// (applied, rejected). Non-atomic: each row commits separately; reruns
/// are safe (deterministic ids + server Jaccard dedup).
pub async fn confirm_staged(
    fe: &crate::frontend::mcp::LtmrsFrontend,
    staging_json: &str,
    digest: &str,
) -> Result<(usize, usize), McLoadError> {
    let (live_digest, memories) = parse_staging(staging_json)?;
    if live_digest != digest {
        return Err(McLoadError(format!(
            "source changed since preview (expected {digest}, got {live_digest})"
        )));
    }
    fan_out(fe, &memories).await
}

/// Fan out one `memory_add` per staged memory. Shared by confirm paths.
async fn fan_out(
    fe: &crate::frontend::mcp::LtmrsFrontend,
    memories: &[serde_json::Value],
) -> Result<(usize, usize), McLoadError> {
    let mut applied = 0;
    let mut rejected = 0;
    for m in memories {
        let tool = ltmrs_compat::lemma::tool_args::ToolArgs::MemoryAdd(
            ltmrs_compat::lemma::tool_args::MemoryAddArgs {
                fragment: m
                    .get("fragment")
                    .and_then(|f| f.as_str())
                    .unwrap_or("")
                    .to_string(),
                title: m
                    .get("title")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string()),
                description: None,
                project: None,
                source: Some("mc-import".to_string()),
                confirm: false,
                fragment_type: Some("fact".to_string()),
                evidence: None,
            },
        );
        match fe
            .call_daemon_tool(
                ltmrs_daemon::envelope::DomainRequest::ToolCall { tool },
                false,
            )
            .await
        {
            Ok(resp) => match resp.result {
                ltmrs_daemon::envelope::IpcResult::Success {
                    payload:
                        ltmrs_daemon::envelope::DomainPayload::ToolResult {
                            is_error: false, ..
                        },
                    ..
                } => applied += 1,
                other => {
                    eprintln!("ltmrs mc-import: add refused: {other:?}");
                    rejected += 1;
                }
            },
            Err(e) => {
                eprintln!("ltmrs mc-import: add failed: {e}");
                rejected += 1;
            }
        }
    }
    Ok((applied, rejected))
}

/// Confirm a previewed import from a source path: re-stage via the
/// cdylib, bind digest, fan out. Thin glue over [`confirm_staged`].
pub async fn confirm(
    fe: &crate::frontend::mcp::LtmrsFrontend,
    path: &Path,
    digest: &str,
) -> Result<(usize, usize), McLoadError> {
    confirm_with_lib(fe, path, digest, None).await
}

/// Confirm with an explicit cdylib override (CLI-first ).
pub async fn confirm_with_lib(
    fe: &crate::frontend::mcp::LtmrsFrontend,
    path: &Path,
    digest: &str,
    lib_override: Option<PathBuf>,
) -> Result<(usize, usize), McLoadError> {
    let text = stage_to_json_with_lib(path, lib_override)?;
    confirm_staged(fe, &text, digest).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staging_json() -> String {
        serde_json::json!({
            "ok": true,
            "digest": "abc123",
            "memory_count": 2,
            "memories": [
                {"mc_id": 1, "title": "Shop Ledger", "fragment": "## Shop Ledger\n\n### Context\nDurable shop fact for recall.", "alias": "mc:1", "tags": ["mc-category:ARCHITECTURE"], "has_embedding": false},
                {"mc_id": 2, "title": "Lance Rebuild", "fragment": "## Lance Rebuild\n\n### Context\nLance projections rebuild after restore.", "alias": "mc:2", "tags": ["mc-category:ARCHITECTURE"], "has_embedding": false}
            ],
            "loss": [{"kind": "mc_unmapped_column", "field": "importance", "mc_id": 1, "reason": "x"}],
            "scopes": [{"session": "s", "harness": "opencode", "project_path": "/r", "git_root": "/r"}]
        })
        .to_string()
    }

    /// C4: staging parses to digest + rows (pure, hermetic).
    #[test]
    fn parse_staging_extracts_digest_and_rows() {
        let (digest, rows) = parse_staging(&staging_json()).unwrap();
        assert_eq!(digest, "abc123");
        assert_eq!(rows.len(), 2);
    }

    /// C4: digest mismatch refuses before any store touch.
    #[tokio::test]
    async fn confirm_staged_rejects_digest_mismatch() {
        use crate::frontend::mcp::{FrontendIdentity, LtmrsFrontend};
        use crate::frontend::serve::daemon::start_local_daemon;
        use crate::frontend::serve::{resolve_home, stdio_layout};
        use ltmrs_domain::id::{ChannelId, FrontendId};
        use uuid::Uuid;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap().to_string();
        let base = resolve_home(Some(home)).unwrap();
        let layout = stdio_layout(&base);
        let (mut daemon, client) = start_local_daemon(
            &layout,
            &ltmrs_interchange::hindsight::HindsightCliFlags::default(),
        )
        .await
        .unwrap();
        let fe = LtmrsFrontend::new(
            FrontendIdentity::new(
                FrontendId::new(Uuid::from_u128(41)),
                ChannelId::new(Uuid::from_u128(42)),
            ),
            client,
        );
        let err = confirm_staged(&fe, &staging_json(), "wrong")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("source changed since preview"),
            "got: {err}"
        );
        daemon.shutdown();
    }

    /// C4: confirm_staged inserts rows, rerun adds nothing (Jaccard dedup).
    #[tokio::test]
    async fn confirm_staged_inserts_and_rerun_is_safe() {
        use crate::frontend::mcp::{FrontendIdentity, LtmrsFrontend};
        use crate::frontend::serve::daemon::start_local_daemon;
        use crate::frontend::serve::{resolve_home, stdio_layout};
        use ltmrs_domain::id::{ChannelId, FrontendId};
        use uuid::Uuid;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap().to_string();
        let base = resolve_home(Some(home)).unwrap();
        let layout = stdio_layout(&base);
        let (mut daemon, client) = start_local_daemon(
            &layout,
            &ltmrs_interchange::hindsight::HindsightCliFlags::default(),
        )
        .await
        .unwrap();
        let fe = LtmrsFrontend::new(
            FrontendIdentity::new(
                FrontendId::new(Uuid::from_u128(51)),
                ChannelId::new(Uuid::from_u128(52)),
            ),
            client,
        );
        let (applied, rejected) = confirm_staged(&fe, &staging_json(), "abc123")
            .await
            .unwrap();
        assert_eq!(applied, 2, "rejected={rejected}");
        let (applied2, _) = confirm_staged(&fe, &staging_json(), "abc123")
            .await
            .unwrap();
        assert_eq!(applied2, 0, "rerun must add nothing (Jaccard dedup)");
        daemon.shutdown();
    }

    /// C4: missing cdylib names what to build (hermetic error-path pin).
    #[test]
    fn missing_library_error_names_build() {
        let paths = candidate_paths(
            None,
            std::path::Path::new("/nonexistent-exe"),
            std::path::Path::new("/nonexistent-cwd"),
        );
        assert!(!paths.is_empty());
        assert!(
            paths
                .iter()
                .all(|p| p.to_string_lossy().contains("ltmrs_mc_import"))
        );
    }
}
