//! backup_create / backup_preview / backup_restore tools (moved verbatim from `tools.rs`).

use std::collections::BTreeMap;

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::{BackupCreateArgs, BackupPreviewArgs, BackupRestoreArgs};
use ltmrs_domain::command::DomainResult;

use super::{err_result, ok_result};

// ---- backup_create (WP-11a; native tool) ----

/// Back up the canonical store to one verified native archive. `directory`
/// is required (Usage-style soft error when absent — ltmrs invents no
/// default backup location, unlike the upstream default; recorded in the
/// native tool description).
pub(crate) fn exec_backup_create(
    disp: &Dispatcher,
    args: &BackupCreateArgs,
) -> DomainResult<DomainPayload> {
    let dir = match args.directory.as_deref().map(str::trim) {
        Some(d) if !d.is_empty() => d.to_string(),
        _ => {
            return Ok(err_result(
                "backup_create requires a destination `directory` (created when missing)",
            ));
        }
    };
    let now = disp.clock().now_millis();
    let report = ltmrs_interchange::backup::export_backup_with_limit(
        disp.repo(),
        std::path::Path::new(&dir),
        "ltmrs",
        now,
        ltmrs_interchange::backup::backup_byte_limit(),
    )
    .map_err(|e| {
        ltmrs_domain::command::DomainError::new(
            ltmrs_domain::command::DomainErrorCode::Validation,
            format!("backup failed: {e}"),
        )
    })?;
    let count = |key: &str| report.counts.get(key).copied().unwrap_or(0);
    let text = format!(
        "Backed up {} memories, {} guides ({} sessions) to {}\nDigest: {}",
        count("memories"),
        count("guides"),
        count("sessions"),
        report.path.display(),
        report.digest,
    );
    Ok(ok_result(
        text,
        serde_json::json!({
            "path": report.path.to_string_lossy(),
            "digest": report.digest,
            "counts": report.counts,
        }),
    ))
}

/// Live per-collection counts in manifest shape (for preview comparison).
fn live_counts(disp: &Dispatcher) -> DomainResult<BTreeMap<String, u64>> {
    let export = disp.repo().export_full()?;
    let count = |n: usize| n as u64;
    Ok(BTreeMap::from([
        ("memories".to_string(), count(export.memories.len())),
        ("relations".to_string(), count(export.relations.len())),
        ("guides".to_string(), count(export.guides.len())),
        ("sessions".to_string(), count(export.sessions.len())),
        ("feedback".to_string(), count(export.feedback.len())),
        ("suggestions".to_string(), count(export.suggestions.len())),
        ("projects".to_string(), count(export.projects.len())),
        ("archives".to_string(), count(export.archives.len())),
        ("history".to_string(), count(export.history.len())),
    ]))
}

// ---- backup_preview / backup_restore (WP-11b; native tools) ----

/// Preview a native backup without replacing anything: verify the file,
/// compare counts, check cooperating connections, and on readiness issue a
/// single-use TTL-bound token (bound to file digest, store generation and
/// channel, mirroring the upstream readiness contract).
pub(crate) fn exec_backup_preview(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    args: &BackupPreviewArgs,
) -> DomainResult<DomainPayload> {
    use ltmrs_interchange::backup::{backup_byte_limit, verify_backup_file};
    let path = match args.path.as_deref().map(str::trim) {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => {
            return Ok(err_result(
                "backup_preview requires a `path` to a .ltmrs-backup file",
            ));
        }
    };
    let verified =
        verify_backup_file(std::path::Path::new(&path), backup_byte_limit()).map_err(|e| {
            ltmrs_domain::command::DomainError::new(
                ltmrs_domain::command::DomainErrorCode::Validation,
                format!("backup preview failed: {e}"),
            )
        })?;
    let live = live_counts(disp)?;
    let generation = disp
        .repo()
        .store_generation()
        .map_err(|e| {
            ltmrs_domain::command::DomainError::new(
                ltmrs_domain::command::DomainErrorCode::Validation,
                format!("backup preview failed: {}", e.message),
            )
        })?
        .as_u64();
    let now = disp.clock().now_millis();
    // Readiness counts LIVE connections: persisted channel bindings
    // outlive their runs (every restart binds anew) and must never
    // block a restore after a daemon restart.
    let channels = disp.registry().live_connection_count();
    let channel = envelope.channel_id.as_uuid().to_string();
    let live_op_seq = disp.repo().op_seq().map_err(|e| {
        ltmrs_domain::command::DomainError::new(
            ltmrs_domain::command::DomainErrorCode::Validation,
            format!("backup preview failed: {}", e.message),
        )
    })?;
    let preview = disp
        .restore_coordinator()
        .preview(ltmrs_interchange::restore::PreviewRequest {
            backup: &verified,
            source_path: std::path::Path::new(&path),
            channel: &channel,
            live_counts: &live,
            live_generation: generation,
            active_channels: channels,
            now_millis: now,
            live_op_seq,
        });
    let text = if preview.ready {
        format!(
            "Restore preview: READY. {}\nConfirm replaces the live store (never merges): call backup_restore with the confirmation token and confirm=true.",
            preview.message
        )
    } else {
        format!(
            "Restore preview: BLOCKED. {}\nKeep this connection open and preview again after other connections close.",
            preview.message
        )
    };
    Ok(ok_result(
        text,
        serde_json::json!({
            "readiness": {"status": if preview.ready { "ready" } else { "blocked" }, "message": preview.message},
            "unknown_top_level": preview.unknown_top_level,
            "confirmation_token": preview.confirmation_token,
            "expires_at": preview.expires_at,
        }),
    ))
}

