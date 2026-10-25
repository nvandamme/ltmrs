//! memory_read tool tests (moved verbatim from `tools.rs`).

use std::sync::Arc;

use super::execute_tool;
use super::test_support::*;
use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, DomainRequest, IpcEnvelope, PROTOCOL_VERSION};
use ltmrs_compat::lemma::privacy;
use ltmrs_compat::lemma::tool_args::{
    MemoryAddArgs, MemoryForgetArgs, MemoryReadArgs, MemoryUpdateArgs, SemanticSearchArgs,
    SessionStartArgs, ToolArgs,
};
use ltmrs_domain::clock::FrozenClock;
use ltmrs_domain::command::Scope;
use ltmrs_domain::id::OperationId;
use ltmrs_service::repository::CanonicalRepository;
use serde_json::json;
use uuid::Uuid;

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
    let resolved = disp
        .repo()
        .resolve_identity(
            &ltmrs_service::projects::IdentityClaim {
                project_id: None,
                canonical_root: None,
                legacy_project: None,
                legacy_explicit: false,
                all: true,
            },
            false,
        )
        .unwrap();

    // retrieve_sync bridges onto the runtime and must run from a
    // synchronous context, exactly like the dispatcher's spawn_blocking.
    let browsed =
        tokio::task::spawn_blocking(move || super::recall::recall_browse(&disp, &args, &resolved))
            .await
            .unwrap()
            .unwrap();
    let out = browsed.page.items;
    let method = browsed.method;
    assert_eq!(
        out.len(),
        1,
        "empty backend must fall back to the snapshot scan"
    );
    assert_eq!(method, "substring_relevance");
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
    let resolved = disp
        .repo()
        .resolve_identity(
            &ltmrs_service::projects::IdentityClaim {
                project_id: None,
                canonical_root: None,
                legacy_project: None,
                legacy_explicit: false,
                all: true,
            },
            false,
        )
        .unwrap();

    let browsed =
        tokio::task::spawn_blocking(move || super::recall::recall_browse(&disp, &args, &resolved))
            .await
            .unwrap()
            .unwrap();
    let out = browsed.page.items;
    let method = browsed.method;
    assert_eq!(out.len(), 1, "empty table falls back to the snapshot");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "dense-capable backend must run the dense leg"
    );
    assert_eq!(method, "substring_relevance");
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
    assert_eq!(
        items[0]["selection"]["method"],
        json!("substring_relevance")
    );
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

#[test]
fn adopt_makes_legacy_rows_visible_under_stable_id() {
    use ltmrs_domain::project_id::generate_project_id;
    let (disp, _dir) = test_dispatcher();
    add_with(
        &disp,
        1,
        "legacy other fragment",
        Some("other"),
        ident(None, None, Some("other"), true),
    );
    let (full_a, id_a) = generate_project_id("shop");
    add_with(
        &disp,
        2,
        "stable shop fragment",
        Some("shop"),
        ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
    );
    assert_eq!(
        read_count(
            &disp,
            3,
            ident(Some(&full_a), Some("/tmp/a/shop"), None, false)
        ),
        1,
        "before adopt the cross-name row stays invisible"
    );
    let outcome = disp
        .repo()
        .adopt_legacy_slug(id_a, "shop", Some("/tmp/a/shop"), "other")
        .unwrap();
    assert_eq!(outcome.attached, 1);
    assert_eq!(
        read_count(
            &disp,
            4,
            ident(Some(&full_a), Some("/tmp/a/shop"), None, false)
        ),
        2,
        "after adopt the attached row is visible under the id"
    );
}

#[test]
fn adopt_request_executes_on_daemon_store() {
    use ltmrs_domain::project_id::generate_project_id;
    let (disp, _dir) = test_dispatcher();
    add_with(
        &disp,
        1,
        "legacy other fragment",
        Some("other"),
        ident(None, None, Some("other"), true),
    );
    let (full_a, _id_a) = generate_project_id("shop");
    add_with(
        &disp,
        2,
        "stable shop fragment",
        Some("shop"),
        ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
    );
    let env = IpcEnvelope {
        protocol_version: PROTOCOL_VERSION,
        store_generation: disp.store_generation().unwrap(),
        frontend_id: fe(1),
        channel_id: ch(1),
        operation_id: OperationId::new(Uuid::from_u128(3)),
        session: None,
        retry_epoch: 1,
        deadline_millis: None,
        scope: Scope::default(),
        identity: ident(Some(&full_a), Some("/tmp/a/shop"), None, false),
        body: DomainRequest::AdoptLegacySlug {
            project_id: full_a.clone(),
            canonical_root: Some("/tmp/a/shop".to_string()),
            slug: "other".to_string(),
        },
    };
    let resp = disp.handle(&env).unwrap();
    match resp.result {
        crate::envelope::IpcResult::Success {
            payload:
                DomainPayload::Adopted {
                    attached,
                    already_attached,
                    ..
                },
            ..
        } => {
            assert_eq!(attached, 1);
            assert!(!already_attached);
        }
        other => panic!("adopt request failed: {other:?}"),
    }
    assert_eq!(
        read_count(
            &disp,
            4,
            ident(Some(&full_a), Some("/tmp/a/shop"), None, false)
        ),
        2,
        "adopted row is visible under the stable id"
    );
}

#[test]
fn adopt_same_slug_twice_is_idempotent() {
    use ltmrs_domain::project_id::generate_project_id;
    let (disp, _dir) = test_dispatcher();
    add_with(
        &disp,
        1,
        "legacy other fragment",
        Some("other"),
        ident(None, None, Some("other"), true),
    );
    let (full_a, id_a) = generate_project_id("shop");
    add_with(
        &disp,
        2,
        "stable shop fragment",
        Some("shop"),
        ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
    );
    let first = disp
        .repo()
        .adopt_legacy_slug(id_a, "shop", Some("/tmp/a/shop"), "other")
        .unwrap();
    assert_eq!(first.attached, 1);
    assert!(!first.already_attached);
    let second = disp
        .repo()
        .adopt_legacy_slug(id_a, "shop", Some("/tmp/a/shop"), "other")
        .unwrap();
    assert!(second.already_attached);
    assert_eq!(second.attached, 0);
}

