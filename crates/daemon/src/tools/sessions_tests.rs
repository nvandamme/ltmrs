//! session tool tests (moved verbatim from `tools.rs`).

use super::execute_tool;
use super::test_support::*;
use crate::dispatcher::Dispatcher;
use ltmrs_compat::lemma::tool_args::{
    GuideCreateArgs, GuideGetArgs, GuidePracticeArgs, MemoryReadArgs, SessionAttemptArgs,
    SessionEndArgs, SessionStartArgs, SessionStatsArgs, ToolArgs,
};
use ltmrs_domain::id::{OperationId, StoreGeneration};
use uuid::Uuid;

/// A retried attempt replays from its durable receipt even after its
/// session ended: start S, attempt X, end S, replay X → X's original
/// result with no second attempt recorded.
#[test]
fn session_attempt_replays_after_session_end() {
    let (disp, _dir) = test_dispatcher();
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec!["rust".to_string()],
        initial_approach: Some("read the code".to_string()),
    });
    let result = run(&disp, &tool_call(1, start.clone()), &start);
    assert!(!result_is_error(&result));

    let attempt = ToolArgs::SessionAttempt(SessionAttemptArgs {
        approach: "try X".to_string(),
        outcome: "rejected".to_string(),
        critique: Some("didn't work".to_string()),
        rationale: None,
        related_memory_id: None,
    });
    let env2 = tool_call(2, attempt.clone());
    let first = run(&disp, &env2, &attempt);
    assert!(!result_is_error(&first));
    let first_text = result_text(&first);
    assert!(first_text.contains("Recorded attempt #1"));

    let end = ToolArgs::SessionEnd(SessionEndArgs {
        outcome: "success".to_string(),
        final_approach: Some("fixed it".to_string()),
        lessons: vec![],
    });
    let result3 = run(&disp, &tool_call(3, end.clone()), &end);
    assert!(!result_is_error(&result3));

    // Retry the exact attempt envelope after the terminal transition.
    let replayed = run(&disp, &env2, &attempt);
    assert!(
        !result_is_error(&replayed),
        "replay after end must succeed, got: {}",
        result_text(&replayed)
    );
    assert_eq!(
        result_text(&replayed),
        first_text,
        "replay must return the original attempt result"
    );
}

