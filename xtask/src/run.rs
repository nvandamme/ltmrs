//! Shared run-record and dispatch core: spawn a child, capture its output,
//! and persist a JSON run-record for later verbs to reuse.

/// Outcome of one child execution.
pub struct RunOutcome {
    /// Process exit code, or `-1` when killed by a signal (see [`execute`]).
    pub exit_code: i32,
    /// Last 4 KiB of captured stdout (kept symmetric with stderr for
    /// failure diagnostics; failure reporters print the stderr tail today).
    #[allow(dead_code)]
    pub stdout_tail: String,
    /// Last 4 KiB of captured stderr.
    pub stderr_tail: String,
    /// Milliseconds since the Unix epoch, taken just before spawn.
    pub started_ms: u64,
    /// Milliseconds since the Unix epoch, taken just after reaping the child.
    pub finished_ms: u64,
}

/// Maximum bytes kept per stream in [`RunOutcome`].
const TAIL_MAX: usize = 4096;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn tail_4k(bytes: &[u8]) -> String {
    let tail = if bytes.len() > TAIL_MAX {
        &bytes[bytes.len() - TAIL_MAX..]
    } else {
        bytes
    };
    String::from_utf8_lossy(tail).into_owned()
}

/// Run `program` with `args` in `workdir`, capturing both streams via pipes.
///
/// The full exit status is preserved in [`RunOutcome::exit_code`]. Shells map
/// a signal kill to 128+signo, but Rust `ExitStatus::code()` returns `None`
/// on signal with no portable signo; record `exit_code: -1` in that case and
/// let [`write_record`] add `"signal": true` so the record stays unambiguous
/// (a real exit status is always 0–255 and never `-1`).
pub fn execute(program: &str, args: &[String], workdir: &std::path::Path) -> RunOutcome {
    execute_with_env(program, args, workdir, &[])
}

/// Run `program` with `args` in `workdir`, applying `env` overrides that are
/// scoped to the child via [`std::process::Command::env`].
///
/// Prefer this over mutating the parent process environment: per-command
/// overrides are thread-safe (no process-wide `set_var`, no `unsafe`), so
/// sandboxed values such as `HOME` can never leak into sibling threads or
/// survive the call.
pub fn execute_with_env(
    program: &str,
    args: &[String],
    workdir: &std::path::Path,
    env: &[(&str, &str)],
) -> RunOutcome {
    let started_ms = now_ms();
    let mut command = std::process::Command::new(program);
    command.args(args).current_dir(workdir);
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output();
    let finished_ms = now_ms();
    match output {
        Ok(out) => {
            let exit_code = out.status.code().unwrap_or(-1);
            RunOutcome {
                exit_code,
                stdout_tail: tail_4k(&out.stdout),
                stderr_tail: tail_4k(&out.stderr),
                started_ms,
                finished_ms,
            }
        }
        Err(err) => RunOutcome {
            exit_code: 127,
            stdout_tail: String::new(),
            stderr_tail: format!("failed to spawn '{program}': {err}"),
            started_ms,
            finished_ms,
        },
    }
}

/// Write `record.json` under `dir` (creating all parents) and return its path.
///
/// The write is atomic (temp file + rename) so a crash can never leave a
/// truncated record behind. Tails are persisted so a spawn failure (exit
/// 127 with only an in-memory message) stays diagnosable post-mortem.
/// Optional `meta` merges verb-specific fields (e.g. the evidence release);
/// absent stays absent so old readers keep working.
pub fn write_record(
    dir: &std::path::Path,
    verb: &str,
    argv: &[String],
    outcome: &RunOutcome,
    meta: Option<&serde_json::Value>,
) -> std::io::Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("record.json");
    let mut record = serde_json::json!({
        "verb": verb,
        "argv": argv,
        "started_ms": outcome.started_ms,
        "finished_ms": outcome.finished_ms,
        "exit_code": outcome.exit_code,
        "signal": outcome.exit_code == -1,
        "stdout_tail": outcome.stdout_tail,
        "stderr_tail": outcome.stderr_tail,
    });
    if let Some(meta) = meta {
        record["meta"] = meta.clone();
    }
    let text = serde_json::to_string_pretty(&record).map_err(std::io::Error::other)?;
    let tmp = dir.join(format!(".record.json.tmp-{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::{execute, execute_with_env, write_record};

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_path_buf();
        std::mem::forget(dir);
        path
    }

    fn fake_outcome() -> super::RunOutcome {
        super::RunOutcome {
            exit_code: 0,
            stdout_tail: String::new(),
            stderr_tail: String::new(),
            started_ms: 1,
            finished_ms: 2,
        }
    }

    #[test]
    fn failing_child_propagates_exit_code() {
        let out = execute(
            "sh",
            &["-c".into(), "exit 3".into()],
            std::path::Path::new("/tmp"),
        );
        assert_eq!(out.exit_code, 3);
    }

    #[test]
    fn child_observes_sandboxed_home() {
        let out = execute_with_env(
            "sh",
            &["-c".into(), "echo $HOME".into()],
            std::path::Path::new("/tmp"),
            &[("HOME", "/tmp/sandbox-XYZ")],
        );
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout_tail.trim(), "/tmp/sandbox-XYZ");
    }

    #[test]
    fn parent_home_unchanged_after_sandboxed_child() {
        let before = std::env::var_os("HOME");
        let out = execute_with_env(
            "sh",
            &["-c".into(), "echo $HOME".into()],
            std::path::Path::new("/tmp"),
            &[("HOME", "/tmp/sandbox-XYZ")],
        );
        assert_eq!(out.exit_code, 0);
        assert_eq!(std::env::var_os("HOME"), before);
    }

    #[test]
    fn record_shape_has_required_fields() {
        let dir = tempfile_dir();
        let path = write_record(
            &dir,
            "evidence",
            &["--release".into(), "v0.1".into()],
            &fake_outcome(),
            None,
        )
        .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for field in ["verb", "argv", "started_ms", "finished_ms", "exit_code"] {
            assert!(v.get(field).is_some(), "missing {field}");
        }
    }

    #[test]
    fn record_carries_tails_and_optional_meta() {
        let dir = tempfile_dir();
        let mut outcome = fake_outcome();
        outcome.stdout_tail = "out".to_string();
        outcome.stderr_tail = "err".to_string();
        let meta = serde_json::json!({"release": "v0.1"});
        let path = write_record(
            &dir,
            "evidence",
            &["--release".into(), "v0.1".into()],
            &outcome,
            Some(&meta),
        )
        .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["stdout_tail"], "out");
        assert_eq!(v["stderr_tail"], "err");
        assert_eq!(v["meta"]["release"], "v0.1");
        // Absent meta stays absent (old readers keep working).
        let plain = write_record(&dir, "evidence", &[], &fake_outcome(), None).unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&plain).unwrap()).unwrap();
        assert!(v.get("meta").is_none(), "meta must be omitted when None");
    }
}