#[test]
fn adopt_shared_slug_warns_and_shares_visibility() {
    use ltmrs_domain::project_id::generate_project_id;
    let (disp, _dir) = test_dispatcher();
    add_with(
        &disp,
        1,
        "legacy shared fragment",
        Some("shared"),
        ident(None, None, Some("shared"), true),
    );
    let (full_a, id_a) = generate_project_id("shop");
    add_with(
        &disp,
        2,
        "stable shop fragment",
        Some("shop"),
        ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
    );
    let (full_b, id_b) = generate_project_id("other");
    add_with(
        &disp,
        3,
        "stable other fragment",
        Some("other"),
        ident(Some(&full_b), Some("/tmp/b/other"), Some("other"), true),
    );
    let a = disp
        .repo()
        .adopt_legacy_slug(id_a, "shop", Some("/tmp/a/shop"), "shared")
        .unwrap();
    assert!(a.warning.is_none());
    let b = disp
        .repo()
        .adopt_legacy_slug(id_b, "other", Some("/tmp/b/other"), "shared")
        .unwrap();
    assert!(
        b.warning.as_deref().is_some_and(|w| w.contains(&full_a)),
        "must name the other id, got: {:?}",
        b.warning
    );
    assert_eq!(
        read_count(
            &disp,
            4,
            ident(Some(&full_a), Some("/tmp/a/shop"), None, false)
        ),
        2,
        "A sees own + shared"
    );
    assert_eq!(
        read_count(
            &disp,
            5,
            ident(Some(&full_b), Some("/tmp/b/other"), None, false)
        ),
        2,
        "B sees own + shared"
    );
}

#[test]
fn adopt_unknown_slug_errors_with_known_list() {
    use ltmrs_domain::project_id::generate_project_id;
    let (disp, _dir) = test_dispatcher();
    add_with(
        &disp,
        1,
        "legacy other fragment",
        Some("other"),
        ident(None, None, Some("other"), true),
    );
    let (full_a, id_a) = generate_project_id("shop");
    add_with(
        &disp,
        2,
        "stable shop fragment",
        Some("shop"),
        ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
    );
    let err = disp
        .repo()
        .adopt_legacy_slug(id_a, "shop", Some("/tmp/a/shop"), "nope")
        .unwrap_err();
    assert_eq!(err.code, ltmrs_domain::command::DomainErrorCode::Validation);
    assert!(
        err.message.contains("nope"),
        "must name the slug, got: {}",
        err.message
    );
    assert!(
        err.message.contains("other"),
        "must list known slugs, got: {}",
        err.message
    );
}

#[test]
fn expand_graph_depth_penalty_orders_by_confidence_times_decay() {
    let (disp, _dir) = test_dispatcher();
    let seed = add_fragment(
        &disp,
        1,
        "## Sundial Courtyard Plaque\n\n### Context\nBronze gnomon inscription marking solstice shadows.",
    );
    let near = add_fragment(
        &disp,
        2,
        "## Kiln Firing Schedule\n\n### Context\nEarthenware bisque temperatures and glaze cooling curves.",
    );
    set_confidence(&disp, 3, &near, 0.4);
    let far = add_fragment(
        &disp,
        4,
        "## Tidal Mill Ledger\n\n### Context\nEstuary sluice tallies and millstone dressing intervals.",
    );
    set_confidence(&disp, 5, &far, 0.95);
    relate(&disp, 6, &seed, &near, "related_to");
    relate(&disp, 7, &near, &far, "related_to");

    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        id: Some(seed.clone()),
        expand_graph: true,
        ..Default::default()
    });
    let result = run(&disp, &tool_call(8, read.clone()), &read);
    assert!(!result_is_error(&result));
    assert_eq!(related_ids(&result), vec![far.clone(), near.clone()]);
    let structured = result_structured(&result).unwrap();
    let graph = structured["related_graph"].as_array().unwrap();
    let far_score = graph[0]["score"].as_f64().unwrap();
    let near_score = graph[1]["score"].as_f64().unwrap();
    assert!(
        (far_score - 0.95 * 0.6f64.powi(2)).abs() < 1e-9,
        "far score must be 0.95 x 0.6^2, got {far_score}"
    );
    assert!(
        (near_score - 0.4 * 0.6).abs() < 1e-9,
        "near score must be 0.4 x 0.6, got {near_score}"
    );
    assert_eq!(structured["graph_truncated"], json!(false));
}

#[test]
fn expand_graph_excludes_cross_project_node_under_stable_id() {
    use ltmrs_domain::project_id::generate_project_id;
    let (disp, _dir) = test_dispatcher();
    let (full_a, _id_a) = generate_project_id("shop");
    let (full_b, _id_b) = generate_project_id("shop");
    let seed_out = add_with(
        &disp,
        1,
        "## Shop seed\n\n### Context\nSeed under identity A.",
        Some("shop"),
        ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
    );
    assert!(!result_is_error(&seed_out));
    let seed_id = result_structured(&seed_out).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let foreign_out = add_with(
        &disp,
        2,
        "## Shop foreign\n\n### Context\nForeign node under identity B.",
        Some("shop"),
        ident(Some(&full_b), Some("/tmp/b/shop"), Some("shop"), true),
    );
    assert!(!result_is_error(&foreign_out));
    let foreign_id = result_structured(&foreign_out).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    // Cross-boundary link via the permissive unattributed path.
    relate(&disp, 3, &seed_id, &foreign_id, "related_to");

    // Under identity A the foreign node must not surface (all=false).
    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        id: Some(seed_id.clone()),
        expand_graph: true,
        ..Default::default()
    });
    let scoped = run(
        &disp,
        &tool_call_with(
            4,
            read.clone(),
            ident(Some(&full_a), Some("/tmp/a/shop"), None, false),
        ),
        &read,
    );
    assert!(!result_is_error(&scoped));
    assert!(
        !related_ids(&scoped).contains(&foreign_id),
        "cross-project node must be excluded under the stable id"
    );
    assert!(
        !result_text(&scoped).contains(&foreign_id),
        "no foreign id may render"
    );
    // ... nor with all=true: expansion is a derived read and must not
    // bootstrap across the boundary.
    let read_all = ToolArgs::MemoryRead(MemoryReadArgs {
        id: Some(seed_id.clone()),
        expand_graph: true,
        all: true,
        ..Default::default()
    });
    let scoped_all = run(
        &disp,
        &tool_call_with(
            5,
            read_all.clone(),
            ident(Some(&full_a), Some("/tmp/a/shop"), None, false),
        ),
        &read_all,
    );
    assert!(!result_is_error(&scoped_all));
    assert!(
        !related_ids(&scoped_all).contains(&foreign_id),
        "all=true must not leak cross-project nodes into expansion"
    );
    // Control: unattributed all=true traverses the link (it exists).
    let control = run(&disp, &tool_call(6, read_all.clone()), &read_all);
    assert!(!result_is_error(&control));
    assert!(
        related_ids(&control).contains(&foreign_id),
        "unattributed all=true must traverse the existing link"
    );
}