/// Whole learning workflow (S6/WP-09 trace): recall, act, persist,
/// practice a guide, record attempts and end — with correct cross-tool
/// attribution through the public tool surface (no hidden reasoning).
#[test]
fn whole_learning_workflow_recall_act_persist() {
    let (disp, _dir) = test_dispatcher();
    // Seed: one task-relevant memory, one unrelated.
    let rust_id = add_fragment(
        &disp,
        1,
        "## Rust Async\n\n### Context\nUse tokio spawn_blocking for blocking work.",
    );
    add_fragment(
        &disp,
        2,
        "## Sourdough\n\n### Context\nBake bread at 240C with steam.",
    );

    // RECALL: start a session; the relevant memory is pre-loaded.
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec!["rust".to_string(), "tokio".to_string()],
        initial_approach: Some("read the code".to_string()),
    });
    let result = run(&disp, &tool_call(10, start.clone()), &start);
    assert!(!result_is_error(&result));
    let structured = result_structured(&result).unwrap();
    let preloaded: Vec<String> = structured["preloaded_memories"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    assert!(
        preloaded.contains(&rust_id),
        "task-relevant memory must preload, got: {preloaded:?}"
    );
    let session_id = structured["session_id"].as_str().unwrap().to_string();

    // RECALL: read the preloaded memory (access recorded).
    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        id: Some(rust_id.clone()),
        ..Default::default()
    });
    let result = run(&disp, &tool_call(11, read.clone()), &read);
    assert!(!result_is_error(&result));
    // The explicit read leaves its own observable mark (access count):
    // one from the preload boost plus one from this read.
    let read_eid = disp.repo().resolve_id(&rust_id).unwrap();
    let read_mem = disp
        .repo()
        .get_memories(&[read_eid])
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        read_mem.access_count, 2,
        "explicit read must record access on top of the preload boost"
    );

    // ACT: record a rejected attempt explicitly.
    let attempt = ToolArgs::SessionAttempt(SessionAttemptArgs {
        approach: "guess from prose".to_string(),
        outcome: "rejected".to_string(),
        critique: Some("no evidence".to_string()),
        rationale: None,
        related_memory_id: Some(rust_id.clone()),
    });
    let result = run(&disp, &tool_call(12, attempt.clone()), &attempt);
    assert!(!result_is_error(&result));

    // PERSIST: save the lesson; it links to the active session.
    let new_id = add_fragment(
        &disp,
        13,
        "## Blocking Lessons\n\n### Context\nNever block Tokio core workers; use spawn_blocking.",
    );

    // PRACTICE: create + practice a guide for the session.
    let create = ToolArgs::GuideCreate(GuideCreateArgs {
        guide: "tokio-discipline".to_string(),
        category: "dev-tool".to_string(),
        description: "## Tokio Discipline\n\n### Protocol\nSpawn blocking.".to_string(),
        contexts: vec!["async".to_string()],
        learnings: vec![],
    });
    let result = run(&disp, &tool_call(14, create.clone()), &create);
    assert!(!result_is_error(&result));
    let practice = ToolArgs::GuidePractice(GuidePracticeArgs {
        guide: "tokio-discipline".to_string(),
        category: "dev-tool".to_string(),
        description: None,
        contexts: vec!["async".to_string()],
        learnings: vec!["spawn_blocking reviewed".to_string()],
        outcome: Some("success".to_string()),
    });
    let result = run(&disp, &tool_call(15, practice.clone()), &practice);
    assert!(!result_is_error(&result));

    // END: close the session; cross-tool attribution must hold.
    let end = ToolArgs::SessionEnd(SessionEndArgs {
        outcome: "success".to_string(),
        final_approach: Some("spawn_blocking everywhere".to_string()),
        lessons: vec!["verify before claiming".to_string()],
    });
    let result = run(&disp, &tool_call(16, end.clone()), &end);
    assert!(!result_is_error(&result));

    // Coherence across the loop, read back from canonical state.
    let handle = ltmrs_domain::id::SessionHandle::new(uuid::Uuid::parse_str(&session_id).unwrap());
    let session = disp.repo().get_session(handle).unwrap().unwrap();
    assert!(session.memories_read.contains(&rust_id));
    assert!(session.memories_created.contains(&new_id));
    assert_eq!(session.attempts.len(), 1);
    assert!(
        session
            .guides_used
            .contains(&"tokio-discipline".to_string())
    );
    assert!(session.status.is_terminal(), "session must be ended");
    let guide = disp.repo().get_guide("tokio-discipline").unwrap().unwrap();
    // Create seeds usage at 1; the explicit practice adds exactly one more.
    assert_eq!(guide.usage_count, 2);
    let eid = disp.repo().resolve_id(&new_id).unwrap();
    let mems = disp.repo().get_memories(&[eid]).unwrap();
    assert_eq!(
        mems[0].session_id.as_deref(),
        Some(session_id.as_str()),
        "persisted memory links the session"
    );
}

