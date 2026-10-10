//! guide_merge tool (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::GuideMergeArgs;
use ltmrs_domain::command::DomainResult;
use ltmrs_domain::guide::Guide;
use ltmrs_domain::id::EntityRevision;
use ltmrs_service::repository::{AdmittedScope, GuideMutation};

use super::err_result;
use super::guide_render::{
    create_guide, guide_op_response, map_guide_tool_error, replay_recorded_guide_op,
};

pub(crate) fn exec_guide_merge(
    disp: &Dispatcher,
    _envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    args: &GuideMergeArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    if args.guides.len() < 2 {
        return Ok(err_result(
            "'guides' must be an array with at least 2 guide names",
        ));
    }
    if args.guide.trim().is_empty() || args.category.trim().is_empty() {
        return Ok(err_result("'guide' and 'category' parameters are required"));
    }
    // Receipted operation (P1-2): replay before planning, so a retry never
    // mistakes consumed sources for a failure. Runs under the entry
    // admission (no TTL revalidation mid-call).
    if let Some(replayed) = replay_recorded_guide_op(repo, admitted)? {
        return Ok(replayed);
    }
    let now = disp.clock().now_millis();

    let mut source_guides: Vec<Guide> = Vec::new();
    let mut not_found: Vec<String> = Vec::new();
    for name in &args.guides {
        match repo.get_guide(name)? {
            Some(g) => source_guides.push(g),
            None => not_found.push(name.clone()),
        }
    }
    if !not_found.is_empty() {
        return Ok(err_result(&format!(
            "Guide(s) not found: {}",
            not_found.join(", ")
        )));
    }

    let contexts = args.contexts.clone().unwrap_or_else(|| {
        let mut set: Vec<String> = Vec::new();
        for g in &source_guides {
            for c in &g.contexts {
                if !set.contains(c) {
                    set.push(c.clone());
                }
            }
        }
        set
    });
    let learnings = args.learnings.clone().unwrap_or_else(|| {
        let mut set: Vec<String> = Vec::new();
        for g in &source_guides {
            for l in &g.learnings {
                if !set.contains(l) {
                    set.push(l.clone());
                }
            }
        }
        set
    });
    let anti_patterns = dedup(
        source_guides
            .iter()
            .flat_map(|g| g.anti_patterns.iter().cloned())
            .collect::<Vec<_>>(),
    );
    let pitfalls = dedup(
        source_guides
            .iter()
            .flat_map(|g| g.pitfalls.iter().cloned())
            .collect::<Vec<_>>(),
    );

    let total_usage: u32 = source_guides.iter().map(|g| g.usage_count).sum();
    let mut new_guide = create_guide(
        &args.guide,
        &args.category,
        args.description.as_deref().unwrap_or(""),
        &contexts,
        &learnings,
        now,
    );
    new_guide.usage_count = total_usage;
    new_guide.anti_patterns = anti_patterns.clone();
    new_guide.pitfalls = pitfalls.clone();
    new_guide.source_memories = dedup(
        source_guides
            .iter()
            .flat_map(|g| g.source_memories.iter().cloned())
            .collect::<Vec<_>>(),
    );
    new_guide.validated_by = dedup(
        source_guides
            .iter()
            .flat_map(|g| g.validated_by.iter().cloned())
            .collect::<Vec<_>>(),
    );

    // Atomic single-transaction merge: references, source deletes and the
    // merged put commit together, guarded by the source revisions read
    // during planning (re-review R3). A concurrent source update rejects
    // explicitly instead of being silently discarded. Recorded atomically
    // with the operation receipt (P1-2): retries replay.
    let expected: Vec<(String, EntityRevision)> = source_guides
        .iter()
        .map(|g| (g.name.clone(), g.entity_revision))
        .collect();
    let recorded = match repo.guide_mutation_idempotent(
        admitted,
        GuideMutation::Merge {
            sources: args.guides.clone(),
            expected,
            result: new_guide,
        },
    ) {
        Ok(recorded) => recorded,
        Err(e) => return map_guide_tool_error(e),
    };
    guide_op_response(&recorded)
}

