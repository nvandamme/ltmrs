//! guide_practice tool (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::GuidePracticeArgs;
use ltmrs_domain::command::{DomainErrorCode, DomainResult};
use ltmrs_service::repository::AdmittedScope;
use serde_json::json;

use super::{err_result, ok_result};

pub(crate) fn exec_guide_practice(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    args: &GuidePracticeArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    if args.guide.trim().is_empty() || args.category.trim().is_empty() {
        return Ok(err_result("'guide' and 'category' parameters are required"));
    }
    // Like the sibling tools (session_attempt/session_end): an unrecognized
    // outcome errors instead of silently dropping the signal.
    if let Some(outcome) = args.outcome.as_deref()
        && outcome != "success"
        && outcome != "failure"
    {
        return Ok(err_result("'outcome' must be one of: success, failure."));
    }
    let now = disp.clock().now_millis();
    // Attribute the practice to the active session (canonical store)
    // before the guide mutation so validated_by links the session's
    // pre-loaded reads. Virtual sessions have no canonical record: their
    // link is a no-op and validated_by stays empty. A failed link fails
    // the tool (staged completion): no success is frozen over a dropped
    // canonical effect, and the retry re-runs this idempotent stage.
    // Runs under the entry admission.
    let validated: Vec<String> =
        match disp.resolve_session(envelope.frontend_id, envelope.channel_id) {
            Some(handle) => {
                disp.repo().track_session_link(
                    admitted,
                    handle,
                    ltmrs_service::repository::SessionLinkField::GuideUsed,
                    std::slice::from_ref(&args.guide.to_lowercase().trim().to_string()),
                )?;
                disp.repo()
                    .get_session(handle)?
                    .map(|s| s.memories_read.clone())
                    .unwrap_or_default()
            }
            None => Vec::new(),
        };
    let outcome_bool = match args.outcome.as_deref() {
        Some("success") => Some(true),
        Some("failure") => Some(false),
        _ => None,
    };
    // Idempotent guide mutation: same operation ID + digest replays the
    // recorded snapshot; a digest mismatch rejects (re-review R5). Runs
    // under the entry admission (no TTL revalidation mid-call).
    let updated = match repo.practice_guide_idempotent(
        admitted,
        &args.guide,
        &args.category,
        args.description.as_deref(),
        &args.contexts,
        &args.learnings,
        &validated,
        outcome_bool,
        now,
    ) {
        Ok(g) => g,
        Err(e)
            if e.code == DomainErrorCode::NotFound
                || e.code == DomainErrorCode::KeyReuseDifferentInput =>
        {
            return Ok(err_result(&e.message));
        }
        Err(e) => return Err(e),
    };

    let is_new = updated.usage_count == 1;
    let action = if is_new { "Created" } else { "Updated" };
    let mut response = format!(
        "{action} guide \"{}\" ({}): {}x usage, {} learnings, {} contexts",
        updated.name,
        updated.category,
        updated.usage_count,
        updated.learnings.len(),
        updated.contexts.len()
    );

    let total_attempts = updated.success_count + updated.failure_count;
    if total_attempts >= 3 {
        let rate = updated.success_count as f64 / total_attempts as f64;
        if rate < 0.4 {
            response.push_str(&format!(
                "\n\n--- HOOK SUGGESTIONS ---\nGuide \"{}\" success rate is {:.2} ({}/{}). Consider guide_update to refine.",
                updated.name,
                rate,
                updated.success_count,
                total_attempts
            ));
        }
    }

    let data = json!({
        "success": true,
        "guide": updated.name,
        "usage_count": updated.usage_count,
    });
    Ok(ok_result(response, data))
}

#[cfg(test)]
use super::execute_tool;
#[cfg(test)]
use super::test_support::*;
#[cfg(test)]
use ltmrs_compat::lemma::tool_args::{SessionStartArgs, ToolArgs};

/// An unrecognized practice outcome errors like the sibling tools
/// (session_attempt/session_end): silently dropping it would lose the
/// signal and skew the success hook.
#[test]
fn guide_practice_rejects_unknown_outcome() {
    let (disp, _dir) = test_dispatcher();
    let args = GuidePracticeArgs {
        guide: "git".to_string(),
        category: "dev-tool".to_string(),
        description: None,
        contexts: vec![],
        learnings: vec!["x".to_string()],
        outcome: Some("maybe".to_string()),
    };
    let env = tool_call(1, ToolArgs::GuidePractice(args.clone()));
    let result = run(&disp, &env, &ToolArgs::GuidePractice(args));
    assert!(
        result_is_error(&result),
        "unknown outcome must error, got: {}",
        result_text(&result)
    );
}

