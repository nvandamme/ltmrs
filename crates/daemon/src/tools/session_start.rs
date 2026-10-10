//! session_start tool (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::{MemoryReadArgs, SessionStartArgs};
use ltmrs_domain::command::{DomainCommand, DomainErrorCode, DomainResult};
use ltmrs_domain::id::{EntityId, SessionHandle};
use ltmrs_domain::memory::{FragmentType, Memory};
use ltmrs_domain::session::{AttemptOutcome, SessionOp, SuggestionStatus};
use ltmrs_service::repository::AdmittedScope;
use serde_json::json;

use super::ids::legacy_id_of;
use super::replay::sub_command_ctx;
use super::{err_result, key_reuse_result, ok_result, persist_before_ack};

pub(crate) fn exec_session_start(
    disp: &Dispatcher,
    envelope: &IpcEnvelope,
    admitted: &AdmittedScope,
    args: &SessionStartArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    if args.task_type.trim().is_empty() {
        return Ok(err_result("'task_type' parameter is required"));
    }
    let now = disp.clock().now_millis();
    // The frozen session_start schema carries no project field; the channel's
    // session is project-less (upstream resolves it from cwd, which the daemon
    // does not observe).
    let project: Option<String> = None;

    // ONE canonical operation (re-review P1-3): abandon-previous, decay,
    // create and receipt commit together in the store. A replay resolves
    // to the recorded handle instead of abandoning and recreating; a
    // digest mismatch rejects. The registry only (re)binds the channel.
    let digest = envelope.request_digest()?;
    let scope = envelope.operation_scope(digest.clone());
    let abandon = disp.resolve_session(envelope.frontend_id, envelope.channel_id);
    let new_handle = ltmrs_domain::id::SessionHandle::new(uuid::Uuid::now_v7());
    let handle = match repo.session_start_tx(
        &scope,
        new_handle,
        project.clone(),
        Some(args.task_type.clone()),
        args.technologies.clone(),
        args.initial_approach.clone(),
        abandon,
        now,
    ) {
        Ok(SessionOp::Applied(h)) => h,
        Ok(SessionOp::Replayed(h)) => {
            // Frozen replay (P2-1): a recorded response returns verbatim
            // instead of being recomputed from live state. Legacy receipts
            // without one fall through to the normal path, which recomputes
            // and then freezes.
            if let Some(frozen) = super::replay::replay_frozen_session_response(repo, admitted)? {
                return Ok(frozen);
            }
            h
        }
        Ok(SessionOp::Conflict) => return key_reuse_result(),
        Err(e) => return Err(e),
    };
    {
        let mut reg = disp.registry();
        reg.bind_session(envelope.frontend_id, envelope.channel_id, handle, false);
    }
    // Binding durability: the channel→handle route must survive restart.
    persist_before_ack(disp)?;

    // Guide suggestions for the task description.
    let task_desc = format!("{} {}", args.task_type, args.technologies.join(" "));
    let guides = repo.get_guides()?;
    let suggestions = super::guide_catalog::suggest_guides(&task_desc, &guides);
    let formatted_suggestions = super::guide_catalog::format_guide_suggestions(&suggestions);

    // Pre-load relevant memories: dense-ranked recall when a search
    // backend is attached, identical lexical fallback otherwise.
    // recall_browse owns both paths (engine ranking + snapshot scan),
    // so preload never diverges from browse recall.
    let browse_args = MemoryReadArgs {
        query: Some(task_desc.clone()),
        project: None,
        all: true,
        ..Default::default()
    };
    let mut relevant: Vec<Memory> = super::recall::recall_browse(disp, &browse_args)?.0;
    relevant.truncate(3);

    // Boost pre-loaded memories (upstream boostConfidence 0.02). A
    // failed boost fails the tool: the boost is a receipted sub-command,
    // so a retry replays-or-applies it instead of double-boosting, and
    // no success is ever frozen over a dropped canonical effect.
    let boosted: Vec<EntityId> = relevant.iter().map(|m| m.id).collect();
    if !boosted.is_empty() {
        let ctx = sub_command_ctx(envelope, 0)?;
        let cmd = DomainCommand::BoostConfidence {
            memory_ids: boosted,
        };
        disp.repo().apply(&ctx, &cmd)?;
    }

    // Track read memories into the session (canonical store, deduped).
    let read_ids: Vec<String> = relevant.iter().map(|m| legacy_id_of(repo, m)).collect();
    repo.track_session_link(
        admitted,
        handle,
        ltmrs_service::repository::SessionLinkField::MemoryRead,
        &read_ids,
    )?;

    let mut response = format!(
        "Session started: {} ({})\n",
        handle.as_uuid(),
        args.task_type
    );
    if !args.technologies.is_empty() {
        response.push_str(&format!("Technologies: {}\n", args.technologies.join(", ")));
    }

    if !relevant.is_empty() {
        response.push_str("\nPre-loaded memories:\n");
        for m in &relevant {
            let scope_tag = m.project.clone().unwrap_or_else(|| "global".to_string());
            response.push_str(&format!(
                "  [{}] [{}] {} ({:.2})\n    {}\n",
                legacy_id_of(repo, m),
                scope_tag,
                m.title,
                m.confidence,
                m.description
            ));
        }
    }

    response.push_str(&format!("\n{formatted_suggestions}"));

    // Continuity recall: dead-ends + lessons + warnings from prior sessions.
    // Recalled-attempt boosts apply exactly once per operation (P2-B):
    // claimed through the session receipt flag, so a crash between the
    // boost and the response freeze cannot double-apply on continuation.
    let (continuity, boost_targets) =
        build_continuity_recall(disp, &args.task_type, project.as_deref(), now)?;
    if !boost_targets.is_empty() {
        match repo.claim_continuity_boost(admitted, &boost_targets, 0.015, now) {
            Ok(_) => {}
            Err(e) if e.code == DomainErrorCode::KeyReuseDifferentInput => {
                return key_reuse_result();
            }
            Err(e) => return Err(e),
        }
    }
    if !continuity.is_empty() {
        response.push_str(&continuity);
    }

    // Surface pending improvement suggestions.
    let pending = repo
        .get_suggestions()?
        .into_iter()
        .filter(|s| s.status == SuggestionStatus::Pending)
        .take(3)
        .collect::<Vec<_>>();
    if !pending.is_empty() {
        response
            .push_str("\n\n## Past improvement suggestions (consider; dismiss if not relevant)\n");
        for s in &pending {
            response.push_str(&format!("- [{}] {}\n", s.id, s.suggestion));
        }
    }

    let guide_names: Vec<String> = suggestions.iter().map(|s| s.guide.clone()).collect();
    let data = json!({
        "session_id": handle.as_uuid().to_string(),
        "guides": guide_names,
        "preloaded_memories": read_ids,
    });
    // Freeze the response into the receipt (P2-1): a lost-response retry
    // returns this verbatim instead of recomputing from live state.
    let payload = ok_result(response, data);
    match super::replay::freeze_session_response(repo, admitted, &payload) {
        Ok(()) => {}
        Err(e) if e.code == DomainErrorCode::KeyReuseDifferentInput => {
            return key_reuse_result();
        }
        Err(e) => return Err(e),
    }
    Ok(payload)
}