/// Order-preserving deduplication (upstream `[...new Set(...)]`).
fn dedup<T: PartialEq>(items: Vec<T>) -> Vec<T> {
    let mut out: Vec<T> = Vec::new();
    for item in items {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

#[cfg(test)]
use super::test_support::*;
#[cfg(test)]
use ltmrs_compat::lemma::tool_args::{GuideCreateArgs, GuideGetArgs, GuideUpdateArgs, ToolArgs};

/// P1 atomic merge: success moves all references and removes sources
/// together; a missing source fails without a half-merge.
#[test]
fn guide_merge_is_all_or_nothing() {
    let (disp, _dir) = test_dispatcher();
    for (op, name) in [(1, "alpha"), (2, "beta")] {
        let args = GuideCreateArgs {
            guide: name.to_string(),
            category: "dev-tool".to_string(),
            description: format!("{name} guide"),
            contexts: vec![],
            learnings: vec![],
        };
        let env = tool_call(op, ToolArgs::GuideCreate(args.clone()));
        run(&disp, &env, &ToolArgs::GuideCreate(args));
    }
    add_fragment(&disp, 10, "## M\n\n### Context\nbody");
    let repo = disp.repo();
    let mut m = repo.export_snapshot().unwrap().memories.remove(0);
    m.related_guides = vec!["alpha".to_string(), "beta".to_string()];
    repo.put_memory_direct(&m).unwrap();
    let args = GuideMergeArgs {
        guides: vec!["alpha".to_string(), "beta".to_string()],
        guide: "gamma".to_string(),
        category: "dev-tool".to_string(),
        description: Some("merged".to_string()),
        contexts: None,
        learnings: None,
    };
    let env = tool_call(11, ToolArgs::GuideMerge(args.clone()));
    let result = run(&disp, &env, &ToolArgs::GuideMerge(args));
    assert!(!result_is_error(&result));
    assert!(repo.get_guide("alpha").unwrap().is_none());
    assert!(repo.get_guide("beta").unwrap().is_none());
    assert!(repo.get_guide("gamma").unwrap().is_some());
    let after = repo.get_memories(&[m.id]).unwrap().remove(0);
    assert!(
        after.related_guides.iter().any(|g| g == "gamma"),
        "merged refs must point at gamma, got: {:?}",
        after.related_guides
    );
    assert!(
        !after
            .related_guides
            .iter()
            .any(|g| g == "alpha" || g == "beta"),
        "no stale source refs, got: {:?}",
        after.related_guides
    );
    // Missing source: no partial state (gamma stays, no new guide).
    let bad = GuideMergeArgs {
        guides: vec!["gamma".to_string(), "missing".to_string()],
        guide: "delta".to_string(),
        category: "dev-tool".to_string(),
        description: None,
        contexts: None,
        learnings: None,
    };
    let bad_env = tool_call(12, ToolArgs::GuideMerge(bad.clone()));
    let bad_result = run(&disp, &bad_env, &ToolArgs::GuideMerge(bad));
    assert!(result_is_error(&bad_result));
    assert!(repo.get_guide("gamma").unwrap().is_some());
    assert!(repo.get_guide("delta").unwrap().is_none());
}

/// Re-review P1-2: a public `guide_update` between merge planning and
/// commit rejects the stale merge instead of losing the update. The
/// update travels the real public tool path.
#[test]
fn guide_merge_rejects_stale_plan_after_public_update() {
    let (disp, _dir) = test_dispatcher();
    for (op, name) in [(1, "alpha"), (2, "beta")] {
        let args = GuideCreateArgs {
            guide: name.to_string(),
            category: "dev-tool".to_string(),
            description: format!("{name} guide"),
            contexts: vec![],
            learnings: vec![],
        };
        let env = tool_call(op, ToolArgs::GuideCreate(args.clone()));
        run(&disp, &env, &ToolArgs::GuideCreate(args));
    }
    // Planning snapshot: current revisions.
    let rev_alpha = disp
        .repo()
        .get_guide("alpha")
        .unwrap()
        .unwrap()
        .entity_revision;
    // Concurrent update through the PUBLIC path.
    let update = ToolArgs::GuideUpdate(GuideUpdateArgs {
        guide: "alpha".to_string(),
        new_name: None,
        category: None,
        description: None,
        add_anti_patterns: vec!["never skip validation".to_string()],
        add_pitfalls: vec![],
        add_depends_on: vec![],
        add_enables: vec![],
        superseded_by: None,
        deprecated: false,
    });
    let update_env = tool_call(3, update.clone());
    let update_result = run(&disp, &update_env, &update);
    assert!(!result_is_error(&update_result));
    // Merge commit with the stale plan: explicit conflict, update kept.
    let mut merged = disp.repo().get_guide("alpha").unwrap().unwrap();
    merged.name = "gamma".to_string();
    let stale = vec![
        ("alpha".to_string(), rev_alpha),
        (
            "beta".to_string(),
            disp.repo()
                .get_guide("beta")
                .unwrap()
                .unwrap()
                .entity_revision,
        ),
    ];
    let err = disp
        .repo()
        .merge_guides_atomically(&["alpha".to_string(), "beta".to_string()], &stale, &merged)
        .unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::RevisionConflict
    );
    let alpha = disp.repo().get_guide("alpha").unwrap().unwrap();
    assert!(
        alpha
            .anti_patterns
            .iter()
            .any(|p| p == "never skip validation"),
        "concurrent update must survive a rejected merge"
    );
    assert!(disp.repo().get_guide("gamma").unwrap().is_none());
}

#[test]
fn guide_merge_combines_guides() {
    let (disp, _dir) = test_dispatcher();
    // Create two guides.
    for (op, name) in [(1, "alpha"), (2, "beta")] {
        let env = tool_call(
            op,
            ToolArgs::GuideCreate(GuideCreateArgs {
                guide: name.to_string(),
                category: "dev-tool".to_string(),
                description: format!("{name} desc"),
                contexts: vec![format!("{name}-ctx")],
                learnings: vec![format!("{name}-learn")],
            }),
        );
        run(
            &disp,
            &env,
            &ToolArgs::GuideCreate(GuideCreateArgs {
                guide: name.to_string(),
                category: "dev-tool".to_string(),
                description: format!("{name} desc"),
                contexts: vec![format!("{name}-ctx")],
                learnings: vec![format!("{name}-learn")],
            }),
        );
    }

    // Merge them.
    let env = tool_call(
        3,
        ToolArgs::GuideMerge(GuideMergeArgs {
            guides: vec!["alpha".to_string(), "beta".to_string()],
            guide: "gamma".to_string(),
            category: "dev-tool".to_string(),
            description: Some("merged".to_string()),
            contexts: None,
            learnings: None,
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::GuideMerge(GuideMergeArgs {
            guides: vec!["alpha".to_string(), "beta".to_string()],
            guide: "gamma".to_string(),
            category: "dev-tool".to_string(),
            description: Some("merged".to_string()),
            contexts: None,
            learnings: None,
        }),
    );
    assert!(!result_is_error(&result));
    assert!(result_text(&result).contains("Merged 2 guides into \"gamma\""));

    // Sources are gone, merged guide exists with combined learnings.
    let env2 = tool_call(
        4,
        ToolArgs::GuideGet(GuideGetArgs {
            guide: Some("gamma".to_string()),
            ..Default::default()
        }),
    );
    let result2 = run(
        &disp,
        &env2,
        &ToolArgs::GuideGet(GuideGetArgs {
            guide: Some("gamma".to_string()),
            ..Default::default()
        }),
    );
    let text2 = result_text(&result2);
    assert!(text2.contains("alpha-learn"));
    assert!(text2.contains("beta-learn"));
}

/// P1-2: a retried `guide_merge` must replay the recorded merge instead
/// of failing on the (now consumed) sources.
#[test]
fn guide_merge_retry_replays_recorded_success() {
    let (disp, _dir) = test_dispatcher();
    for (op, name) in [(1, "alpha"), (2, "beta")] {
        let create = ToolArgs::GuideCreate(GuideCreateArgs {
            guide: name.to_string(),
            category: "dev-tool".to_string(),
            description: format!("{name} guide"),
            contexts: vec![],
            learnings: vec![],
        });
        run(&disp, &tool_call(op, create.clone()), &create);
    }
    let merge = ToolArgs::GuideMerge(GuideMergeArgs {
        guides: vec!["alpha".to_string(), "beta".to_string()],
        guide: "gamma".to_string(),
        category: "dev-tool".to_string(),
        description: Some("merged".to_string()),
        contexts: None,
        learnings: None,
    });
    let env = tool_call(3, merge.clone());
    let first = run(&disp, &env, &merge);
    assert!(
        !result_is_error(&first),
        "merge failed: {}",
        result_text(&first)
    );
    let second = run(&disp, &env, &merge);
    assert!(
        !result_is_error(&second),
        "merge retry must replay success, got: {}",
        result_text(&second)
    );
    assert_eq!(result_text(&second), result_text(&first));
}