#[test]
fn expand_graph_excludes_non_recallable_node() {
    let (disp, _dir) = test_dispatcher();
    let seed = add_fragment(
        &disp,
        1,
        "## Granary Inventory Scroll\n\n### Context\nWheat bushel counts and silo moisture readings.",
    );
    let retired = add_fragment(
        &disp,
        2,
        "## Cooper Barrel Order\n\n### Context\nOak stave shipments for cider cask assembly.",
    );
    relate(&disp, 3, &seed, &retired, "related_to");
    let forget = ToolArgs::MemoryForget(MemoryForgetArgs {
        id: retired.clone(),
        invalidate: true,
        ..Default::default()
    });
    let forgot = run(&disp, &tool_call(4, forget.clone()), &forget);
    assert!(!result_is_error(&forgot));

    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        id: Some(seed.clone()),
        expand_graph: true,
        ..Default::default()
    });
    let result = run(&disp, &tool_call(5, read.clone()), &read);
    assert!(!result_is_error(&result));
    assert!(
        !related_ids(&result).contains(&retired),
        "invalidated node must be excluded from expansion"
    );
    assert_eq!(
        result_structured(&result).unwrap()["graph_truncated"],
        json!(false)
    );
    // Control: the row itself is preserved and directly readable.
    let direct = run(
        &disp,
        &tool_call(
            6,
            ToolArgs::MemoryRead(MemoryReadArgs {
                id: Some(retired.clone()),
                ..Default::default()
            }),
        ),
        &ToolArgs::MemoryRead(MemoryReadArgs {
            id: Some(retired.clone()),
            ..Default::default()
        }),
    );
    assert!(
        !result_is_error(&direct),
        "invalidated row must stay directly readable"
    );
}

#[test]
fn expand_graph_hub_is_bounded_truncated_and_deterministic() {
    let (disp, _dir) = test_dispatcher();
    let hub = add_fragment(
        &disp,
        1,
        "## Cartography Atlas Index\n\n### Context\nSurvey charts of archipelago harbors and meridian baselines.",
    );
    let topics = [
        "## Basalt Quarry Ledger\n\n### Context\nExcavation tallies of obsidian strata and pumice veins.",
        "## Maritime Lantern Log\n\n### Context\nHarbor beacon rotations and tide signal schedules.",
        "## Observatory Star Charts\n\n### Context\nNocturnal telescope sightings of comet perihelion passages.",
        "## Bakery Oven Roster\n\n### Context\nSourdough fermentation timetables and rye flour quotas.",
        "## Clockwork Gear Census\n\n### Context\nBrass escapement inventories and pendulum calibration drafts.",
        "## Vineyard Harvest Almanac\n\n### Context\nAutumn grape pressing yields and barrel cooperage orders.",
        "## Lighthouse Beacon Registry\n\n### Context\nCoastal foghorn maintenance windows and keeper shift rosters.",
        "## Falconry Meadow Notes\n\n### Context\nRaptor molting cycles and lure training meadow rotations.",
    ];
    let mut leaves = Vec::new();
    for (i, fragment) in topics.iter().enumerate() {
        let id = add_fragment(&disp, 2 + i as u64, fragment);
        // Ascending confidence: leaf i holds 0.1 * (i + 1).
        set_confidence(&disp, 10 + i as u64, &id, 0.1 * (i as f64 + 1.0));
        leaves.push(id);
    }
    let child = add_fragment(
        &disp,
        20,
        "## Apiary Honey Census\n\n### Context\nHive brood inspections and wax foundation replacements.",
    );
    set_confidence(&disp, 21, &child, 0.9);
    let grandchild = add_fragment(
        &disp,
        22,
        "## Glacier Crevasse Survey\n\n### Context\nIcefall serac mapping and meltwater channel soundings.",
    );
    // Hub fanout: link all eight leaves; chain hangs off the top leaf.
    for (i, leaf) in leaves.iter().enumerate() {
        relate(&disp, 30 + i as u64, &hub, leaf, "related_to");
    }
    relate(&disp, 40, &leaves[7], &child, "related_to");
    relate(&disp, 41, &child, &grandchild, "related_to");

    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        id: Some(hub.clone()),
        expand_graph: true,
        ..Default::default()
    });
    let first = run(&disp, &tool_call(50, read.clone()), &read);
    assert!(!result_is_error(&first));
    let ids = related_ids(&first);
    // Depth-1 top five by confidence (0.8..0.4) plus the depth-2 child
    // (0.9 * 0.36 = 0.324 slots between 0.36 and 0.30).
    let expected = vec![
        leaves[7].clone(),
        leaves[6].clone(),
        leaves[5].clone(),
        child.clone(),
        leaves[4].clone(),
        leaves[3].clone(),
    ];
    assert_eq!(ids, expected, "graph must be confidence-ordered");
    for excluded in [&leaves[0], &leaves[1], &leaves[2], &grandchild] {
        assert!(
            !ids.contains(excluded),
            "fanout-pruned or too-deep node must be absent: {excluded}"
        );
    }
    let structured = result_structured(&first).unwrap();
    assert_eq!(
        structured["graph_truncated"],
        json!(true),
        "fanout cap must be indicated"
    );
    assert!(
        result_text(&first).contains("truncated"),
        "text must note the truncation"
    );
    // Deterministic: a second read yields the identical order.
    let second = run(&disp, &tool_call(51, read.clone()), &read);
    assert_eq!(related_ids(&second), ids);
}

#[test]
fn legacy_attached_visible_under_stable_id() {
    use ltmrs_domain::project_id::generate_project_id;
    let (disp, _dir) = test_dispatcher();

    // Legacy row, no identity anywhere (unattributed envelope).
    let out = add_with(
        &disp,
        1,
        "legacy shop fragment",
        Some("shop"),
        ident(None, None, Some("shop"), true),
    );
    assert!(!result_is_error(&out));

    // First stable write attaches the bucket and reports the count.
    let (full_a, _id_a) = generate_project_id("shop");
    let out = add_with(
        &disp,
        2,
        "stable shop fragment",
        Some("shop"),
        ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
    );
    let text = result_text(&out);
    assert!(
        text.contains("attached 1 legacy"),
        "must report the adopted count, got: {text}"
    );

    // The stable scope sees both rows; a foreign stable id sees neither
    // shop row (legacy excluded, own missing).
    assert_eq!(
        read_count(
            &disp,
            3,
            ident(Some(&full_a), Some("/tmp/a/shop"), None, false)
        ),
        2
    );
    let (full_z, _id_z) = generate_project_id("other");
    // Register Z elsewhere first so the read resolves (unknown ids
    // refuse reads).
    add_with(
        &disp,
        4,
        "other fragment",
        Some("other"),
        ident(Some(&full_z), Some("/tmp/z/other"), Some("other"), true),
    );
    assert_eq!(
        read_count(
            &disp,
            5,
            ident(Some(&full_z), Some("/tmp/z/other"), None, false)
        ),
        1,
        "Z sees only its own write"
    );
}

