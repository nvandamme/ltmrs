//! Legacy Lemma interchange verdict (WP-11c; T-IMPORT-02 scope note).
//!
//! Lemma `.lemma-backup` files carry a base64 SQLite payload. Opening one
//! requires a C database engine, which the audited native-code policy
//! (RQ-21: no C++ database engine; CPU release) excludes. The verdict is
//! therefore explicit refusal, not conversion:
//! - [`LEGACY_FORMAT`] envelopes are detected by marker and rejected with
//!   [`crate::backup::BackupError::LegacyUnsupported`]
//!   carrying the reason (never silently converted, never relabeled as a
//!   native archive — a `.ltmrs-backup` file is always native-verified).
//! - Rejection happens on envelope bytes alone: the source file is opened
//!   read-only and never mutated (RQ-26 no-mutation proof in tests).
//! - Recorded as an unsupported conformance target in the compatibility
//!   ledger; native backup/restore is the supported interchange path.

/// Upstream backup format marker (detected, never produced or converted).
pub const LEGACY_FORMAT: &str = "lemma-backup";

/// True when raw bytes look like a legacy envelope (JSON object whose
/// `format` marker is [`LEGACY_FORMAT`]). Best-effort sniff for routing;
/// authoritative classification happens in verification.
pub fn is_legacy_backup(bytes: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| v.get("format")?.as_str().map(|f| f.to_string()))
        == Some(LEGACY_FORMAT.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Legacy envelopes are detected by marker (native files are not).
    #[test]
    fn legacy_marker_sniff() {
        assert!(is_legacy_backup(
            br#"{"format":"lemma-backup","format_version":1,"database":"e30="}"#
        ));
        assert!(!is_legacy_backup(br#"{"format":"ltmrs-backup"}"#));
        assert!(!is_legacy_backup(b"not json at all"));
    }

    /// Refused imports leave the source byte-identical (RQ-26: the
    /// importer never mutates the original, even on rejection paths).
    #[test]
    fn refused_import_leaves_source_untouched() {
        use crate::backup::{BackupError, MAX_BACKUP_BYTES, verify_backup_file};
        use sha2::{Digest, Sha256};
        let dir = tempfile::tempdir().unwrap();
        let foreign = dir.path().join("legacy.lemma-backup");
        // Envelope shaped like the upstream writer (pinned baseline §backup).
        std::fs::write(
            &foreign,
            r#"{"format":"lemma-backup","format_version":1,"lemma_version":"0.21.0","schema_version":8,"created_at":"2026-01-01T00:00:00.000Z","database_sha256":"abc","database":"e30="}"#,
        )
        .unwrap();
        let before = Sha256::digest(std::fs::read(&foreign).unwrap());
        let err = verify_backup_file(&foreign, MAX_BACKUP_BYTES).unwrap_err();
        assert!(
            matches!(err, BackupError::LegacyUnsupported { .. }),
            "explicit refusal, got: {err}"
        );
        assert!(
            err.to_string().contains("RQ-21"),
            "reason recorded, got: {err}"
        );
        let after = Sha256::digest(std::fs::read(&foreign).unwrap());
        assert_eq!(before[..], after[..], "source untouched by refusal");
        assert!(!is_legacy_backup(b"{}"));
    }
}
