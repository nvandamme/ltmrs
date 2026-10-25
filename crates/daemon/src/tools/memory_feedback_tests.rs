//! memory_feedback tool tests (moved verbatim from `tools.rs`).

use super::ids::resolve_id;
use super::replay::sub_command_ctx;
use super::test_support::*;
use ltmrs_compat::lemma::tool_args::{
    MemoryAddArgs, MemoryFeedbackArgs, MemoryUpdateArgs, ToolArgs,
};
use ltmrs_domain::command::DomainCommand;

#[test]
fn memory_feedback_positive_boosts_confidence() {
    let (disp, _dir) = test_dispatcher();
    let id = add_fragment(
        &disp,
        1,
        "## Feedback Target\n\n### Context\nFor feedback testing.",
    );
    let eid = disp.repo().resolve_id(&id).unwrap();
    // Lower confidence first (default is 1.0, already at the ceiling).
    let upd = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
        id: id.clone(),
        confidence: Some(0.5),
        ..Default::default()
    });
    let env = tool_call(2, upd.clone());
    run(&disp, &env, &upd);
    let before = disp.repo().get_memories(&[eid]).unwrap()[0].confidence;
    let fb = ToolArgs::MemoryFeedback(MemoryFeedbackArgs {
        id: id.clone(),
        useful: true,
    });
    let env = tool_call(3, fb.clone());
    let result = run(&disp, &env, &fb);
    assert!(!result_is_error(&result));
    assert!(result_text(&result).contains("Positive feedback"));
    // Upstream contract: +0.015 confidence + access_count bump.
    let m = &disp.repo().get_memories(&[eid]).unwrap()[0];
    assert!((m.confidence - (before + 0.015)).abs() < 1e-9);
    assert_eq!(m.access_count, 1);
    assert_eq!(m.positive_feedback, 1);
}

#[test]
fn memory_feedback_negative_reduces_confidence() {
    let (disp, _dir) = test_dispatcher();
    let id = add_fragment(
        &disp,
        1,
        "## Feedback Target Neg\n\n### Context\nFor negative feedback.",
    );
    let eid = disp.repo().resolve_id(&id).unwrap();
    let before = disp.repo().get_memories(&[eid]).unwrap()[0].confidence;
    let env = tool_call(
        2,
        ToolArgs::MemoryFeedback(MemoryFeedbackArgs {
            id: id.clone(),
            useful: false,
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::MemoryFeedback(MemoryFeedbackArgs {
            id: id.clone(),
            useful: false,
        }),
    );
    assert!(!result_is_error(&result));
    assert!(result_text(&result).contains("Negative feedback"));
    // Upstream contract: -0.02 confidence + negative_hits increment.
    let m = &disp.repo().get_memories(&[eid]).unwrap()[0];
    assert!((m.confidence - (before - 0.02)).abs() < 1e-9);
    assert_eq!(m.negative_hits, 1);
    assert_eq!(m.negative_feedback, 1);
}

/// P2 (replay fidelity): a feedback replay must report the confidence
/// recorded by its own execution, not the current value — a later
/// op moved it to 0.53, but the first op's replay still says 0.515.
/// Only frozen bytes can do this; any recomputation drifts.
#[test]
fn feedback_replay_reports_original_confidence() {
    let (disp, _dir) = test_dispatcher();
    let mem = ToolArgs::MemoryAdd(MemoryAddArgs {
        fragment: "## Drift Anchor\n\n### Context\nConfidence fixture.".to_string(),
        ..Default::default()
    });
    let result = run(&disp, &tool_call(80, mem.clone()), &mem);
    assert!(!result_is_error(&result));
    let legacy = result_structured(&result).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    // Negative feedback from the 1.0 creation baseline: 1.00
    // first, 0.98 second — the replay must still say 0.98.
    let fb = ToolArgs::MemoryFeedback(MemoryFeedbackArgs {
        id: legacy.clone(),
        useful: false,
    });
    let first = run(&disp, &tool_call(81, fb.clone()), &fb);
    assert!(!result_is_error(&first));
    let first_text = result_text(&first);
    assert!(
        first_text.contains("0.98"),
        "first feedback must report 0.98, got: {first_text}"
    );
    // A second, independent feedback moves confidence again.
    let again = run(&disp, &tool_call(82, fb.clone()), &fb);
    assert!(!result_is_error(&again));
    // Replay the FIRST feedback envelope: byte-identical text.
    let replayed = run(&disp, &tool_call(81, fb.clone()), &fb);
    assert!(!result_is_error(&replayed));
    assert_eq!(
        result_text(&replayed),
        first_text,
        "replay must return the frozen original, not recomputed state"
    );
}

/// P2-high (crash-window fidelity): an UNFROZEN feedback receipt
/// (committed, response lost before freezing) rebuilds from the
/// request alone — a confidence move landing after the commit must
/// not rewrite what the first operation reported.
#[test]
fn feedback_unfrozen_rebuild_reports_no_later_confidence() {
    let (disp, _dir) = test_dispatcher();
    let legacy = add_fragment(
        &disp,
        90,
        "## Crash Window\n\n### Context\nUnfrozen rebuild fixture.",
    );
    let eid = resolve_id(disp.repo(), &legacy).unwrap();
    // Crash window: op-91 feedback commits its receipt directly,
    // freezing nothing.
    let fb = ToolArgs::MemoryFeedback(MemoryFeedbackArgs {
        id: legacy.clone(),
        useful: true,
    });
    let env91 = tool_call(91, fb.clone());
    let ctx = sub_command_ctx(&env91, 0).unwrap();
    disp.repo()
        .apply(
            &ctx,
            &DomainCommand::Feedback {
                memory_id: eid,
                useful: true,
            },
        )
        .unwrap();
    // A second, independent feedback moves confidence after the commit.
    let again = run(&disp, &tool_call(92, fb.clone()), &fb);
    assert!(!result_is_error(&again));
    // Retry op 91: the Unfrozen receipt rebuilds — direction only,
    // never the moved absolute.
    let replayed = run(&disp, &env91, &fb);
    let text = result_text(&replayed);
    assert!(
        !result_is_error(&replayed),
        "replay must succeed, got: {text}"
    );
    assert_eq!(
        text,
        format!("Positive feedback recorded for [{legacy}]."),
        "unfrozen rebuild must not claim live state, got: {text}"
    );
    assert!(
        result_structured(&replayed)
            .unwrap()
            .get("confidence")
            .is_none(),
        "unfrozen rebuild must not fabricate a confidence value"
    );
}