#[test]
fn stable_identity_isolates_same_basename_checkouts() {
    use ltmrs_domain::project_id::generate_project_id;
    let (disp, _dir) = test_dispatcher();
    let (full_a, id_a) = generate_project_id("shop");
    let (full_b, _id_b) = generate_project_id("shop");

    // A binds (registers) and writes under its identity.
    let out = add_with(
        &disp,
        1,
        "A-only fragment",
        Some("shop"),
        ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
    );
    assert!(
        !result_is_error(&out),
        "A write failed: {}",
        result_text(&out)
    );
    assert!(
        !result_text(&out).contains("attached"),
        "no legacy rows to adopt, got: {}",
        result_text(&out)
    );

    // B's scope sees nothing of A: B registers on its own first write
    // (reads never register — unknown identities refuse), then each
    // scope sees only its own write.
    let out = add_with(
        &disp,
        3,
        "B-only fragment",
        Some("shop"),
        ident(Some(&full_b), Some("/tmp/b/shop"), Some("shop"), true),
    );
    assert!(!result_is_error(&out));
    assert_eq!(
        read_count(
            &disp,
            4,
            ident(Some(&full_b), Some("/tmp/b/shop"), None, false)
        ),
        1,
        "B sees only its own write"
    );
    assert_eq!(
        read_count(
            &disp,
            5,
            ident(Some(&full_a), Some("/tmp/a/shop"), None, false)
        ),
        1,
        "A sees only its own write"
    );

    // B claiming A's id from B's root is a conflicting claim: refused.
    let tool = ToolArgs::MemoryAdd(MemoryAddArgs {
        fragment: "hijack".into(),
        ..Default::default()
    });
    let env = tool_call_with(
        6,
        tool.clone(),
        ident(Some(&full_a), Some("/tmp/b/shop"), None, false),
    );
    let err = execute_tool(&disp, &env, &tool).unwrap_err();
    assert!(
        err.message.contains("project bind"),
        "conflicting claim must name remediation, got: {}",
        err.message
    );

    // Sessions carry the resolved stable identity.
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "t".into(),
        ..Default::default()
    });
    let env = tool_call_with(
        7,
        start.clone(),
        ident(Some(&full_a), Some("/tmp/a/shop"), None, false),
    );
    execute_tool(&disp, &env, &start).unwrap();
    let sessions = disp.repo().all_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].project_id, Some(id_a));
    assert_eq!(sessions[0].project.as_deref(), Some("shop"));
}

#[test]
fn stable_recall_sees_own_attached_and_global() {
    use ltmrs_domain::project_id::generate_project_id;
    let (disp, _dir) = test_dispatcher();
    let (full_x, _) = generate_project_id("shop");
    add_with(
        &disp,
        1,
        "stable shop fragment",
        Some("shop"),
        ident(Some(&full_x), Some("/tmp/a/shop"), Some("shop"), true),
    );
    add_with(
        &disp,
        2,
        "legacy shop fragment",
        Some("shop"),
        ident(None, None, Some("shop"), true),
    );
    add_with(
        &disp,
        3,
        "global fragment",
        None,
        ident(None, None, None, false),
    );
    assert_eq!(
        read_count(
            &disp,
            4,
            ident(Some(&full_x), Some("/tmp/a/shop"), None, false)
        ),
        3,
        "stable recall must see own + attached + global"
    );
}

#[test]
fn provisional_write_from_shadowed_checkout_refused() {
    use ltmrs_domain::project_id::generate_project_id;
    let (disp, _dir) = test_dispatcher();
    let (full_a, _id_a) = generate_project_id("shop");
    add_with(
        &disp,
        1,
        "A fragment",
        Some("shop"),
        ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
    );
    let tool = ToolArgs::MemoryAdd(MemoryAddArgs {
        fragment: "provisional".into(),
        ..Default::default()
    });
    let env = tool_call_with(
        2,
        tool.clone(),
        ident(None, Some("/tmp/c/shop"), None, false),
    );
    let err = execute_tool(&disp, &env, &tool).unwrap_err();
    assert_eq!(err.code, ltmrs_domain::command::DomainErrorCode::Validation);
    assert!(err.message.contains("project bind"), "got: {}", err.message);
}

#[test]
fn memory_read_b5d_browse_serves_confidence_scores() {
    let (disp, _dir) = test_dispatcher();
    let id_a = add_fragment(
        &disp,
        1,
        "## Sundial Courtyard Plaque\n\n### Context\nBronze gnomon inscription.",
    );
    let id_b = add_fragment(
        &disp,
        2,
        "## Kiln Firing Schedule\n\n### Context\nEarthenware bisque temperatures.",
    );
    set_confidence(&disp, 3, &id_a, 0.9);
    set_confidence(&disp, 4, &id_b, 0.3);
    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        explain: true,
        ..Default::default()
    });
    let result = run(&disp, &tool_call(5, read.clone()), &read);
    assert!(!result_is_error(&result));
    let items = result_structured(&result).unwrap()["recall_explanation"]["items"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], json!(id_a));
    assert_eq!(items[0]["selection"]["method"], json!("confidence_browse"));
    assert_eq!(items[0]["selection"]["score"], json!(0.9));
    assert_eq!(
        items[0]["selection"]["score_kind"],
        json!("stored_confidence")
    );
    assert_eq!(items[1]["id"], json!(id_b));
    assert_eq!(items[1]["selection"]["score"], json!(0.3));
}