/// Continuity recall: dead-ends, lessons and warnings from prior sessions
/// (upstream buildContinuityRecall). Storage failures propagate — a dead
/// store must read as "continuity unavailable" (loud, retryable), never as
/// "no relevant previous context" (silent missing knowledge).
fn build_continuity_recall(
    disp: &Dispatcher,
    task_type: &str,
    project: Option<&str>,
    _now: u64,
) -> DomainResult<(String, Vec<(SessionHandle, u32)>)> {
    let sessions = disp.repo().all_sessions()?;

    // Layer 1 — dead ends from similar prior sessions.
    let mut dead_ends: Vec<(SessionHandle, u32, String, Option<String>)> = Vec::new();
    for s in &sessions {
        if s.task_type.as_deref() != Some(task_type) {
            continue;
        }
        if let Some(p) = project
            && let Some(sp) = &s.project
            && sp != p
        {
            continue;
        }
        for a in &s.attempts {
            if matches!(
                a.outcome,
                AttemptOutcome::Rejected | AttemptOutcome::Partial
            ) && a.confidence >= 0.2
            {
                dead_ends.push((s.handle, a.seq, a.approach.clone(), a.critique.clone()));
            }
        }
    }
    dead_ends.sort_by(|a, b| {
        b.3.clone()
            .unwrap_or_default()
            .len()
            .cmp(&a.3.clone().unwrap_or_default().len())
    });
    dead_ends.truncate(15);

    // Layer 2 — lessons from completed similar sessions.
    let mut lessons: Vec<String> = Vec::new();
    for s in &sessions {
        if s.task_type.as_deref() != Some(task_type) || s.outcome.is_none() {
            continue;
        }
        if let Some(p) = project
            && let Some(sp) = &s.project
            && sp != p
        {
            continue;
        }
        for l in &s.lessons {
            if !l.trim().is_empty() && !lessons.contains(l) {
                lessons.push(l.clone());
            }
        }
    }
    lessons.truncate(5);

    // Layer 3 — warning fragments for this project (or global).
    let repo = disp.repo();
    let export = repo.export_snapshot()?;
    let warnings: Vec<&Memory> = export
        .memories
        .iter()
        .filter(|m| m.lifecycle.is_recallable())
        .filter(|m| matches!(m.fragment_type, FragmentType::Warning))
        .filter(|m| {
            project
                .map(|p| m.project.as_deref() == Some(p) || m.project.is_none())
                .unwrap_or(true)
        })
        .collect();

    if dead_ends.is_empty() && lessons.is_empty() && warnings.is_empty() {
        return Ok((String::new(), Vec::new()));
    }

    let mut block = format!("\n\n## Prior reasoning on similar {task_type} tasks");
    // Recalled-attempt boost targets (P2-B): this function stays a pure
    // read — the caller claims each boost exactly once per operation
    // through the session receipt flag.
    let mut boosted: Vec<(SessionHandle, u32)> = Vec::new();
    if !dead_ends.is_empty() {
        block.push_str("\n### Dead ends (don't repeat)");
        for (handle, seq, approach, critique) in &dead_ends {
            block.push_str(&format!(
                "\n- Tried: {approach}. Rejected because: {}",
                critique.as_deref().unwrap_or("unknown")
            ));
            boosted.push((*handle, *seq));
        }
    }
    if !lessons.is_empty() {
        block.push_str("\n### What worked / lessons");
        for l in &lessons {
            block.push_str(&format!("\n- {l}"));
        }
    }
    if !warnings.is_empty() {
        block.push_str("\n### Warnings");
        for w in warnings.iter().take(5) {
            let text = if !w.title.trim().is_empty() {
                w.title.clone()
            } else {
                w.fragment.chars().take(120).collect::<String>()
            };
            block.push_str(&format!("\n- {text}"));
        }
    }
    Ok((block, boosted))
}

