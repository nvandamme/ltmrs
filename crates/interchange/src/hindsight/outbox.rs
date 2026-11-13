//! Durable retain outbox (Slice G1d; omp `HindsightOutbox` parity).
//!
//! Each retain becomes one JSON event file under `<home>/hindsight-outbox/<destination>/`,
//! synced + atomically renamed into place. Refused/malformed/expired events move to
//! `hindsight-outbox/quarantine/` (deleted 30 days after quarantine).
//! Pure file layout + naming; delivery (retry/backoff, drain lease) lives in the
//! daemon worker, which reuses these paths.

use std::path::{Path, PathBuf};

/// Outbox root under a home directory.
pub fn outbox_root(home: &Path) -> PathBuf {
    home.join("hindsight-outbox")
}

/// Destination directory for one bank (bank names are path-flattened:
/// separators become `_`, never subdirectories).
pub fn dest_dir(home: &Path, bank: &str) -> PathBuf {
    let flat: String = bank
        .chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect();
    let flat = flat.trim();
    let flat = if flat.is_empty() { "default" } else { flat };
    outbox_root(home).join(flat)
}

/// Quarantine directory (refused/malformed/expired events).
pub fn quarantine_dir(home: &Path) -> PathBuf {
    outbox_root(home).join("quarantine")
}

/// Event file name: `<document_id>.<nanos>.json` (document_id is already
/// unique per call, nanos disambiguates same-id retries).
pub fn event_filename(document_id: &str, nanos: u64) -> String {
    let safe: String = document_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else if c == ':' {
                // ':' is valid on Linux ext4 but invalid on NTFS
                // (ERROR_INVALID_NAME, os code 123), so flatten it on Windows.
                #[cfg(windows)]
                {
                    '_'
                }
                #[cfg(not(windows))]
                {
                    ':'
                }
            } else {
                '_'
            }
        })
        .collect();
    format!("{safe}.{nanos}.json")
}

/// Quarantine retention: events quarantined 30+ days ago are deleted.
/// Returns the cutoff (now - 30 days) in millis for testability.
pub fn quarantine_cutoff_millis(now_millis: u64) -> u64 {
    now_millis.saturating_sub(30 * 24 * 60 * 60 * 1000)
}

/// Outbox expiry: undelivered events written 7+ days ago move to quarantine.
pub fn outbox_expiry_millis(now_millis: u64) -> u64 {
    now_millis.saturating_sub(7 * 24 * 60 * 60 * 1000)
}

