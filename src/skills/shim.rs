//! Opt-in legacy executable shim (WP-10 task 4; T-SKILL-01 adjacent).
//!
//! Hosts still configured with the legacy `lemma` command can opt in to a
//! `<home>/.local/bin/lemma` symlink pointing at the running ltmrs binary.
//! Nothing is ever replaced silently: an existing file that is not our
//! symlink refuses with the path; other `lemma` executables found on PATH
//! are reported loudly (collision list) but do not block the install.

use std::path::{Path, PathBuf};

/// User binary directory (XDG convention) holding the shim, under home.
pub const SHIM_DIR: &str = ".local/bin";
/// Legacy executable name the shim provides.
pub const SHIM_NAME: &str = "lemma";

/// Outcome of a shim install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShimOutcome {
    /// Symlink created.
    Installed,
    /// Symlink already points at this binary.
    AlreadyCurrent,
}

/// Install report: the outcome plus PATH collisions to tell the user about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShimReport {
    pub outcome: ShimOutcome,
    /// Other `lemma` executables on PATH (PATH order, shim excluded).
    pub collisions: Vec<PathBuf>,
}

/// Shim failure (refusals name the blocking path; IO carries context).
#[derive(Debug, thiserror::Error)]
pub enum ShimError {
    #[error(
        "refused: {0} exists and is not managed by ltmrs; back it up, then remove it to install the shim"
    )]
    RefusedForeign(PathBuf),
    #[error("cannot install shim at {path}: {message}")]
    Io { path: String, message: String },
}

/// Shim path under `home`.
pub fn shim_path(home: &Path) -> PathBuf {
    home.join(SHIM_DIR).join(SHIM_NAME)
}

/// Install the legacy shim pointing at `exe` (normally `current_exe`).
/// `path_env` overrides PATH for tests (`None` reads the real PATH);
/// passing an override never mutates the process environment.
pub fn install_shim(
    home: &Path,
    exe: &Path,
    path_env: Option<&str>,
) -> Result<ShimReport, ShimError> {
    let shim = shim_path(home);
    let outcome = match std::fs::symlink_metadata(&shim) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = shim.parent() {
                std::fs::create_dir_all(parent).map_err(|e| ShimError::Io {
                    path: parent.to_string_lossy().into_owned(),
                    message: e.to_string(),
                })?;
            }
            match std::os::unix::fs::symlink(exe, &shim) {
                Ok(()) => ShimOutcome::Installed,
                // A concurrent installer won the race: re-read instead of
                // failing (our symlink => current, anything else => refuse).
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    check_current(&shim, exe).map(|_| ShimOutcome::AlreadyCurrent)?
                }
                Err(e) => {
                    return Err(ShimError::Io {
                        path: shim.to_string_lossy().into_owned(),
                        message: e.to_string(),
                    });
                }
            }
        }
        Ok(_) => {
            // Anything already at the shim path must be our own symlink;
            // foreign files AND foreign symlinks refuse (never replace).
            check_current(&shim, exe).map(|_| ShimOutcome::AlreadyCurrent)?
        }
        Err(e) => {
            return Err(ShimError::Io {
                path: shim.to_string_lossy().into_owned(),
                message: e.to_string(),
            });
        }
    };
    Ok(ShimReport {
        outcome,
        collisions: find_collisions(&shim, path_env),
    })
}

/// The shim is ours when it links straight at `exe` (textual match) or at
/// the same file through another spelling (`./`, symlinked dirs). Anything
/// else — including a broken or foreign link — refuses.
fn check_current(shim: &Path, exe: &Path) -> Result<(), ShimError> {
    let current = std::fs::read_link(shim).ok();
    if current.as_deref() == Some(exe) {
        return Ok(());
    }
    if let (Some(target), Ok(want)) = (current.as_deref(), exe.canonicalize())
        && let Ok(have) = std::fs::canonicalize(target)
        && have == want
    {
        return Ok(());
    }
    Err(ShimError::RefusedForeign(shim.to_path_buf()))
}