#[cfg(test)]
use super::execute_tool;
#[cfg(test)]
use super::test_support::*;
#[cfg(test)]
use ltmrs_compat::lemma::tool_args::{
    MemoryUpdateArgs, SessionAttemptArgs, SessionEndArgs, ToolArgs,
};
#[cfg(test)]
use ltmrs_domain::clock::FrozenClock;
#[cfg(test)]
use ltmrs_service::repository::CanonicalRepository;
#[cfg(test)]
use std::sync::Arc;
#[test]
fn session_start_attempt_end_lifecycle() {
    let (disp, _dir) = test_dispatcher();
    // Start.
    let env = tool_call(
        1,
        ToolArgs::SessionStart(SessionStartArgs {
            task_type: "debugging".to_string(),
            technologies: vec!["rust".to_string()],
            initial_approach: Some("read the code".to_string()),
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::SessionStart(SessionStartArgs {
            task_type: "debugging".to_string(),
            technologies: vec!["rust".to_string()],
            initial_approach: Some("read the code".to_string()),
        }),
    );
    assert!(!result_is_error(&result));
    assert!(result_text(&result).contains("Session started:"));

    // Attempt.
    let env2 = tool_call(
        2,
        ToolArgs::SessionAttempt(SessionAttemptArgs {
            approach: "try X".to_string(),
            outcome: "rejected".to_string(),
            critique: Some("didn't work".to_string()),
            rationale: None,
            related_memory_id: None,
        }),
    );
    let result2 = run(
        &disp,
        &env2,
        &ToolArgs::SessionAttempt(SessionAttemptArgs {
            approach: "try X".to_string(),
            outcome: "rejected".to_string(),
            critique: Some("didn't work".to_string()),
            rationale: None,
            related_memory_id: None,
        }),
    );
    assert!(!result_is_error(&result2));
    assert!(text_contains(&result2, "Recorded attempt #1"));

    // End.
    let env3 = tool_call(
        3,
        ToolArgs::SessionEnd(SessionEndArgs {
            outcome: "success".to_string(),
            final_approach: Some("fixed it".to_string()),
            lessons: vec!["lesson one".to_string()],
        }),
    );
    let result3 = run(
        &disp,
        &env3,
        &ToolArgs::SessionEnd(SessionEndArgs {
            outcome: "success".to_string(),
            final_approach: Some("fixed it".to_string()),
            lessons: vec!["lesson one".to_string()],
        }),
    );
    assert!(!result_is_error(&result3));
    assert!(text_contains(&result3, "ended: success"));
}

#[test]
fn session_start_preload_boosts_confidence_by_002() {
    let (disp, _dir) = test_dispatcher();
    // Add a memory matching the task description so it gets pre-loaded.
    let id = add_fragment(
        &disp,
        1,
        "## Rust debugging fragment\n\n### Context\nRust debugging notes.",
    );
    let eid = disp.repo().resolve_id(&id).unwrap();
    // Lower confidence so the +0.02 boost is observable.
    let upd = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
        id: id.clone(),
        confidence: Some(0.5),
        ..Default::default()
    });
    let env = tool_call(2, upd.clone());
    run(&disp, &env, &upd);

    // Start a session matching "rust debugging".
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec!["rust".to_string()],
        initial_approach: None,
    });
    let env = tool_call(3, start.clone());
    let result = run(&disp, &env, &start);
    assert!(!result_is_error(&result));
    assert!(result_text(&result).contains("Pre-loaded memories:"));

    // The pre-load boost must be +0.02 (upstream boostConfidence), not +0.015.
    let mems = disp.repo().get_memories(&[eid]).unwrap();
    assert!(
        (mems[0].confidence - 0.52).abs() < 1e-9,
        "expected 0.52, got {}",
        mems[0].confidence
    );
    assert_eq!(mems[0].access_count, 1);
}

