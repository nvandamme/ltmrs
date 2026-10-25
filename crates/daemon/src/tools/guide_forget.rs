//! guide_forget tool (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::GuideForgetArgs;
use ltmrs_domain::command::DomainResult;
use ltmrs_service::repository::{AdmittedScope, GuideMutation};

use super::err_result;
use super::guide_render::{guide_op_response, map_guide_tool_error, replay_recorded_guide_op};

pub(crate) fn exec_guide_forget(
    disp: &Dispatcher,
    _envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    args: &GuideForgetArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    if args.guide.trim().is_empty() {
        return Ok(err_result("'guide' parameter is required"));
    }
    // Receipted operation (P1-2): replay before planning. Runs under the
    // entry admission (no TTL revalidation mid-call).
    if let Some(replayed) = replay_recorded_guide_op(repo, admitted)? {
        return Ok(replayed);
    }
    let existing = repo.get_guide(&args.guide)?;
    if existing.is_none() {
        return Ok(err_result(&format!("Guide \"{}\" not found.", args.guide)));
    }
    // Single-transaction forget (re-review R3): reference removal and the
    // guide delete commit together — no dangling references to a deleted
    // guide and no surviving guide with half-removed references. Recorded
    // atomically with the operation receipt (P1-2): retries replay.
    let recorded = match repo.guide_mutation_idempotent(
        admitted,
        GuideMutation::Forget {
            name: args.guide.clone(),
        },
    ) {
        Ok(recorded) => recorded,
        Err(e) => return map_guide_tool_error(e),
    };
    guide_op_response(&recorded)
}

#[cfg(test)]
#[cfg(test)]
use super::test_support::*;
#[cfg(test)]
use ltmrs_compat::lemma::tool_args::{GuideCreateArgs, GuideDistillArgs, GuideGetArgs, ToolArgs};

#[test]
fn guide_forget_removes_guide() {
    let (disp, _dir) = test_dispatcher();
    let env = tool_call(
        1,
        ToolArgs::GuideCreate(GuideCreateArgs {
            guide: "temp".to_string(),
            category: "dev-tool".to_string(),
            description: "temp guide".to_string(),
            contexts: vec![],
            learnings: vec![],
        }),
    );
    run(
        &disp,
        &env,
        &ToolArgs::GuideCreate(GuideCreateArgs {
            guide: "temp".to_string(),
            category: "dev-tool".to_string(),
            description: "temp guide".to_string(),
            contexts: vec![],
            learnings: vec![],
        }),
    );

    let env2 = tool_call(
        2,
        ToolArgs::GuideForget(GuideForgetArgs {
            guide: "temp".to_string(),
        }),
    );
    let result = run(
        &disp,
        &env2,
        &ToolArgs::GuideForget(GuideForgetArgs {
            guide: "temp".to_string(),
        }),
    );
    assert!(!result_is_error(&result));
    assert!(result_text(&result).contains("Successfully forgot guide: temp"));

    // Verify it's gone.
    let env3 = tool_call(
        3,
        ToolArgs::GuideGet(GuideGetArgs {
            guide: Some("temp".to_string()),
            ..Default::default()
        }),
    );
    let result3 = run(
        &disp,
        &env3,
        &ToolArgs::GuideGet(GuideGetArgs {
            guide: Some("temp".to_string()),
            ..Default::default()
        }),
    );
    assert!(result_text(&result3).contains("Guide not found"));
}

#[test]
fn guide_forget_removes_memory_references() {
    let (disp, _dir) = test_dispatcher();
    // Create a memory and distill it into a guide (sets related_guides).
    let mem_id = add_fragment(
        &disp,
        1,
        "## Distillable pattern\n\n### Context\nReusable skill.",
    );
    let distill = ToolArgs::GuideDistill(GuideDistillArgs {
        memory_id: mem_id.clone(),
        guide: "react".to_string(),
        category: Some("web-frontend".to_string()),
    });
    let env = tool_call(2, distill.clone());
    run(&disp, &env, &distill);

    // Confirm the memory now references the guide.
    let eid = disp.repo().resolve_id(&mem_id).unwrap();
    let mems = disp.repo().get_memories(&[eid]).unwrap();
    assert!(mems[0].related_guides.iter().any(|g| g == "react"));

    // Forget the guide.
    let forget = ToolArgs::GuideForget(GuideForgetArgs {
        guide: "react".to_string(),
    });
    let env2 = tool_call(3, forget.clone());
    let result = run(&disp, &env2, &forget);
    assert!(!result_is_error(&result));

    // The memory must no longer reference the forgotten guide.
    let mems = disp.repo().get_memories(&[eid]).unwrap();
    assert!(
        !mems[0]
            .related_guides
            .iter()
            .any(|g| g.eq_ignore_ascii_case("react")),
        "related_guides should not reference a forgotten guide"
    );
}

/// P1-2: a retried `guide_forget` must replay "Successfully forgot"
/// instead of failing with "not found".
#[test]
fn guide_forget_retry_replays_recorded_success() {
    let (disp, _dir) = test_dispatcher();
    let create = ToolArgs::GuideCreate(GuideCreateArgs {
        guide: "alpha".to_string(),
        category: "dev-tool".to_string(),
        description: "alpha guide".to_string(),
        contexts: vec![],
        learnings: vec![],
    });
    run(&disp, &tool_call(1, create.clone()), &create);
    let forget = ToolArgs::GuideForget(GuideForgetArgs {
        guide: "alpha".to_string(),
    });
    let env = tool_call(2, forget.clone());
    let first = run(&disp, &env, &forget);
    assert!(
        !result_is_error(&first),
        "forget failed: {}",
        result_text(&first)
    );
    let second = run(&disp, &env, &forget);
    assert!(
        !result_is_error(&second),
        "forget retry must replay success, got: {}",
        result_text(&second)
    );
    assert_eq!(result_text(&second), result_text(&first));
}

/// Guide removal rewrites references the same way: no document churn,
/// entity revision advance preserved.
#[test]
fn guide_remove_keeps_document_revision() {
    let (disp, _dir) = test_dispatcher();
    add_fragment(&disp, 1, "## Linked\n\n### Context\nbody");
    let create = ToolArgs::GuideCreate(GuideCreateArgs {
        guide: "old".to_string(),
        category: "dev-tool".to_string(),
        description: "old guide".to_string(),
        contexts: vec![],
        learnings: vec![],
    });
    let create_env = tool_call(2, create.clone());
    run(&disp, &create_env, &create);
    let repo = disp.repo();
    let mut m = repo.export_snapshot().unwrap().memories.remove(0);
    m.related_guides = vec!["old".to_string()];
    repo.put_memory_direct(&m).unwrap();
    let rev = m.document_revision;
    let rev_entity = m.entity_revision;
    assert!(repo.forget_guide_atomically("old").unwrap());
    assert!(repo.get_guide("old").unwrap().is_none());
    let after = repo.get_memories(&[m.id]).unwrap().remove(0);
    assert!(after.related_guides.is_empty());
    assert_eq!(
        after.document_revision, rev,
        "unindexed remove must not churn the revision"
    );
    assert_eq!(
        after.entity_revision,
        rev_entity.next(),
        "entity revision still advances for conflict detection"
    );
}