#[tokio::test]
async fn memory_read_b5d_engine_serves_fused_scores() {
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
        "## Engine Recall Ledger\n\n### Context\nFused engine relevance scores.",
    );

    let lance_dir = tempfile::tempdir().unwrap();
    let table = SearchTable::open(lance_dir.path().to_str().unwrap())
        .await
        .unwrap();
    publish_search_row(
        &repo,
        &table,
        "engine recall ledger fused relevance",
        ltmrs_embeddings::e5_small::E5_SMALL_FINGERPRINT,
    )
    .await;
    table.create_fts_index().await.unwrap();
    let embedder = Arc::new(ClosureEmbedder::new(|_| Ok(vec![0.0; 384])));
    let backend = Arc::new(SearchBackend::new(
        Arc::clone(&repo),
        table,
        Arc::new(ltmrs_search::search::backend::embedders::QueryEmbedderAdapter::new(embedder)),
    ));
    let disp =
        Dispatcher::new(repo, crate::registry::FrontendRegistry::new(), clock).with_search(backend);
    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        query: Some("engine recall".to_string()),
        explain: true,
        ..Default::default()
    });
    let search = ToolArgs::SemanticSearch(SemanticSearchArgs {
        query: "engine recall".to_string(),
        project: None,
        top_k: None,
        offset: None,
        hybrid: None,
        explain: true,
        response_format: None,
    });

    let (read_result, search_result) = tokio::task::spawn_blocking(move || {
        let read_out = run(&disp, &tool_call(2, read.clone()), &read);
        let search_out = run(&disp, &tool_call(3, search.clone()), &search);
        (read_out, search_out)
    })
    .await
    .unwrap();
    assert!(!result_is_error(&read_result));
    assert!(!result_is_error(&search_result));
    let read_items = result_structured(&read_result).unwrap()["recall_explanation"]["items"]
        .as_array()
        .unwrap()
        .clone();
    let search_items = result_structured(&search_result).unwrap()["recall_explanation"]["items"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(read_items.len(), 1);
    assert_eq!(search_items.len(), 1);
    assert_eq!(read_items[0]["id"], search_items[0]["id"]);
    assert_eq!(read_items[0]["selection"]["method"], json!("engine_rrf"));
    assert_eq!(read_items[0]["selection"]["rank"], json!(1));
    assert_eq!(
        read_items[0]["selection"]["score_kind"],
        json!("engine_relevance_higher_is_better")
    );
    let read_score = read_items[0]["selection"]["score"].as_f64().unwrap();
    let search_score = search_items[0]["selection"]["score"].as_f64().unwrap();
    assert!(
        (read_score - search_score).abs() < 1e-12,
        "memory_read engine score {read_score} must equal the semantic score {search_score}"
    );
}

#[test]
fn memory_read_b5d_explicit_id_stays_scoreless() {
    let (disp, _dir) = test_dispatcher();
    let id = add_fragment(
        &disp,
        1,
        "## Explain Fragment\n\n### Context\nFor scoreless testing.",
    );
    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        id: Some(id.clone()),
        explain: true,
        ..Default::default()
    });
    let result = run(&disp, &tool_call(2, read.clone()), &read);
    assert!(!result_is_error(&result));
    let items = result_structured(&result).unwrap()["recall_explanation"]["items"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["selection"]["method"], json!("explicit_id"));
    assert!(items[0]["selection"]["score"].is_null());
    assert!(items[0]["selection"]["score_kind"].is_null());
}

#[test]
fn memory_read_b5d_fallback_serves_hit_count_scores() {
    let (disp, _dir) = test_dispatcher();
    let id_a = add_fragment(
        &disp,
        1,
        "## Alpha Beta Gamma\n\n### Context\nAlpha beta gamma ledger.",
    );
    let id_b = add_fragment(
        &disp,
        2,
        "## Harbor Lantern Manifest\n\n### Context\nMaritime shipping log.",
    );
    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        query: Some("alpha beta gamma".to_string()),
        explain: true,
        ..Default::default()
    });
    let result = run(&disp, &tool_call(3, read.clone()), &read);
    assert!(!result_is_error(&result));
    let items = result_structured(&result).unwrap()["recall_explanation"]["items"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], json!(id_a));
    assert_eq!(
        items[0]["selection"]["method"],
        json!("substring_relevance")
    );
    assert_eq!(items[0]["selection"]["score"], json!(3.0));
    assert_eq!(
        items[0]["selection"]["score_kind"],
        json!("hit_count_higher_is_better")
    );
    assert_eq!(items[1]["id"], json!(id_b));
    assert_eq!(
        items[1]["selection"]["method"],
        json!("substring_relevance")
    );
    assert_eq!(items[1]["selection"]["score"], json!(0.0));
    assert_eq!(
        items[1]["selection"]["score_kind"],
        json!("hit_count_higher_is_better")
    );
}

#[test]
fn memory_read_b5d_graph_selection_carries_depth_penalty_score() {
    let (disp, _dir) = test_dispatcher();
    let seed = add_fragment(
        &disp,
        1,
        "## Sundial Courtyard Plaque\n\n### Context\nBronze gnomon inscription marking solstice shadows.",
    );
    let near = add_fragment(
        &disp,
        2,
        "## Kiln Firing Schedule\n\n### Context\nEarthenware bisque temperatures and glaze cooling curves.",
    );
    set_confidence(&disp, 3, &near, 0.4);
    relate(&disp, 4, &seed, &near, "related_to");
    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        id: Some(seed.clone()),
        expand_graph: true,
        explain: true,
        ..Default::default()
    });
    let result = run(&disp, &tool_call(5, read.clone()), &read);
    assert!(!result_is_error(&result));
    let structured = result_structured(&result).unwrap();
    let graph = structured["related_graph"].as_array().unwrap().clone();
    assert_eq!(graph.len(), 1);
    let items = structured["recall_explanation"]["items"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(items.len(), 2);
    assert_eq!(items[1]["id"], json!(near));
    assert_eq!(items[1]["selection"]["method"], json!("graph_expansion"));
    assert_eq!(
        items[1]["selection"]["score"],
        json!(graph[0]["score"].clone())
    );
    assert_eq!(
        items[1]["selection"]["score_kind"],
        json!("graph_score_with_depth_penalty")
    );
}

