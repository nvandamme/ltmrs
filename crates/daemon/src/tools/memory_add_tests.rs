//! memory_add tool tests (moved verbatim from `tools.rs`).

use std::sync::Arc;

use super::test_support::*;
use crate::dispatcher::Dispatcher;
use ltmrs_compat::lemma::tool_args::{MemoryAddArgs, SessionStartArgs, ToolArgs};
use ltmrs_domain::clock::FrozenClock;
use ltmrs_service::repository::CanonicalRepository;
use serde_json::json;

// ---- memory_add ----

#[test]
fn memory_add_stores_and_returns_id() {
    let (disp, _dir) = test_dispatcher();
    let env = tool_call(
        1,
        ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: "## Test Fragment\n\n### Context\nA test memory.\n".to_string(),
            project: Some("testproj".to_string()),
            ..Default::default()
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: "## Test Fragment\n\n### Context\nA test memory.\n".to_string(),
            project: Some("testproj".to_string()),
            ..Default::default()
        }),
    );
    assert!(!result_is_error(&result));
    let text = result_text(&result);
    assert!(text.contains("Added fragment [m"));
    let structured = result_structured(&result).unwrap();
    assert_eq!(structured["success"], json!(true));
    assert!(structured["id"].as_str().unwrap().starts_with("m"));
}

#[test]
fn memory_add_redacts_secrets_by_default() {
    let (disp, _dir) = test_dispatcher();
    let frag = "api_key = sk_abc1234567890";
    let env = tool_call(
        1,
        ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: frag.to_string(),
            ..Default::default()
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: frag.to_string(),
            ..Default::default()
        }),
    );
    assert!(!result_is_error(&result));
    // The stored fragment must be redacted.
    let structured = result_structured(&result).unwrap();
    let id = structured["id"].as_str().unwrap().to_string();
    let eid = disp.repo().resolve_id(&id).unwrap();
    let mems = disp.repo().get_memories(&[eid]).unwrap();
    assert!(mems[0].fragment.contains("[REDACTED]"));
    assert!(!mems[0].fragment.contains("sk_abc1234567890"));
}

#[test]
fn memory_add_confirm_stores_verbatim() {
    let (disp, _dir) = test_dispatcher();
    let frag = "api_key = sk_abc1234567890";
    let env = tool_call(
        1,
        ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: frag.to_string(),
            confirm: true,
            ..Default::default()
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: frag.to_string(),
            confirm: true,
            ..Default::default()
        }),
    );
    assert!(!result_is_error(&result));
    let structured = result_structured(&result).unwrap();
    let id = structured["id"].as_str().unwrap().to_string();
    let eid = disp.repo().resolve_id(&id).unwrap();
    let mems = disp.repo().get_memories(&[eid]).unwrap();
    assert!(mems[0].fragment.contains("sk_abc1234567890"));
}

#[test]
fn memory_add_rejects_duplicates() {
    let (disp, _dir) = test_dispatcher();
    add_fragment(
        &disp,
        1,
        "## Unique Topic Alpha\n\n### Context\nSomething unique about alpha topics and testing.",
    );
    let env = tool_call(2, ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: "## Unique Topic Alpha\n\n### Context\nSomething unique about alpha topics and testing.".to_string(),
            ..Default::default()
        }));
    let result = run(&disp, &env, &ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: "## Unique Topic Alpha\n\n### Context\nSomething unique about alpha topics and testing.".to_string(),
            ..Default::default()
        }));
    assert!(result_is_error(&result));
    assert!(result_text(&result).contains("similar memory already exists"));
}