/// P1 (tool atomicity): a failed confidence-boost stage must fail the
/// tool — never freeze success over a dropped canonical effect.
/// Deterministic seed: a conflicting receipt under the boost sub-key
/// (index 0) makes the boost apply reject as key reuse.
#[test]
fn session_start_boost_conflict_fails_loudly() {
    let (disp, _dir) = test_dispatcher();
    // Seed one memory matching the task description (non-empty boost).
    add_fragment(
        &disp,
        210,
        "## Rust debugging fragment\n\n### Context\nRust debugging notes.",
    );
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec!["rust".to_string()],
        initial_approach: None,
    });
    let env = tool_call(211, start.clone());
    // Conflicting receipt under the boost sub-key: same op key,
    // different digest.
    let mut conflict_ctx = sub_command_ctx(&env, 0).unwrap();
    conflict_ctx.request_digest = "conflicting-digest".to_string();
    disp.repo()
        .apply(
            &conflict_ctx,
            &DomainCommand::Access {
                memory_ids: vec![],
                context: None,
            },
        )
        .unwrap();
    let err = execute_tool(&disp, &env, &start).unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::KeyReuseDifferentInput,
        "boost stage failure must fail the tool, got: {}",
        err.message
    );
}

/// P1 (staged completion): a session receipt without a frozen response
/// (crash between commit and freeze) completes every stage on retry —
/// boost applied, links tracked, response frozen.
#[test]
fn session_start_unfrozen_receipt_completes_stages_and_freezes() {
    let (disp, _dir) = test_dispatcher();
    let id = add_fragment(
        &disp,
        220,
        "## Rust debugging fragment\n\n### Context\nRust debugging notes.",
    );
    let eid = disp.repo().resolve_id(&id).unwrap();
    // Lower confidence so the +0.02 completion boost is observable
    // (boosts cap at 1.0).
    let upd = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
        id: id.clone(),
        confidence: Some(0.5),
        ..Default::default()
    });
    run(&disp, &tool_call(222, upd.clone()), &upd);
    // Crash window: start op commits its receipt directly (no freeze).
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec!["rust".to_string()],
        initial_approach: None,
    });
    let env = tool_call(221, start.clone());
    let digest = env.request_digest().unwrap();
    let scope = env.operation_scope(digest);
    let handle = ltmrs_domain::id::SessionHandle::new(uuid::Uuid::from_u128(221));
    disp.repo()
        .session_start_tx(
            &scope,
            handle,
            None,
            Some("debugging".to_string()),
            vec!["rust".to_string()],
            None,
            None,
            1000,
        )
        .unwrap();
    // Retry completes the boost stage instead of skipping it.
    let first = run(&disp, &env, &start);
    assert!(
        !result_is_error(&first),
        "retry must succeed, got: {}",
        result_text(&first)
    );
    let mems = disp.repo().get_memories(&[eid]).unwrap();
    assert!(
        (mems[0].confidence - 0.52).abs() < 1e-9,
        "unfrozen retry must complete the boost, got {}",
        mems[0].confidence
    );
    // And the completed response is frozen for the next retry.
    let second = run(&disp, &env, &start);
    assert_eq!(result_text(&second), result_text(&first));
}

