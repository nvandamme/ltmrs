//! suggestion_respond tool tests (moved verbatim from `tools.rs`).

use super::test_support::*;
use ltmrs_compat::lemma::tool_args::{
    SessionAttemptArgs, SessionStartArgs, SuggestionRespondArgs, ToolArgs,
};
use ltmrs_domain::memory::Instant;
use ltmrs_domain::session::{Suggestion, SuggestionStatus};

#[test]
fn suggestion_respond_requires_valid_action() {
    let (disp, _dir) = test_dispatcher();
    let env = tool_call(
        1,
        ToolArgs::SuggestionRespond(SuggestionRespondArgs {
            id: 1,
            action: "bogus".to_string(),
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::SuggestionRespond(SuggestionRespondArgs {
            id: 1,
            action: "bogus".to_string(),
        }),
    );
    assert!(result_is_error(&result));
    assert!(result_text(&result).contains("must be one of: accept, dismiss"));
}

#[test]
fn suggestion_respond_missing_suggestion_is_error() {
    let (disp, _dir) = test_dispatcher();
    let env = tool_call(
        1,
        ToolArgs::SuggestionRespond(SuggestionRespondArgs {
            id: 999,
            action: "accept".to_string(),
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::SuggestionRespond(SuggestionRespondArgs {
            id: 999,
            action: "accept".to_string(),
        }),
    );
    assert!(result_is_error(&result));
    assert!(result_text(&result).contains("Could not update this suggestion"));
}

/// P1-2: a retried `suggestion_respond` must not apply the attempt
/// confidence adjustment a second time (dismiss + retry penalizes once).
/// (The accept path starts at confidence 1.0 where +0.02 clamps
/// invisibly, so the dismiss path carries the observable assertion;
/// both share the one receipt boundary being added.)
#[test]
fn suggestion_respond_retry_adjusts_attempt_once() {
    let (disp, _dir) = test_dispatcher();
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec![],
        initial_approach: None,
    });
    let start_result = run(&disp, &tool_call(1, start.clone()), &start);
    assert!(!result_is_error(&start_result));
    let handle = disp
        .registry()
        .channel_session(fe(1), ch(1))
        .expect("channel must be bound after start");
    let attempt = ToolArgs::SessionAttempt(SessionAttemptArgs {
        approach: "try X".to_string(),
        outcome: "rejected".to_string(),
        critique: None,
        rationale: None,
        related_memory_id: None,
    });
    let attempt_result = run(&disp, &tool_call(2, attempt.clone()), &attempt);
    assert!(!result_is_error(&attempt_result));
    let suggestion = Suggestion {
        id: 1,
        session_id: Some(handle.as_uuid().to_string()),
        suggestion: "Try Y next.".to_string(),
        status: SuggestionStatus::Pending,
        created_at: Instant::new(1000),
        resolved_at: None,
    };
    disp.repo().put_suggestion(&suggestion).unwrap();
    let respond = ToolArgs::SuggestionRespond(SuggestionRespondArgs {
        id: 1,
        action: "dismiss".to_string(),
    });
    let env = tool_call(3, respond.clone());
    let first = run(&disp, &env, &respond);
    assert!(
        !result_is_error(&first),
        "respond failed: {}",
        result_text(&first)
    );
    let confidence_after_first =
        disp.repo().get_session(handle).unwrap().unwrap().attempts[0].confidence;
    let second = run(&disp, &env, &respond);
    assert!(
        !result_is_error(&second),
        "respond retry must replay success, got: {}",
        result_text(&second)
    );
    let confidence_after_retry =
        disp.repo().get_session(handle).unwrap().unwrap().attempts[0].confidence;
    assert_eq!(
        confidence_after_retry, confidence_after_first,
        "retry must not adjust confidence twice"
    );
}
