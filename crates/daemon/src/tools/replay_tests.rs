//! replay / receipt / envelope-purity tests (moved verbatim from `tools.rs`).

use super::execute_tool;
use super::ids::new_legacy_id;
use super::replay::sub_command_ctx;
use super::test_support::*;
use crate::dispatcher::Dispatcher;
use ltmrs_compat::lemma::tool_args::{
    BackupCreateArgs, BackupPreviewArgs, BackupRestoreArgs, MemoryAddArgs, MemoryFeedbackArgs,
    MemoryForgetArgs, MemoryRelateArgs, MemoryUpdateArgs, SessionStartArgs, ToolArgs,
};
use ltmrs_domain::command::DomainCommand;
use ltmrs_domain::id::EntityId;
use ltmrs_domain::memory::{FragmentType, Instant, Memory, MemorySource};

/// P1 (replay durability): a primary commit whose barrier fails must
/// not report success on retry while the barrier keeps failing —
/// receipt visibility is never proof of durable completion. Barrier
/// faults stay armed across both attempts; healing the barrier lets
/// the same envelope converge to success via rebuild+freeze.
#[test]
fn tool_replay_needs_durable_barrier() {
    let (disp, _dir) = test_dispatcher();
    disp.repo().fault_injector().set_persist_failures(1000);
    let args = ToolArgs::MemoryAdd(MemoryAddArgs {
        fragment: "## Barrier Replay\n\n### Context\nDurability fixture.".to_string(),
        ..Default::default()
    });
    let env = tool_call(70, args.clone());
    assert!(
        execute_tool(&disp, &env, &args).is_err(),
        "unflushed primary must error"
    );
    // Retry with the barrier still failing: receipt exists, nothing
    // is frozen — must NOT report success.
    assert!(
        execute_tool(&disp, &env, &args).is_err(),
        "replay without durability must error"
    );
    // Heal: the same envelope rebuilds from the receipt, freezes,
    // and succeeds exactly once.
    disp.repo().fault_injector().set_persist_failures(0);
    let result = run(&disp, &env, &args);
    assert!(!result_is_error(&result));
    let count = disp
        .repo()
        .export_snapshot()
        .unwrap()
        .memories
        .iter()
        .filter(|m| m.fragment.contains("Durability fixture"))
        .count();
    assert_eq!(count, 1, "exactly one effect allowed");
    // Frozen replay still barriers: re-arm faults and retry the
    // now-frozen operation — must NOT report success either.
    disp.repo().fault_injector().set_persist_failures(1000);
    assert!(
        execute_tool(&disp, &env, &args).is_err(),
        "frozen replay without durability must error"
    );
    disp.repo().fault_injector().set_persist_failures(0);
}

/// P1 (replay purity): replaying an add must create no new effects
/// and return the original response verbatim — even when the store
/// changed since (B now overlaps A). Second variant below: A
/// originally linked X, then a stronger Y arrives; replay must
/// still describe X and create no A→Y edge.
#[test]
fn add_replay_creates_no_new_effects() {
    let (disp, _dir) = test_dispatcher();
    let find = |frag: &str| {
        disp.repo()
            .export_snapshot()
            .unwrap()
            .memories
            .into_iter()
            .find(|m| m.fragment.contains(frag))
            .expect("fixture memory must exist")
            .id
    };
    // Outgoing edges only: B legitimately links TO A on its own
    // first execution; replay must add none FROM A.
    let edges_of = |id: ltmrs_domain::id::EntityId| {
        disp.repo()
            .neighbors(id)
            .unwrap()
            .into_iter()
            .filter(|r| r.source == id)
            .count()
    };
    // A lands with no overlap: no auto-link anywhere.
    let args_a = ToolArgs::MemoryAdd(MemoryAddArgs {
        fragment: "## Quiescent Solo\n\n### Context\nZirconium lattice meridians.".to_string(),
        ..Default::default()
    });
    let env_a = tool_call(71, args_a.clone());
    let first = run(&disp, &env_a, &args_a);
    assert!(!result_is_error(&first));
    let first_text = result_text(&first);
    let eid_a = find("Quiescent Solo");
    assert_eq!(edges_of(eid_a), 0, "A must land link-free");
    // B arrives overlapping A.
    let args_b = ToolArgs::MemoryAdd(MemoryAddArgs {
        fragment: "## Overlapping Other\n\n### Context\nZirconium lattice parallels.".to_string(),
        ..Default::default()
    });
    let env_b = tool_call(72, args_b.clone());
    assert!(!result_is_error(&run(&disp, &env_b, &args_b)));
    // Replay the ORIGINAL A envelope: same bytes, no new relation.
    let replayed = run(&disp, &env_a, &args_a);
    assert!(!result_is_error(&replayed));
    assert_eq!(
        result_text(&replayed),
        first_text,
        "replay must return the original bytes verbatim"
    );
    assert_eq!(edges_of(eid_a), 0, "replay must create no relations");
}

