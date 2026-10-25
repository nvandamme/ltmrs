//! session_attempt / session_end / session_stats tools (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::privacy;
use ltmrs_compat::lemma::tool_args::{SessionAttemptArgs, SessionEndArgs, SessionStatsArgs};
use ltmrs_domain::command::{DomainErrorCode, DomainResult};
use ltmrs_domain::id::SessionHandle;
use ltmrs_domain::session::{AttemptOutcome, Session, SessionOp, TaskOutcome};
use ltmrs_service::repository::AdmittedScope;
use serde_json::json;

use super::{err_result, format_result, key_reuse_result, ok_result};

// ---- session_attempt ----

/// Render + freeze a session-attempt response for an applied or replayed
/// attempt (shared by the fresh path and the receipt pre-check path, which
/// replays with the RECORDED handle when the session has since gone
/// terminal).
pub(crate) fn render_attempt_response(
    disp: &Dispatcher,
    admitted: &AdmittedScope,
    outcome: &AttemptOutcome,
    approach_redacted: &str,
    handle: SessionHandle,
    seq: u32,
) -> DomainResult<DomainPayload> {
    let value_tag = match outcome {
        AttemptOutcome::Rejected => "(dead end — most valuable)",
        AttemptOutcome::Partial => "(partial)",
        AttemptOutcome::Promising => "(promising)",
    };
    let preview = if approach_redacted.len() > 80 {
        let mut end = 80;
        while !approach_redacted.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &approach_redacted[..end])
    } else {
        approach_redacted.to_string()
    };
    let response = format!("Recorded attempt #{seq} — {preview} {value_tag}.");
    let data = json!({
        "recorded": true,
        "attempt_id": format!("{}#{}", handle.as_uuid(), seq),
    });
    let payload = ok_result(response, data);
    match super::replay::freeze_session_response(disp.repo(), admitted, &payload) {
        Ok(()) => {}
        Err(e) if e.code == DomainErrorCode::KeyReuseDifferentInput => {
            return key_reuse_result();
        }
        Err(e) => return Err(e),
    }
    Ok(payload)
}

pub(crate) fn exec_session_attempt(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    args: &SessionAttemptArgs,
) -> DomainResult<DomainPayload> {
    if args.approach.trim().is_empty() || args.outcome.trim().is_empty() {
        return Ok(err_result(
            "'approach' and 'outcome' are required for session_attempt.",
        ));
    }
    let outcome = match AttemptOutcome::parse(&args.outcome) {
        Some(o) => o,
        None => {
            return Ok(err_result(
                "'outcome' must be one of: rejected, partial, promising.",
            ));
        }
    };

    // Redact secrets from free-text fields (upstream redactSecrets).
    let approach_redacted = privacy::redact(&args.approach);
    let critique_redacted = args.critique.as_deref().map(privacy::redact);

    // Operation identity for the canonical call below.
    let digest = envelope.request_digest()?;
    let scope = envelope.operation_scope(digest.clone());

    // Replay before session routing: a retried envelope replays from its
    // durable receipt even when the session has since gone terminal (the
    // recorded handle routes the response, never the live binding).
    // Digest mismatch rejects as key reuse, like the tx path.
    match disp.repo().session_attempt_receipt(&scope) {
        Ok(Some((rec_handle, seq))) => {
            if let Some(frozen) =
                super::replay::replay_frozen_session_response(disp.repo(), admitted)?
            {
                return Ok(frozen);
            }
            return render_attempt_response(
                disp,
                admitted,
                &outcome,
                &approach_redacted,
                rec_handle,
                seq,
            );
        }
        Ok(None) => {}
        Err(e) if e.code == DomainErrorCode::KeyReuseDifferentInput => return key_reuse_result(),
        Err(e) => return Err(e),
    }

    // Resolve the channel's active session (canonical liveness).
    let session = disp.resolve_session(envelope.frontend_id, envelope.channel_id);
    let Some(handle) = session else {
        return Ok(err_result(
            "No active session. Call session_start before recording attempts.",
        ));
    };

    // Resolve the related memory ID (best-effort).
    let related_memory_id = args
        .related_memory_id
        .as_deref()
        .and_then(|id| disp.repo().resolve_id(id).ok());

    let now = disp.clock().now_millis();

    // ONE canonical operation (re-review P1-3): record, counters and
    // receipt commit together in the store. Replays resolve to the
    // recorded sequence number (the rebuilt response needs no live
    // session); digest mismatch rejects; barrier failures fail loudly.
    // A frozen response (P2-1) returns verbatim; otherwise the response is
    // rebuilt and then frozen.
    let (handle, seq) = match disp.repo().session_attempt_tx(
        &scope,
        handle,
        approach_redacted.clone(),
        outcome,
        critique_redacted.clone(),
        args.rationale.clone(),
        related_memory_id,
        now,
    ) {
        Ok(SessionOp::Applied((handle, seq))) => (handle, seq),
        Ok(SessionOp::Replayed((rec_handle, seq))) => {
            if let Some(frozen) =
                super::replay::replay_frozen_session_response(disp.repo(), admitted)?
            {
                return Ok(frozen);
            }
            (rec_handle, seq)
        }
        Ok(SessionOp::Conflict) => return key_reuse_result(),
        Err(e) => return Err(e),
    };
    render_attempt_response(disp, admitted, &outcome, &approach_redacted, handle, seq)
}

