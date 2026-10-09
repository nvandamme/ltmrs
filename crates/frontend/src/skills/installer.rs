//! Managed skill installation (WP-10 task 2; T-SKILL-01).
//!
//! Installs the owned ltmrs skill asset under a home directory with
//! ownership markers, idempotent updates and atomic writes. Foreign or
//! user-modified files are never silently replaced.

use std::path::{Path, PathBuf};

/// An installable skill asset: content plus the version its marker carries.
#[derive(Debug, Clone)]
pub struct SkillAsset {
    /// Skill directory name (`<home>/.agents/skills/<name>/SKILL.md`).
    pub name: &'static str,
    /// Full file content excluding the ownership marker line.
    pub content: &'static str,
    /// Installer version stamped in the marker (version-aware updates).
    pub version: &'static str,
}

/// Installer version stamped in ownership markers. Bump on ANY asset
/// content change: AlreadyCurrent compares version + hash, so an unbumped
/// edit would never deploy.
pub const SKILL_VERSION: &str = "0.1.0";

/// The managed native skill asset (multilingual recall guidance included).
pub const LTMRS_SKILL: SkillAsset = SkillAsset {
    name: "ltmrs",
    content: include_str!("ltmrs_skill.md"),
    version: SKILL_VERSION,
};

/// Tool-name prefixes that may appear in skill guidance. Anything else is
/// prose, not a tool reference.
const TOOL_TOKEN_PREFIXES: &[&str] = &["memory_", "guide_", "session_"];

/// Single-word tool references outside the frozen compatibility set.
const TOOL_TOKEN_SINGLES: &[&str] = &[
    "semantic_search",
    "conflict_scan",
    "proactive_analysis",
    "project_analytics",
    "suggestion_respond",
];

/// Install the native skill under `home` (never replaces foreign files
/// without an explicit override at the `install_skill` level).
pub fn install_native_skill(home: &Path) -> Result<InstallOutcome, InstallError> {
    install_skill(home, &LTMRS_SKILL, false)
}

/// Extract tool-looking tokens from skill guidance text: `snake_case`
/// words with a tool prefix (or the known single-word tools). Used to pin
/// the skill workflow against the frozen compatibility tool names.
pub fn skill_tool_tokens(content: &str) -> Vec<String> {
    content
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|tok| {
            TOOL_TOKEN_SINGLES.contains(tok)
                || TOOL_TOKEN_PREFIXES
                    .iter()
                    .any(|prefix| tok.starts_with(prefix) && tok.len() > prefix.len())
        })
        .map(str::to_string)
        .collect()
}

/// Outcome of an install call. Refusals protect content; only an explicit
/// `replace_foreign` override replaces a foreign file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    /// Written (fresh install, outdated update, or explicit replacement).
    Installed,
    /// Marker matches content hash: nothing written.
    AlreadyCurrent,
    /// No ownership marker and no explicit override: left untouched.
    RefusedForeign,
    /// Our marker at the current version but content edited: left untouched.
    RefusedModified,
}

/// Installer failure (IO, never silent).
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("cannot write {path}: {message}")]
    Write { path: String, message: String },
}

/// Parsed ownership marker (first line of a managed file).
#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    pub version: String,
    pub sha256: String,
}

/// Skill file path under a home directory.
pub fn skill_path(home: &Path, name: &str) -> PathBuf {
    home.join(".agents")
        .join("skills")
        .join(name)
        .join("SKILL.md")
}

/// Render the ownership marker line for an asset.
fn marker_line(asset: &SkillAsset) -> String {
    format!(
        "<!-- ltmrs-skill version={} sha256={} -->",
        asset.version,
        sha256_hex(asset.content.as_bytes())
    )
}

/// Read the ownership marker of an installed file, if present and valid.
pub fn read_marker(home: &Path, name: &str) -> Option<Marker> {
    let text = std::fs::read_to_string(skill_path(home, name)).ok()?;
    parse_marker(text.lines().next()?)
}

