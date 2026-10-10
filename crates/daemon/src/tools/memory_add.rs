//! memory_add tool: orchestration, planning, rendering and session attribution
//! (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::privacy;
use ltmrs_compat::lemma::tool_args::MemoryAddArgs;
use ltmrs_domain::command::{DomainCommand, DomainError, DomainErrorCode, DomainResult};
use ltmrs_domain::id::{EntityId, SessionHandle};
use ltmrs_domain::memory::{Evidence, FragmentType, Instant, Memory, MemorySource};
use ltmrs_domain::relation::{Relation, RelationType};
use ltmrs_search::similarity::{
    AUTOLINK_JACCARD_BAND, DEDUP_JACCARD_THRESHOLD, SimilarityPurpose, SimilarityQuery,
};
use ltmrs_service::repository::AdmittedScope;
use serde_json::json;

use super::ids::{legacy_id_of, new_legacy_id};
use super::replay::{freeze_tool_payload, replay_tool_call, sub_command_ctx, sub_scope};
use super::text::{generate_description, generate_title, normalize_project};
use super::{err_result, new_relation, ok_result, similarity_service};

fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    result.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn exec_memory_add(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    args: &MemoryAddArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();

    // Privacy: redact secrets unless confirm=true (design 12.1, DEV-002).
    let has_secrets = privacy::contains_secret(&args.fragment);
    let final_fragment = if has_secrets && !args.confirm {
        privacy::redact(&args.fragment)
    } else {
        args.fragment.clone()
    };

    // Replay before validation: a retried envelope finds the memory the
    // first delivery created and would reject itself as a duplicate —
    // the recorded receipt rebuilds the response instead. The rebuild
    // first completes the session-attribution stage (dedup merge: safe
    // to re-run), then renders purely from the request + receipt (never
    // a live re-read): content, title and description are the requested
    // ones, so a concurrent edit after the commit cannot rewrite what
    // this operation reported. Live-derived sections (overlaps, link
    // details, suggestions) are omitted; the structured id and the core
    // line match the fresh render.
    if let Some(replayed) = replay_tool_call(disp, envelope, admitted, 0, |receipt| {
        match &receipt.outcome {
            ltmrs_domain::command::ReceiptOutcome::Success { affected } if !affected.is_empty() => {
            }
            _ => {
                return Err(DomainError::new(
                    DomainErrorCode::Validation,
                    "recorded add outcome carries no memory",
                ));
            }
        }
        let legacy_id = new_legacy_id(envelope);
        ensure_memory_created_link(
            disp,
            admitted,
            resolve_add_session(disp, envelope)?,
            &legacy_id,
        )?;
        let title = args
            .title
            .clone()
            .unwrap_or_else(|| generate_title(&final_fragment));
        let description = args
            .description
            .clone()
            .unwrap_or_else(|| generate_description(&final_fragment));
        let scope_info = args
            .project
            .as_deref()
            .and_then(normalize_project)
            .map(|p| format!(" (project: {p})"))
            .unwrap_or_else(|| " (global)".to_string());
        let mut response = format!(
            "Added fragment [{legacy_id}]{scope_info}: \"{title}\"\nSummary: {description}"
        );
        if has_secrets && !args.confirm {
            response.push_str(
                "\n\n⚠️ Privacy: potential secret(s) detected and auto-redacted. Use confirm: true to store as-is.",
            );
        }
        Ok(ok_result(
            response,
            json!({
                "success": true,
                "id": legacy_id,
                "conflicts": [],
            }),
        ))
    })? {
        return Ok(replayed);
    }

    // Deduplication through the one similarity contract: reject when the
    // ranked union of indexed candidates and pending writes scores at the
    // frozen rule. Held under the similarity gate with the commit below so
    // racing near-duplicates cannot both miss. The query project is
    // normalized like the stored form (a raw-case mismatch used to miss).
    let _similarity_guard = disp
        .similarity_gate()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let similar = similarity_service(disp)
        .find_similar_sync(&SimilarityQuery {
            text: final_fragment.clone(),
            project: args.project.as_deref().and_then(normalize_project),
            exclude: None,
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
                "A similar memory already exists [{sid}]: \"{}\"\nUse memory_update on [{sid}] if you want to modify it.",
                target.title
            )));
        }
    }

    // Resolve fragment type.
    let fragment_type = args
        .fragment_type
        .as_deref()
        .and_then(FragmentType::parse)
        .unwrap_or(FragmentType::Fact);

    // Resolve source.
    let source = args
        .source
        .as_deref()
        .and_then(MemorySource::parse)
        .unwrap_or(MemorySource::Ai);

    // Resolve project.
    let project = args.project.as_deref().and_then(normalize_project);

    // Generate title and description.
    let title = args
        .title
        .clone()
        .unwrap_or_else(|| generate_title(&final_fragment));
    let description = args
        .description
        .clone()
        .unwrap_or_else(|| generate_description(&final_fragment));

    // Build evidence.
    let evidence = args
        .evidence
        .as_ref()
        .map(|e| {
            vec![Evidence {
                file: e.file.clone(),
                symbol: e.symbol.clone(),
                snippet: e.snippet.clone(),
                snippet_sha256: sha256_hex(&e.snippet),
            }]
        })
        .unwrap_or_default();

    // Resolve the session link BEFORE building the record so attribution
    // lands in the single AddMemory apply: the channel's traced session
    // (canonical store) when present, else its virtual session —
    // session-less calls are attributed per channel, never silently
    // dropped (DEV-003: no daemon-global session). A two-step link would
    // blind-overwrite a concurrent change with no revision check and no
    // receipt.
    let session_handle = resolve_add_session(disp, envelope)?;
    let session_task_type = if disp.registry().virtual_record(session_handle).is_some() {
        String::new()
    } else {
        disp.repo()
            .get_session(session_handle)?
            .and_then(|s| s.task_type.clone())
            .unwrap_or_default()
    };

    // Build the memory.
    let legacy_id = new_legacy_id(envelope);
    let eid = EntityId::new(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("ltmrs:entity:{}", envelope.operation_id.as_uuid()).as_bytes(),
    ));
    let now = disp.clock().now_millis();
    let memory = Memory {
        id: eid,
        external_alias: Some(ltmrs_domain::id::ExternalAlias::new(legacy_id.clone())),
        title: title.clone(),
        fragment: final_fragment.clone(),
        description: description.clone(),
        fragment_type,
        project: project.clone(),
        source,
        confidence: 1.0,
        quality_score: None,
        lifecycle: ltmrs_domain::memory::MemoryLifecycle::Live,
        tags: Vec::new(),
        associated_with: Vec::new(),
        relations: Vec::new(),
        parent_id: None,
        child_ids: Vec::new(),
        session_id: Some(session_handle.as_uuid().to_string()),
        task_type: Some(session_task_type),
        related_guides: Vec::new(),
        evidence,
        access_count: 0,
        last_accessed_at: None,
        positive_feedback: 0,
        negative_feedback: 0,
        negative_hits: 0,
        refinement_count: 0,
        distill_candidate: matches!(fragment_type, FragmentType::Pattern | FragmentType::Lesson),
        entity_revision: ltmrs_domain::id::EntityRevision::new(0),
        document_revision: ltmrs_domain::id::DocumentRevision::new(0),
        eligibility_revision: ltmrs_domain::id::EligibilityRevision::new(0),
        created_at: Instant::new(now),
        updated_at: Instant::new(now),
        raw_created: None,
        unknown_fields: std::collections::BTreeMap::new(),
    };

    // Apply the command, with the planned auto-link inside the same
    // transaction (both endpoints live here: the memory below, the
    // target from the similarity union). Planning output rides the
    // receipt, so replay never re-plans it. The strongest global match
    // wins (the old scan took the first qualifying memory in export
    // order, not the best).
    let planned_link = similarity_service(disp)
        .find_similar_sync(&SimilarityQuery {
            text: final_fragment.clone(),
            project: None,
            exclude: Some(eid),
            limit: 5,
            purpose: SimilarityPurpose::AutoLink,
        })?
        .into_iter()
        .find(|h| AUTOLINK_JACCARD_BAND.contains(&h.score))
        .map(|strongest| {
            new_relation(
                envelope,
                disp.clock().now_millis(),
                eid,
                strongest.memory_id,
                RelationType::RelatedTo,
                Some(format!(
                    "Auto-linked: topic overlap ({:.2})",
                    strongest.score
                )),
            )
        });
    let cmd = DomainCommand::AddMemory {
        memory: memory.clone(),
        session: None,
        auto_link: planned_link.clone(),
    };
    let ctx = sub_command_ctx(envelope, 0)?;
    let receipt = disp.repo().apply(&ctx, &cmd)?;
    // The recorded link (if the transaction created it — a duplicate
    // edge skips) is what the response renders, never a recomputation.
    let recorded_link = match &receipt.outcome {
        ltmrs_domain::command::ReceiptOutcome::Success { affected } => {
            affected.get(1).copied().and_then(|id| {
                repo.all_relations()
                    .unwrap_or_default()
                    .into_iter()
                    .find(|r| r.id == id)
            })
        }
        _ => None,
    };

    // Attribute the created memory to the session in the canonical store
    // (deduped), or to the virtual record when session-less. A failed
    // link fails the tool: no success is frozen over a dropped canonical
    // effect, and the retry's completing replay re-runs this stage.
    ensure_memory_created_link(disp, admitted, session_handle, &legacy_id)?;

    let payload = finish_add_response(
        disp,
        args,
        &memory,
        &final_fragment,
        has_secrets,
        recorded_link,
    )?;
    freeze_tool_payload(disp, admitted, &sub_scope(envelope, 0)?, &payload)?;
    Ok(payload)
}