/// Re-review R5: replaying session_start returns the recorded session
/// instead of abandoning it and creating another; key reuse rejects.
#[test]
fn session_start_replay_returns_recorded_session() {
    let (disp, _dir) = test_dispatcher();
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec![],
        initial_approach: None,
    });
    let env = tool_call(1, start.clone());
    let first = run(&disp, &env, &start);
    assert!(!result_is_error(&first));
    let first_id = result_structured(&first).unwrap()["session_id"].clone();
    // Same operation again: same session, no replacement.
    let second = run(&disp, &env, &start);
    assert!(!result_is_error(&second));
    assert_eq!(
        result_structured(&second).unwrap()["session_id"],
        first_id,
        "replay must return the recorded session"
    );
    assert_eq!(
        disp.repo().all_sessions().unwrap().len(),
        1,
        "replay must not create another session"
    );
    // Same operation ID, different arguments: reject, never execute.
    // NOTE: a new envelope carries the changed body (the digest binds
    // the envelope body, so reusing the old envelope would replay).
    let changed = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "different task".to_string(),
        technologies: vec![],
        initial_approach: None,
    });
    let changed_env = tool_call(1, changed.clone());
    let third = run(&disp, &changed_env, &changed);
    assert!(result_is_error(&third));
    assert!(result_text(&third).contains("different input"));
    assert_eq!(disp.repo().all_sessions().unwrap().len(), 1);
}

/// P2-1: a retried `session_start` (lost response) must return the
/// recorded response verbatim, even when memories added afterwards
/// would change a recomputed preload.
#[test]
fn session_start_retry_returns_frozen_response() {
    let (disp, _dir) = test_dispatcher();
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec![],
        initial_approach: None,
    });
    let env = tool_call(1, start.clone());
    let first = run(&disp, &env, &start);
    assert!(!result_is_error(&first));
    // State that would change a recomputed preload response.
    add_fragment(&disp, 2, "## Preload Changer\n\n### Context\nNew memory.");
    let second = run(&disp, &env, &start);
    assert!(!result_is_error(&second));
    assert_eq!(
        result_text(&second),
        result_text(&first),
        "retry must return the frozen original response"
    );
}

/// P2-B: `session_start` boosts recalled dead-ends through the receipt
/// claim (first execution applies once; the frozen replay path applies
/// nothing further).
#[test]
fn session_start_boosts_recalled_dead_end_once() {
    let (disp, _dir) = test_dispatcher();
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec![],
        initial_approach: None,
    });
    run(&disp, &tool_call(1, start.clone()), &start);
    let handle_a = disp.registry().channel_session(fe(1), ch(1)).unwrap();
    let attempt = ToolArgs::SessionAttempt(SessionAttemptArgs {
        approach: "try X".to_string(),
        outcome: "rejected".to_string(),
        critique: Some("bad idea".to_string()),
        rationale: None,
        related_memory_id: None,
    });
    run(&disp, &tool_call(2, attempt.clone()), &attempt);
    // Lower below the ceiling so the recall boost is observable.
    disp.repo().adjust_attempt(handle_a, 1, -0.5, 1000).unwrap();
    // A new session on the same task recalls A's dead-end and boosts it.
    let result = run(&disp, &tool_call(3, start.clone()), &start);
    assert!(!result_is_error(&result));
    assert!(
        result_text(&result).contains("Dead ends"),
        "continuity must surface the dead-end, got: {}",
        result_text(&result)
    );
    let confidence = disp.repo().get_session(handle_a).unwrap().unwrap().attempts[0].confidence;
    // 0.5 decayed by the new start (-0.002) then boosted once (+0.015).
    assert!(
        (confidence - 0.513).abs() < 1e-9,
        "recall boost must apply exactly once, got {confidence}"
    );
}

