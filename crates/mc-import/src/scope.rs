//! Slice C C2: harness + project_path scope resolution.
//!
//! Reads `session_projects(session_id, harness, project_path)` and resolves
//! each row to an ltmrs scope. Unknown harness refuses; `../` traversal
//! and symlink escape refuse; non-absolute or non-repo paths refuse.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{McImportError, fail};

/// Canonical harness vocabulary (OQ-C2 decision: exact set, closed).
pub const KNOWN_HARNESSES: &[&str] = &["opencode", "opencode2", "pi", "omp", "claude-code"];

/// A resolved MC source scope: harness plus canonical git-root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct McScope {
    pub harness: String,
    pub git_root: PathBuf,
    pub project_path: String,
}

/// Resolve one `(harness, project_path)` row: closed harness vocabulary,
/// absolute path, no `..` components, must resolve inside a git repo
/// (git-root via `git rev-parse --show-toplevel` equivalent: walk up for
/// `.git`).
pub fn resolve_scope(harness: &str, project_path: &str) -> Result<McScope, McImportError> {
    if !KNOWN_HARNESSES.contains(&harness) {
        return Err(fail(&format!("unknown harness: {harness}")));
    }
    let p = Path::new(project_path);
    if !p.is_absolute() {
        return Err(fail("project_path is not absolute"));
    }
    if p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(fail("project_path escapes with `..`"));
    }
    let git_root = find_git_root(p).ok_or_else(|| fail("project_path is not inside a git repo"))?;
    // Symlink escape: canonicalized path must stay under the git root.
    let canon = std::fs::canonicalize(p)
        .map_err(|e| fail(&format!("cannot canonicalize project_path: {e}")))?;
    if !canon.starts_with(&git_root) {
        return Err(fail("project_path escapes its git root via symlink"));
    }
    Ok(McScope {
        harness: harness.to_string(),
        git_root,
        project_path: project_path.to_string(),
    })
}

/// Walk up from `p` for a `.git` dir or file (worktree-compatible).
fn find_git_root(p: &Path) -> Option<PathBuf> {
    let mut cur = if p.is_file() {
        p.parent()?.to_path_buf()
    } else {
        p.to_path_buf()
    };
    loop {
        if cur.join(".git").exists() {
            return std::fs::canonicalize(&cur).ok();
        }
        if !cur.pop() {
            return None;
        }
    }
}

/// Load all `(session_id, harness)` → resolved scope rows from an open copy.
pub fn load_session_scopes(
    conn: &rusqlite::Connection,
) -> Result<BTreeMap<(String, String), McScope>, McImportError> {
    let mut stmt = conn
        .prepare("SELECT session_id, harness, project_path FROM session_projects")
        .map_err(|e| fail(&format!("cannot read session_projects: {e}")))?;
    let rows = stmt
        .query_map([], |r| {
            let s: String = r.get(0)?;
            let h: String = r.get(1)?;
            let p: String = r.get(2)?;
            Ok((s, h, p))
        })
        .map_err(|e| fail(&format!("cannot iterate session_projects: {e}")))?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (session, harness, path) =
            row.map_err(|e| fail(&format!("cannot decode session_projects row: {e}")))?;
        let scope = resolve_scope(&harness, &path)?;
        out.insert((session, harness), scope);
    }
    Ok(out)
}
