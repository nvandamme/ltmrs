//! guide_update tool (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::GuideUpdateArgs;
use ltmrs_domain::command::DomainResult;
use ltmrs_domain::memory::Instant;
use ltmrs_service::repository::{AdmittedScope, GuideMutation};

use super::err_result;
use super::guide_render::{
    guide_op_response, map_guide_tool_error, merge_guide_refs, replay_recorded_guide_op,
};

pub(crate) fn exec_guide_update(
    disp: &Dispatcher,
    _envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    args: &GuideUpdateArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    if args.guide.trim().is_empty() {
        return Ok(err_result("'guide' parameter is required"));
    }
    // Receipted operation (P1-2): replay before planning, so a retry never
    // mistakes a concurrently changed store for a failure. Runs under the
    // entry admission (no TTL revalidation mid-call).
    if let Some(replayed) = replay_recorded_guide_op(repo, admitted)? {
        return Ok(replayed);
    }
    let now = disp.clock().now_millis();
    let mut guide = match repo.get_guide(&args.guide)? {
        Some(g) => g,
        None => return Ok(err_result(&format!("Guide \"{}\" not found.", args.guide))),
    };
    // Planning revision (re-review P1-2): every mutation below validates
    // against it, so a concurrent writer invalidates this stale plan
    // instead of being silently overwritten by it.
    let expected_revision = guide.entity_revision;

    let old_name = guide.name.clone();
    if let Some(new_name) = &args.new_name
        && !new_name.trim().is_empty()
    {
        guide.name = new_name.to_lowercase().trim().to_string();
    }
    if let Some(category) = &args.category
        && !category.trim().is_empty()
    {
        guide.category = category.to_lowercase().trim().to_string();
    }
    if let Some(description) = &args.description
        && !description.trim().is_empty()
    {
        guide.description = description.trim().to_string();
    }
    if !args.add_anti_patterns.is_empty() {
        guide.anti_patterns.extend(args.add_anti_patterns.clone());
    }
    if !args.add_pitfalls.is_empty() {
        guide.pitfalls.extend(args.add_pitfalls.clone());
    }
    if !args.add_depends_on.is_empty() {
        guide.depends_on = merge_guide_refs(&guide.depends_on, &args.add_depends_on, &guide.name);
    }
    if !args.add_enables.is_empty() {
        guide.enables = merge_guide_refs(&guide.enables, &args.add_enables, &guide.name);
    }
    if let Some(superseded_by) = &args.superseded_by
        && !superseded_by.trim().is_empty()
    {
        guide.superseded_by = Some(superseded_by.clone());
    }
    if args.deprecated {
        guide.deprecated = true;
    }
    guide.updated_at = Instant::new(now);

    // Rename path (re-review R3, single transaction): the renamed put,
    // memory reference moves and old-key delete commit together — a
    // failure anywhere leaves no half-rename. The planning revision
    // guards against concurrent updates (re-review P1-2). Recorded
    // atomically with the operation receipt (P1-2): retries replay.
    let mutation = if !old_name.eq_ignore_ascii_case(&guide.name) {
        GuideMutation::Update {
            expected: Some(expected_revision),
            guide: guide.clone(),
            old_name: Some(old_name.clone()),
        }
    } else {
        GuideMutation::Update {
            expected: Some(expected_revision),
            guide: guide.clone(),
            old_name: None,
        }
    };
    let recorded = match repo.guide_mutation_idempotent(admitted, mutation) {
        Ok(recorded) => recorded,
        Err(e) => return map_guide_tool_error(e),
    };
    guide_op_response(&recorded)
}

#[cfg(test)]
use super::test_support::*;
#[cfg(test)]
use ltmrs_compat::lemma::tool_args::{
    GuideCreateArgs, GuideDistillArgs, GuidePracticeArgs, MemoryUpdateArgs, ToolArgs,
};

