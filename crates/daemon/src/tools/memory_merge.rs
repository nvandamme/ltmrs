//! memory_merge tool (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::MemoryMergeArgs;
use ltmrs_domain::command::{DomainCommand, DomainResult};
use ltmrs_domain::id::EntityId;

use ltmrs_domain::memory::{FragmentType, Instant, Memory, MemorySource};
use ltmrs_service::repository::AdmittedScope;
use serde_json::json;

use super::ids::{new_legacy_id, resolve_id};
use super::replay::{freeze_tool_payload, replay_tool_call, sub_command_ctx, sub_scope};
use super::text::{generate_description, normalize_project};
use super::{err_result, ok_result};

pub(crate) fn exec_memory_merge(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    args: &MemoryMergeArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();

    if args.ids.len() < 2 {
        return Ok(err_result(
            "'ids' must be an array with at least 2 fragment IDs",
        ));
    }

    // Replay before validation (uniform tool rule): after the first
    // execution the sources are archived, so resolution would fail —
    // the recorded receipt rebuilds the response instead.
    if let Some(replayed) = replay_tool_call(disp, envelope, admitted, 0, |_| {
        let legacy_id = new_legacy_id(envelope);
        let project = args.project.as_deref().and_then(normalize_project);
        let scope_info = project
            .as_ref()
            .map(|p| format!(" (project: {p})"))
            .unwrap_or_else(|| " (global)".to_string());
        let response = if args.consolidate {
            format!(
                "Consolidated {} fragments into [{legacy_id}]{scope_info}: \"{}\"\nSuperseded (kept, down-weighted, reversible) IDs: {}",
                args.ids.len(),
                args.title,
                args.ids.join(", ")
            )
        } else {
            format!(
                "Merged {} fragments into [{legacy_id}]{scope_info}: \"{}\"\nRemoved IDs: {}",
                args.ids.len(),
                args.title,
                args.ids.join(", ")
            )
        };
        Ok(ok_result(
            response,
            json!({
                "success": true,
                "id": legacy_id,
                "merged_ids": args.ids,
            }),
        ))
    })? {
        return Ok(replayed);
    }

    // Resolve all source IDs; any missing is a hard error.
    let mut source_ids: Vec<EntityId> = Vec::new();
    let mut not_found: Vec<String> = Vec::new();
    for id in &args.ids {
        match resolve_id(repo, id) {
            Ok(eid) => {
                if repo.get_memories(&[eid])?.is_empty() {
                    not_found.push(id.clone());
                } else {
                    source_ids.push(eid);
                }
            }
            Err(_) => not_found.push(id.clone()),
        }
    }
    if !not_found.is_empty() {
        return Ok(err_result(&format!(
            "Fragment(s) not found: {}",
            not_found.join(", ")
        )));
    }

    // Build the merged memory.
    let legacy_id = new_legacy_id(envelope);
    let eid = EntityId::new(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("ltmrs:entity:{}", envelope.operation_id.as_uuid()).as_bytes(),
    ));
    let project = args.project.as_deref().and_then(normalize_project);
    let now = disp.clock().now_millis();
    let result = Memory {
        id: eid,
        external_alias: Some(ltmrs_domain::id::ExternalAlias::new(legacy_id.clone())),
        title: args.title.clone(),
        fragment: args.fragment.clone(),
        description: generate_description(&args.fragment),
        fragment_type: FragmentType::Fact,
        project: project.clone(),
        source: MemorySource::Ai,
        confidence: 1.0,
        quality_score: None,
        lifecycle: ltmrs_domain::memory::MemoryLifecycle::Live,
        tags: Vec::new(),
        associated_with: Vec::new(),
        relations: Vec::new(),
        parent_id: None,
        child_ids: Vec::new(),
        session_id: None,
        task_type: None,
        related_guides: Vec::new(),
        evidence: Vec::new(),
        access_count: 0,
        last_accessed_at: None,
        positive_feedback: 0,
        negative_feedback: 0,
        negative_hits: 0,
        refinement_count: 0,
        distill_candidate: false,
        entity_revision: ltmrs_domain::id::EntityRevision::new(0),
        document_revision: ltmrs_domain::id::DocumentRevision::new(0),
        eligibility_revision: ltmrs_domain::id::EligibilityRevision::new(0),
        created_at: Instant::new(now),
        updated_at: Instant::new(now),
        raw_created: None,
        unknown_fields: std::collections::BTreeMap::new(),
    };

    let cmd = DomainCommand::Merge {
        source_ids,
        result,
        consolidate: args.consolidate,
    };
    let ctx = sub_command_ctx(envelope, 0)?;
    // Merge changes recallability: commit under the similarity gate so a
    // concurrent mutation preflight cannot interleave its check here.
    {
        let _similarity_guard = disp
            .similarity_gate()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        disp.repo().apply(&ctx, &cmd)?;
    }

    // Consolidation edges record inside the merge transaction above: no
    // post-commit relation tail, which would reject on removed endpoints
    // and silently drop the edges.

    let scope_info = project
        .as_ref()
        .map(|p| format!(" (project: {p})"))
        .unwrap_or_else(|| " (global)".to_string());
    let response = if args.consolidate {
        format!(
            "Consolidated {} fragments into [{legacy_id}]{scope_info}: \"{}\"\nSuperseded (kept, down-weighted, reversible) IDs: {}",
            args.ids.len(),
            args.title,
            args.ids.join(", ")
        )
    } else {
        format!(
            "Merged {} fragments into [{legacy_id}]{scope_info}: \"{}\"\nRemoved IDs: {}",
            args.ids.len(),
            args.title,
            args.ids.join(", ")
        )
    };

    let structured = json!({
        "success": true,
        "id": legacy_id,
        "merged_ids": args.ids,
    });
    let payload = ok_result(response, structured);
    freeze_tool_payload(disp, admitted, &sub_scope(envelope, 0)?, &payload)?;
    Ok(payload)
}