/// Mutation-time dedup admission: two barrier-synchronized near-duplicate
/// adds admit exactly one. Without the similarity gate serializing
/// check+commit, both preflights would miss each other and commit.
#[test]
fn concurrent_near_duplicate_adds_admit_exactly_one() {
    let (disp, _dir) = test_dispatcher();
    let fragment =
        "## Raced Write\n\n### Context\nBarrier synchronized duplicate content here.".to_string();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|s| {
        let fragment2 = fragment.clone();
        let (b, d) = (&barrier, &disp);
        let t1 = s.spawn(move || {
            b.wait();
            let args = ToolArgs::MemoryAdd(MemoryAddArgs {
                fragment: fragment.clone(),
                ..Default::default()
            });
            run(d, &tool_call(1, args.clone()), &args)
        });
        let t2 = s.spawn(move || {
            b.wait();
            let args = ToolArgs::MemoryAdd(MemoryAddArgs {
                fragment: fragment2,
                ..Default::default()
            });
            run(d, &tool_call(2, args.clone()), &args)
        });
        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();
        let e1 = result_is_error(&r1);
        let e2 = result_is_error(&r2);
        assert_ne!(e1, e2, "exactly one racer must be rejected as duplicate");
        let (ok_text, err_text) = if e1 {
            (result_text(&r2), result_text(&r1))
        } else {
            (result_text(&r1), result_text(&r2))
        };
        assert!(ok_text.contains("Added fragment"));
        assert!(err_text.contains("similar memory already exists"));
    });
}

/// Unprojected duplicate rejected via the pending overlay: with a usable
/// table but no projection run, a near-duplicate of a pending write is
/// still rejected — no blind spot between commit and indexing.
#[tokio::test]
async fn unprojected_duplicate_rejected_via_overlay() {
    use ltmrs_search::search::backend::SearchBackend;
    use ltmrs_search::search::backend::embedders::NoDenseEmbedder;
    use ltmrs_search::search::table::SearchTable;

    let dir = tempfile::tempdir().unwrap();
    let clock: Arc<dyn ltmrs_domain::clock::Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
    let repo = Arc::new(
        CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), Arc::clone(&clock))
            .unwrap(),
    );
    repo.issue_namespace(fe(1), ch(1), 1000).unwrap();
    let lance_dir = tempfile::tempdir().unwrap();
    let table = SearchTable::open(lance_dir.path().to_str().unwrap())
        .await
        .unwrap();
    table.ensure_fts_index().await.unwrap();
    let backend = Arc::new(SearchBackend::new(
        Arc::clone(&repo),
        table,
        Arc::new(NoDenseEmbedder),
    ));
    let disp =
        Dispatcher::new(repo, crate::registry::FrontendRegistry::new(), clock).with_search(backend);
    // Sync bridge contract: blocking context for table-backed tools.
    let out = tokio::task::spawn_blocking(move || {
        let args_a = ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: "the quick brown fox jumps over the lazy dog".to_string(),
            ..Default::default()
        });
        let r_a = run(&disp, &tool_call(1, args_a.clone()), &args_a);
        assert!(!result_is_error(&r_a));
        let args_b = ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: "the quick brown fox jumps over the lazy dog today".to_string(),
            ..Default::default()
        });
        run(&disp, &tool_call(2, args_b.clone()), &args_b)
    })
    .await
    .unwrap();
    assert!(result_is_error(&out));
    assert!(result_text(&out).contains("similar memory already exists"));
}

#[test]
fn memory_add_flags_distill_candidate() {
    let (disp, _dir) = test_dispatcher();
    let env = tool_call(
        1,
        ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: "## A Lesson Learned\n\n### Context\nA lesson about testing.".to_string(),
            fragment_type: Some("lesson".to_string()),
            ..Default::default()
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: "## A Lesson Learned\n\n### Context\nA lesson about testing.".to_string(),
            fragment_type: Some("lesson".to_string()),
            ..Default::default()
        }),
    );
    assert!(result_text(&result).contains("distill candidate"));
}
/// Without a traced session, memory_add links the fragment to the
/// channel's virtual session (per-channel upstream parity) instead of
/// leaving it unlinked.
#[test]
fn memory_add_links_virtual_session_without_traced() {
    let (disp, _dir) = test_dispatcher();
    let id = add_fragment(
        &disp,
        1,
        "## Virtual Link\n\n### Context\nUnlinked without virtual sessions.",
    );
    let eid = disp.repo().resolve_id(&id).unwrap();
    let mems = disp.repo().get_memories(&[eid]).unwrap();
    let virtual_handle = disp
        .registry()
        .virtual_session(fe(1), ch(1))
        .expect("virtual session must exist after session-less add");
    assert_eq!(
        mems[0].session_id.as_deref(),
        Some(virtual_handle.as_uuid().to_string()).as_deref(),
        "fragment must link the virtual session"
    );
    let session = disp
        .registry()
        .virtual_record(virtual_handle)
        .unwrap()
        .clone();
    assert!(
        session.is_virtual && session.memories_created.contains(&id),
        "virtual session must track the created memory"
    );
}

