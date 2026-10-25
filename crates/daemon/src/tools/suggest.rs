//! suggestion_respond tool (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::SuggestionRespondArgs;
use ltmrs_domain::command::{DomainErrorCode, DomainResult};
use ltmrs_domain::session::SuggestionStatus;
use ltmrs_service::repository::AdmittedScope;
use serde_json::json;

use super::{err_result, ok_result};

// ---- suggestion_respond ----

pub(crate) fn exec_suggestion_respond(
    disp: &Dispatcher,
    _envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    args: &SuggestionRespondArgs,
) -> DomainResult<DomainPayload> {
    let action = args.action.to_lowercase();
    if !matches!(action.as_str(), "accept" | "dismiss") {
        return Ok(err_result("'action' must be one of: accept, dismiss."));
    }
    let repo = disp.repo();
    let status = if action == "accept" {
        SuggestionStatus::Accepted
    } else {
        SuggestionStatus::Dismissed
    };

    // Receipted operation (P1-2): status transition + attempt adjustments
    // commit atomically with the receipt, so a retry replays instead of
    // adjusting twice. Runs under the entry admission.
    let now = disp.clock().now_millis();
    match repo.respond_suggestion_idempotent(admitted, args.id, status, now) {
        Ok(_) => {}
        Err(e)
            if e.code == DomainErrorCode::NotFound
                || e.code == DomainErrorCode::KeyReuseDifferentInput =>
        {
            return Ok(err_result(&e.message));
        }
        Err(e) => return Err(e),
    }

    let message = if action == "accept" {
        format!(
            "Accepted suggestion #{}. It will no longer be surfaced; related promising attempts were reinforced.",
            args.id
        )
    } else {
        format!(
            "Dismissed suggestion #{}. It will no longer be surfaced; related dead ends were de-prioritized.",
            args.id
        )
    };
    let data = json!({ "resolved": true, "id": args.id });
    Ok(ok_result(message, data))
}
