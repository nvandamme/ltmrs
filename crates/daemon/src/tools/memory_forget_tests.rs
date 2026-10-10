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