/// P1 (replay purity, stronger-overlap variant): A originally linked
/// X; a stronger Y arrives later. Replaying A must describe X (the
/// recorded link), create no A→Y edge, and return the original text.
#[test]
fn add_replay_keeps_original_autolink() {
    let (disp, _dir) = test_dispatcher();
    let find = |frag: &str| {
        disp.repo()
            .export_snapshot()
            .unwrap()
            .memories
            .into_iter()
            .find(|m| m.fragment.contains(frag))
            .expect("fixture memory must exist")
            .id
    };
    // X first (nothing to link to).
    let args_x = ToolArgs::MemoryAdd(MemoryAddArgs {
        fragment: "## Anchor Xray\n\n### Context\ntungsten carbide tooling delta echo".to_string(),
        ..Default::default()
    });
    let result = run(&disp, &tool_call(73, args_x.clone()), &args_x);
    assert!(!result_is_error(&result));
    let eid_x = find("Anchor Xray");
    // A overlaps X: links X on first execution.
    let args_a = ToolArgs::MemoryAdd(MemoryAddArgs {
        fragment: "## Linked Apex\n\n### Context\ntungsten carbide latency alpha bravo".to_string(),
        ..Default::default()
    });
    let env_a = tool_call(74, args_a.clone());
    let first = run(&disp, &env_a, &args_a);
    assert!(!result_is_error(&first));
    let first_text = result_text(&first);
    assert!(
        first_text.contains("AUTO-LINKED"),
        "A must link on first execution, got: {first_text}"
    );
    let eid_a = find("Linked Apex");
    let outgoing = |id: ltmrs_domain::id::EntityId| {
        disp.repo()
            .neighbors(id)
            .unwrap()
            .into_iter()
            .filter(|r| r.source == id)
            .map(|r| r.target)
            .collect::<Vec<_>>()
    };
    let linked_to = outgoing(eid_a);
    assert_eq!(
        linked_to,
        vec![eid_x],
        "A must link exactly X, got {linked_to:?}"
    );
    // Y arrives overlapping A at least as strongly.
    let args_y = ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: "## Rival Yonder\n\n### Context\ntungsten carbide latency alpha bravo foxtrot golf hotel".to_string(),
            ..Default::default()
        });
    let result = run(&disp, &tool_call(75, args_y.clone()), &args_y);
    assert!(!result_is_error(&result));
    // Sanity: Y really does overlap A more strongly than X does (the
    // test only bites if a re-plan would prefer Y).
    assert!(
        ltmrs_search::similarity::jaccard(
            "tungsten carbide latency alpha bravo foxtrot golf hotel",
            "tungsten carbide latency alpha bravo"
        ) > ltmrs_search::similarity::jaccard(
            "tungsten carbide latency alpha bravo",
            "tungsten carbide tooling delta echo"
        ),
        "Y must out-overlap X for the regression to bite"
    );
    // Replay the ORIGINAL A envelope.
    let replayed = run(&disp, &env_a, &args_a);
    assert!(!result_is_error(&replayed));
    assert_eq!(
        result_text(&replayed),
        first_text,
        "replay must return the original bytes verbatim"
    );
    let linked_to = outgoing(eid_a);
    assert_eq!(
        linked_to,
        vec![eid_x],
        "replay must create no new edges, got {linked_to:?}"
    );
}

