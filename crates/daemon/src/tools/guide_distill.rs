//! guide_distill tool (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::GuideDistillArgs;
use ltmrs_domain::command::DomainResult;
use ltmrs_service::repository::AdmittedScope;
use serde_json::json;

use super::guide_render::format_guide_detail;
use super::ids::resolve_id;
use super::{err_result, ok_result};

pub(crate) fn exec_guide_distill(
    disp: &Dispatcher,
    _envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    args: &GuideDistillArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    if args.memory_id.trim().is_empty() || args.guide.trim().is_empty() {
        return Ok(err_result(
            "'memory_id' and 'guide' parameters are required",
        ));
    }
    // Replay before any mutable-state lookup (uniform tool rule): a retried
    // envelope whose memory has since vanished still replays its recorded
    // guide instead of failing resolution. Runs under the entry admission.
    match disp.repo().read_recorded_distill_op(admitted) {
        Ok(None) => {}
        Ok(Some(recorded)) => {
            return Ok(ok_result(
                format!(
                    "Successfully distilled memory [{}] into guide \"{}\" ({}).\n\n{}",
                    args.memory_id,
                    recorded.name,
                    recorded.category,
                    format_guide_detail(&recorded)
                ),
                json!({
                    "success": true,
                    "guide": recorded.name,
                    "memory_id": args.memory_id,
                }),
            ));
        }
        Err(e) if e.code == ltmrs_domain::command::DomainErrorCode::KeyReuseDifferentInput => {
            return Ok(err_result(&e.message));
        }
        Err(e) => return Err(e),
    }
    let now = disp.clock().now_millis();
    let eid = match resolve_id(repo, &args.memory_id) {
        Ok(id) => id,
        Err(_) => {
            return Ok(err_result(&format!(
                "Memory fragment with ID '{}' not found.",
                args.memory_id
            )));
        }
    };
    let category = args
        .category
        .clone()
        .unwrap_or_else(|| "dev-tool".to_string());
    // ONE canonical operation (re-review R2): memory + guide are read fresh
    // inside the transaction and commit together — no stale clone can
    // overwrite a concurrent content update.
    let updated = match repo.distill_memory_link(admitted, eid, &args.guide, &category, now) {
        Ok(g) => g,
        Err(e) if e.code == ltmrs_domain::command::DomainErrorCode::NotFound => {
            return Ok(err_result(&format!(
                "Memory fragment with ID '{}' not found.",
                args.memory_id
            )));
        }
        Err(e) if e.code == ltmrs_domain::command::DomainErrorCode::KeyReuseDifferentInput => {
            return Ok(err_result(&e.message));
        }
        Err(e) => return Err(e),
    };

    let response = format!(
        "Successfully distilled memory [{}] into guide \"{}\" ({}).\n\n{}",
        args.memory_id,
        updated.name,
        updated.category,
        format_guide_detail(&updated)
    );
    Ok(ok_result(
        response,
        json!({
            "success": true,
            "guide": updated.name,
            "memory_id": args.memory_id,
        }),
    ))
}

#[cfg(test)]
use super::test_support::*;
#[cfg(test)]
use ltmrs_compat::lemma::tool_args::{GuideGetArgs, MemoryUpdateArgs, ToolArgs};

#[test]
fn guide_distill_links_memory_to_guide() {
    let (disp, _dir) = test_dispatcher();
    // Create a memory.
    let mem_id = add_fragment(
        &disp,
        1,
        "## A pattern worth distilling\n\n### Context\nReusable skill.",
    );
    // Distill it.
    let env = tool_call(
        2,
        ToolArgs::GuideDistill(GuideDistillArgs {
            memory_id: mem_id.clone(),
            guide: "react".to_string(),
            category: Some("web-frontend".to_string()),
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::GuideDistill(GuideDistillArgs {
            memory_id: mem_id.clone(),
            guide: "react".to_string(),
            category: Some("web-frontend".to_string()),
        }),
    );
    assert!(!result_is_error(&result));
    assert!(result_text(&result).contains("Successfully distilled memory"));

    // The guide now contains the memory's fragment as a learning.
    let env2 = tool_call(
        3,
        ToolArgs::GuideGet(GuideGetArgs {
            guide: Some("react".to_string()),
            ..Default::default()
        }),
    );
    let result2 = run(
        &disp,
        &env2,
        &ToolArgs::GuideGet(GuideGetArgs {
            guide: Some("react".to_string()),
            ..Default::default()
        }),
    );
    assert!(result_text(&result2).contains("A pattern worth distilling"));
}

/// Re-review R2: distilling after a concurrent content update preserves
/// the new content and still links the guide; distilling first then
/// updating keeps both effects as well.
#[test]
fn guide_distill_preserves_concurrent_content_update() {
    let (disp, _dir) = test_dispatcher();
    let mem_id = add_fragment(&disp, 1, "## Linked\n\n### Context\noriginal body");
    // Concurrent content update through the canonical tool path.
    let update = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
        id: mem_id.clone(),
        fragment: Some("## Linked\n\n### Context\nnew body".to_string()),
        ..Default::default()
    });
    let update_env = tool_call(2, update.clone());
    assert!(!result_is_error(&run(&disp, &update_env, &update)));
    // Distill after the update: new content must survive with the link.
    let distill = ToolArgs::GuideDistill(GuideDistillArgs {
        memory_id: mem_id.clone(),
        guide: "react".to_string(),
        category: Some("web-frontend".to_string()),
    });
    let env = tool_call(3, distill.clone());
    let result = run(&disp, &env, &distill);
    assert!(!result_is_error(&result));
    let eid = disp.repo().resolve_id(&mem_id).unwrap();
    let mems = disp.repo().get_memories(&[eid]).unwrap();
    assert!(
        mems[0].fragment.contains("new body"),
        "distill must preserve concurrent content, got: {}",
        mems[0].fragment
    );
    assert!(mems[0].related_guides.iter().any(|g| g == "react"));
    assert!(!mems[0].distill_candidate);
    let guide = disp.repo().get_guide("react").unwrap().unwrap();
    assert!(guide.learnings.iter().any(|l| l.contains("new body")));
    // Replay of the same distill operation: usage counted once.
    let replayed = run(&disp, &env, &distill);
    assert!(!result_is_error(&replayed));
    let guide = disp.repo().get_guide("react").unwrap().unwrap();
    assert_eq!(guide.usage_count, 1, "distill replay must not recount");
    // Same operation ID, different arguments: reject.
    let changed = ToolArgs::GuideDistill(GuideDistillArgs {
        memory_id: mem_id.clone(),
        guide: "other".to_string(),
        category: Some("web-frontend".to_string()),
    });
    let changed_env = tool_call(3, changed.clone());
    let changed_result = run(&disp, &changed_env, &changed);
    assert!(result_is_error(&changed_result));
    assert!(result_text(&changed_result).contains("different input"));
    // Unknown memory: honest error, never success.
    let bad = ToolArgs::GuideDistill(GuideDistillArgs {
        memory_id: "m000000000000".to_string(),
        guide: "react".to_string(),
        category: None,
    });
    let bad_env = tool_call(4, bad.clone());
    let bad_result = run(&disp, &bad_env, &bad);
    assert!(result_is_error(&bad_result));
}