#[test]
fn memory_read_batch_coread_links() {
    let (disp, _dir) = test_dispatcher();
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        ..Default::default()
    });
    run(&disp, &tool_call(1, start.clone()), &start);
    let id_a = add_fragment(
        &disp,
        2,
        "## Quartz Basalt Columns\n\n### Context\nGeology quarry ledger.",
    );
    let id_b = add_fragment(
        &disp,
        3,
        "## Harbor Lantern Manifest\n\n### Context\nMaritime shipping log.",
    );
    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        ids: Some(vec![id_a.clone(), id_b.clone()]),
        ..Default::default()
    });
    let result = run(&disp, &tool_call(4, read.clone()), &read);
    assert!(!result_is_error(&result));
    let eid_a = disp.repo().resolve_id(&id_a).unwrap();
    let eid_b = disp.repo().resolve_id(&id_b).unwrap();
    let mems = disp.repo().get_memories(&[eid_a, eid_b]).unwrap();
    let (ma, mb) = (&mems[0], &mems[1]);
    let (ma, mb) = if ma.id == eid_a { (ma, mb) } else { (mb, ma) };
    assert!(
        ma.associated_with.contains(&id_b),
        "A must list B, got {:?}",
        ma.associated_with
    );
    assert!(
        mb.associated_with.contains(&id_a),
        "B must list A, got {:?}",
        mb.associated_with
    );
    let rels = disp.repo().all_relations().unwrap();
    let coread: Vec<_> = rels
        .iter()
        .filter(|r| {
            r.relation_type == ltmrs_domain::relation::RelationType::RelatedTo
                && ((r.source == eid_a && r.target == eid_b)
                    || (r.source == eid_b && r.target == eid_a))
        })
        .collect();
    assert!(
        !coread.is_empty() && coread.len() <= 3,
        "expected 1..=3 related_to, got {}",
        coread.len()
    );
}

#[test]
fn memory_read_batch_detail_is_post_boost() {
    let (disp, _dir) = test_dispatcher();
    let id_a = add_fragment(&disp, 1, "## Post Boost A\n\n### Context\nBoost A.");
    let id_b = add_fragment(&disp, 2, "## Post Boost B\n\n### Context\nBoost B.");
    for (op, id) in [(3u64, &id_a), (4, &id_b)] {
        let upd = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
            id: id.clone(),
            confidence: Some(0.5),
            ..Default::default()
        });
        run(&disp, &tool_call(op, upd.clone()), &upd);
    }
    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        ids: Some(vec![id_a.clone(), id_b.clone()]),
        ..Default::default()
    });
    let result = run(&disp, &tool_call(5, read.clone()), &read);
    assert!(!result_is_error(&result));
    let structured = result_structured(&result).unwrap();
    for frag in structured["fragments"].as_array().unwrap() {
        let conf = frag["confidence"].as_f64().unwrap();
        assert!(
            (conf - 0.515).abs() < 1e-9,
            "batch detail must be post-boost 0.515, got {conf}"
        );
    }
}

#[test]
fn memory_read_batch_tracks_session() {
    let (disp, _dir) = test_dispatcher();
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        ..Default::default()
    });
    run(&disp, &tool_call(1, start.clone()), &start);
    let id_a = add_fragment(&disp, 2, "## Batch A\n\n### Context\nFirst.");
    let id_b = add_fragment(&disp, 3, "## Batch B\n\n### Context\nSecond.");
    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        ids: Some(vec![id_a.clone(), id_b.clone()]),
        ..Default::default()
    });
    let result = run(&disp, &tool_call(4, read.clone()), &read);
    assert!(!result_is_error(&result));
    let handle = disp.registry().channel_session(fe(1), ch(1)).unwrap();
    let session = disp.repo().get_session(handle).unwrap().unwrap();
    assert!(session.memories_read.contains(&id_a), "batch must link A");
    assert!(session.memories_read.contains(&id_b), "batch must link B");
}

#[test]
fn memory_read_coread_cross_project_not_linked() {
    use ltmrs_domain::project_id::generate_project_id;
    let (disp, _dir) = test_dispatcher();
    let (full_a, _id_a) = generate_project_id("shop");
    let (full_b, _id_b) = generate_project_id("shop");
    let out_a = add_with(
        &disp,
        1,
        "## Quartz Basalt Columns\n\n### Context\nGeology quarry ledger.",
        Some("shop"),
        ident(Some(&full_a), Some("/tmp/a/shop"), Some("shop"), true),
    );
    assert!(!result_is_error(&out_a));
    let out_b = add_with(
        &disp,
        3,
        "## Harbor Lantern Manifest\n\n### Context\nMaritime shipping log.",
        Some("shop"),
        ident(Some(&full_b), Some("/tmp/b/shop"), Some("shop"), true),
    );
    assert!(!result_is_error(&out_b));
    let id_a = result_structured(&out_a).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let id_b = result_structured(&out_b).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "t".into(),
        ..Default::default()
    });
    let env = tool_call_with(
        5,
        start.clone(),
        ident(Some(&full_a), Some("/tmp/a/shop"), None, false),
    );
    execute_tool(&disp, &env, &start).unwrap();
    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        ids: Some(vec![id_a.clone(), id_b.clone()]),
        all: true,
        ..Default::default()
    });
    let env = tool_call_with(
        6,
        read.clone(),
        ident(Some(&full_a), Some("/tmp/a/shop"), None, false),
    );
    let result = execute_tool(&disp, &env, &read).unwrap();
    assert!(!result_is_error(&result));
    let eid_a = disp.repo().resolve_id(&id_a).unwrap();
    let eid_b = disp.repo().resolve_id(&id_b).unwrap();
    let mems = disp.repo().get_memories(&[eid_a, eid_b]).unwrap();
    for m in &mems {
        assert!(
            m.associated_with.is_empty(),
            "cross-project must not link, got {:?}",
            m.associated_with
        );
    }
    let rels = disp.repo().all_relations().unwrap();
    assert!(
        rels.iter()
            .filter(|r| {
                r.relation_type == ltmrs_domain::relation::RelationType::RelatedTo
                    && ((r.source == eid_a && r.target == eid_b)
                        || (r.source == eid_b && r.target == eid_a))
            })
            .count()
            == 0,
        "cross-project must create no relation"
    );
}