/// P2-high (crash-window fidelity): an UNFROZEN add receipt rebuilds
/// from the request alone — a fragment/title edit landing after the
/// commit must not rewrite what the first operation reported.
#[test]
fn add_unfrozen_rebuild_uses_request_content_only() {
    let (disp, _dir) = test_dispatcher();
    let args = ToolArgs::MemoryAdd(MemoryAddArgs {
        fragment: "## Crash Add\n\n### Context\nOriginal content.".to_string(),
        title: Some("Original Title".to_string()),
        ..Default::default()
    });
    let env131 = tool_call(131, args.clone());
    // Crash window: op-131 add commits its receipt directly (same
    // deterministic ids the fresh path would mint), freezing nothing.
    let legacy_id = new_legacy_id(&env131);
    let eid = EntityId::new(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("ltmrs:entity:{}", env131.operation_id.as_uuid()).as_bytes(),
    ));
    let memory = Memory {
        id: eid,
        external_alias: Some(ltmrs_domain::id::ExternalAlias::new(legacy_id.clone())),
        title: "Original Title".to_string(),
        fragment: "## Crash Add\n\n### Context\nOriginal content.".to_string(),
        description: "Original content.".to_string(),
        fragment_type: FragmentType::Fact,
        project: None,
        project_id: None,
        source: MemorySource::Ai,
        confidence: 1.0,
        quality_score: None,
        lifecycle: ltmrs_domain::memory::MemoryLifecycle::Live,
        tags: Vec::new(),
        associated_with: Vec::new(),
        relations: Vec::new(),
        parent_id: None,
        child_ids: Vec::new(),
        session_id: None,
        task_type: None,
        related_guides: Vec::new(),
        evidence: Vec::new(),
        access_count: 0,
        last_accessed_at: None,
        positive_feedback: 0,
        negative_feedback: 0,
        negative_hits: 0,
        refinement_count: 0,
        distill_candidate: false,
        entity_revision: ltmrs_domain::id::EntityRevision::new(0),
        document_revision: ltmrs_domain::id::DocumentRevision::new(0),
        eligibility_revision: ltmrs_domain::id::EligibilityRevision::new(0),
        created_at: Instant::new(1000),
        updated_at: Instant::new(1000),
        raw_created: None,
        unknown_fields: std::collections::BTreeMap::new(),
    };
    let ctx = sub_command_ctx(&env131, 0).unwrap();
    disp.repo()
        .apply(
            &ctx,
            &DomainCommand::AddMemory {
                memory,
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    // A concurrent edit lands after the commit.
    let edit = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
        id: legacy_id.clone(),
        title: Some("Edited Title".to_string()),
        fragment: Some("Edited content.".to_string()),
        confidence: None,
    });
    let edited = run(&disp, &tool_call(132, edit.clone()), &edit);
    assert!(!result_is_error(&edited));
    // Retry op 131: the Unfrozen receipt rebuilds from the request —
    // the later edit must not rewrite the reported content.
    let replayed = run(&disp, &env131, &args);
    let text = result_text(&replayed);
    assert!(
        !result_is_error(&replayed),
        "replay must succeed, got: {text}"
    );
    assert!(
        text.contains("\"Original Title\""),
        "unfrozen rebuild must report the requested title, got: {text}"
    );
    assert!(
        !text.contains("Edited"),
        "unfrozen rebuild must not report later state, got: {text}"
    );
}