/// Shared add-response tail for the fresh path: topic overlaps, the
/// recorded auto-link, privacy/distill notes and the structured payload.
/// The link section renders the transactionally recorded link only —
/// never plans or applies one (replay renders the frozen response
/// instead; see below).
pub(crate) fn finish_add_response(
    disp: &Dispatcher,
    args: &MemoryAddArgs,
    memory: &Memory,
    final_fragment: &str,
    has_secrets: bool,
    recorded_link: Option<Relation>,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    let export = repo.export_snapshot()?;
    let legacy_id = legacy_id_of(repo, memory);
    let eid = memory.id;
    let project = memory.project.clone();
    let title = memory.title.clone();
    let description = memory.description.clone();
    // Other overlaps, for the informational list (read-only: no effects).
    // Same contract as auto-link planning, globally score-ordered.
    let overlaps: Vec<Memory> = similarity_service(disp)
        .find_similar_sync(&SimilarityQuery {
            text: final_fragment.to_string(),
            project: None,
            exclude: Some(eid),
            limit: 5,
            purpose: SimilarityPurpose::AutoLink,
        })?
        .into_iter()
        .filter(|h| AUTOLINK_JACCARD_BAND.contains(&h.score))
        .filter_map(|h| {
            disp.repo()
                .get_memories(&[h.memory_id])
                .ok()?
                .into_iter()
                .next()
        })
        .collect();

    // Build response text.
    let scope_info = project
        .as_deref()
        .map(|p| format!(" (project: {p})"))
        .unwrap_or_else(|| " (global)".to_string());
    let mut response =
        format!("Added fragment [{legacy_id}]{scope_info}: \"{title}\"\nSummary: {description}");
    if memory.distill_candidate {
        response.push_str(&format!(
            "\nFlagged as distill candidate (type: {}).",
            memory.fragment_type.as_str()
        ));
    }
    if has_secrets && !args.confirm {
        response.push_str(
            "\n\n⚠️ Privacy: potential secret(s) detected and auto-redacted. Use confirm: true to store as-is.",
        );
    }

    // Recorded auto-link section (informational tail lists the other
    // overlaps without linking them).
    if let Some(link) = recorded_link {
        let target = export.memories.iter().find(|m| m.id == link.target);
        let (target_title, target_confidence, strongest_id) = match target {
            Some(t) => (t.title.clone(), t.confidence, legacy_id_of(repo, t)),
            None => (String::new(), 0.0, String::new()),
        };
        response.push_str("\n\nRelated memories (auto-linked to strongest match):");
        response.push_str(&format!(
            "\n  [{strongest_id}] \"{target_title}\" ({target_confidence:.2}) — AUTO-LINKED"
        ));
        for o in overlaps.iter().filter(|o| o.id != link.target).take(4) {
            response.push_str(&format!(
                "\n  [{}] \"{}\" ({:.2})",
                legacy_id_of(repo, o),
                o.title,
                o.confidence
            ));
        }
    }

    // Add suggestions for pattern/lesson.
    if matches!(
        memory.fragment_type,
        FragmentType::Pattern | FragmentType::Lesson
    ) {
        response.push_str(&format!(
            "\n\nSUGGESTED ACTIONS:\n- This is a {}. Consider guide_distill to promote it into a reusable skill.",
            memory.fragment_type.as_str()
        ));
    }

    // Distill candidate count suggestion (the recorded memory is already
    // in this snapshot, so no +1 adjustment is needed).
    let distill_count = export
        .memories
        .iter()
        .filter(|m| m.distill_candidate && m.lifecycle.is_recallable())
        .count();
    if distill_count >= 3 {
        response.push_str(&format!(
            "\n--- SUGGESTIONS ---\n  [*] {distill_count} memories marked as distill candidates. Consider promoting them to guides.\n---"
        ));
    }

    let structured = json!({
        "success": true,
        "id": legacy_id,
        "conflicts": [],
    });
    Ok(ok_result(response, structured))
}