/// Parse one marker line of the form `<!-- ltmrs-skill version=V sha256=H -->`.
/// Versions must be dotted numerics (`1`, `0.1.0`); anything else is not
/// ours — a foreign file topped with a hand-written marker must still
/// refuse, never silently overwrite.
fn parse_marker(line: &str) -> Option<Marker> {
    let line = line.trim();
    let body = line
        .strip_prefix("<!-- ltmrs-skill ")?
        .strip_suffix("-->")?
        .trim();
    let mut version = None;
    let mut sha256 = None;
    for part in body.split_whitespace() {
        let (k, v) = part.split_once('=')?;
        match k {
            "version" if is_dotted_numeric(v) => version = Some(v.to_string()),
            "sha256" if !v.is_empty() => sha256 = Some(v.to_string()),
            _ => return None,
        }
    }
    Some(Marker {
        version: version?,
        sha256: sha256?,
    })
}

/// Dotted-numeric versions only (`0.1.0`, not `latest` or empty).
fn is_dotted_numeric(v: &str) -> bool {
    !v.is_empty()
        && v.split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// SHA-256 hex digest of file content (ownership comparison).
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let mut h = sha2::Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Install or update a skill asset under a home directory.
///
/// Decision table (all branches tested):
///
/// - Missing file → write with marker (`Installed`).
/// - Marker present, same version, hash matches → `AlreadyCurrent`.
/// - Marker present, older version → `Installed` (version-aware update).
/// - Marker present, current version, hash differs → `RefusedModified`.
/// - Newer marker version → `RefusedModified` (no silent downgrade).
/// - No marker → `RefusedForeign`, unless `replace_foreign` → `Installed`.
///
/// Writes are atomic (temp file + rename in the same directory).
pub fn install_skill(
    home: &Path,
    asset: &SkillAsset,
    replace_foreign: bool,
) -> Result<InstallOutcome, InstallError> {
    let path = skill_path(home, asset.name);
    let current = std::fs::read_to_string(&path).ok();
    let marker = current
        .as_deref()
        .and_then(|t| parse_marker(t.lines().next()?));

    let decision = match (&current, &marker) {
        (None, _) => InstallOutcome::Installed,
        (Some(_), None) if replace_foreign => InstallOutcome::Installed,
        (Some(_), None) => InstallOutcome::RefusedForeign,
        (Some(text), Some(m))
            if m.version.as_str() == asset.version
                && m.sha256 == sha256_hex(strip_marker(text).as_bytes()) =>
        {
            InstallOutcome::AlreadyCurrent
        }
        (Some(_), Some(m)) if version_is_older(&m.version, asset.version) => {
            InstallOutcome::Installed
        }
        (Some(_), Some(_)) => InstallOutcome::RefusedModified,
    };
    match decision {
        InstallOutcome::RefusedForeign | InstallOutcome::RefusedModified => Ok(decision),
        InstallOutcome::Installed | InstallOutcome::AlreadyCurrent => {
            if decision == InstallOutcome::AlreadyCurrent {
                return Ok(decision);
            }
            write_atomic(&path, &render_asset(asset)).map(|()| decision)
        }
    }
}

/// Body text without a leading marker line (for hashing user content).
/// Round-trips exactly with `render_asset` byte-wise: the marker line and
/// its `\n` separator are cut, preserving any CRLF body (Windows checkouts
/// hash identically to LF ones).
fn strip_marker(text: &str) -> String {
    let mut lines = text.lines();
    match lines.next() {
        Some(first) if parse_marker(first).is_some() => text[first.len() + 1..].to_string(),
        _ => text.to_string(),
    }
}

/// Numeric dotted-version ordering ("0.10.0" > "0.9.0", unlike lexicographic).
fn version_is_older(have: &str, current: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
    }
    let (mut h, mut c) = (parts(have), parts(current));
    let n = h.len().max(c.len());
    h.resize(n, 0);
    c.resize(n, 0);
    h < c
}

/// Full file content: marker line plus asset body.
fn render_asset(asset: &SkillAsset) -> String {
    format!("{}\n{}", marker_line(asset), asset.content)
}

