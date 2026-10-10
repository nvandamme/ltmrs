//! memory_relate tool (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::MemoryRelateArgs;
use ltmrs_domain::command::{DomainCommand, DomainError, DomainErrorCode, DomainResult};
use ltmrs_domain::id::EntityId;
use ltmrs_domain::relation::RelationType;
use ltmrs_service::repository::AdmittedScope;
use serde_json::json;

use super::ids::resolve_id;
use super::replay::{freeze_tool_payload, replay_tool_call, sub_command_ctx, sub_scope};
use super::{err_result, new_relation, ok_result};

fn relation_exists(
    repo: &ltmrs_service::repository::CanonicalRepository,
    source: EntityId,
    target: EntityId,
    rtype: RelationType,
) -> DomainResult<bool> {
    Ok(repo.all_relations()?.iter().any(|r| {
        r.relation_type == rtype
            && ((r.source == source && r.target == target)
                // Symmetric relations keep one canonical edge: the reverse
                // endpoint order duplicates it (mirrors validate_new_edge).
                || (rtype.is_symmetric() && r.source == target && r.target == source))
    }))
}

pub(crate) fn exec_memory_relate(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    args: &MemoryRelateArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();

    // Replay before any mutable-state lookup (uniform tool rule): a retried
    // envelope whose endpoints have since vanished still replays its
    // recorded response instead of failing resolution.
    if let Some(replayed) = replay_tool_call(disp, envelope, admitted, 0, |_| {
        Ok(ok_result(
            format!(
                "Created relation: [{}] --{}--> [{}]{}",
                args.source_id,
                args.relation_type,
                args.target_id,
                args.note
                    .as_deref()
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default()
            ),
            json!({
                "success": true,
                "relation": args.relation_type,
            }),
        ))
    })? {
        return Ok(replayed);
    }

    let rtype = RelationType::parse(&args.relation_type).ok_or_else(|| {
        DomainError::new(
            DomainErrorCode::Validation,
            "'type' must be one of: contradicts, supersedes, supports, related_to".to_string(),
        )
    })?;

    let Ok(source_eid) = resolve_id(repo, &args.source_id) else {
        return Ok(err_result(&format!(
            "Source fragment [{}] not found",
            args.source_id
        )));
    };
    let Ok(target_eid) = resolve_id(repo, &args.target_id) else {
        return Ok(err_result(&format!(
            "Target fragment [{}] not found",
            args.target_id
        )));
    };

    if source_eid == target_eid {
        return Ok(err_result("sourceId and targetId cannot be the same"));
    }

    if repo.get_memories(&[source_eid])?.is_empty() {
        return Ok(err_result(&format!(
            "Source fragment [{}] not found",
            args.source_id
        )));
    }
    if repo.get_memories(&[target_eid])?.is_empty() {
        return Ok(err_result(&format!(
            "Target fragment [{}] not found",
            args.target_id
        )));
    }

    // Reject an identical existing edge.
    if relation_exists(repo, source_eid, target_eid, rtype)? {
        return Ok(err_result(&format!(
            "Relation already exists between [{}] and [{}] with type '{}'",
            args.source_id, args.target_id, args.relation_type
        )));
    }

    let relation = new_relation(
        envelope,
        disp.clock().now_millis(),
        source_eid,
        target_eid,
        rtype,
        args.note.clone(),
    );
    let cmd = DomainCommand::Relate { relation };
    let ctx = sub_command_ctx(envelope, 0)?;
    disp.repo().apply(&ctx, &cmd)?;

    let response = format!(
        "Created relation: [{}] --{}--> [{}]{}",
        args.source_id,
        args.relation_type,
        args.target_id,
        args.note
            .as_deref()
            .map(|n| format!(" ({n})"))
            .unwrap_or_default()
    );

    let structured = json!({
        "success": true,
        "relation": args.relation_type,
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
fn memory_relate_creates_relation() {
    let (disp, _dir) = test_dispatcher();
    let id1 = add_fragment(
        &disp,
        1,
        "## Relate Source A\n\n### Context\nSource of relation.",
    );
    let id2 = add_fragment(
        &disp,
        2,
        "## Relate Target B\n\n### Context\nTarget of relation.",
    );
    let env = tool_call(
        3,
        ToolArgs::MemoryRelate(MemoryRelateArgs {
            source_id: id1.clone(),
            target_id: id2.clone(),
            relation_type: "supports".to_string(),
            note: None,
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::MemoryRelate(MemoryRelateArgs {
            source_id: id1.clone(),
            target_id: id2.clone(),
            relation_type: "supports".to_string(),
            note: None,
        }),
    );
    assert!(!result_is_error(&result));
    assert!(result_text(&result).contains("Created relation"));
    let structured = result_structured(&result).unwrap();
    assert_eq!(structured["relation"], json!("supports"));
}

/// P1 (tool replay): the same MemoryRelate envelope twice must replay
/// success (exactly one edge) — never fail the second delivery on
/// the edge the first delivery created. (A *different* op id with the
/// same body still rejects as a duplicate; see below.)
#[test]
fn memory_relate_same_envelope_replays_success() {
    let (disp, _dir) = test_dispatcher();
    let id1 = add_fragment(&disp, 61, "## Replay Rel A\n\n### Context\nSource.");
    let id2 = add_fragment(&disp, 62, "## Replay Rel B\n\n### Context\nTarget.");
    let tool = ToolArgs::MemoryRelate(MemoryRelateArgs {
        source_id: id1.clone(),
        target_id: id2.clone(),
        relation_type: "supports".to_string(),
        note: None,
    });
    let env = tool_call(63, tool.clone());
    let first = run(&disp, &env, &tool);
    assert!(!result_is_error(&first));
    let second = run(&disp, &env, &tool);
    assert!(
        !result_is_error(&second),
        "same-envelope replay must succeed, got: {}",
        result_text(&second)
    );
    assert!(result_text(&second).contains("Created relation"));
}

/// P1 (symmetric uniqueness): A related_to B followed by B related_to A
/// (different operations) must reject as a duplicate — one canonical
/// edge per logical symmetric relation, never two directional rows.
#[test]
fn memory_relate_rejects_reversed_symmetric_duplicate() {
    let (disp, _dir) = test_dispatcher();
    // Near-disjoint token sets (Jaccard ~0.14): no auto-link may
    // pre-create either direction, isolating the reported scenario.
    let id1 = add_fragment(
        &disp,
        1,
        "## Zebras\n\n### Context\nPhotovoltaic inverters hummed quietly midnight zebra stripes savanna voltage.",
    );
    let id2 = add_fragment(
        &disp,
        2,
        "## Quilts\n\n### Context\nSourdough fermentation bubbles kitchen quilt stitching grandmother yeast.",
    );
    let forward = ToolArgs::MemoryRelate(MemoryRelateArgs {
        source_id: id1.clone(),
        target_id: id2.clone(),
        relation_type: "related_to".to_string(),
        note: None,
    });
    let result = run(&disp, &tool_call(3, forward.clone()), &forward);
    assert!(!result_is_error(&result));
    let backward = ToolArgs::MemoryRelate(MemoryRelateArgs {
        source_id: id2.clone(),
        target_id: id1.clone(),
        relation_type: "related_to".to_string(),
        note: None,
    });
    let result = run(&disp, &tool_call(4, backward.clone()), &backward);
    assert!(result_is_error(&result));
    assert!(
        result_text(&result).contains("already exists"),
        "reversed symmetric edge must reject as duplicate, got: {}",
        result_text(&result)
    );
}

#[test]
fn memory_relate_rejects_duplicate() {
    let (disp, _dir) = test_dispatcher();
    let id1 = add_fragment(
        &disp,
        1,
        "## Dup Relate A\n\n### Context\nFirst relation source.",
    );
    let id2 = add_fragment(
        &disp,
        2,
        "## Dup Relate B\n\n### Context\nFirst relation target.",
    );
    let tool = ToolArgs::MemoryRelate(MemoryRelateArgs {
        source_id: id1.clone(),
        target_id: id2.clone(),
        relation_type: "supports".to_string(),
        note: None,
    });
    let env1 = tool_call(3, tool.clone());
    let _ = run(&disp, &env1, &tool);
    let env2 = tool_call(4, tool.clone());
    let result = run(&disp, &env2, &tool);
    assert!(result_is_error(&result));
    assert!(result_text(&result).contains("already exists"));
}

#[test]
fn memory_relate_same_id_is_error() {
    let (disp, _dir) = test_dispatcher();
    let id = add_fragment(
        &disp,
        1,
        "## Self Relate\n\n### Context\nSelf relation test.",
    );
    let env = tool_call(
        2,
        ToolArgs::MemoryRelate(MemoryRelateArgs {
            source_id: id.clone(),
            target_id: id.clone(),
            relation_type: "supports".to_string(),
            note: None,
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::MemoryRelate(MemoryRelateArgs {
            source_id: id.clone(),
            target_id: id.clone(),
            relation_type: "supports".to_string(),
            note: None,
        }),
    );
    assert!(result_is_error(&result));
    assert!(result_text(&result).contains("cannot be the same"));
}