/// Scan PATH (or `path_env`) for executable `lemma` files other than
/// `shim`: PATH order, deduplicated.
fn find_collisions(shim: &Path, path_env: Option<&str>) -> Vec<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let path_env = path_env
        .map(str::to_string)
        .or_else(|| std::env::var("PATH").ok());
    let mut out = Vec::new();
    for dir in path_env.iter().flat_map(|p| std::env::split_paths(p)) {
        // Empty entries mean CWD: never resolve the shim name against it
        // (a `./lemma` would otherwise flake collision detection).
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(SHIM_NAME);
        if candidate == shim {
            continue;
        }
        if out.contains(&candidate) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&candidate) else {
            continue;
        };
        if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
            continue;
        }
        out.push(candidate);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_exe(dir: &Path) -> PathBuf {
        let exe = dir.join("ltmrs-test-bin");
        std::fs::write(&exe, b"#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&exe).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt as _;
        perms.set_mode(0o755);
        std::fs::set_permissions(&exe, perms).unwrap();
        exe
    }

    /// Fresh install creates the symlink; rerun is a no-op.
    #[test]
    fn installs_symlink_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(dir.path());
        let report = install_shim(dir.path(), &exe, Some("")).unwrap();
        assert_eq!(report.outcome, ShimOutcome::Installed);
        assert!(report.collisions.is_empty());
        let link = shim_path(dir.path());
        assert_eq!(std::fs::read_link(&link).unwrap(), exe);
        let again = install_shim(dir.path(), &exe, Some("")).unwrap();
        assert_eq!(again.outcome, ShimOutcome::AlreadyCurrent);
    }

    /// A foreign file at the shim path refuses (never replaced).
    #[test]
    fn foreign_file_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(dir.path());
        let link = shim_path(dir.path());
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::fs::write(&link, b"mine").unwrap();
        let err = install_shim(dir.path(), &exe, Some("")).unwrap_err();
        assert!(matches!(err, ShimError::RefusedForeign(_)));
        assert!(err.to_string().contains("lemma"));
        assert_eq!(std::fs::read(&link).unwrap(), b"mine");
    }

    /// A foreign symlink (pointing elsewhere) also refuses.
    #[test]
    fn foreign_symlink_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(dir.path());
        let other = dir.path().join("other-bin");
        std::fs::write(&other, b"x").unwrap();
        let link = shim_path(dir.path());
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&other, &link).unwrap();
        assert!(matches!(
            install_shim(dir.path(), &exe, Some("")),
            Err(ShimError::RefusedForeign(_))
        ));
        assert_eq!(std::fs::read_link(&link).unwrap(), other);
    }

    /// A `lemma` executable elsewhere on PATH is reported (PATH order) but
    /// does not block the install; the shim itself is never listed.
    #[test]
    fn path_collisions_reported_not_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(dir.path());
        let shadow = dir.path().join("shadowbin");
        std::fs::create_dir_all(&shadow).unwrap();
        let shadow_lemma = shadow.join("lemma");
        std::fs::write(&shadow_lemma, b"#!/bin/sh\n").unwrap();
        let mut perms = std::fs::metadata(&shadow_lemma).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt as _;
        perms.set_mode(0o755);
        std::fs::set_permissions(&shadow_lemma, perms).unwrap();
        // A non-executable `lemma` is not a collision.
        let dead = dir.path().join("deadbin");
        std::fs::create_dir_all(&dead).unwrap();
        std::fs::write(dead.join("lemma"), b"not executable").unwrap();

        let path_env = format!("{}:{}", shadow.display(), dead.display());
        let report = install_shim(dir.path(), &exe, Some(&path_env)).unwrap();
        assert_eq!(report.outcome, ShimOutcome::Installed);
        assert_eq!(report.collisions, vec![shadow_lemma]);
    }

    /// Reinstalling with a differently-spelled path to the same binary
    /// (`./` component) still recognizes our own shim (no refusal).
    #[test]
    fn same_binary_other_spelling_stays_current() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(dir.path());
        assert_eq!(
            install_shim(dir.path(), &exe, Some("")).unwrap().outcome,
            ShimOutcome::Installed
        );
        // Same file through a symlinked parent: components differ textually
        // (`.` would be normalized away by `Path` comparison).
        let alias = dir.path().join("alias-dir");
        std::os::unix::fs::symlink(dir.path(), &alias).unwrap();
        let respelled = alias.join("ltmrs-test-bin");
        assert_ne!(respelled, exe, "spellings must differ textually");
        assert_eq!(
            install_shim(dir.path(), &respelled, Some(""))
                .unwrap()
                .outcome,
            ShimOutcome::AlreadyCurrent
        );
    }
}
