//! Native restore state machine (WP-11b; T-BACKUP-02/03, T-REC-02).
//!
//! Preview issues a single-use, TTL-bound confirmation token bound to the
//! backup digest (and the live store generation); confirm re-validates
//! everything before replacing anything. Restore atomically replaces the
//! domain records, bumps the store generation (retiring pre-restore
//! pipelines, invalidating their publish rights), and reports quarantined
//! references plus abandoned sessions separately. Rollback is a second
//! restore of the safety backup the coordinator always writes first.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

mod coordinator;
#[cfg(test)]
mod restore_tests;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
mod test_support;
pub mod verify;
#[cfg(test)]
mod verify_tests;

/// Preview token TTL (mirrors the upstream 10-minute single-use token).
pub const RESTORE_PREVIEW_TTL_MILLIS: u64 = 10 * 60 * 1000;

/// Restore failure (every rejection names its cause; nothing is replaced
/// before all checks pass).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RestoreError {
    #[error("unknown confirmation token (preview again for a fresh one)")]
    InvalidToken,
    #[error("confirmation token expired (preview again for a fresh one)")]
    Expired,
    #[error("confirmation token already used (single-use; preview again)")]
    AlreadyUsed,
    #[error("restore requires explicit confirmation (confirm=true)")]
    NeedsConfirm,
    #[error("backup file changed since preview (digest {actual} != {expected}); preview again")]
    StaleSource { expected: String, actual: String },
    #[error("store changed since preview (generation {actual} != {expected}); preview again")]
    GenerationChanged { expected: u64, actual: u64 },
    #[error("restore blocked: {0}")]
    Blocked(String),
    #[error("store error: {0}")]
    Store(String),
    #[error("backup io at {path}: {message}")]
    Io { path: String, message: String },
}

/// Readiness followed by an optional single-use token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePreview {
    /// Whether confirm may proceed.
    pub ready: bool,
    /// Human-readable readiness state.
    pub message: String,
    /// Unknown top-level keys the backup carries (counted, dropped on
    /// restore, warned about here so confirm never drops them unseen).
    pub unknown_top_level: usize,
    /// Single-use token (None when blocked).
    pub confirmation_token: Option<String>,
    /// Token expiry millis (None when blocked).
    pub expires_at: Option<u64>,
}

/// A quarantined reference: skipped on restore, reported for manual repair.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct QuarantinedRef {
    /// Relation id (UUID string) that was skipped.
    pub relation: String,
    /// Missing endpoint description.
    pub missing: String,
}

/// Restore outcome report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    /// Records written per collection.
    pub restored: BTreeMap<String, u64>,
    /// References skipped with reasons.
    pub quarantined: Vec<QuarantinedRef>,
    /// New store generation activated.
    pub generation: u64,
    /// Unknown top-level keys the backup carried (counted, reported, not
    /// resurrected: untyped data has no keyspace to land in).
    pub unknown_top_level: usize,
    /// Backup sessions that were non-terminal and restored as Abandoned
    /// instead (a backup is knowledge, not a live lease).
    pub sessions_marked_abandoned: u64,
}

/// Daemon-lifetime preview registry (single-use TTL tokens bound to backup
/// digest + live store generation). Held behind a mutex by the dispatcher.
#[derive(Debug, Default)]
pub struct RestoreCoordinator {
    pending: HashMap<String, PreviewRecord>,
}

#[derive(Debug, Clone)]
struct PreviewRecord {
    digest: String,
    generation: u64,
    created_at: u64,
    used: bool,
    /// Backup file the digest was verified from (re-verified on confirm).
    source: PathBuf,
    /// Channel the preview came from (confirm must arrive on it).
    channel: String,
    /// Live mutation watermark at preview time (op_seq): writes landing
    /// between preview and confirm are acknowledged at confirm, never
    /// drained unseen.
    op_seq: u64,
}

/// Preview inputs bundled (keeps the coordinator call under control).
pub struct PreviewRequest<'a> {
    pub backup: &'a crate::backup::VerifiedBackup,
    pub source_path: &'a Path,
    pub channel: &'a str,
    pub live_counts: &'a BTreeMap<String, u64>,
    pub live_generation: u64,
    pub active_channels: usize,
    pub now_millis: u64,
    /// Live mutation watermark (repo op_seq) bound into the record.
    pub live_op_seq: u64,
}

/// Confirm inputs bundled (mirrors `PreviewRequest`; keeps the coordinator
/// call under control as checks accumulate).
pub struct ConfirmRequest<'a> {
    pub token: &'a str,
    pub confirm: bool,
    pub digest: &'a str,
    pub live_generation: u64,
    pub channel: &'a str,
    pub active_channels: usize,
    pub now_millis: u64,
    /// Live mutation watermark at confirm time (compared to the preview's).
    pub live_op_seq: u64,
}
