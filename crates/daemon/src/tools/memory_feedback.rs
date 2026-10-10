//! memory_feedback tool (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::MemoryFeedbackArgs;
use ltmrs_domain::command::{DomainCommand, DomainError, DomainErrorCode, DomainResult};
use ltmrs_service::repository::AdmittedScope;
use serde_json::json;

use super::ids::resolve_id;
use super::replay::{freeze_tool_payload, replay_tool_call, sub_command_ctx, sub_scope};
use super::{err_result, ok_result};

pub(crate) fn exec_memory_feedback(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    args: &MemoryFeedbackArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();

    // Replay before validation (uniform tool rule). Crash-window
    // rebuild is a pure function of the request + receipt: the recorded
    // absolute is not in the receipt, so the rebuild reports the
    // direction only — a later confidence move must never rewrite what
    // this operation reported.
    if let Some(replayed) = replay_tool_call(disp, envelope, admitted, 0, |receipt| {
        match &receipt.outcome {
            ltmrs_domain::command::ReceiptOutcome::Success { affected } if !affected.is_empty() => {
            }
            _ => {
                return Err(DomainError::new(
                    DomainErrorCode::Validation,
                    "recorded feedback outcome carries no memory",
                ));
            }
        }
        let response = if args.useful {
            format!("Positive feedback recorded for [{}].", args.id)
        } else {
            format!("Negative feedback recorded for [{}].", args.id)
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

    let cmd = DomainCommand::Feedback {
        memory_id: eid,
        useful: args.useful,
    };
    let ctx = sub_command_ctx(envelope, 0)?;
    disp.repo().apply(&ctx, &cmd)?;

    // Re-read to get the updated confidence.
    let updated_mems = repo.get_memories(&[eid])?;
    let updated = updated_mems.first().unwrap();
    let new_confidence = updated.confidence;

    let response = if args.useful {
        format!(
            "Positive feedback recorded for [{}]. Confidence boosted to {:.2}.",
            args.id, new_confidence
        )
    } else {
        format!(
            "Negative feedback recorded for [{}]. Confidence reduced to {:.2}.",
            args.id, new_confidence
        )
    };

    let structured = json!({
        "success": true,
        "id": args.id,
        "confidence": new_confidence,
    });
    let payload = ok_result(response, structured);
    freeze_tool_payload(disp, admitted, &sub_scope(envelope, 0)?, &payload)?;
    Ok(payload)
}