#[test]
fn guide_practice_increments_usage() {
    let (disp, _dir) = test_dispatcher();
    // First practice creates the guide (usage_count = 1).
    let env = tool_call(
        1,
        ToolArgs::GuidePractice(GuidePracticeArgs {
            guide: "git".to_string(),
            category: "dev-tool".to_string(),
            description: None,
            contexts: vec!["commits".to_string()],
            learnings: vec!["always stage selectively".to_string()],
            outcome: Some("success".to_string()),
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::GuidePractice(GuidePracticeArgs {
            guide: "git".to_string(),
            category: "dev-tool".to_string(),
            description: None,
            contexts: vec!["commits".to_string()],
            learnings: vec!["always stage selectively".to_string()],
            outcome: Some("success".to_string()),
        }),
    );
    assert!(!result_is_error(&result));
    assert!(result_text(&result).contains("Created guide \"git\""));
    let structured = result_structured(&result).unwrap();
    assert_eq!(structured["usage_count"], json!(1));

    // Second practice increments usage.
    let env2 = tool_call(
        2,
        ToolArgs::GuidePractice(GuidePracticeArgs {
            guide: "git".to_string(),
            category: "dev-tool".to_string(),
            description: None,
            contexts: vec!["branches".to_string()],
            learnings: vec!["rebase before merge".to_string()],
            outcome: Some("failure".to_string()),
        }),
    );
    let result2 = run(
        &disp,
        &env2,
        &ToolArgs::GuidePractice(GuidePracticeArgs {
            guide: "git".to_string(),
            category: "dev-tool".to_string(),
            description: None,
            contexts: vec!["branches".to_string()],
            learnings: vec!["rebase before merge".to_string()],
            outcome: Some("failure".to_string()),
        }),
    );
    assert!(result_text(&result2).contains("Updated guide \"git\""));
    let structured2 = result_structured(&result2).unwrap();
    assert_eq!(structured2["usage_count"], json!(2));
}

/// P1 replay: repeating the same guide-practice operation (same envelope
/// operation ID) must not double-count usage/success counters.
#[test]
fn guide_practice_replay_does_not_double_count() {
    let (disp, _dir) = test_dispatcher();
    let args = GuidePracticeArgs {
        guide: "git".to_string(),
        category: "dev-tool".to_string(),
        description: None,
        contexts: vec!["commits".to_string()],
        learnings: vec!["stage selectively".to_string()],
        outcome: Some("success".to_string()),
    };
    let env = tool_call(1, ToolArgs::GuidePractice(args.clone()));
    let first = run(&disp, &env, &ToolArgs::GuidePractice(args.clone()));
    assert!(!result_is_error(&first));
    let second = run(&disp, &env, &ToolArgs::GuidePractice(args));
    assert!(!result_is_error(&second));
    let guide = disp.repo().get_guide("git").unwrap().expect("guide exists");
    assert_eq!(guide.usage_count, 1, "replay must not recount usage");
    assert_eq!(guide.success_count, 1, "replay must not recount success");
}

/// P1 (staged completion): a practice receipt without session
/// attribution (link stage lost) completes the GuideUsed link on
/// retry instead of freezing success over a missing attribution —
/// without re-practicing (usage stays 1).
#[test]
fn guide_practice_unfrozen_receipt_completes_session_link() {
    let (disp, _dir) = test_dispatcher();
    // Canonical session on the channel.
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "linking".to_string(),
        technologies: vec![],
        initial_approach: None,
    });
    let started = run(&disp, &tool_call(240, start.clone()), &start);
    assert!(!result_is_error(&started));
    // Crash window: op-241 practice commits directly (no link).
    let args = ToolArgs::GuidePractice(GuidePracticeArgs {
        guide: "git".to_string(),
        category: "dev-tool".to_string(),
        description: None,
        contexts: vec![],
        learnings: vec!["stage selectively".to_string()],
        outcome: Some("success".to_string()),
    });
    let env = tool_call(241, args.clone());
    let digest = env.request_digest().unwrap();
    let admitted = disp
        .repo()
        .admit_scope(&env.operation_scope(digest))
        .unwrap();
    disp.repo()
        .practice_guide_idempotent(
            &admitted,
            "git",
            "dev-tool",
            None,
            &[],
            &["stage selectively".to_string()],
            &[],
            Some(true),
            1000,
        )
        .unwrap();
    // Practice has no tool-level recorded replay: every call flows
    // through link + practice, and the primitive replays internally
    // (usage stays 1 below proves the direct receipt is hit).
    // Retry completes the link; the recorded snapshot stands.
    let replayed = run(&disp, &env, &args);
    assert!(
        !result_is_error(&replayed),
        "retry must succeed, got: {}",
        result_text(&replayed)
    );
    let sessions = disp.repo().all_sessions().unwrap();
    assert!(
        sessions
            .iter()
            .any(|s| s.guides_used.contains(&"git".to_string())),
        "unfrozen retry must complete the GuideUsed link"
    );
    let guide = disp.repo().get_guide("git").unwrap().expect("guide exists");
    assert_eq!(
        guide.usage_count, 1,
        "completing replay must not re-practice"
    );
    assert_eq!(guide.learnings.len(), 1);
}

