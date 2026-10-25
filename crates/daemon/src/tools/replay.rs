//! Sub-command identity, replay-before-validation and response freezing
//! for mutating memory tools (moved verbatim from `tools.rs`).
//!
//! Every tool-call step derives its own operation key so the receipt
//! ledger cannot replay the first command in place of the second.

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_domain::command::{
    CommandContext, CommandReceipt, DomainError, DomainErrorCode, DomainResult, OperationScope,
};
use ltmrs_domain::id::OperationId;
use ltmrs_service::repository::{AdmittedScope, ToolReplayStatus};

/// Derived sub-command identity for one tool-call step, shared by the
/// context builder below and the replay pre-check: the operation id and
/// digest are deterministic in the envelope, so a retried tool call
/// replays cleanly.
pub(crate) fn sub_command_parts(
    envelope: &IpcEnvelope,
    index: u32,
) -> DomainResult<(OperationId, String)> {
    let base_digest = envelope.request_digest()?;
    let op = OperationId::new(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("{}:cmd{}", envelope.operation_id.as_uuid(), index).as_bytes(),
    ));
    Ok((op, format!("{base_digest}:cmd{index}")))
}

/// Build a command context for the Nth sub-command of a tool call.
///
/// A tool call may issue several canonical commands (e.g. memory_add also
/// auto-links). Each needs its own operation key so the receipt ledger
/// cannot replay the first command in place of the second.
pub(crate) fn sub_command_ctx(envelope: &IpcEnvelope, index: u32) -> DomainResult<CommandContext> {
    let (op, digest) = sub_command_parts(envelope, index)?;
    let mut ctx = envelope.to_command_context(digest);
    ctx.operation_id = op;
    Ok(ctx)
}

/// Sub-command scope for one tool-call step: the envelope identity with
/// the derived sub-operation id and digest (shared by the replay check,
/// the gateway context, and the freeze below).
pub(crate) fn sub_scope(envelope: &IpcEnvelope, index: u32) -> DomainResult<OperationScope> {
    let (op, digest) = sub_command_parts(envelope, index)?;
    Ok(OperationScope {
        store_generation: envelope.store_generation,
        frontend_id: envelope.frontend_id,
        channel_id: envelope.channel_id,
        retry_epoch: envelope.retry_epoch,
        operation_id: op,
        request_digest: digest,
    })
}

/// Convert a frozen tool response back into its tool payload.
pub(crate) fn frozen_to_payload(
    frozen: &ltmrs_domain::session::FrozenToolResponse,
) -> DomainPayload {
    DomainPayload::ToolResult {
        text: frozen.text.clone(),
        structured: frozen.structured.clone(),
        is_error: frozen.is_error,
    }
}

/// Freeze a freshly rendered tool response under the primary
/// sub-command's scoped key (admitted: no revalidation past the
/// primary commit). Only tool results freeze; anything else skips
/// silently (no new failure mode on an already-rendered response).
pub(crate) fn freeze_tool_payload(
    disp: &Dispatcher,
    admitted: &AdmittedScope,
    scope: &OperationScope,
    payload: &DomainPayload,
) -> DomainResult<()> {
    if let DomainPayload::ToolResult {
        text,
        structured,
        is_error,
    } = payload
    {
        disp.repo().freeze_tool_result(
            admitted,
            scope,
            &ltmrs_domain::session::FrozenToolResponse {
                text: text.clone(),
                structured: structured.clone(),
                is_error: *is_error,
            },
        )?;
    }
    Ok(())
}

/// Frozen-response replay for session tools (P2-1): when this operation
/// already completed with a frozen response, return it verbatim instead of
/// recomputing from live state. Absent receipts (or legacy receipts without
/// a frozen response) fall through to the normal path, which recomputes
/// and then freezes.
pub(crate) fn replay_frozen_session_response(
    repo: &ltmrs_service::repository::CanonicalRepository,
    admitted: &AdmittedScope,
) -> DomainResult<Option<DomainPayload>> {
    match repo.session_receipt(admitted)? {
        Some(rec) => Ok(rec.response.map(|r| DomainPayload::ToolResult {
            text: r.text,
            structured: r.structured,
            is_error: r.is_error,
        })),
        None => Ok(None),
    }
}

/// Freeze a freshly computed session tool response into its receipt (P2-1),
/// so a lost-response retry returns the original verbatim. A digest
/// mismatch rejects as key reuse; any other failure is a wire error.
pub(crate) fn freeze_session_response(
    repo: &ltmrs_service::repository::CanonicalRepository,
    admitted: &AdmittedScope,
    payload: &DomainPayload,
) -> DomainResult<()> {
    use ltmrs_domain::session::FrozenToolResponse;
    let response = match payload {
        DomainPayload::ToolResult {
            text,
            structured,
            is_error,
        } => FrozenToolResponse {
            text: text.clone(),
            structured: structured.clone(),
            is_error: *is_error,
        },
        _ => {
            return Err(DomainError::new(
                DomainErrorCode::Validation,
                "only tool results can be frozen",
            ));
        }
    };
    repo.store_session_response(admitted, &response)
}

/// Replay-before-validation for mutating memory tools (P1 replay
/// depth): a frozen result returns verbatim (barriered); a receipt
/// without a frozen result (crash window) rebuilds from the recorded
/// receipt — never re-plans — then freezes; absence means fresh
/// execution proceeds. A stored receipt with a divergent digest
/// rejects as key reuse, exactly like the gateway.
pub(crate) fn replay_tool_call(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    index: u32,
    rebuild: impl FnOnce(&CommandReceipt) -> DomainResult<DomainPayload>,
) -> DomainResult<Option<DomainPayload>> {
    let scope = sub_scope(envelope, index)?;
    match disp.repo().check_tool_replay(&scope)? {
        ToolReplayStatus::Miss => Ok(None),
        ToolReplayStatus::Frozen(frozen) => Ok(Some(frozen_to_payload(&frozen))),
        ToolReplayStatus::Unfrozen(receipt) => {
            let payload = rebuild(&receipt)?;
            freeze_tool_payload(disp, admitted, &scope, &payload)?;
            Ok(Some(payload))
        }
    }
}