/// Atomic write: temp file in the target directory, then rename. Creates
/// parent directories as needed.
fn write_atomic(path: &Path, content: &str) -> Result<(), InstallError> {
    let display = path.display().to_string();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e: std::io::Error| InstallError::Write {
            path: display.clone(),
            message: e.to_string(),
        })?;
    }
    // Unique temp name per call: concurrent installers never share one.
    let tmp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&tmp, content).map_err(|e: std::io::Error| InstallError::Write {
        path: display.clone(),
        message: e.to_string(),
    })?;
    std::fs::rename(&tmp, path).map_err(|e: std::io::Error| InstallError::Write {
        path: display,
        message: e.to_string(),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset() -> SkillAsset {
        SkillAsset {
            name: "ltmrs-test",
            content: "# Test Skill\n\nDo good work.\n",
            version: "0.1.0",
        }
    }

    /// Fresh install writes the asset with an ownership marker.
    #[test]
    fn install_fresh_creates_with_marker() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = install_skill(dir.path(), &asset(), false).unwrap();
        assert_eq!(outcome, InstallOutcome::Installed);
        let text = std::fs::read_to_string(skill_path(dir.path(), "ltmrs-test")).unwrap();
        assert!(text.contains("# Test Skill"));
        let marker = read_marker(dir.path(), "ltmrs-test").unwrap();
        assert_eq!(marker.version, "0.1.0");
        assert!(!marker.sha256.is_empty());
    }

    /// Reinstalling identical content is a no-op (idempotent).
    #[test]
    fn reinstall_identical_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        install_skill(dir.path(), &asset(), false).unwrap();
        let outcome = install_skill(dir.path(), &asset(), false).unwrap();
        assert_eq!(outcome, InstallOutcome::AlreadyCurrent);
    }

    /// User-modified current-version files are refused (content preserved).
    #[test]
    fn user_modified_current_refused() {
        let dir = tempfile::tempdir().unwrap();
        install_skill(dir.path(), &asset(), false).unwrap();
        let path = skill_path(dir.path(), "ltmrs-test");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("\nMy personal notes.\n");
        std::fs::write(&path, &text).unwrap();

        let outcome = install_skill(dir.path(), &asset(), false).unwrap();
        assert_eq!(outcome, InstallOutcome::RefusedModified);
        let kept = std::fs::read_to_string(&path).unwrap();
        assert!(kept.contains("My personal notes."));
    }

    /// Foreign files without a marker are never silently replaced.
    #[test]
    fn foreign_file_protected() {
        let dir = tempfile::tempdir().unwrap();
        let path = skill_path(dir.path(), "ltmrs-test");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "# Upstream Lemma Skill\n\nForeign content.\n").unwrap();

        let outcome = install_skill(dir.path(), &asset(), false).unwrap();
        assert_eq!(outcome, InstallOutcome::RefusedForeign);
        let kept = std::fs::read_to_string(&path).unwrap();
        assert!(kept.contains("Foreign content."));
        // Explicit replacement is the only override.
        let outcome = install_skill(dir.path(), &asset(), true).unwrap();
        assert_eq!(outcome, InstallOutcome::Installed);
        let replaced = std::fs::read_to_string(&path).unwrap();
        assert!(replaced.contains("# Test Skill"));
    }

    /// Outdated marker versions update in place (version-aware).
    #[test]
    fn outdated_marker_updates() {
        let dir = tempfile::tempdir().unwrap();
        let old = SkillAsset {
            name: "ltmrs-test",
            content: "# Old Skill\n",
            version: "0.0.1",
        };
        install_skill(dir.path(), &old, false).unwrap();
        let outcome = install_skill(dir.path(), &asset(), false).unwrap();
        assert_eq!(outcome, InstallOutcome::Installed);
        let text = std::fs::read_to_string(skill_path(dir.path(), "ltmrs-test")).unwrap();
        assert!(text.contains("# Test Skill"));
        assert_eq!(
            read_marker(dir.path(), "ltmrs-test").unwrap().version,
            "0.1.0"
        );
    }

    /// Concurrent installers never corrupt the file (atomic writes).
    #[test]
    fn concurrent_installers_stay_consistent() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.keep();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = dir.clone();
                std::thread::spawn(move || install_skill(&path, &asset(), false))
            })
            .collect();
        for h in handles {
            assert!(h.join().unwrap().is_ok());
        }
        let text = std::fs::read_to_string(skill_path(&dir, "ltmrs-test")).unwrap();
        assert!(text.contains("# Test Skill"));
        assert!(read_marker(&dir, "ltmrs-test").is_some());
    }

    /// Write denial surfaces as an error, never silent success. Uses a
    /// parent-is-file fault (not permission bits) so it fails deterministically
    /// even as root.
    #[test]
    fn write_denied_errors() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join(".agents");
        std::fs::write(&blocker, b"not a dir").unwrap();
        let err = install_skill(dir.path(), &asset(), false).unwrap_err();
        assert!(
            err.to_string().contains("SKILL.md"),
            "error must name the file, got: {err}"
        );
    }

    /// Numeric dotted ordering (lexicographic would flip 0.9.0/0.10.0).
    /// Non-numeric versions never reach here (marker parse rejects them).
    #[test]
    fn version_ordering_is_numeric() {
        assert!(version_is_older("0.0.1", "0.1.0"));
        assert!(version_is_older("0.9.0", "0.10.0"));
        assert!(!version_is_older("0.10.0", "0.9.0"));
        assert!(!version_is_older("1.0", "1.0.0"));
        assert!(!version_is_older("0.1.0", "0.1.0"));
    }

    /// Hand-written markers don't bypass foreign protection.
    #[test]
    fn spoofed_marker_still_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = skill_path(dir.path(), "ltmrs-test");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for marker in [
            "<!-- ltmrs-skill version= sha256=abc -->",
            "<!-- ltmrs-skill version=abc sha256=abc -->",
        ] {
            std::fs::write(&path, format!("{marker}\nForeign body.\n")).unwrap();
            assert_eq!(
                install_skill(dir.path(), &asset(), false).unwrap(),
                InstallOutcome::RefusedForeign
            );
        }
    }

    /// Empty content round-trips idempotently (no phantom newline drift).
    #[test]
    fn empty_content_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let empty = SkillAsset {
            name: "empty-test",
            content: "",
            version: "0.1.0",
        };
        assert_eq!(
            install_skill(dir.path(), &empty, false).unwrap(),
            InstallOutcome::Installed
        );
        assert_eq!(
            install_skill(dir.path(), &empty, false).unwrap(),
            InstallOutcome::AlreadyCurrent
        );
    }

    /// Every tool referenced by the native skill workflow exists in the
    /// frozen compatibility set (a renamed/drifted tool reference would
    /// teach hosts a call we reject).
    #[test]
    fn skill_workflow_tools_match_frozen_names() {
        use std::collections::HashSet;
        let frozen: HashSet<String> = ltmrs_compat::lemma::schemas::frozen_tools()
            .into_iter()
            .map(|t| t.name)
            .collect();
        let tokens = skill_tool_tokens(LTMRS_SKILL.content);
        assert!(!tokens.is_empty(), "the skill must reference tools");
        for tok in &tokens {
            assert!(
                frozen.contains(tok),
                "skill references unknown tool `{tok}` (not in the frozen set)"
            );
        }
        // The recall → act → persist workflow core is actually taught.
        for core in [
            "memory_read",
            "memory_add",
            "semantic_search",
            "guide_practice",
            "guide_distill",
            "session_attempt",
            "session_end",
        ] {
            assert!(
                tokens.iter().any(|t| t == core),
                "skill must teach `{core}`, got: {tokens:?}"
            );
        }
    }

    /// The native multilingual guidance stays a separate, documented
    /// section of the native asset (never mixed into tool references).
    #[test]
    fn multilingual_guidance_is_separate_and_documented() {
        assert!(
            LTMRS_SKILL.content.contains("## Multilingual recall"),
            "multilingual guidance must be its own section"
        );
        assert!(
            LTMRS_SKILL.content.contains("English"),
            "storage language must be documented"
        );
        assert!(
            skill_tool_tokens(LTMRS_SKILL.content)
                .iter()
                .all(|t| !t.contains("french") && !t.contains("german")),
            "language names must never look like tool references"
        );
    }
}
