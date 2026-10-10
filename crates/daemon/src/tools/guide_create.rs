//! guide_create tool (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::GuideCreateArgs;
use ltmrs_domain::command::DomainResult;
use ltmrs_domain::memory::Instant;
use ltmrs_service::repository::{AdmittedScope, GuideMutation};

use super::err_result;
use super::guide_render::{
    create_guide, guide_op_response, map_guide_tool_error, replay_recorded_guide_op,
};

pub(crate) fn exec_guide_create(
    disp: &Dispatcher,
    _envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    args: &GuideCreateArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    if args.guide.trim().is_empty()
        || args.category.trim().is_empty()
        || args.description.trim().is_empty()
    {
        return Ok(err_result(
            "'guide', 'category', and 'description' parameters are required",
        ));
    }
    // Receipted operation (P1-2): same operation ID + digest replays the
    // recorded response instead of re-executing. Runs under the entry
    // admission (no TTL revalidation mid-call).
    if let Some(replayed) = replay_recorded_guide_op(repo, admitted)? {
        return Ok(replayed);
    }
    let now = disp.clock().now_millis();

    if let Some(existing) = repo.get_guide(&args.guide)? {
        let expected = existing.entity_revision;
        let mut updated = existing;
        updated.description = args.description.clone();
        updated.updated_at = Instant::new(now);
        // Revision-checked: a concurrent mutation since the read rejects
        // instead of being overwritten (re-review P1-2).
        let recorded = match repo.guide_mutation_idempotent(
            admitted,
            GuideMutation::CreateUpdate {
                expected: Some(expected),
                guide: updated,
            },
        ) {
            Ok(recorded) => recorded,
            Err(e) => return map_guide_tool_error(e),
        };
        return guide_op_response(&recorded);
    }

    let guides = repo.get_guides()?;
    let normalized_lower = args.guide.to_lowercase();
    let normalized = normalized_lower.trim();
    if let Some(similar) = guides
        .iter()
        .find(|g| g.name.contains(normalized) || normalized.contains(g.name.as_str()))
    {
        let expected = similar.entity_revision;
        let mut updated = similar.clone();
        updated.description = args.description.clone();
        updated.updated_at = Instant::new(now);
        let recorded = match repo.guide_mutation_idempotent(
            admitted,
            GuideMutation::CreateUpdate {
                expected: Some(expected),
                guide: updated,
            },
        ) {
            Ok(recorded) => recorded,
            Err(e) => return map_guide_tool_error(e),
        };
        return guide_op_response(&recorded);
    }

    let new_guide = create_guide(
        &args.guide,
        &args.category,
        &args.description,
        &args.contexts,
        &args.learnings,
        now,
    );
    // Create-if-absent: a concurrent creation wins instead of being
    // overwritten (re-review P1-2).
    let recorded = match repo
        .guide_mutation_idempotent(admitted, GuideMutation::Create { guide: new_guide })
    {
        Ok(recorded) => recorded,
        Err(e) => return map_guide_tool_error(e),
    };
    guide_op_response(&recorded)
}

#[cfg(test)]
use super::test_support::*;
#[cfg(test)]
use ltmrs_compat::lemma::tool_args::ToolArgs;

/// P1-2: a retried `guide_create` (same envelope = lost response +
/// transport retry) must replay the recorded success, not fail with
/// "already exists".
#[test]
fn guide_create_retry_replays_recorded_success() {
    let (disp, _dir) = test_dispatcher();
    let args = ToolArgs::GuideCreate(GuideCreateArgs {
        guide: "alpha".to_string(),
        category: "dev-tool".to_string(),
        description: "alpha guide".to_string(),
        contexts: vec![],
        learnings: vec![],
    });
    let env = tool_call(1, args.clone());
    let first = run(&disp, &env, &args);
    assert!(
        !result_is_error(&first),
        "create failed: {}",
        result_text(&first)
    );
    let second = run(&disp, &env, &args);
    assert!(
        !result_is_error(&second),
        "create retry must replay success, got: {}",
        result_text(&second)
    );
    assert_eq!(result_text(&second), result_text(&first));
}