/// P1 (staged completion): an UNFROZEN add receipt (committed, link +
/// freeze lost) completes the canonical session link on retry instead
/// of freezing success over a missing attribution.
#[test]
fn add_unfrozen_receipt_completes_session_link_and_freezes() {
    let (disp, _dir) = test_dispatcher();
    // Canonical session on the channel.
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "linking".to_string(),
        technologies: vec![],
        initial_approach: None,
    });
    let started = run(&disp, &tool_call(230, start.clone()), &start);
    assert!(!result_is_error(&started));
    // Crash window: op-231 add commits its receipt directly (no link,
    // no freeze).
    let args = ToolArgs::MemoryAdd(MemoryAddArgs {
        fragment: "## Link Me\n\n### Context\nLink fixture.".to_string(),
        title: Some("Link Me".to_string()),
        ..Default::default()
    });
    let env = tool_call(231, args.clone());
    let legacy_id = new_legacy_id(&env);
    let eid = EntityId::new(uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("ltmrs:entity:{}", env.operation_id.as_uuid()).as_bytes(),
    ));
    let memory = Memory {
        id: eid,
        external_alias: Some(ltmrs_domain::id::ExternalAlias::new(legacy_id.clone())),
        title: "Link Me".to_string(),
        fragment: "## Link Me\n\n### Context\nLink fixture.".to_string(),
        description: "Link fixture.".to_string(),
        fragment_type: FragmentType::Fact,
        project: None,
        project_id: None,
        source: MemorySource::Ai,
        confidence: 1.0,
        quality_score: None,
        lifecycle: ltmrs_domain::memory::MemoryLifecycle::Live,
        tags: Vec::new(),
        associated_with: Vec::new(),
        relations: Vec::new(),
        parent_id: None,
        child_ids: Vec::new(),
        session_id: None,
        task_type: None,
        related_guides: Vec::new(),
        evidence: Vec::new(),
        access_count: 0,
        last_accessed_at: None,
        positive_feedback: 0,
        negative_feedback: 0,
        negative_hits: 0,
        refinement_count: 0,
        distill_candidate: false,
        entity_revision: ltmrs_domain::id::EntityRevision::new(0),
        document_revision: ltmrs_domain::id::DocumentRevision::new(0),
        eligibility_revision: ltmrs_domain::id::EligibilityRevision::new(0),
        created_at: Instant::new(1000),
        updated_at: Instant::new(1000),
        raw_created: None,
        unknown_fields: std::collections::BTreeMap::new(),
    };
    let ctx = sub_command_ctx(&env, 0).unwrap();
    disp.repo()
        .apply(
            &ctx,
            &DomainCommand::AddMemory {
                memory,
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    // Retry completes the canonical link, then freezes.
    let first = run(&disp, &env, &args);
    assert!(
        !result_is_error(&first),
        "retry must succeed, got: {}",
        result_text(&first)
    );
    let linked = disp
        .repo()
        .all_sessions()
        .unwrap()
        .iter()
        .any(|s| s.memories_created.contains(&legacy_id));
    assert!(
        linked,
        "unfrozen retry must complete the session link for [{legacy_id}]"
    );
    let second = run(&disp, &env, &args);
    assert_eq!(result_text(&second), result_text(&first));
}

/// P1 (tool replay): the same MemoryAdd envelope twice must replay
/// success (exactly one memory) — never fail the second delivery in
/// the dedup scan against the memory the first delivery created.
#[test]
fn memory_add_same_envelope_replays_success() {
    let (disp, _dir) = test_dispatcher();
    let fragment = "## Replay Add\n\n### Context\nSame-envelope replay fixture.";
    let args = ToolArgs::MemoryAdd(MemoryAddArgs {
        fragment: fragment.to_string(),
        ..Default::default()
    });
    let env = tool_call(60, args.clone());
    let first = run(&disp, &env, &args);
    assert!(!result_is_error(&first));
    let second = run(&disp, &env, &args);
    assert!(
        !result_is_error(&second),
        "same-envelope replay must succeed, got: {}",
        result_text(&second)
    );
    let count = disp
        .repo()
        .export_snapshot()
        .unwrap()
        .memories
        .iter()
        .filter(|m| m.fragment == fragment)
        .count();
    assert_eq!(count, 1, "exactly one memory may exist");
}

/// Error envelopes: unknown IDs and out-of-range values fail loudly
/// with actionable messages (never silent success or empty results).
#[test]
fn error_envelope_unknown_ids_and_bad_values() {
    let (disp, _dir) = test_dispatcher();
    let id = add_fragment(
        &disp,
        1,
        "## Envelope Fragment\n\n### Context\nFor error testing.",
    );

    // Feedback on an unknown memory.
    let fb = ToolArgs::MemoryFeedback(MemoryFeedbackArgs {
        id: "missing".to_string(),
        useful: true,
    });
    let result = run(&disp, &tool_call(2, fb.clone()), &fb);
    assert!(result_is_error(&result));

    // Forget on an unknown memory.
    let forget = ToolArgs::MemoryForget(MemoryForgetArgs {
        id: "missing".to_string(),
        ..Default::default()
    });
    let result = run(&disp, &tool_call(3, forget.clone()), &forget);
    assert!(result_is_error(&result));

    // Relate to an unknown target.
    let relate = ToolArgs::MemoryRelate(MemoryRelateArgs {
        source_id: id.clone(),
        target_id: "missing".to_string(),
        relation_type: "supports".to_string(),
        note: None,
    });
    let result = run(&disp, &tool_call(4, relate.clone()), &relate);
    assert!(result_is_error(&result));

    // Update with an out-of-range confidence.
    let upd = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
        id,
        confidence: Some(5.0),
        ..Default::default()
    });
    let result = run(&disp, &tool_call(5, upd.clone()), &upd);
    assert!(result_is_error(&result));
}