#[tokio::test]
async fn memory_read_filtered_engine_total_counts_only_passing_set() {
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
    seed_pagetoken_memories(&seed, 1, 3);

    let lance_dir = tempfile::tempdir().unwrap();
    let table = SearchTable::open(lance_dir.path().to_str().unwrap())
        .await
        .unwrap();
    publish_pagetoken_rows(&repo, &table, "pagetoken").await;
    let embedder = Arc::new(ClosureEmbedder::new(|_| Ok(vec![0.0; 384])));
    let backend = Arc::new(SearchBackend::new(
        Arc::clone(&repo),
        table,
        Arc::new(ltmrs_search::search::backend::embedders::QueryEmbedderAdapter::new(embedder)),
    ));
    let disp = Arc::new(
        Dispatcher::new(repo, crate::registry::FrontendRegistry::new(), clock).with_search(backend),
    );

    // FrozenClock sits at 1000ms; after_date 1970-01-02 excludes every
    // memory while the query still matches all three lexically.
    let args = MemoryReadArgs {
        query: Some("pagetoken".to_string()),
        all: true,
        limit: Some(2),
        after_date: Some("1970-01-02".to_string()),
        ..Default::default()
    };
    let env = tool_call(10, ToolArgs::MemoryRead(args.clone()));
    let tool = ToolArgs::MemoryRead(args);
    let d = Arc::clone(&disp);
    let result = tokio::task::spawn_blocking(move || run(&d, &env, &tool))
        .await
        .unwrap();
    assert!(!result_is_error(&result));
    let structured = result_structured(&result).unwrap();
    assert_eq!(
        structured["total"].as_u64().unwrap(),
        0,
        "total must count only the date-passing set"
    );
    assert_eq!(structured["count"].as_u64().unwrap(), 0);
    assert!(!structured["has_more"].as_bool().unwrap());
    assert!(structured["next_offset"].is_null());
}

#[tokio::test]
async fn memory_read_pagination_honest_totals_hybrid() {
    use ltmrs_search::search::backend::SearchBackend;
    use ltmrs_search::search::backend::embedders::ClosureEmbedder;
    use ltmrs_search::search::table::SearchTable;
    use std::sync::atomic::{AtomicUsize, Ordering};

    for n in [31usize, 51, 100, 500] {
        let dir = tempfile::tempdir().unwrap();
        let clock: Arc<dyn ltmrs_domain::clock::Clock + Send + Sync> =
            Arc::new(FrozenClock::new(1000));
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
        seed_pagetoken_memories(&seed, 1, n);

        let lance_dir = tempfile::tempdir().unwrap();
        let table = SearchTable::open(lance_dir.path().to_str().unwrap())
            .await
            .unwrap();
        publish_pagetoken_rows(&repo, &table, "pagetoken").await;
        let calls = Arc::new(AtomicUsize::new(0));
        let embedder = Arc::new(ClosureEmbedder::new({
            let calls = Arc::clone(&calls);
            move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec![0.0; 384])
            }
        }));
        let backend = Arc::new(SearchBackend::new(
            Arc::clone(&repo),
            table,
            Arc::new(ltmrs_search::search::backend::embedders::QueryEmbedderAdapter::new(embedder)),
        ));
        let disp = std::sync::Arc::new(
            Dispatcher::new(repo, crate::registry::FrontendRegistry::new(), clock)
                .with_search(backend),
        );

        let offsets = [0u64, 7, n as u64 / 2, n as u64 - 1, n as u64, n as u64 + 50];
        for offset in offsets {
            let args = MemoryReadArgs {
                query: Some("pagetoken".to_string()),
                all: true,
                offset: if offset == 0 {
                    None
                } else {
                    Some(offset as usize)
                },
                ..Default::default()
            };
            let env = tool_call(1000 + offset, ToolArgs::MemoryRead(args.clone()));
            let tool = ToolArgs::MemoryRead(args);
            let d = std::sync::Arc::clone(&disp);
            let result = tokio::task::spawn_blocking(move || run(&d, &env, &tool))
                .await
                .unwrap();
            assert!(!result_is_error(&result), "offset {offset} of {n}");
            let structured = result_structured(&result).unwrap();
            expect_pagination_page(&structured, n as u64, 30, offset);
        }
        assert!(
            calls.load(Ordering::SeqCst) >= 1,
            "dense leg must run on the hybrid path"
        );
    }
}

#[test]
fn memory_read_pagination_honest_totals_lexical() {
    for n in [31usize, 51, 100, 500] {
        let (disp, _dir) = test_dispatcher();
        seed_pagetoken_memories(&disp, 1, n);
        let offsets = [0u64, 7, n as u64 / 2, n as u64 - 1, n as u64, n as u64 + 50];
        for offset in offsets {
            let args = MemoryReadArgs {
                query: Some("pagetoken".to_string()),
                all: true,
                offset: if offset == 0 {
                    None
                } else {
                    Some(offset as usize)
                },
                ..Default::default()
            };
            let env = tool_call(1000 + offset, ToolArgs::MemoryRead(args.clone()));
            let result = run(&disp, &env, &ToolArgs::MemoryRead(args));
            assert!(!result_is_error(&result), "offset {offset} of {n}");
            let structured = result_structured(&result).unwrap();
            expect_pagination_page(&structured, n as u64, 30, offset);
        }
    }
}

#[test]
fn memory_read_query_summary_is_post_boost() {
    let (disp, _dir) = test_dispatcher();
    let id_a = add_fragment(&disp, 1, "## Post Boost QA\n\n### Context\nBoostQA.");
    let id_b = add_fragment(&disp, 2, "## Post Boost QB\n\n### Context\nBoostQB.");
    for (op, id) in [(3u64, &id_a), (4, &id_b)] {
        let upd = ToolArgs::MemoryUpdate(MemoryUpdateArgs {
            id: id.clone(),
            confidence: Some(0.5),
            ..Default::default()
        });
        run(&disp, &tool_call(op, upd.clone()), &upd);
    }
    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        query: Some("BoostQA BoostQB".to_string()),
        ..Default::default()
    });
    let result = run(&disp, &tool_call(5, read.clone()), &read);
    assert!(!result_is_error(&result));
    let structured = result_structured(&result).unwrap();
    assert_eq!(structured["total"], json!(2));
    for frag in structured["fragments"].as_array().unwrap() {
        let conf = frag["confidence"].as_f64().unwrap();
        assert!(
            (conf - 0.515).abs() < 1e-9,
            "query summary must be post-boost 0.515, got {conf}"
        );
    }
}

#[test]
fn memory_read_query_tracks_session() {
    let (disp, _dir) = test_dispatcher();
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        ..Default::default()
    });
    run(&disp, &tool_call(1, start.clone()), &start);
    let id = add_fragment(
        &disp,
        2,
        "## Query Track Zebra\n\n### Context\nUnique zebra query tracking.",
    );
    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        query: Some("zebra".to_string()),
        ..Default::default()
    });
    let result = run(&disp, &tool_call(3, read.clone()), &read);
    assert!(!result_is_error(&result));
    let handle = disp.registry().channel_session(fe(1), ch(1)).unwrap();
    let session = disp.repo().get_session(handle).unwrap().unwrap();
    assert!(
        session.memories_read.contains(&id),
        "query read must link session, got {:?}",
        session.memories_read
    );
}

