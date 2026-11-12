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
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':' {
                c
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
        assert!(event_filename("ltmrs:memory:x", 7).starts_with("ltmrs:memory:x.7."));
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
}