/// T-CONC-03 lose-then-replay after expiry: commit, cross the namespace
/// TTL, then replay the same envelope. Admission must refuse the expired
/// epoch (StaleReplay) — never replay into it, never re-execute as new.
#[test]
fn replay_after_namespace_expiry_is_refused_not_reexecuted() {
    use std::sync::{Arc, Mutex};
    struct AdvancingClock(Mutex<u64>);
    impl ltmrs_domain::clock::Clock for AdvancingClock {
        fn now_millis(&self) -> u64 {
            *self.0.lock().unwrap()
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let clock: Arc<AdvancingClock> = Arc::new(AdvancingClock(Mutex::new(1000)));
    let repo = Arc::new(
        ltmrs_service::repository::CanonicalRepository::open_with_clock(
            dir.path().to_str().unwrap(),
            Arc::clone(&clock) as Arc<dyn ltmrs_domain::clock::Clock + Send + Sync>,
        )
        .unwrap(),
    );
    repo.issue_namespace(fe(1), ch(1), 1000).unwrap();
    let disp = Dispatcher::new(
        Arc::clone(&repo),
        crate::registry::FrontendRegistry::new(),
        Arc::clone(&clock) as Arc<dyn ltmrs_domain::clock::Clock + Send + Sync>,
    );
    let add = ToolArgs::MemoryAdd(MemoryAddArgs {
        fragment: "## Expiry Replay\n\n### Context\nCommitted before the TTL crossed.".to_string(),
        ..Default::default()
    });
    let env = tool_call(1, add.clone());
    let first = run(&disp, &env, &add);
    assert!(!result_is_error(&first));
    // Cross the 24h TTL: the recording epoch is now expired.
    *clock.0.lock().unwrap() += ltmrs_service::repository::DEFAULT_NAMESPACE_TTL_MILLIS + 1;
    let err = execute_tool(&disp, &env, &add).unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::StaleReplay,
        "expired replay must be refused, got: {}",
        err.message
    );
    let count = repo
        .export_snapshot()
        .unwrap()
        .memories
        .iter()
        .filter(|m| m.fragment.contains("Expiry Replay"))
        .count();
    assert_eq!(count, 1, "expired replay must not execute as new");
}

/// T-CONC-03 + T-REC-02 generation cut, frozen receipt: the recorded
/// response replays verbatim. No new effect is recorded under the cut
/// generation — a frozen replay returns bytes, it does not write.
#[test]
fn replay_after_generation_cut_returns_frozen_without_new_write() {
    let (disp, _dir) = test_dispatcher();
    let add = ToolArgs::MemoryAdd(MemoryAddArgs {
        fragment: "## Generation Cut\n\n### Context\nCommitted before the cut.".to_string(),
        ..Default::default()
    });
    let env = tool_call(1, add.clone());
    let first = run(&disp, &env, &add);
    assert!(!result_is_error(&first));
    let first_text = result_text(&first);
    disp.repo()
        .set_store_generation(ltmrs_domain::id::StoreGeneration::new(2))
        .unwrap();
    let replayed = run(&disp, &env, &add);
    assert!(
        !result_is_error(&replayed),
        "frozen replay must stay servable, got: {}",
        result_text(&replayed)
    );
    assert_eq!(
        result_text(&replayed),
        first_text,
        "cut-generation replay must return the recorded bytes"
    );
    let count = disp
        .repo()
        .export_snapshot()
        .unwrap()
        .memories
        .iter()
        .filter(|m| m.fragment.contains("Generation Cut"))
        .count();
    assert_eq!(count, 1, "cut-generation replay must not write");
}

/// T-CONC-03 fresh write under a cut generation: the durability fence
/// refuses loudly (StaleGeneration) instead of recording the write
/// outside the live generation.
#[test]
fn fresh_write_under_cut_generation_is_refused() {
    let (disp, _dir) = test_dispatcher();
    disp.repo()
        .set_store_generation(ltmrs_domain::id::StoreGeneration::new(2))
        .unwrap();
    let add = ToolArgs::MemoryAdd(MemoryAddArgs {
        fragment: "## Cut Write\n\n### Context\nFresh op under a stale envelope.".to_string(),
        ..Default::default()
    });
    // Fresh operation id, but the envelope still names generation FIRST.
    let env = tool_call(1, add.clone());
    let err = execute_tool(&disp, &env, &add).unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::StaleGeneration,
        "fresh write under a cut generation must be refused, got: {}",
        err.message
    );
    let count = disp
        .repo()
        .export_snapshot()
        .unwrap()
        .memories
        .iter()
        .filter(|m| m.fragment.contains("Cut Write"))
        .count();
    assert_eq!(count, 0, "refused write must not land");
}