#[test]
fn memory_read_single_tracks_session() {
    let (disp, _dir) = test_dispatcher();
    let start = ToolArgs::SessionStart(SessionStartArgs {
        task_type: "debugging".to_string(),
        ..Default::default()
    });
    run(&disp, &tool_call(1, start.clone()), &start);
    let id = add_fragment(&disp, 2, "## Track Single\n\n### Context\nSession link.");
    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        id: Some(id.clone()),
        ..Default::default()
    });
    let result = run(&disp, &tool_call(3, read.clone()), &read);
    assert!(!result_is_error(&result));
    let handle = disp.registry().channel_session(fe(1), ch(1)).unwrap();
    let session = disp.repo().get_session(handle).unwrap().unwrap();
    assert!(
        session.memories_read.contains(&id),
        "single read must link session, got {:?}",
        session.memories_read
    );
}

#[test]
fn memory_read_without_session_is_canonical_noop() {
    let (disp, _dir) = test_dispatcher();
    let id = add_fragment(&disp, 1, "## No Session\n\n### Context\nLonely.");
    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        id: Some(id.clone()),
        ..Default::default()
    });
    let result = run(&disp, &tool_call(2, read.clone()), &read);
    assert!(
        !result_is_error(&result),
        "read must succeed without session"
    );
    assert!(
        disp.repo().all_sessions().unwrap().is_empty(),
        "no canonical session must be created"
    );
}

#[test]
fn memory_read_without_session_tracks_virtual_read() {
    let (disp, _dir) = test_dispatcher();
    let id = add_fragment(&disp, 1, "## Virtual Read\n\n### Context\nVirtual.");
    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        id: Some(id.clone()),
        ..Default::default()
    });
    let result = run(&disp, &tool_call(2, read.clone()), &read);
    assert!(!result_is_error(&result));
    let handle = disp
        .registry()
        .virtual_session(fe(1), ch(1))
        .expect("channel must have a virtual session after a session-less read");
    let registry = disp.registry();
    let rec = registry
        .virtual_record(handle)
        .expect("virtual record must exist");
    assert!(
        rec.memories_read.contains(&id),
        "virtual record must carry the read id, got {:?}",
        rec.memories_read
    );
    assert!(
        disp.repo().all_sessions().unwrap().is_empty(),
        "no canonical session must be created"
    );
}

#[test]
fn pagination_absurd_values_are_validation_errors() {
    use ltmrs_domain::command::DomainErrorCode;
    let (disp, _dir) = test_dispatcher();
    seed_pagetoken_memories(&disp, 1, 3);

    let huge_offset = MemoryReadArgs {
        query: Some("pagetoken".to_string()),
        all: true,
        offset: Some(usize::MAX),
        ..Default::default()
    };
    let env = tool_call(11, ToolArgs::MemoryRead(huge_offset.clone()));
    let err = execute_tool(&disp, &env, &ToolArgs::MemoryRead(huge_offset)).unwrap_err();
    assert_eq!(err.code, DomainErrorCode::Validation, "memory_read offset");

    let huge_limit = MemoryReadArgs {
        query: Some("pagetoken".to_string()),
        all: true,
        limit: Some(usize::MAX),
        ..Default::default()
    };
    let env = tool_call(12, ToolArgs::MemoryRead(huge_limit.clone()));
    let err = execute_tool(&disp, &env, &ToolArgs::MemoryRead(huge_limit)).unwrap_err();
    assert_eq!(err.code, DomainErrorCode::Validation, "memory_read limit");

    let huge_search_offset = SemanticSearchArgs {
        query: "pagetoken".to_string(),
        project: None,
        top_k: None,
        offset: Some(usize::MAX),
        hybrid: None,
        explain: false,
        response_format: None,
    };
    let env = tool_call(13, ToolArgs::SemanticSearch(huge_search_offset.clone()));
    let err = execute_tool(&disp, &env, &ToolArgs::SemanticSearch(huge_search_offset)).unwrap_err();
    assert_eq!(
        err.code,
        DomainErrorCode::Validation,
        "semantic_search offset"
    );

    let huge_top_k = SemanticSearchArgs {
        query: "pagetoken".to_string(),
        project: None,
        top_k: Some(usize::MAX),
        offset: None,
        hybrid: None,
        explain: false,
        response_format: None,
    };
    let env = tool_call(14, ToolArgs::SemanticSearch(huge_top_k.clone()));
    let err = execute_tool(&disp, &env, &ToolArgs::SemanticSearch(huge_top_k)).unwrap_err();
    assert_eq!(
        err.code,
        DomainErrorCode::Validation,
        "semantic_search top_k"
    );
}

#[test]
fn t_sec_04_explanations_carry_no_unredacted_fragment_text() {
    let (disp, _dir) = test_dispatcher();
    let secret = "api_key = sk_live_abc123XYZ456";
    let fragment = format!("## Deploys\n\n### Context\nRotate the key {secret} before Friday.");
    assert!(privacy::contains_secret(&fragment));
    assert!(!privacy::contains_critical_secret(secret));

    let add = ToolArgs::MemoryAdd(MemoryAddArgs {
        fragment: fragment.clone(),
        confirm: true,
        ..Default::default()
    });
    let added = run(&disp, &tool_call(1, add.clone()), &add);
    assert!(!result_is_error(&added));
    let id = result_structured(&added).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let read = ToolArgs::MemoryRead(MemoryReadArgs {
        id: Some(id.clone()),
        explain: true,
        ..Default::default()
    });
    let result = run(&disp, &tool_call(2, read.clone()), &read);
    assert!(!result_is_error(&result));

    let structured = result_structured(&result).unwrap();
    let exp_json = serde_json::to_string(&structured["recall_explanation"]).unwrap();
    assert!(
        !exp_json.contains(secret),
        "recall_explanation JSON must not carry the raw secret"
    );
    // Non-vacuity lock (M4): the explanation carries exactly the read
    // record, so the absence above is proven on a populated explanation.
    let items = structured["recall_explanation"]["items"]
        .as_array()
        .expect("explanation items present");
    assert_eq!(items.len(), 1, "explicit-id read explains one record");
    assert_eq!(items[0]["id"].as_str().unwrap(), id);
    let text = result_text(&result);
    let expl_at = text
        .find("## Why these memories?")
        .expect("explain text present");
    assert!(
        !text[expl_at..].contains(secret),
        "rendered explanation must not carry the raw secret"
    );
    // Non-vacuous control: the stored record IS verbatim (confirm path)
    // and the authorized detail section shows it.
    assert!(text[..expl_at].contains(secret));
}
