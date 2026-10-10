//! memory_update tool (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::MemoryUpdateArgs;
use ltmrs_domain::command::{
    DomainCommand, DomainError, DomainErrorCode, DomainResult, MemoryPatch,
};
use ltmrs_search::similarity::{DEDUP_JACCARD_THRESHOLD, SimilarityPurpose, SimilarityQuery};
use ltmrs_service::repository::AdmittedScope;
use serde_json::json;

use super::ids::{legacy_id_of, resolve_id};
use super::replay::{freeze_tool_payload, replay_tool_call, sub_command_ctx, sub_scope};
use super::{err_result, ok_result, similarity_service};

pub(crate) fn exec_memory_update(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    args: &MemoryUpdateArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();

    // Replay before any mutable-state lookup (uniform tool rule): a retried
    // envelope whose target has since vanished still replays its recorded
    // response instead of failing resolution. Crash-window rebuild is a
    // pure function of the request + receipt: the pre-update title is not
    // in the receipt, so without an explicit title none is quoted — a
    // later rename must never rewrite what this operation reported.
    if let Some(replayed) = replay_tool_call(disp, envelope, admitted, 0, |receipt| {
        match &receipt.outcome {
            ltmrs_domain::command::ReceiptOutcome::Success { affected } if !affected.is_empty() => {
            }
            _ => {
                return Err(DomainError::new(
                    DomainErrorCode::Validation,
                    "recorded update outcome carries no memory",
                ));
            }
        }
        let response = match &args.title {
            Some(t) => format!("Updated fragment [{}]: \"{t}\"", args.id),
            None => format!("Updated fragment [{}].", args.id),
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
    let Some(m) = mems.first() else {
        return Ok(err_result(&format!(
            "Fragment with ID '{}' not found",
            args.id
        )));
    };

    // Validate confidence range.
    if let Some(c) = args.confidence
        && !(0.0..=1.0).contains(&c)
    {
        return Ok(err_result("'confidence' must be a number between 0 and 1"));
    }

    // Duplicate detection on fragment change, through the one similarity
    // contract (update scans every recallable memory except the target,
    // exactly as before). Held under the similarity gate with the commit
    // below so a racing add cannot slip a duplicate between check and
    // commit.
    let _similarity_guard = disp
        .similarity_gate()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(fragment) = &args.fragment {
        let similar = similarity_service(disp)
            .find_similar_sync(&SimilarityQuery {
                text: fragment.clone(),
                project: None,
                exclude: Some(eid),
                limit: 5,
                purpose: SimilarityPurpose::Dedup,
            })?
            .into_iter()
            .find(|h| h.score >= DEDUP_JACCARD_THRESHOLD);
        if let Some(similar) = similar {
            // A raced deletion between check and read means no duplicate.
            if let Some(target) = disp
                .repo()
                .get_memories(&[similar.memory_id])?
                .into_iter()
                .next()
            {
                let sid = legacy_id_of(repo, &target);
                return Ok(err_result(&format!(
                    "Similar fragment already exists: [{sid}] \"{}\". Use a different content or update the existing one.",
                    target.title
                )));
            }
        }
    }

    let patch = MemoryPatch {
        title: args.title.clone(),
        fragment: args.fragment.clone(),
        description: None,
        fragment_type: None,
        project: None,
        confidence: args.confidence,
        quality_score: None,
        tags: None,
        evidence: None,
    };

    let cmd = DomainCommand::UpdateMemory {
        id: eid,
        expected_revision: None,
        patch,
    };
    let ctx = sub_command_ctx(envelope, 0)?;
    disp.repo().apply(&ctx, &cmd)?;

    let display_title = args.title.clone().unwrap_or_else(|| m.title.clone());
    let response = format!("Updated fragment [{}]: \"{}\"", args.id, display_title);

    let structured = json!({
        "success": true,
        "id": args.id,
    });
    let payload = ok_result(response, structured);
    freeze_tool_payload(disp, admitted, &sub_scope(envelope, 0)?, &payload)?;
    Ok(payload)
}