/// T-REC-02 replay after a real restore: the replace drains receipts
/// and namespaces, so the pre-restore envelope is refused at admission
/// (unknown namespace) — never replayed, never re-executed.
#[test]
fn replay_after_restore_is_refused_not_replayed() {
    let (disp, _dir) = test_dispatcher();
    let add = ToolArgs::MemoryAdd(MemoryAddArgs {
        fragment: "## Restore Replay\n\n### Context\nCommitted before restore.".to_string(),
        ..Default::default()
    });
    let env = tool_call(1, add.clone());
    let first = run(&disp, &env, &add);
    assert!(!result_is_error(&first));
    // Back up, then restore: the generation cuts and receipts drain.
    let out = tempfile::tempdir().unwrap();
    let create = ToolArgs::BackupCreate(BackupCreateArgs {
        directory: Some(out.path().to_str().unwrap().to_string()),
    });
    let result = run(&disp, &tool_call(2, create.clone()), &create);
    let backup_path = result_structured(&result).unwrap()["path"]
        .as_str()
        .unwrap()
        .to_string();
    let preview = ToolArgs::BackupPreview(BackupPreviewArgs {
        path: Some(backup_path.clone()),
    });
    let result = run(&disp, &tool_call(3, preview.clone()), &preview);
    let token = result_structured(&result).unwrap()["confirmation_token"]
        .as_str()
        .unwrap()
        .to_string();
    let restore = ToolArgs::BackupRestore(BackupRestoreArgs {
        confirmation_token: Some(token),
        confirm: Some(true),
    });
    let result = run(&disp, &tool_call(4, restore.clone()), &restore);
    assert!(!result_is_error(&result), "restore must succeed");
    // The pre-restore envelope is now admission-unknown: refused loudly.
    let err = execute_tool(&disp, &env, &add).unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::StaleReplay,
        "post-restore replay must be refused, got: {}",
        err.message
    );
    let count = disp
        .repo()
        .export_snapshot()
        .unwrap()
        .memories
        .iter()
        .filter(|m| m.fragment.contains("Restore Replay"))
        .count();
    assert_eq!(count, 1, "post-restore replay must not re-execute");
}

/// T-REC-02 retry with projection state: replaying a committed add
/// records nothing new — no duplicate memory, no fresh projection job.
#[test]
fn retry_after_projection_state_replays_without_new_effects() {
    let (disp, _dir) = test_dispatcher();
    let add = ToolArgs::MemoryAdd(MemoryAddArgs {
        fragment: "## Projection Replay\n\n### Context\nCommitted with a projection job pending."
            .to_string(),
        ..Default::default()
    });
    let env = tool_call(1, add.clone());
    let first = run(&disp, &env, &add);
    assert!(!result_is_error(&first));
    let eid = disp
        .repo()
        .resolve_id(result_structured(&first).unwrap()["id"].as_str().unwrap())
        .unwrap();
    assert!(
        disp.repo().projection_job(eid).is_ok(),
        "committed add must enqueue a projection job"
    );
    let replayed = run(&disp, &env, &add);
    assert!(!result_is_error(&replayed));
    assert_eq!(
        result_text(&replayed),
        result_text(&first),
        "replay must return the recorded response"
    );
    let count = disp
        .repo()
        .export_snapshot()
        .unwrap()
        .memories
        .iter()
        .filter(|m| m.fragment.contains("Projection Replay"))
        .count();
    assert_eq!(count, 1, "replay must not duplicate the memory");
}
