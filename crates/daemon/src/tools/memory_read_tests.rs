//! memory_read tool tests (moved verbatim from `tools.rs`).

use std::sync::Arc;

use super::test_support::*;
use crate::dispatcher::Dispatcher;
use ltmrs_compat::lemma::tool_args::{MemoryReadArgs, MemoryUpdateArgs, ToolArgs};
use ltmrs_domain::clock::FrozenClock;
use ltmrs_service::repository::CanonicalRepository;
use serde_json::json;

// ---- memory_read ----

#[test]
fn memory_read_browse_returns_summary() {
    let (disp, _dir) = test_dispatcher();
    add_fragment(
        &disp,
        1,
        "## Browseable Fragment One\n\n### Context\nFirst memory for browse testing.",
    );
    add_fragment(
        &disp,
        2,
        "## Browseable Fragment Two\n\n### Context\nSecond memory for browse testing.",
    );
    let env = tool_call(3, ToolArgs::MemoryRead(MemoryReadArgs::default()));
    let result = run(
        &disp,
        &env,
        &ToolArgs::MemoryRead(MemoryReadArgs::default()),
    );
    assert!(!result_is_error(&result));
    let text = result_text(&result);
    assert!(text.contains("## Memory Fragments"));
    assert!(text.contains("Browseable Fragment One"));
    let structured = result_structured(&result).unwrap();
    assert_eq!(structured["count"].as_u64().unwrap(), 2);
}

/// An attached but empty search backend must not hide canonical
/// knowledge: browse falls back to the snapshot scan (fresh-E5-start
/// regression test — an empty index is not a no-answer).
#[tokio::test]
async fn memory_read_browse_falls_back_on_empty_backend() {
    use ltmrs_search::search::backend::SearchBackend;
    use ltmrs_search::search::backend::embedders::ClosureEmbedder;
    use ltmrs_search::search::table::SearchTable;

    let dir = tempfile::tempdir().unwrap();
    let clock: Arc<dyn ltmrs_domain::clock::Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
    let repo = Arc::new(
        CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), Arc::clone(&clock))
            .unwrap(),
    );
    repo.issue_namespace(fe(1), ch(1), 1000).unwrap();
    let seed = Dispatcher::new(
        Arc::clone(&repo),
        crate::registry::FrontendRegistry::new(),
        Arc::clone(&clock),
    );
    add_fragment(
        &seed,
        1,
        "## Fallback Fragment\n\n### Context\nVisible without an index.",
    );

    // Empty projection table behind a working embedder.
    let lance_dir = tempfile::tempdir().unwrap();
    let table = SearchTable::open(lance_dir.path().to_str().unwrap())
        .await
        .unwrap();
    let embedder = Arc::new(ClosureEmbedder::new(|_| Ok(vec![0.0; 384])));
    let backend = Arc::new(SearchBackend::new(
        Arc::clone(&repo),
        table,
        Arc::new(ltmrs_search::search::backend::embedders::QueryEmbedderAdapter::new(embedder)),
    ));
    let disp =
        Dispatcher::new(repo, crate::registry::FrontendRegistry::new(), clock).with_search(backend);
    let args = MemoryReadArgs {
        query: Some("fallback".to_string()),
        all: true,
        ..Default::default()
    };

    // retrieve_sync bridges onto the runtime and must run from a
    // synchronous context, exactly like the dispatcher's spawn_blocking.
    let (out, method) =
        tokio::task::spawn_blocking(move || super::recall::recall_browse(&disp, &args))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        out.len(),
        1,
        "empty backend must fall back to the snapshot scan"
    );
    assert_eq!(method, "degraded_snapshot");
}