#[test]
fn guide_update_renames_memory_references() {
    let (disp, _dir) = test_dispatcher();
    // Create a memory and distill it into a guide.
    let mem_id = add_fragment(
        &disp,
        1,
        "## Renamable pattern\n\n### Context\nReusable skill.",
    );
    let distill = ToolArgs::GuideDistill(GuideDistillArgs {
        memory_id: mem_id.clone(),
        guide: "react".to_string(),
        category: Some("web-frontend".to_string()),
    });
    let env = tool_call(2, distill.clone());
    run(&disp, &env, &distill);

    let eid = disp.repo().resolve_id(&mem_id).unwrap();
    let mems = disp.repo().get_memories(&[eid]).unwrap();
    assert!(mems[0].related_guides.iter().any(|g| g == "react"));

    // Rename the guide.
    let update = ToolArgs::GuideUpdate(GuideUpdateArgs {
        guide: "react".to_string(),
        new_name: Some("react18".to_string()),
        ..Default::default()
    });
    let env2 = tool_call(3, update.clone());
    let result = run(&disp, &env2, &update);
    assert!(!result_is_error(&result));

    // The memory must now reference the new name.
    let mems = disp.repo().get_memories(&[eid]).unwrap();
    assert!(
        mems[0]
            .related_guides
            .iter()
            .any(|g| g.eq_ignore_ascii_case("react18")),
        "related_guides should reference the renamed guide"
    );
    assert!(
        !mems[0]
            .related_guides
            .iter()
            .any(|g| g.eq_ignore_ascii_case("react")),
        "related_guides should not reference the old name"
    );
}

/// P1-2: a retried `guide_update` must not re-apply the field transform
/// (appending the anti-pattern a second time and advancing the revision
/// again) — same envelope replays the recorded outcome.
#[test]
fn guide_update_retry_does_not_reapply_transform() {
    let (disp, _dir) = test_dispatcher();
    let create = ToolArgs::GuideCreate(GuideCreateArgs {
        guide: "alpha".to_string(),
        category: "dev-tool".to_string(),
        description: "alpha guide".to_string(),
        contexts: vec![],
        learnings: vec![],
    });
    run(&disp, &tool_call(1, create.clone()), &create);
    let update = ToolArgs::GuideUpdate(GuideUpdateArgs {
        guide: "alpha".to_string(),
        new_name: None,
        category: None,
        description: None,
        add_anti_patterns: vec!["don't do X".to_string()],
        add_pitfalls: vec![],
        add_depends_on: vec![],
        add_enables: vec![],
        superseded_by: None,
        deprecated: false,
    });
    let env = tool_call(2, update.clone());
    let first = run(&disp, &env, &update);
    assert!(
        !result_is_error(&first),
        "update failed: {}",
        result_text(&first)
    );
    let second = run(&disp, &env, &update);
    assert!(
        !result_is_error(&second),
        "update retry must replay success, got: {}",
        result_text(&second)
    );
    assert_eq!(
        result_text(&second),
        result_text(&first),
        "retry must return the recorded response, not a re-applied one"
    );
    let stored = disp.repo().get_guide("alpha").unwrap().unwrap();
    assert_eq!(
        stored.anti_patterns,
        vec!["don't do X".to_string()],
        "anti-pattern applied exactly once, got: {:?}",
        stored.anti_patterns
    );
}

/// Guide renames rewrite references without churning revisions:
/// related_guides is not indexed text, so the document revision must
/// stay put (a bump without a projection job would skew canonical
/// ahead of the projection with no refresh coming).
#[test]
fn guide_rename_keeps_document_revision() {
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
    let existing = repo.get_guide("old").unwrap().expect("old exists");
    let mut renamed = existing.clone();
    renamed.name = "new".to_string();
    repo.rename_guide_atomically("old", existing.entity_revision, &renamed)
        .unwrap();
    let after = repo.get_memories(&[m.id]).unwrap().remove(0);
    assert_eq!(after.related_guides, vec!["new".to_string()]);
    assert!(repo.get_guide("old").unwrap().is_none());
    assert!(repo.get_guide("new").unwrap().is_some());
    assert_eq!(
        after.document_revision, rev,
        "unindexed rename must not churn the revision"
    );
    assert_eq!(
        after.entity_revision,
        rev_entity.next(),
        "entity revision still advances for conflict detection"
    );
}