#[test]
fn session_stats_reports_active_completed_and_empty() {
    let stats = |disp: &Dispatcher, op: u64| {
        let args = ToolArgs::SessionStats(SessionStatsArgs {
            count: Some(10),
            response_format: None,
        });
        run(disp, &tool_call(op, args.clone()), &args)
    };
    // Empty store: no sessions recorded yet.
    let (disp, _dir) = test_dispatcher();
    let empty = stats(&disp, 1);
    assert!(!result_is_error(&empty));
    assert!(result_text(&empty).contains("No past sessions recorded yet."));

    // Active session with one attempt and technologies.
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec!["rust".to_string()],
        initial_approach: None,
    });
    run(&disp, &tool_call(2, start.clone()), &start);
    let attempt = ToolArgs::SessionAttempt(SessionAttemptArgs {
        approach: "try X".to_string(),
        outcome: "rejected".to_string(),
        critique: None,
        rationale: None,
        related_memory_id: None,
    });
    run(&disp, &tool_call(3, attempt.clone()), &attempt);
    let active = stats(&disp, 4);
    assert!(!result_is_error(&active));
    let text = result_text(&active);
    assert!(text.contains("Active session: 1 tool calls"));
    assert!(text.contains("Technologies: rust"));

    // Ended session moves to recent history.
    let end = ToolArgs::SessionEnd(SessionEndArgs {
        outcome: "success".to_string(),
        final_approach: None,
        lessons: vec![],
    });
    run(&disp, &tool_call(5, end.clone()), &end);
    let done = stats(&disp, 6);
    assert!(!result_is_error(&done));
    let text = result_text(&done);
    assert!(text.contains("Recent sessions (1):"));
    assert!(!text.contains("Active session:"));
}

/// RQ-06 dispatch gate: a mutating tool under an unknown/expired
/// namespace is refused before reaching any repository primitive,
/// while read-only tools on the same dead epoch still serve.
#[test]
fn dispatch_gate_refuses_mutating_tool_on_dead_namespace() {
    let (disp, _dir) = test_dispatcher();
    // Mutating tool, unknown epoch: refused at the gate (raw error,
    // never reaching the primitive — run() would unwrap-panic).
    let create = ToolArgs::GuideCreate(GuideCreateArgs {
        guide: "gated".to_string(),
        category: "test".to_string(),
        description: "gated fixture".to_string(),
        contexts: vec![],
        learnings: vec![],
    });
    let mut env = tool_call(50, create.clone());
    env.retry_epoch = 99;
    let err = execute_tool(&disp, &env, &create).unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::StaleReplay
    );
    assert!(disp.repo().get_guide("gated").unwrap().is_none());
    // Read-only tool, same dead epoch: unaffected.
    let get = ToolArgs::GuideGet(GuideGetArgs {
        task: Some("gated".to_string()),
        ..Default::default()
    });
    let mut getenv = tool_call(51, get.clone());
    getenv.retry_epoch = 99;
    let result = run(&disp, &getenv, &get);
    assert!(
        !result_is_error(&result),
        "reads stay available without a live namespace"
    );
}

/// T-CONC-02: 32 channels × independent session starts, barrier
/// released, each with its own namespace — every start applies on
/// first attempt with no caller-level retry (per-channel watermarks,
/// no shared contention key).
#[test]
fn thirty_two_channels_start_without_contention() {
    use std::sync::{Arc, Barrier};
    let (disp, _dir) = test_dispatcher();
    let disp = Arc::new(disp);
    for n in 2..=32u64 {
        disp.repo().issue_namespace(fe(1), ch(n), 1000).unwrap();
    }
    let start = Arc::new(Barrier::new(33));
    let mut handles = Vec::new();
    for n in 1..=32u64 {
        let disp = Arc::clone(&disp);
        let start = Arc::clone(&start);
        handles.push(std::thread::spawn(move || {
            start.wait();
            let scope = ltmrs_domain::command::OperationScope {
                store_generation: StoreGeneration::FIRST,
                frontend_id: fe(1),
                channel_id: ch(n),
                retry_epoch: n,
                operation_id: OperationId::new(Uuid::from_u128(n as u128)),
                request_digest: format!("conc-{n}"),
            };
            let handle = ltmrs_domain::id::SessionHandle::new(Uuid::from_u128(1000 + n as u128));
            disp.repo()
                .session_start_tx(&scope, handle, None, None, vec![], None, None, 1000)
                .unwrap()
        }));
    }
    start.wait();
    for (i, h) in handles.into_iter().enumerate() {
        match h.join().unwrap() {
            ltmrs_domain::session::SessionOp::Applied(_) => {}
            other => panic!("channel {} must apply first-try, got {other:?}", i + 1),
        }
    }
    assert_eq!(disp.repo().all_sessions().unwrap().len(), 32);
}

