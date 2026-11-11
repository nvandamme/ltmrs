//! memory_forget tool tests (moved from `memory_forget.rs`).

use super::test_support::*;
use ltmrs_compat::lemma::tool_args::{MemoryForgetArgs, ToolArgs};

#[test]
fn memory_forget_hard_delete() {
    let (disp, _dir) = test_dispatcher();
    let id = add_fragment(&disp, 1, "## Forget Me\n\n### Context\nTo be deleted.");
    let eid = disp.repo().resolve_id(&id).unwrap();
    let env = tool_call(
        2,
        ToolArgs::MemoryForget(MemoryForgetArgs {
            id: id.clone(),
            ..Default::default()
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::MemoryForget(MemoryForgetArgs {
            id: id.clone(),
            ..Default::default()
        }),
    );
    assert!(!result_is_error(&result));
    assert!(result_text(&result).contains("Forgot fragment"));
    let mems = disp.repo().get_memories(&[eid]).unwrap();
    assert!(matches!(
        mems[0].lifecycle,
        ltmrs_domain::memory::MemoryLifecycle::Deleted { .. }
    ));
}

/// Stable-scope isolation: forget naming a foreign stable row is refused
/// as not-found (no cross-boundary lifecycle move).
#[test]
fn memory_forget_excludes_foreign_stable_row() {
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
    let tool = ToolArgs::MemoryForget(MemoryForgetArgs {
        id: id_a,
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
        "foreign forget must refuse, got: {}",
        result_text(&result)
    );
}

/// Retry safety (T-CONC-03): the same forget envelope twice
/// replays the recorded response (target already gone on retry).
#[test]
fn memory_forget_same_envelope_replays_recorded() {
    let (disp, _dir) = test_dispatcher();
    let id = add_fragment(&disp, 1, "## Replay Forget\n\n### Context\nReplay fixture.");
    let args = ToolArgs::MemoryForget(MemoryForgetArgs {
        id: id.clone(),
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
fn memory_forget_invalidate_hides_from_recall() {
    let (disp, _dir) = test_dispatcher();
    let id = add_fragment(
        &disp,
        1,
        "## Invalidate Me\n\n### Context\nTo be invalidated.",
    );
    let eid = disp.repo().resolve_id(&id).unwrap();
    let env = tool_call(
        2,
        ToolArgs::MemoryForget(MemoryForgetArgs {
            id: id.clone(),
            invalidate: true,
            ..Default::default()
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::MemoryForget(MemoryForgetArgs {
            id: id.clone(),
            invalidate: true,
            ..Default::default()
        }),
    );
    assert!(!result_is_error(&result));
    assert!(result_text(&result).contains("Invalidated fragment"));
    let mems = disp.repo().get_memories(&[eid]).unwrap();
    assert!(!mems[0].lifecycle.is_recallable());
}