/// P1 (tool atomicity): a failed GuideUsed-link stage must fail the
/// tool, never be swallowed into a success. Deterministic seed: one
/// armed barrier fault, consumed by the link (the practice replay
/// runs after it, so the fault isolates the link stage).
#[test]
fn guide_practice_link_failure_fails_loudly() {
    let (disp, _dir) = test_dispatcher();
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "linking".to_string(),
        technologies: vec![],
        initial_approach: None,
    });
    let started = run(&disp, &tool_call(250, start.clone()), &start);
    assert!(!result_is_error(&started));
    // Crash window: op-251 practice commits directly (no link).
    let args = ToolArgs::GuidePractice(GuidePracticeArgs {
        guide: "git".to_string(),
        category: "dev-tool".to_string(),
        description: None,
        contexts: vec![],
        learnings: vec!["stage selectively".to_string()],
        outcome: Some("success".to_string()),
    });
    let env = tool_call(251, args.clone());
    let digest = env.request_digest().unwrap();
    let admitted = disp
        .repo()
        .admit_scope(&env.operation_scope(digest))
        .unwrap();
    disp.repo()
        .practice_guide_idempotent(
            &admitted,
            "git",
            "dev-tool",
            None,
            &[],
            &["stage selectively".to_string()],
            &[],
            Some(true),
            1000,
        )
        .unwrap();
    // The link runs before the practice replay, so the single armed
    // fault fails exactly the link stage.
    disp.repo().fault_injector().set_persist_failures(1);
    let err = execute_tool(&disp, &env, &args).unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::Validation,
        "link stage failure must fail the tool, got: {}",
        err.message
    );
    // The practice itself never re-ran; a clean retry completes.
    let guide = disp.repo().get_guide("git").unwrap().expect("guide exists");
    assert_eq!(guide.usage_count, 1);
    let replayed = run(&disp, &env, &args);
    assert!(
        !result_is_error(&replayed),
        "clean retry must complete, got: {}",
        result_text(&replayed)
    );
    let sessions = disp.repo().all_sessions().unwrap();
    assert!(
        sessions
            .iter()
            .any(|s| s.guides_used.contains(&"git".to_string())),
        "clean retry must complete the GuideUsed link"
    );
}

/// Re-review R5: practice replay returns the RECORDED snapshot (not
/// current contents); key reuse rejects.
#[test]
fn guide_practice_replay_returns_recorded_snapshot() {
    let (disp, _dir) = test_dispatcher();
    let practice = |learning: &str| {
        ToolArgs::GuidePractice(GuidePracticeArgs {
            guide: "git".to_string(),
            category: "dev-tool".to_string(),
            description: None,
            contexts: vec![],
            learnings: vec![learning.to_string()],
            outcome: Some("success".to_string()),
        })
    };
    let env5 = tool_call(5, practice("first"));
    let first = run(&disp, &env5, &practice("first"));
    assert!(!result_is_error(&first));
    // A different operation moves the guide forward.
    let env6 = tool_call(6, practice("second"));
    let _ = run(&disp, &env6, &practice("second"));
    let live = disp.repo().get_guide("git").unwrap().unwrap();
    assert_eq!(live.usage_count, 2);
    // Replaying op 5 resolves to its recorded outcome (usage 1).
    let replayed = run(&disp, &env5, &practice("first"));
    assert!(!result_is_error(&replayed));
    assert_eq!(
        result_structured(&replayed).unwrap()["usage_count"],
        serde_json::json!(1),
        "replay must resolve to the recorded outcome, not current state"
    );
    let live = disp.repo().get_guide("git").unwrap().unwrap();
    assert_eq!(live.usage_count, 2, "replay must not recount");
    // Same operation ID, different arguments: reject. The changed call
    // needs its own envelope (same op, changed body) so the digest
    // actually differs.
    let changed_env = tool_call(5, practice("changed"));
    let changed = run(&disp, &changed_env, &practice("changed"));
    assert!(result_is_error(&changed));
    assert!(result_text(&changed).contains("different input"));
}