/// Fingerprint plumbing: a dense-capable backend (fingerprint declared)
/// runs the dense leg (embedder invoked once); a backend without one
/// stays lexical-only. Empty table → fallback results either way.
#[tokio::test]
async fn recall_browse_passes_fingerprint_to_dense_leg() {
    use ltmrs_search::search::backend::SearchBackend;
    use ltmrs_search::search::backend::embedders::ClosureEmbedder;
    use ltmrs_search::search::table::SearchTable;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let dir = tempfile::tempdir().unwrap();
    let clock: Arc<dyn ltmrs_domain::clock::Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
    let repo = Arc::new(
        CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), Arc::clone(&clock))
            .unwrap(),
    );
    repo.issue_namespace(fe(1), ch(1), 1000).unwrap();
    let seed = Dispatcher::new(
        Arc::clone(&repo),
        crate::registry::FrontendRegistry::new(),
        Arc::clone(&clock),
    );
    add_fragment(
        &seed,
        1,
        "## Plumbed Fragment\n\n### Context\nDense leg must run.",
    );

    // Empty table behind a counting embedder.
    let lance_dir = tempfile::tempdir().unwrap();
    let table = SearchTable::open(lance_dir.path().to_str().unwrap())
        .await
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let embedder = Arc::new(ClosureEmbedder::new({
        let calls = Arc::clone(&calls);
        move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![0.0; 384])
        }
    }));
    // Dense-capable test double: declare the fingerprint so the dense
    // leg runs (backends without one are lexical-only by design).
    let backend = Arc::new(
        SearchBackend::new(
            Arc::clone(&repo),
            table,
            Arc::new(ltmrs_search::search::backend::embedders::QueryEmbedderAdapter::new(embedder)),
        )
        .with_model_fingerprint(ltmrs_embeddings::e5_small::E5_SMALL_FINGERPRINT),
    );
    let disp =
        Dispatcher::new(repo, crate::registry::FrontendRegistry::new(), clock).with_search(backend);
    let args = MemoryReadArgs {
        query: Some("plumbed".to_string()),
        all: true,
        ..Default::default()
    };

    let (out, method) =
        tokio::task::spawn_blocking(move || super::recall::recall_browse(&disp, &args))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(out.len(), 1, "empty table falls back to the snapshot");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "dense-capable backend must run the dense leg"
    );
    assert_eq!(method, "degraded_snapshot");
}
#[test]
fn memory_read_by_id_returns_detail() {
    let (disp, _dir) = test_dispatcher();
    let id = add_fragment(
        &disp,
        1,
        "## Detail Fragment\n\n### Context\nA detail memory.",
    );
    let env = tool_call(
        2,
        ToolArgs::MemoryRead(MemoryReadArgs {
            id: Some(id.clone()),
            ..Default::default()
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::MemoryRead(MemoryReadArgs {
            id: Some(id.clone()),
            ..Default::default()
        }),
    );
    assert!(!result_is_error(&result));
    let text = result_text(&result);
    assert!(text.contains("=== MEMORY FRAGMENT DETAIL ==="));
    assert!(text.contains(&id));
    let structured = result_structured(&result).unwrap();
    assert_eq!(structured["count"].as_u64().unwrap(), 1);
    assert_eq!(structured["fragments"][0]["id"], json!(id));
}

#[test]
fn memory_read_unknown_id_is_error() {
    let (disp, _dir) = test_dispatcher();
    let env = tool_call(
        1,
        ToolArgs::MemoryRead(MemoryReadArgs {
            id: Some("nonexistent".to_string()),
            ..Default::default()
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::MemoryRead(MemoryReadArgs {
            id: Some("nonexistent".to_string()),
            ..Default::default()
        }),
    );
    assert!(result_is_error(&result));
}

#[test]
fn memory_read_explain_includes_recall_explanation() {
    let (disp, _dir) = test_dispatcher();
    let id = add_fragment(
        &disp,
        1,
        "## Explain Fragment\n\n### Context\nFor explain testing.",
    );
    let env = tool_call(
        2,
        ToolArgs::MemoryRead(MemoryReadArgs {
            id: Some(id.clone()),
            explain: true,
            ..Default::default()
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::MemoryRead(MemoryReadArgs {
            id: Some(id.clone()),
            explain: true,
            ..Default::default()
        }),
    );
    assert!(!result_is_error(&result));
    let text = result_text(&result);
    assert!(text.contains("## Why these memories?"));
    assert!(text.contains("Requested by ID; no relevance ranking"));
    let structured = result_structured(&result).unwrap();
    let exp = &structured["recall_explanation"];
    assert_eq!(exp["applies_to"], json!("this_call"));
    assert_eq!(exp["scope"]["mode"], json!("explicit_ids"));
    assert_eq!(exp["items"].as_array().unwrap().len(), 1);
    assert_eq!(exp["items"][0]["selection"]["method"], json!("explicit_id"));
    assert_eq!(
        exp["items"][0]["provenance"]["recorded_source"],
        json!("ai")
    );
    assert_eq!(exp["items"][0]["freshness"]["status"], json!("no_evidence"));
}

#[test]
fn memory_read_explain_query_mode_uses_ranked_method() {
    let (disp, _dir) = test_dispatcher();
    add_fragment(
        &disp,
        1,
        "## Query Explain Alpha\n\n### Context\nAbout alpha query explain testing.",
    );
    let env = tool_call(
        2,
        ToolArgs::MemoryRead(MemoryReadArgs {
            query: Some("alpha query explain".to_string()),
            explain: true,
            ..Default::default()
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::MemoryRead(MemoryReadArgs {
            query: Some("alpha query explain".to_string()),
            explain: true,
            ..Default::default()
        }),
    );
    assert!(!result_is_error(&result));
    let structured = result_structured(&result).unwrap();
    let exp = &structured["recall_explanation"];
    assert_eq!(exp["scope"]["mode"], json!("project_and_global"));
    let items = exp["items"].as_array().unwrap();
    assert!(!items.is_empty());
    assert_eq!(items[0]["selection"]["method"], json!("degraded_snapshot"));
    assert_eq!(items[0]["selection"]["rank"], json!(1));
}

#[test]
fn memory_read_without_explain_has_no_explanation() {
    let (disp, _dir) = test_dispatcher();
    let id = add_fragment(
        &disp,
        1,
        "## No Explain Fragment\n\n### Context\nExplain flag off.",
    );
    let env = tool_call(
        2,
        ToolArgs::MemoryRead(MemoryReadArgs {
            id: Some(id.clone()),
            ..Default::default()
        }),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::MemoryRead(MemoryReadArgs {
            id: Some(id.clone()),
            ..Default::default()
        }),
    );
    assert!(!result_is_error(&result));
    assert!(!result_text(&result).contains("Why these memories?"));
    let structured = result_structured(&result).unwrap();
    assert!(structured.get("recall_explanation").is_none());
}

#[test]
fn memory_read_records_access_side_effect() {
    let (disp, _dir) = test_dispatcher();
    let id = add_fragment(
        &disp,
        1,
        "## Access Test Fragment\n\n### Context\nTesting access tracking.",
    );
    let eid = disp.repo().resolve_id(&id).unwrap();
    // Lower confidence so the +0.015 boost is observable (default is 1.0).
    let upd = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
        id: id.clone(),
        confidence: Some(0.5),
        ..Default::default()
    });
    let env = tool_call(2, upd.clone());
    run(&disp, &env, &upd);

    // Read it with a context tag.
    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        id: Some(id.clone()),
        context: Some("refactoring".to_string()),
        ..Default::default()
    });
    let env = tool_call(3, read.clone());
    let _ = run(&disp, &env, &read);

    // The contract-visible read side effects must be persisted:
    // access_count +1, last_accessed_at, confidence +0.015, context tag.
    let mems = disp.repo().get_memories(&[eid]).unwrap();
    assert_eq!(mems[0].access_count, 1);
    assert!(mems[0].last_accessed_at.is_some());
    assert!((mems[0].confidence - 0.515).abs() < 1e-9);
    assert!(mems[0].tags.contains(&"refactoring".to_string()));
}
