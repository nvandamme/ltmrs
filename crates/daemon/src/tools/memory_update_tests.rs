//! memory_update tool tests (moved verbatim from `tools.rs`).

use super::ids::resolve_id;
use super::replay::sub_command_ctx;
use super::test_support::*;
use ltmrs_compat::lemma::tool_args::{MemoryUpdateArgs, ToolArgs};
use ltmrs_domain::command::DomainCommand;
use ltmrs_domain::command::MemoryPatch;

#[test]
fn memory_update_changes_content() {
    let (disp, _dir) = test_dispatcher();
    let id = add_fragment(&disp, 1, "## Original Content\n\n### Context\nOriginal.");
    let env = tool_call(
        2,
        ToolArgs::MemoryUpdate(MemoryUpdateArgs {
            id: id.clone(),
            fragment: Some("## Updated Content\n\n### Context\nUpdated now.".to_string()),
            ..Default::default()
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::MemoryUpdate(MemoryUpdateArgs {
            id: id.clone(),
            fragment: Some("## Updated Content\n\n### Context\nUpdated now.".to_string()),
            ..Default::default()
        }),
    );
    assert!(!result_is_error(&result));
    assert!(result_text(&result).contains("Updated fragment"));
    let eid = disp.repo().resolve_id(&id).unwrap();
    let mems = disp.repo().get_memories(&[eid]).unwrap();
    assert!(mems[0].fragment.contains("Updated Content"));
}

/// Retry safety (T-CONC-03): the same update envelope twice replays
/// the recorded response with exactly one effect.
#[test]
fn memory_update_same_envelope_replays_recorded() {
    let (disp, _dir) = test_dispatcher();
    let id = add_fragment(&disp, 1, "## Replay Update\n\n### Context\nReplay fixture.");
    let args = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
        id: id.clone(),
        fragment: Some("## Updated\n\n### Context\nReplayed.".to_string()),
        ..Default::default()
    });
    let env = tool_call(2, args.clone());
    let first = run(&disp, &env, &args);
    assert!(
        !result_is_error(&first),
        "first failed: {}",
        result_text(&first)
    );
    let second = run(&disp, &env, &args);
    assert!(
        !result_is_error(&second),
        "replay failed: {}",
        result_text(&second)
    );
    assert_eq!(
        result_text(&second),
        result_text(&first),
        "replay must be byte-identical"
    );
}

#[test]
fn memory_update_unknown_id_is_error() {
    let (disp, _dir) = test_dispatcher();
    let env = tool_call(
        1,
        ToolArgs::MemoryUpdate(MemoryUpdateArgs {
            id: "missing".to_string(),
            ..Default::default()
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::MemoryUpdate(MemoryUpdateArgs {
            id: "missing".to_string(),
            ..Default::default()
        }),
    );
    assert!(result_is_error(&result));
}

/// Stable-scope isolation: an update naming a foreign stable row is
/// refused as not-found (never leaks, never writes cross-boundary).
#[test]
fn memory_update_excludes_foreign_stable_row() {
    use ltmrs_domain::project_id::generate_project_id;
    let (disp, _dir) = test_dispatcher();
    let (full_a, _) = generate_project_id("shop");
    let (full_b, _) = generate_project_id("shop");
    let out_a = add_with(
        &disp,
        1,
        "## Shop A\n\n### Context\nA-only fragment.",
        Some("shop"),
        ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
    );
    assert!(!result_is_error(&out_a));
    let id_a = result_structured(&out_a).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    // B's update naming A's row must refuse.
    let tool = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
        id: id_a,
        fragment: Some("hijacked".into()),
        ..Default::default()
    });
    let env = tool_call_with(
        2,
        tool.clone(),
        ident(Some(&full_b), Some("/tmp/b/shop"), Some("shop"), true),
    );
    let result = run(&disp, &env, &tool);
    assert!(
        result_text(&result).contains("not found"),
        "foreign update must refuse, got: {}",
        result_text(&result)
    );
}
/// rebuilds from the request alone - a title change landing after
/// the commit must not rewrite what the first operation reported.
#[test]
fn update_unfrozen_rebuild_uses_no_later_title() {
    let (disp, _dir) = test_dispatcher();
    let legacy = add_fragment(
        &disp,
        110,
        "## Update Window\n\n### Context\nUnfrozen title fixture.",
    );
    let eid = resolve_id(disp.repo(), &legacy).unwrap();
    // Crash window: op-111 update (fragment only, no title) commits
    // its receipt, freezing nothing.
    let args = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
        id: legacy.clone(),
        title: None,
        fragment: Some("replacement fragment".to_string()),
        confidence: None,
    });
    let env111 = tool_call(111, args.clone());
    let ctx = sub_command_ctx(&env111, 0).unwrap();
    disp.repo()
        .apply(
            &ctx,
            &DomainCommand::UpdateMemory {
                id: eid,
                expected_revision: None,
                patch: MemoryPatch {
                    fragment: Some("replacement fragment".to_string()),
                    ..Default::default()
                },
            },
        )
        .unwrap();
    // A concurrent rename lands after the commit.
    let rename = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
        id: legacy.clone(),
        title: Some("Later Title".to_string()),
        fragment: None,
        confidence: None,
    });
    let renamed = run(&disp, &tool_call(112, rename.clone()), &rename);
    assert!(!result_is_error(&renamed));
    // Retry op 111: the Unfrozen receipt rebuilds from the request —
    // no title was given, so none is quoted (never the later one).
    let replayed = run(&disp, &env111, &args);
    let text = result_text(&replayed);
    assert!(
        !result_is_error(&replayed),
        "replay must succeed, got: {text}"
    );
    assert_eq!(
        text,
        format!("Updated fragment [{legacy}]."),
        "unfrozen rebuild must not quote later state, got: {text}"
    );
}