/// Exec-level T2: channel B resubmitting channel A's session-start
/// operation ID + body (its own namespace) starts B's OWN session —
/// never replays A's receipt, never binds B to A's session.
#[test]
fn cross_channel_op_reuse_starts_own_session() {
    let (disp, _dir) = test_dispatcher();
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec![],
        initial_approach: None,
    });
    let env_a = tool_call(70, start.clone());
    let ra = run(&disp, &env_a, &start);
    assert!(!result_is_error(&ra));
    let ha = disp.resolve_session(fe(1), ch(1)).expect("A bound");
    // Channel B under its own namespace, same op id + body.
    disp.repo().issue_namespace(fe(1), ch(2), 1000).unwrap();
    let mut env_b = tool_call(70, start.clone());
    env_b.channel_id = ch(2);
    env_b.retry_epoch = 2;
    let rb = run(&disp, &env_b, &start);
    assert!(!result_is_error(&rb));
    let hb = disp.resolve_session(fe(1), ch(2)).expect("B bound");
    assert_ne!(ha, hb, "B must own a fresh session, not A's");
    assert_eq!(disp.resolve_session(fe(1), ch(1)), Some(ha));
    assert!(disp.repo().get_session(ha).unwrap().is_some());
}

#[test]
fn session_end_is_retry_safe_no_double_count() {
    let (disp, _dir) = test_dispatcher();
    // Start a session and practice a guide into it.
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec![],
        initial_approach: None,
    });
    let env = tool_call(1, start.clone());
    run(&disp, &env, &start);

    let practice = ToolArgs::GuidePractice(GuidePracticeArgs {
        guide: "git".to_string(),
        category: "dev-tool".to_string(),
        description: None,
        contexts: vec![],
        learnings: vec![],
        outcome: None,
    });
    let env = tool_call(2, practice.clone());
    run(&disp, &env, &practice);

    // First end: success. The guide's success_count should be 1.
    let end = ToolArgs::SessionEnd(SessionEndArgs {
        outcome: "success".to_string(),
        final_approach: None,
        lessons: vec![],
    });
    let env = tool_call(3, end.clone());
    let result = run(&disp, &env, &end);
    assert!(!result_is_error(&result));
    let guide = disp.repo().get_guide("git").unwrap().unwrap();
    assert_eq!(guide.success_count, 1, "first end should count once");

    // Second end (retry): must be rejected and must NOT double-count.
    let env = tool_call(4, end.clone());
    let result2 = run(&disp, &env, &end);
    assert!(result_is_error(&result2));
    assert!(result_text(&result2).contains("No active session"));
    let guide = disp.repo().get_guide("git").unwrap().unwrap();
    assert_eq!(
        guide.success_count, 1,
        "a retried session_end must not double-count guide outcomes"
    );
}