/// P1 race: a guide-reference rename preserves a concurrent content
/// update (fresh-read patch, no stale-clone overwrite).
#[test]
fn guide_rename_preserves_concurrent_content_update() {
    let (disp, _dir) = test_dispatcher();
    let id = add_fragment(&disp, 1, "## Linked\n\n### Context\nbody");
    let repo = disp.repo();
    let m = repo.export_snapshot().unwrap().memories.remove(0);
    let mut seeded = m.clone();
    seeded.related_guides = vec!["old".to_string()];
    repo.put_memory_direct(&seeded).unwrap();
    // Concurrent content update through the canonical tool path.
    let update = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
        id: id.clone(),
        fragment: Some("## Linked\n\n### Context\nnew body".to_string()),
        ..Default::default()
    });
    let update_env = tool_call(2, update.clone());
    let update_result = run(&disp, &update_env, &update);
    assert!(!result_is_error(&update_result));
    // Guide rename after the content commit must keep the new text.
    let create = ToolArgs::GuideCreate(GuideCreateArgs {
        guide: "old".to_string(),
        category: "dev-tool".to_string(),
        description: "old guide".to_string(),
        contexts: vec![],
        learnings: vec![],
    });
    let create_env = tool_call(3, create.clone());
    run(&disp, &create_env, &create);
    let existing = repo.get_guide("old").unwrap().expect("old exists");
    let mut renamed = existing.clone();
    renamed.name = "new".to_string();
    repo.rename_guide_atomically("old", existing.entity_revision, &renamed)
        .unwrap();
    let after = repo.get_memories(&[m.id]).unwrap().remove(0);
    assert!(
        after.fragment.contains("new body"),
        "rename must preserve concurrent content, got: {}",
        after.fragment
    );
    assert_eq!(after.related_guides, vec!["new".to_string()]);
}

/// Re-review P1-2: a public `guide_practice` between rename planning
/// and commit rejects the stale rename; counters never regress.
#[test]
fn guide_rename_rejects_stale_plan_after_public_practice() {
    let (disp, _dir) = test_dispatcher();
    let create = ToolArgs::GuideCreate(GuideCreateArgs {
        guide: "old".to_string(),
        category: "dev-tool".to_string(),
        description: "old guide".to_string(),
        contexts: vec![],
        learnings: vec![],
    });
    run(&disp, &tool_call(1, create.clone()), &create);
    // Planning snapshot.
    let planned = disp.repo().get_guide("old").unwrap().unwrap();
    // Concurrent practice through the PUBLIC path (bumps revision).
    let practice = ToolArgs::GuidePractice(GuidePracticeArgs {
        guide: "old".to_string(),
        category: "dev-tool".to_string(),
        description: None,
        contexts: vec![],
        learnings: vec!["fresh learning".to_string()],
        outcome: None,
    });
    let practice_env = tool_call(2, practice.clone());
    assert!(!result_is_error(&run(&disp, &practice_env, &practice)));
    // Stale rename: explicit conflict, practiced state intact.
    let mut renamed = planned.clone();
    renamed.name = "new".to_string();
    let err = disp
        .repo()
        .rename_guide_atomically("old", planned.entity_revision, &renamed)
        .unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::RevisionConflict
    );
    let old = disp.repo().get_guide("old").unwrap().unwrap();
    assert_eq!(
        old.usage_count, 2,
        "create counts once, practice counts once more"
    );
    assert!(old.learnings.contains(&"fresh learning".to_string()));
    assert!(disp.repo().get_guide("new").unwrap().is_none());
}