/// Restore a previewed backup (REPLACE, never merge): re-verify the file,
/// consume the single-use token, write a safety backup first, replace the
/// records including canonical sessions, bump the generation (invalidating
/// pre-restore pipelines and op receipts) and report. Rollback is a second
/// restore of the safety file.
pub(crate) fn exec_backup_restore(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    args: &BackupRestoreArgs,
) -> DomainResult<DomainPayload> {
    use ltmrs_domain::command::{DomainError, DomainErrorCode};
    use ltmrs_interchange::backup::{
        backup_byte_limit, encode_backup_with_limit, export_backup_to_with_limit,
        verify_backup_file,
    };
    use ltmrs_interchange::restore::RestoreError;
    use ltmrs_interchange::restore::verify::{restore_verified_guarded, safety_backup_path};
    let fail = |message: String| DomainError::new(DomainErrorCode::Validation, message);
    let token = match args.confirmation_token.as_deref().map(str::trim) {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => {
            return Ok(err_result(
                "backup_restore requires the `confirmation_token` from backup_preview (preview again for a fresh one)",
            ));
        }
    };
    if args.confirm != Some(true) {
        return Ok(err_result(
            "backup_restore replaces the live store (never merges). Pass confirm=true to acknowledge, or preview again.",
        ));
    }
    let now = disp.clock().now_millis();
    let channel = envelope.channel_id.as_uuid().to_string();
    let source = match disp.restore_coordinator().source_path(&token) {
        Some(p) => p,
        None => {
            return Ok(err_result(
                "unknown confirmation token (preview again for a fresh one)",
            ));
        }
    };
    let verified = verify_backup_file(&source, backup_byte_limit())
        .map_err(|e| fail(format!("backup restore failed: {e}")))?;
    // Exclusive restore fence (P1 restore quiescence): held from here
    // through the context reset, every mutating entry point blocks at
    // its shared fence instead of committing while this restore runs.
    // In-flight mutations drain before the fence is granted, so the
    // safety snapshot below contains every write acknowledged before
    // it — no acknowledged write can land between the safety backup
    // and the replace and be drained unseen. Released before the
    // sessions-file persist (file IO, no store interplay).
    let restore_guard = disp.repo().restore_write_guard();
    let live_generation = disp
        .repo()
        .store_generation()
        .map_err(|e| fail(format!("backup restore failed: {}", e.message)))?
        .as_u64();
    // Re-check the preview lease: a connection that arrived after the
    // preview may hold acknowledged writes the replace would drain unseen.
    // Writes that landed anyway (same channel, transient writers) are
    // counted, not refused: the replace drains them, so the report must
    // acknowledge the delta (recoverable from the safety backup).
    // Live connections again (see preview): history never blocks.
    let active_channels = disp.registry().live_connection_count();
    let live_op_seq = disp
        .repo()
        .op_seq()
        .map_err(|e| fail(format!("backup restore failed: {}", e.message)))?;
    let (_, live_writes_since_preview) = disp
        .restore_coordinator()
        .confirm(ltmrs_interchange::restore::ConfirmRequest {
            token: &token,
            confirm: true,
            digest: &verified.digest,
            live_generation,
            channel: &channel,
            active_channels,
            now_millis: now,
            live_op_seq,
        })
        .map_err(|e| match e {
            RestoreError::InvalidToken
            | RestoreError::Expired
            | RestoreError::AlreadyUsed
            | RestoreError::NeedsConfirm => fail(format!(
                "backup restore refused: {e} (preview again for a fresh token)"
            )),
            other => fail(format!("backup restore refused: {other}")),
        })?;
    // Safety backup of the live store first (rollback source on failure):
    // one coherent snapshot (sessions included), same as a fresh export.
    let safety_path = safety_backup_path(&source, now);
    let live_export = disp
        .repo()
        .export_full()
        .map_err(|e| fail(format!("safety backup failed: {}", e.message)))?;
    let limit = backup_byte_limit();
    let (safety_bytes, _, _) = encode_backup_with_limit(&live_export, live_generation, now, limit)
        .map_err(|e| fail(format!("safety backup failed: {e}")))?;
    export_backup_to_with_limit(&safety_path, &safety_bytes, limit)
        .map_err(|e| fail(format!("safety backup failed: {e}")))?;
    // Replace + bump in one durable transaction (P1-1): domain records,
    // canonical sessions from the backup, all op-receipt logs drained so
    // no pre-restore identity replays across the generation cut.
    // Rollback is a second restore of the safety file. Runs under the
    // exclusive fence acquired above (guard passed through).
    let new_generation = ltmrs_domain::id::StoreGeneration::new(live_generation + 1);
    let report = restore_verified_guarded(disp.repo(), &restore_guard, &verified, new_generation)
        .map_err(|e| fail(format!("backup restore failed: {e}")))?;
    // Generation cut invalidates every pre-restore execution context (P2-A):
    // traced routes, leases, virtual routes/leases and the virtual session
    // store are all reset (the next call on each channel binds fresh).
    // Still under the fence (registry only, no store interplay); the
    // fence releases before the sessions-file persist below (no-op
    // without a sessions path; loud failure otherwise — a crash before
    // the next persist must not reload contexts pointing at drained
    // sessions).
    let bindings_dropped = disp.registry().reset_execution_contexts();
    drop(restore_guard);
    disp.persist_sessions()
        .map_err(|e| fail(format!("backup restore failed: {e}")))?;
    let sessions_restored = report.restored.get("sessions").copied().unwrap_or(0);
    let quarantined = report
        .quarantined
        .iter()
        .map(|q| format!("{} (missing {})", q.relation, q.missing))
        .collect::<Vec<_>>()
        .join("; ");
    let text = format!(
        "Restored {} memories, {} guides, {} sessions from {}\nGeneration {} active; safety backup at {}.{}{}{}{}",
        report.restored.get("memories").copied().unwrap_or(0),
        report.restored.get("guides").copied().unwrap_or(0),
        sessions_restored,
        source.display(),
        report.generation,
        safety_path.display(),
        if report.sessions_marked_abandoned == 0 {
            String::new()
        } else {
            format!(
                "\n{} formerly-active session(s) marked abandoned.",
                report.sessions_marked_abandoned
            )
        },
        if quarantined.is_empty() {
            String::new()
        } else {
            format!("\nQuarantined (skipped, kept for repair): {quarantined}")
        },
        if report.unknown_top_level == 0 {
            String::new()
        } else {
            format!(
                "\n{} unknown top-level key(s) dropped (counted, not restored).",
                report.unknown_top_level
            )
        },
        if live_writes_since_preview == 0 {
            String::new()
        } else {
            format!(
                "\n{live_writes_since_preview} live write(s) landed after the preview and were replaced (recoverable from the safety backup)."
            )
        },
    );
    Ok(ok_result(
        text,
        serde_json::json!({
            "restored": report.restored,
            "quarantined": report.quarantined,
            "unknown_top_level": report.unknown_top_level,
            "generation": report.generation,
            "safety_backup": safety_path.to_string_lossy(),
            "restored_sessions": sessions_restored,
            "sessions_marked_abandoned": report.sessions_marked_abandoned,
            "bindings_dropped": bindings_dropped,
            "live_writes_since_preview": live_writes_since_preview,
        }),
    ))
}