/// Resolve the session an add attributes to: the channel's live canonical
/// session, else its virtual session (the link then becomes a registry
/// record instead of a canonical row write). Deterministic per channel,
/// so fresh execution and completing replay resolve identically.
pub(crate) fn resolve_add_session(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
) -> DomainResult<SessionHandle> {
    match disp.resolve_session(envelope.frontend_id, envelope.channel_id) {
        Some(handle) => Ok(handle),
        None => {
            let now = disp.clock().now_millis();
            Ok(disp.registry().ensure_virtual_session(
                envelope.frontend_id,
                envelope.channel_id,
                now,
            ))
        }
    }
}

/// Complete the memory-created session attribution (staged tool
/// completion): the canonical link is a dedup merge, so replay safely
/// re-runs it; virtual sessions track in the registry instead. Fails
/// loudly — callers freeze success only after this returns.
pub(crate) fn ensure_memory_created_link(
    disp: &Dispatcher,
    admitted: &AdmittedScope,
    session_handle: SessionHandle,
    legacy_id: &str,
) -> DomainResult<()> {
    if disp.registry().virtual_record(session_handle).is_some() {
        disp.registry()
            .track_virtual_created(session_handle, std::slice::from_ref(&legacy_id.to_string()));
        return Ok(());
    }
    disp.repo().track_session_link(
        admitted,
        session_handle,
        ltmrs_service::repository::SessionLinkField::MemoryCreated,
        std::slice::from_ref(&legacy_id.to_string()),
    )
}