#[test]
fn session_attempt_without_session_is_error() {
    let (disp, _dir) = test_dispatcher();
    let env = tool_call(
        1,
        ToolArgs::SessionAttempt(SessionAttemptArgs {
            approach: "try X".to_string(),
            outcome: "rejected".to_string(),
            critique: None,
            rationale: None,
            related_memory_id: None,
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::SessionAttempt(SessionAttemptArgs {
            approach: "try X".to_string(),
            outcome: "rejected".to_string(),
            critique: None,
            rationale: None,
            related_memory_id: None,
        }),
    );
    assert!(result_is_error(&result));
    assert!(result_text(&result).contains("No active session"));
}

#[test]
fn session_end_without_session_is_error() {
    let (disp, _dir) = test_dispatcher();
    let env = tool_call(
        1,
        ToolArgs::SessionEnd(SessionEndArgs {
            outcome: "success".to_string(),
            final_approach: None,
            lessons: vec![],
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::SessionEnd(SessionEndArgs {
            outcome: "success".to_string(),
            final_approach: None,
            lessons: vec![],
        }),
    );
    assert!(result_is_error(&result));
    assert!(result_text(&result).contains("No active session"));
}

/// P1 replay safety at the tool layer: repeating the same
/// `session_attempt` operation (same envelope operation ID) records
/// exactly one attempt and increments counters exactly once.
#[test]
fn session_attempt_tool_replay_records_once() {
    let (disp, _dir) = test_dispatcher();
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec![],
        initial_approach: None,
    });
    run(&disp, &tool_call(1, start.clone()), &start);
    let attempt = ToolArgs::SessionAttempt(SessionAttemptArgs {
        approach: "try X".to_string(),
        outcome: "rejected".to_string(),
        critique: Some("bad".to_string()),
        rationale: None,
        related_memory_id: None,
    });
    // Same operation ID twice = one retried operation.
    let env = tool_call(2, attempt.clone());
    let first = run(&disp, &env, &attempt);
    assert!(!result_is_error(&first));
    let second = run(&disp, &env, &attempt);
    assert!(!result_is_error(&second));
    let sessions = disp.repo().all_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0].attempts.len(),
        1,
        "tool replay must not duplicate the attempt"
    );
    assert_eq!(
        sessions[0].refinement_attempts, 1,
        "tool replay must not double-count refinement"
    );
    assert_eq!(
        sessions[0].self_critique_count, 1,
        "tool replay must not double-count self-critique"
    );
}

/// Re-review R5: replaying session_end returns the recorded response and
/// never recounts guide outcomes; key reuse rejects.
#[test]
fn session_end_replay_returns_recorded_outcome() {
    let (disp, _dir) = test_dispatcher();
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec![],
        initial_approach: None,
    });
    run(&disp, &tool_call(1, start.clone()), &start);
    let practice = ToolArgs::GuidePractice(GuidePracticeArgs {
        guide: "git".to_string(),
        category: "dev-tool".to_string(),
        description: None,
        contexts: vec![],
        learnings: vec![],
        outcome: None,
    });
    run(&disp, &tool_call(2, practice.clone()), &practice);
    let end = ToolArgs::SessionEnd(SessionEndArgs {
        outcome: "success".to_string(),
        final_approach: Some("fixed".to_string()),
        lessons: vec!["check logs".to_string()],
    });
    let env = tool_call(3, end.clone());
    let first = run(&disp, &env, &end);
    assert!(!result_is_error(&first));
    let first_text = result_text(&first);
    // Replay: identical response, guide counted exactly once.
    let second = run(&disp, &env, &end);
    assert!(!result_is_error(&second));
    assert_eq!(result_text(&second), first_text);
    let guide = disp.repo().get_guide("git").unwrap().unwrap();
    assert_eq!(guide.success_count, 1);
    // Same operation ID, different arguments: reject (new envelope so
    // the digest actually differs — reusing the old one would replay).
    let changed = ToolArgs::SessionEnd(SessionEndArgs {
        outcome: "success".to_string(),
        final_approach: None,
        lessons: vec!["different".to_string()],
    });
    let changed_env = tool_call(3, changed.clone());
    let third = run(&disp, &changed_env, &changed);
    assert!(result_is_error(&third));
    assert!(result_text(&third).contains("different input"));
    let guide = disp.repo().get_guide("git").unwrap().unwrap();
    assert_eq!(guide.success_count, 1);
}