// ---- session_end ----

pub(crate) fn exec_session_end(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    args: &SessionEndArgs,
) -> DomainResult<DomainPayload> {
    if args.outcome.trim().is_empty() {
        return Ok(err_result("'outcome' parameter is required"));
    }
    let outcome = match TaskOutcome::parse(&args.outcome) {
        Some(o) => o,
        None => {
            return Ok(err_result(
                "'outcome' must be one of: success, partial, failure, abandoned.",
            ));
        }
    };

    let now = disp.clock().now_millis();

    // Operation identity for the canonical call below.
    let digest = envelope.request_digest()?;
    let scope = envelope.operation_scope(digest.clone());
    // Resolve the channel's session binding (live or terminal — replays
    // after a terminal session still resolve to the recorded outcome).
    let bound = disp
        .registry()
        .channel_session(envelope.frontend_id, envelope.channel_id);
    let Some(handle) = bound else {
        return Ok(err_result("No active session to end."));
    };

    // ONE canonical operation (re-review P1-3): required guide outcomes,
    // the terminal transition and the receipt commit in a single
    // transaction. A replay resolves; a digest mismatch rejects; barrier
    // failures fail loudly. Ending an already-terminal session reports
    // "no active session" (nothing was done, so nothing is recorded).
    // A frozen response (P2-1) returns verbatim with no further effects;
    // otherwise the response is rebuilt from canonical state and frozen.
    let improvement_lines = match disp.repo().session_end_tx(
        &scope,
        handle,
        outcome,
        args.final_approach.clone(),
        args.lessons.clone(),
        now,
    ) {
        Ok(SessionOp::Applied((_, lines, true))) => lines,
        Ok(SessionOp::Replayed((_, lines, _))) => {
            if let Some(frozen) =
                super::replay::replay_frozen_session_response(disp.repo(), admitted)?
            {
                return Ok(frozen);
            }
            lines
        }
        Ok(SessionOp::Applied((_, _, false))) => {
            return Ok(err_result("No active session to end."));
        }
        Ok(SessionOp::Conflict) => return key_reuse_result(),
        Err(e) => return Err(e),
    };

    // Improvement suggestions file inside session_end_tx (same atomic
    // boundary as the terminal transition): nothing to file here, and a
    // read-check-file tail would race duplicate deliveries into filing
    // twice. Replays return the frozen response above without side
    // effects; a fresh Applied already filed exactly its lines.

    // Rebuild the response from canonical state (identical on replay:
    // the session is terminal with the recorded outcome/lessons).
    let session_end_info = disp.repo().get_session(handle)?;
    let started = session_end_info
        .as_ref()
        .map(|s| s.started_at.as_millis())
        .unwrap_or(now);

    let mut response = format!("Session {} ended: {}\n", handle.as_uuid(), args.outcome);
    if let Some(s) = &session_end_info {
        response.push_str(&format!(
            "Task: {} | Duration: {} → {}\n",
            s.task_type.clone().unwrap_or_default(),
            super::recall::iso8601(started),
            super::recall::iso8601(now)
        ));
        if !s.lessons.is_empty() {
            response.push_str(&format!("Lessons: {} recorded\n", s.lessons.len()));
        }
    }
    if !improvement_lines.is_empty() {
        response.push_str(&format!(
            "\nIMPROVEMENT SUGGESTIONS:\n{}\n",
            improvement_lines.join("\n")
        ));
    }

    // Session review.
    if let Some(s) = &session_end_info
        && (!s.memories_read.is_empty()
            || !s.memories_created.is_empty()
            || !s.guides_used.is_empty())
    {
        response.push_str("\nSESSION REVIEW:");
        if !s.memories_read.is_empty() {
            response.push_str(&format!(
                "\n  Memories read: {}",
                s.memories_read
                    .iter()
                    .map(|m| format!("[{m}]"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !s.memories_created.is_empty() {
            response.push_str(&format!(
                "\n  Memories created: {}",
                s.memories_created
                    .iter()
                    .map(|m| format!("[{m}]"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !s.guides_used.is_empty() {
            response.push_str(&format!("\n  Guides used: {}", s.guides_used.join(", ")));
        }
    }

    let data = json!({
        "outcome_recorded": true,
        "suggestions": improvement_lines,
    });
    // No separate record/persist tail: the receipt committed atomically
    // with the end transition (and its barrier) in session_end_tx above.
    // The response freezes into the receipt (P2-1) so replays return it
    // verbatim instead of recomputing from live guide state.
    let payload = ok_result(response, data);
    match super::replay::freeze_session_response(disp.repo(), admitted, &payload) {
        Ok(()) => {}
        Err(e) if e.code == DomainErrorCode::KeyReuseDifferentInput => {
            return key_reuse_result();
        }
        Err(e) => return Err(e),
    }
    Ok(payload)
}

// ---- session_stats ----

pub(crate) fn exec_session_stats(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    args: &SessionStatsArgs,
) -> DomainResult<DomainPayload> {
    let count = args.count.unwrap_or(10);
    let format = args.response_format;

    // Canonical session snapshot (the registry holds bindings only).
    let sessions = disp.repo().all_sessions()?;

    // Recent completed sessions (most recent first).
    let mut completed: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.status == ltmrs_domain::session::SessionStatus::Ended)
        .collect();
    completed.sort_by_key(|s| std::cmp::Reverse(s.started_at.as_millis()));
    completed.truncate(count.min(5));

    // Active session for this channel (canonical liveness).
    let active = disp
        .resolve_session(envelope.frontend_id, envelope.channel_id)
        .and_then(|h| disp.repo().get_session(h).unwrap_or(None));

    let mut output = String::from("## Session Stats\n");
    if let Some(current) = &active {
        output.push_str(&format!(
            "Active session: {} tool calls\n",
            current.attempts.len()
        ));
        if !current.technologies.is_empty() {
            output.push_str(&format!(
                "Technologies: {}\n",
                current.technologies.join(", ")
            ));
        }
        if !current.guides_used.is_empty() {
            output.push_str(&format!(
                "Guides used: {}\n",
                current.guides_used.join(", ")
            ));
        }
        output.push('\n');
    }

    if !completed.is_empty() {
        output.push_str(&format!("Recent sessions ({}):\n", completed.len()));
        for s in &completed {
            let techs = if !s.technologies.is_empty() {
                format!(" [{}]", s.technologies.join(", "))
            } else {
                String::new()
            };
            output.push_str(&format!(
                "  {}: {} calls{techs}\n",
                s.handle.as_uuid(),
                s.attempts.len()
            ));
        }
    } else {
        output.push_str("No past sessions recorded yet.\n");
    }

    let data = json!({
        "active_session": active.as_ref().map(|c| {
            json!({
                "tool_calls": c.attempts.len(),
                "technologies": c.technologies,
                "guides_used": c.guides_used,
            })
        }),
        "recent_sessions": completed.iter().map(|s| {
            json!({
                "id": s.handle.as_uuid().to_string(),
                "duration_tool_calls": s.attempts.len(),
                "technologies": s.technologies,
            })
        }).collect::<Vec<_>>(),
    });
    Ok(format_result(output, data, format))
}