#[cfg(test)]
use super::test_support::*;
#[cfg(test)]
use ltmrs_compat::lemma::tool_args::ToolArgs;

#[test]
fn memory_merge_combines_fragments() {
    let (disp, _dir) = test_dispatcher();
    let id1 = add_fragment(
        &disp,
        1,
        "## Merge Source One\n\n### Context\nFirst source of merge.",
    );
    let id2 = add_fragment(
        &disp,
        2,
        "## Merge Source Two\n\n### Context\nSecond source of merge.",
    );
    let env = tool_call(
        3,
        ToolArgs::MemoryMerge(MemoryMergeArgs {
            ids: vec![id1.clone(), id2.clone()],
            title: "Merged Result".to_string(),
            fragment: "## Merged Result\n\n### Context\nCombined content.".to_string(),
            project: None,
            consolidate: false,
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::MemoryMerge(MemoryMergeArgs {
            ids: vec![id1.clone(), id2.clone()],
            title: "Merged Result".to_string(),
            fragment: "## Merged Result\n\n### Context\nCombined content.".to_string(),
            project: None,
            consolidate: false,
        }),
    );
    assert!(!result_is_error(&result));
    assert!(result_text(&result).contains("Merged 2 fragments"));
    let structured = result_structured(&result).unwrap();
    assert_eq!(structured["success"], json!(true));
    assert_eq!(structured["merged_ids"].as_array().unwrap().len(), 2);
    // Frozen contract (consolidate=false): sources are deleted, not
    // recallable.
    for id in [&id1, &id2] {
        let eid = resolve_id(disp.repo(), id).unwrap();
        let mem = disp.repo().get_memories(&[eid]).unwrap().pop().unwrap();
        assert!(
            !mem.lifecycle.is_recallable(),
            "merged-away source must not be recallable"
        );
    }
}

#[test]
fn memory_merge_consolidate_keeps_and_down_weights_sources() {
    let (disp, _dir) = test_dispatcher();
    let id1 = add_fragment(
        &disp,
        1,
        "## Merge Keep One\n\n### Context\nFirst kept source.",
    );
    let id2 = add_fragment(
        &disp,
        2,
        "## Merge Keep Two\n\n### Context\nSecond kept source.",
    );
    let args = MemoryMergeArgs {
        ids: vec![id1.clone(), id2.clone()],
        title: "Merged Result".to_string(),
        fragment: "## Merged Result\n\n### Context\nCombined content.".to_string(),
        project: None,
        consolidate: true,
    };
    let env = tool_call(3, ToolArgs::MemoryMerge(args.clone()));
    let result = run(&disp, &env, &ToolArgs::MemoryMerge(args));
    assert!(!result_is_error(&result));
    assert!(result_text(&result).contains("Superseded"));
    // Frozen contract (consolidate=true): sources are kept live and
    // down-weighted, never archived.
    for id in [&id1, &id2] {
        let eid = resolve_id(disp.repo(), id).unwrap();
        let mem = disp.repo().get_memories(&[eid]).unwrap().pop().unwrap();
        assert!(
            mem.lifecycle.is_recallable(),
            "consolidated source stays recallable"
        );
        assert_eq!(mem.confidence, 0.05);
    }
}

#[test]
fn memory_merge_requires_two_ids() {
    let (disp, _dir) = test_dispatcher();
    let id1 = add_fragment(
        &disp,
        1,
        "## Single Source\n\n### Context\nOnly one source.",
    );
    let env = tool_call(
        2,
        ToolArgs::MemoryMerge(MemoryMergeArgs {
            ids: vec![id1.clone()],
            title: "Bad Merge".to_string(),
            fragment: "## Bad\n\n### Context\nToo few.".to_string(),
            project: None,
            consolidate: false,
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::MemoryMerge(MemoryMergeArgs {
            ids: vec![id1.clone()],
            title: "Bad Merge".to_string(),
            fragment: "## Bad\n\n### Context\nToo few.".to_string(),
            project: None,
            consolidate: false,
        }),
    );
    assert!(result_is_error(&result));
}
