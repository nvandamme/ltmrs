//! memory_forget tool (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::MemoryForgetArgs;
use ltmrs_domain::command::{DomainCommand, DomainResult, ForgetMode, MemoryPatch};
use ltmrs_domain::memory::CONSOLIDATED_CONFIDENCE;
use ltmrs_service::repository::AdmittedScope;
use serde_json::json;

use super::ids::resolve_id;
use super::replay::{freeze_tool_payload, replay_tool_call, sub_command_ctx, sub_scope};
use super::{err_result, ok_result, stable_visible};

pub(crate) fn exec_memory_forget(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    args: &MemoryForgetArgs,
    resolved: &ltmrs_service::projects::ResolvedScope,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();

    // Replay before validation (uniform tool rule): after a hard
    // delete the target is gone, so only the receipt can answer.
    if let Some(replayed) = replay_tool_call(disp, envelope, admitted, 0, |_| {
        let response = if args.invalidate {
            format!(
                "Invalidated fragment [{}] — hidden from recall but preserved (content + history kept). Reversible.",
                args.id
            )
        } else if args.consolidate {
            format!(
                "Archived fragment [{}] — down-weighted to 0.05 (kept and reversible), not deleted. Pass consolidate=false to hard-delete.",
                args.id
            )
        } else {
            format!("Forgot fragment with ID: {}", args.id)
        };
        Ok(ok_result(
            response,
            json!({
                "success": true,
                "id": args.id,
            }),
        ))
    })? {
        return Ok(replayed);
    }

    let Ok(eid) = resolve_id(repo, &args.id) else {
        return Ok(err_result(&format!(
            "Fragment with ID '{}' not found",
            args.id
        )));
    };
    let mems = repo.get_memories(&[eid])?;
    if mems.is_empty() {
        return Ok(err_result(&format!(
            "Fragment with ID '{}' not found",
            args.id
        )));
    }
    // Stable-scope gate (Wave A3b): under a stable identity a foreign row
    // is missing (no leak, no cross-project forget). Legacy/global scopes
    // keep the permissive frozen behavior.
    if let Some(m) = mems.first()
        && !stable_visible(resolved, m)?
    {
        return Ok(err_result(&format!(
            "Fragment with ID '{}' not found",
            args.id
        )));
    }

    let response = if args.invalidate {
        // Logical invalidation: hide from recall, keep content + history.
        let cmd = DomainCommand::Forget {
            id: eid,
            mode: ForgetMode::Invalidate,
        };
        let ctx = sub_command_ctx(envelope, 0)?;
        disp.repo().apply(&ctx, &cmd)?;
        format!(
            "Invalidated fragment [{}] — hidden from recall but preserved (content + history kept). Reversible.",
            args.id
        )
    } else if args.consolidate {
        // Non-destructive archive: down-weight, keep the row.
        let patch = MemoryPatch {
            confidence: Some(CONSOLIDATED_CONFIDENCE),
            ..Default::default()
        };
        let cmd = DomainCommand::UpdateMemory {
            id: eid,
            expected_revision: None,
            patch,
        };
        let ctx = sub_command_ctx(envelope, 0)?;
        disp.repo().apply(&ctx, &cmd)?;
        format!(
            "Archived fragment [{}] — down-weighted to 0.05 (kept and reversible), not deleted. Pass consolidate=false to hard-delete.",
            args.id
        )
    } else {
        // Hard delete.
        let cmd = DomainCommand::Forget {
            id: eid,
            mode: ForgetMode::Delete,
        };
        let ctx = sub_command_ctx(envelope, 0)?;
        disp.repo().apply(&ctx, &cmd)?;
        format!("Forgot fragment with ID: {}", args.id)
    };

    let structured = json!({
        "success": true,
        "id": args.id,
    });
    // The row is gone or hidden: drop its cached signature so later
    // scans never match a stale fragment (re-extracted lazily if the
    // memory becomes live again).
    disp.conflict_signatures().prune(eid);
    let payload = ok_result(response, structured);
    freeze_tool_payload(disp, admitted, &sub_scope(envelope, 0)?, &payload)?;
    Ok(payload)
}