/// Re-review R5: a retried attempt with changed arguments rejects
/// instead of recording different content under one identity.
#[test]
fn session_attempt_replay_with_changed_args_rejects() {
    let (disp, _dir) = test_dispatcher();
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec![],
        initial_approach: None,
    });
    run(&disp, &tool_call(1, start.clone()), &start);
    let attempt = ToolArgs::SessionAttempt(SessionAttemptArgs {
        approach: "try X".to_string(),
        outcome: "rejected".to_string(),
        critique: None,
        rationale: None,
        related_memory_id: None,
    });
    let env = tool_call(2, attempt.clone());
    assert!(!result_is_error(&run(&disp, &env, &attempt)));
    let changed = ToolArgs::SessionAttempt(SessionAttemptArgs {
        approach: "try Y instead".to_string(),
        outcome: "rejected".to_string(),
        critique: None,
        rationale: None,
        related_memory_id: None,
    });
    let changed_env = tool_call(2, changed.clone());
    let result = run(&disp, &changed_env, &changed);
    assert!(result_is_error(&result));
    assert!(result_text(&result).contains("different input"));
    let sessions = disp.repo().all_sessions().unwrap();
    assert_eq!(sessions[0].attempts.len(), 1);
}

/// Re-review R1: a session-save failure fails the tool instead of
/// reporting success. The sessions path points into a nonexistent
/// directory, so every persist fails deterministically. (Dispatcher
/// paths are covered in dispatcher.rs tests.)
#[test]
fn session_tools_fail_when_persist_fails() {
    let (disp, dir) = test_dispatcher();
    disp.set_sessions_path(Some(dir.path().join("no-such-dir").join("sessions.json")));
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec![],
        initial_approach: None,
    });
    let env = tool_call(1, start.clone());
    let err = execute_tool(&disp, &env, &start).unwrap_err();
    assert!(
        err.message.contains("persist"),
        "save failure must fail loudly, got: {}",
        err.message
    );
}
/// P2-1: a retried `session_end` must return the recorded response
/// verbatim, even when guide outcomes recorded afterwards would change
/// recomputed improvement lines (success rate 0.00 → 0.25).
#[test]
fn session_end_retry_returns_frozen_response() {
    let (disp, _dir) = test_dispatcher();
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec![],
        initial_approach: None,
    });
    run(&disp, &tool_call(1, start.clone()), &start);
    // Three failures drive the used guide below the improvement
    // threshold (rate 0.00); practice also links the guide to the
    // session so `session_end` evaluates it.
    for op in [2, 3, 4] {
        let practice = ToolArgs::GuidePractice(GuidePracticeArgs {
            guide: "git".to_string(),
            category: "dev-tool".to_string(),
            description: None,
            contexts: vec![],
            learnings: vec!["learn it".to_string()],
            outcome: Some("failure".to_string()),
        });
        let result = run(&disp, &tool_call(op, practice.clone()), &practice);
        assert!(!result_is_error(&result));
    }
    let end = ToolArgs::SessionEnd(SessionEndArgs {
        outcome: "failure".to_string(),
        final_approach: None,
        lessons: vec![],
    });
    let env = tool_call(5, end.clone());
    let first = run(&disp, &env, &end);
    assert!(
        !result_is_error(&first),
        "end failed: {}",
        result_text(&first)
    );
    assert!(
        result_text(&first).contains("IMPROVEMENT SUGGESTIONS"),
        "fixture must produce improvement lines, got: {}",
        result_text(&first)
    );
    // A later success changes the rate a recompute would render.
    let practice = ToolArgs::GuidePractice(GuidePracticeArgs {
        guide: "git".to_string(),
        category: "dev-tool".to_string(),
        description: None,
        contexts: vec![],
        learnings: vec!["learn it".to_string()],
        outcome: Some("success".to_string()),
    });
    run(&disp, &tool_call(6, practice.clone()), &practice);
    let second = run(&disp, &env, &end);
    assert!(!result_is_error(&second));
    assert_eq!(
        result_text(&second),
        result_text(&first),
        "retry must return the frozen original response"
    );
}
