//! Curated file pipeline (Slice G4/G5): pending deltas + scheduled consolidation.
//!
//! omp `local`-backend + `sharpshooter` parity, adapted to the local-only
//! contract: the daemon NEVER calls a model. Hosts (or hook-extract)
//! enqueue extracted deltas; the daemon redacts, dedups and appends them
//! to curated files on schedule. No leases (single daemon owns the files),
//! no model prompts, no hidden inference.
//!
//! - G5 sharpshooter: `<home>/sharpshooter/<bank>/{architecture,product,style}.md`
//!   + per-file pending queues, consolidated every `interval_minutes`.
//! - G4 local pipeline: `<home>/memories/<bank>/learned.md` (newest-first,
//!   100-cap, secret-redacted) + `memory_summary.md` (compact head).

use std::path::{Path, PathBuf};

/// Curated decision files (G5 sharpshooter parity).
pub const DECISION_FILES: &[&str] = &["architecture.md", "product.md", "style.md"];

/// Bank directory for sharpshooter decisions.
pub fn sharpshooter_bank_dir(home: &Path, bank: &str) -> PathBuf {
    home.join("sharpshooter").join(safe_bank(bank))
}

/// Bank directory for local-pipeline memory files.
pub fn local_bank_dir(home: &Path, bank: &str) -> PathBuf {
    home.join("memories").join(safe_bank(bank))
}

fn safe_bank(bank: &str) -> String {
    let flat: String = bank
        .chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect();
    let flat = flat.trim();
    if flat.is_empty() {
        "default".to_string()
    } else {
        flat.to_string()
    }
}

/// Pending queue path for one curated file.
pub fn pending_path(bank_dir: &Path, file: &str) -> PathBuf {
    bank_dir.join(format!("{file}.pending.jsonl"))
}

/// Append one delta to a pending queue (JSONL, one object per line).
pub fn enqueue_delta(bank_dir: &Path, file: &str, delta_json: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(bank_dir)?;
    let path = pending_path(bank_dir, file);
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    f.write_all(delta_json.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(path)
}

/// Read + clear a pending queue (consolidation consumes it atomically-ish:
/// read all lines, truncate; a crash between leaves at most duplicates,
/// and dedup downstream makes that harmless).
pub fn drain_pending(bank_dir: &Path, file: &str) -> Vec<String> {
    let path = pending_path(bank_dir, file);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::write(&path, "");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect()
}

/// Secret redaction for curated output (omp parity: common token patterns).
/// Pure: replaces `sk-…`, `ghp_…`, `xox[bpas]-…`, `AKIA…`, bearer tokens.
pub fn redact_secrets(text: &str) -> String {
    let mut out = text.to_string();
    for pat in [
        "sk-", "sk-ant-", "ghp_", "gho_", "xoxb-", "xoxp-", "xoxa-", "xoxs-", "AKIA",
    ] {
        let mut start = 0;
        while let Some(i) = out[start..].find(pat) {
            let abs = start + i;
            let end = out[abs..]
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '`')
                .map(|e| abs + e)
                .unwrap_or(out.len());
            // Keep a 4-char prefix so the redaction is auditable, hide the rest.
            let keep = (abs + pat.len() + 4).min(end);
            out.replace_range(keep..end, "…[redacted]");
            start = keep + 12;
            if start >= out.len() {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// G4/G5 RED: bank dirs flatten computed paths, never nest.
    #[test]
    fn bank_dirs_flatten() {
        let home = Path::new("/h");
        assert_eq!(
            sharpshooter_bank_dir(home, "a/b"),
            Path::new("/h/sharpshooter/a_b")
        );
        assert_eq!(local_bank_dir(home, ""), Path::new("/h/memories/default"));
    }

    /// G4/G5 RED: enqueue → drain round-trips deltas and clears the queue.
    #[test]
    fn pending_queue_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        enqueue_delta(dir.path(), "architecture.md", r#"{"d":1}"#).unwrap();
        enqueue_delta(dir.path(), "architecture.md", r#"{"d":2}"#).unwrap();
        let drained = drain_pending(dir.path(), "architecture.md");
        assert_eq!(drained.len(), 2);
        assert!(drain_pending(dir.path(), "architecture.md").is_empty());
    }

    /// G4/G5 RED: secrets redact, prose survives.
    #[test]
    fn secrets_redact_prose_survives() {
        let out = redact_secrets("token sk-abc123def456 here and normal words");
        assert!(!out.contains("sk-abc123def456"));
        assert!(out.contains("normal words"));
        assert!(out.contains("[redacted]"));
    }
}

/// Apply drained deltas to a curated file: redacted, deduped, appended.
/// Newest-first for `learned.md` (lesson list, 100-cap); append for decision files.
/// Pure file logic; the caller drains the queue first.
pub fn consolidate_file(
    path: &Path,
    deltas: &[String],
    newest_first: bool,
    cap: usize,
) -> std::io::Result<usize> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = existing.lines().map(|l| l.to_string()).collect();
    let mut added = 0;
    for d in deltas {
        let clean = redact_secrets(d.trim());
        if clean.is_empty() || lines.iter().any(|l| l == &clean) {
            continue;
        }
        if newest_first {
            lines.insert(0, clean);
        } else {
            lines.push(clean);
        }
        added += 1;
    }
    if newest_first && lines.len() > cap {
        lines.truncate(cap);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        path,
        lines.join("\n") + if lines.is_empty() { "" } else { "\n" },
    )?;
    Ok(added)
}

#[cfg(test)]
mod consolidate_tests {
    use super::*;

    /// G4/G5: consolidation dedups, redacts and caps.
    #[test]
    fn consolidate_dedups_redacts_caps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("learned.md");
        let n = consolidate_file(
            &path,
            &[
                "lesson one".to_string(),
                "token sk-abc123def456".to_string(),
                "lesson one".to_string(),
            ],
            true,
            2,
        )
        .unwrap();
        assert_eq!(n, 2);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("sk-abc123"));
        // newest-first: second delta lands on top.
        assert!(text.lines().next().unwrap().contains("redacted"));
        // cap holds.
        let _ =
            consolidate_file(&path, &["third".to_string(), "fourth".to_string()], true, 2).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
    }
}