/// A traced session shadows the virtual one: new fragments link the
/// traced handle, and the virtual session stays separate.
#[test]
fn memory_add_prefers_traced_over_virtual() {
    let (disp, _dir) = test_dispatcher();
    let first = add_fragment(
        &disp,
        1,
        "## Before Traced\n\n### Context\nLinks virtual first.",
    );
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        technologies: vec![],
        initial_approach: None,
    });
    run(&disp, &tool_call(2, start.clone()), &start);
    let second = add_fragment(
        &disp,
        3,
        "## After Traced\n\n### Context\nLinks traced now.",
    );
    let traced = disp
        .resolve_session(fe(1), ch(1))
        .expect("traced session must be active");
    let virtual_handle = disp
        .registry()
        .virtual_session(fe(1), ch(1))
        .expect("virtual session persists alongside");
    assert_ne!(traced, virtual_handle);
    let get = |id: &str| {
        let eid = disp.repo().resolve_id(id).unwrap();
        disp.repo().get_memories(&[eid]).unwrap().pop().unwrap()
    };
    assert_eq!(
        get(&first).session_id.as_deref(),
        Some(virtual_handle.as_uuid().to_string()).as_deref()
    );
    assert_eq!(
        get(&second).session_id.as_deref(),
        Some(traced.as_uuid().to_string()).as_deref(),
        "traced session must shadow the virtual one"
    );
}
/// P1 (admission lifetime): the namespace may expire between the
/// primary commit and the tool tail (TTL crossed mid-tool) — the
/// admitted tool must still succeed, never report failure for an
/// executed mutation. The commit hook advances the clock past the
/// TTL right after the primary commit, deterministically.
#[test]
fn memory_add_survives_namespace_expiry_after_commit() {
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
        CanonicalRepository::open_with_clock(
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
    // Cross the TTL exactly once, on the first post-commit hook
    // (i.e. immediately after the primary AddMemory commits).
    let ticker = Arc::clone(&clock);
    repo.set_commit_hook(Arc::new(move || {
        let mut t = ticker.0.lock().unwrap();
        if *t == 1000 {
            *t += ltmrs_service::repository::DEFAULT_NAMESPACE_TTL_MILLIS + 1;
        }
    }));
    let args = ToolArgs::MemoryAdd(MemoryAddArgs {
        fragment: "## Admitted Add\n\n### Context\nNamespace-expiry fixture.".to_string(),
        ..Default::default()
    });
    let env = tool_call(64, args.clone());
    let result = run(&disp, &env, &args);
    assert!(
        !result_is_error(&result),
        "admitted tool must succeed despite mid-tool namespace expiry, got: {}",
        result_text(&result)
    );
    let count = disp
        .repo()
        .export_snapshot()
        .unwrap()
        .memories
        .iter()
        .filter(|m| m.fragment.contains("Namespace-expiry fixture"))
        .count();
    assert_eq!(count, 1, "exactly one effect allowed");
}
/// Session linkage lands in the single AddMemory apply: no blind second
/// write (no clobber window), no document bump, one projection job.
#[test]
fn memory_add_links_session_in_single_apply() {
    let (disp, _dir) = test_dispatcher();
    let id = add_fragment(&disp, 1, "## Linked\n\n### Context\nSession link fixture.");
    let eid = disp.repo().resolve_id(&id).unwrap();
    let stored = disp.repo().get_memories(&[eid]).unwrap();
    assert_eq!(stored.len(), 1);
    assert!(
        stored[0].session_id.is_some(),
        "session link must be stored on the record"
    );
    assert_eq!(
        stored[0].document_revision.as_u64(),
        0,
        "single apply performs no link bump"
    );
    let job = disp
        .repo()
        .projection_job(eid)
        .unwrap()
        .expect("add enqueues one pending job");
    assert_eq!(job.seq, 1, "single enqueue, no re-point");
}