/// Write one event atomically (temp file + rename in the same directory).
/// Pure I/O helper; the caller serializes the event first.
pub fn write_event(dir: &Path, filename: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{filename}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    // Best-effort durability: sync the file before renaming into place.
    if let Ok(f) = std::fs::File::open(&tmp) {
        let _ = f.sync_all();
    }
    let dest = dir.join(filename);
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// One pending event for startup replay: the bank it was filed under plus
/// the raw event bytes (the daemon re-parses and re-enqueues through the
/// normal retain path, so replayed events get fresh filenames/retries).
#[derive(Debug, Clone, PartialEq)]
pub struct PendingEvent {
    pub bank: String,
    pub body: Vec<u8>,
    /// Source file: the replayer deletes it after successful re-enqueue
    /// (the normal persist-first path regenerates it on next failure).
    pub path: std::path::PathBuf,
}

/// Collect pending events for startup replay: scan every bank dir under
/// the outbox root; events older than the 7-day expiry move to quarantine,
/// the rest return for resend. Missing root = no pending work (Ok empty),
/// never an error — a fresh home has nothing to replay.
pub fn collect_pending(home: &Path, now_millis: u64) -> std::io::Result<Vec<PendingEvent>> {
    collect_from_root(&outbox_root(home), home, now_millis)
}

/// Scan an explicit outbox root (the configured dir, not a home): same
/// walk as [`collect_pending`], for daemons whose `outbox_dir` IS the root.
pub fn collect_from_root(
    root: &Path,
    home: &Path,
    now_millis: u64,
) -> std::io::Result<Vec<PendingEvent>> {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let cutoff = outbox_expiry_millis(now_millis);
    let mut pending = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() || dir.file_name().map(|n| n == "quarantine").unwrap_or(false) {
            continue;
        }
        let bank = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("default")
            .to_string();
        for event in std::fs::read_dir(&dir)?.flatten() {
            let path = event.path();
            if path.extension().map(|e| e != "json").unwrap_or(true) {
                continue;
            }
            let mtime_millis: u64 = event
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            if mtime_millis < cutoff {
                let qdir = quarantine_dir(home);
                let _ = std::fs::create_dir_all(&qdir);
                let dest = qdir.join(format!(
                    "{}-{}.json",
                    bank,
                    path.file_stem().and_then(|s| s.to_str()).unwrap_or("event")
                ));
                let _ = std::fs::rename(&path, dest);
                continue;
            }
            match std::fs::read(&path) {
                Ok(body) => pending.push(PendingEvent {
                    bank: bank.clone(),
                    body,
                    path: path.clone(),
                }),
                Err(_) => continue,
            }
        }
    }
    Ok(pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// G1d RED: layout paths flatten banks, never nest.
    #[test]
    fn layout_flattens_and_defaults() {
        let home = Path::new("/h");
        assert_eq!(
            dest_dir(home, "team"),
            Path::new("/h/hindsight-outbox/team")
        );
        assert_eq!(dest_dir(home, "a/b"), Path::new("/h/hindsight-outbox/a_b"));
        assert_eq!(dest_dir(home, ""), Path::new("/h/hindsight-outbox/default"));
        assert_eq!(
            quarantine_dir(home),
            Path::new("/h/hindsight-outbox/quarantine")
        );
        // ':' is kept on Linux, flattened to '_' on Windows (NTFS).
        #[cfg(not(windows))]
        let prefix = "ltmrs:memory:x.7.";
        #[cfg(windows)]
        let prefix = "ltmrs_memory_x.7.";
        assert!(event_filename("ltmrs:memory:x", 7).starts_with(prefix));
    }

    /// G1d RED: expiry cutoffs are exact day arithmetic.
    #[test]
    fn cutoffs_are_day_exact() {
        let now = 30 * 24 * 60 * 60 * 1000u64;
        assert_eq!(quarantine_cutoff_millis(now), 0);
        assert_eq!(outbox_expiry_millis(now), 23 * 24 * 60 * 60 * 1000);
    }

    /// G1d RED: atomic write round-trips bytes.
    #[test]
    fn write_event_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let dest = write_event(dir.path(), "a.1.json", b"{}").unwrap();
        assert_eq!(std::fs::read(dest).unwrap(), b"{}");
    }

    /// Startup replay: pending events collect for resend; expired ones
    /// (by file mtime vs the 7-day cutoff) move to quarantine. RED:
    /// fails — no collect function exists yet.
    #[test]
    fn collect_pending_resends_fresh_quarantines_expired() {
        let home = tempfile::tempdir().unwrap();
        let bank_dir = dest_dir(home.path(), "team");
        std::fs::create_dir_all(&bank_dir).unwrap();
        // Fresh event: written now, must collect.
        write_event(&bank_dir, "doc1.1.json", b"{\"a\":1}").unwrap();
        // Expired event: backdate mtime 8 days.
        let old_path = write_event(&bank_dir, "doc2.2.json", b"{\"b\":2}").unwrap();
        let eight_days_ago =
            std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 24 * 60 * 60);
        std::fs::File::options()
            .write(true)
            .open(&old_path)
            .unwrap()
            .set_modified(eight_days_ago)
            .unwrap();
        let now_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let pending = collect_pending(home.path(), now_millis).unwrap();
        assert_eq!(pending.len(), 1, "got: {pending:?}");
        assert_eq!(pending[0].bank, "team");
        assert!(quarantine_dir(home.path()).exists());
        let quarantined: Vec<_> = std::fs::read_dir(quarantine_dir(home.path()))
            .unwrap()
            .collect();
        assert_eq!(quarantined.len(), 1, "expired must quarantine");
    }
}