/// Dense preload: with a backend attached, a memory with zero lexical
/// overlap but a perfect dense match is proposed at session start,
/// while pure lexical ranking would truncate it away.
#[tokio::test]
async fn session_start_preload_uses_dense_when_attached() {
    use ltmrs_embeddings::e5_small::E5_SMALL_FINGERPRINT;
    use ltmrs_search::search::backend::SearchBackend;
    use ltmrs_search::search::backend::embedders::ClosureEmbedder;
    use ltmrs_search::search::projector::{FixedEmbedder, Projector, render_text};
    use ltmrs_search::search::table::SearchTable;

    let dir = tempfile::tempdir().unwrap();
    let clock: Arc<dyn ltmrs_domain::clock::Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
    let repo = Arc::new(
        CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), Arc::clone(&clock))
            .unwrap(),
    );
    repo.issue_namespace(fe(1), ch(1), 1000).unwrap();

    // Three lexically strong memories plus one zero-overlap tail.
    let seed = Dispatcher::new(
        Arc::clone(&repo),
        crate::registry::FrontendRegistry::new(),
        Arc::clone(&clock),
    );
    for (n, title, frag) in [
        (1, "Rust Async", "tokio runtime task spawn"),
        (2, "Rust Errors", "result option unwrap expect"),
        (3, "Rust Tests", "cargo test assert module"),
    ] {
        add_fragment(&seed, n, &format!("## {title}\n\n### Context\n{frag}."));
    }
    let tail_id = add_fragment(
        &seed,
        4,
        "## Tail Memory\n\n### Context\nQuantum bananas orbit pluto.",
    );

    // Project all four with fixed vectors (no FTS index: lexical leg
    // stays empty, dense decides alone).
    let lance_dir = tempfile::tempdir().unwrap();
    let table = SearchTable::open(lance_dir.path().to_str().unwrap())
        .await
        .unwrap();
    let mut proj = Projector::new(
        Arc::clone(&repo),
        table.clone(),
        Box::new(FixedEmbedder { dim: 384 }),
        E5_SMALL_FINGERPRINT,
        ltmrs_domain::id::StoreGeneration::FIRST,
    );
    proj.run_until_idle().await.unwrap();

    // Query embedder returns the tail row's exact vector whatever the
    // task text is: dense similarity 1.0 for the tail only.
    use ltmrs_search::search::projector::Embedder as _;
    let mut fx = FixedEmbedder { dim: 384 };
    let tail_vec = fx
        .embed(&render_text("Tail Memory", "Quantum bananas orbit pluto."))
        .unwrap();
    let embedder = Arc::new(ClosureEmbedder::new(move |_| Ok(tail_vec.clone())));
    let backend = Arc::new(
        SearchBackend::new(
            Arc::clone(&repo),
            table,
            Arc::new(ltmrs_search::search::backend::embedders::QueryEmbedderAdapter::new(embedder)),
        )
        .with_model_fingerprint(E5_SMALL_FINGERPRINT),
    );
    let disp =
        Dispatcher::new(repo, crate::registry::FrontendRegistry::new(), clock).with_search(backend);

    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec!["rust".to_string()],
        initial_approach: None,
    });
    let env = tool_call(10, start.clone());
    // Same sync-context rule as the dispatcher: bridge from blocking code.
    let result = tokio::task::spawn_blocking(move || run(&disp, &env, &start))
        .await
        .unwrap();
    assert!(!result_is_error(&result));
    let structured = result_structured(&result).unwrap();
    let preloaded: Vec<String> = structured["preloaded_memories"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    assert!(
        preloaded.contains(&tail_id),
        "dense perfect match must be proposed, got: {preloaded:?}"
    );
}
