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

/// P2-high (crash-window fidelity): an UNFROZEN update receipt
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
