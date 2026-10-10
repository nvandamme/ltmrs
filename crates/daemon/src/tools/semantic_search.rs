//! semantic_search tool (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::SemanticSearchArgs;
use ltmrs_domain::command::DomainResult;
use ltmrs_domain::memory::Memory;
use serde_json::{Value, json};

use super::ids::legacy_id_of;

use super::format_result;

pub(crate) fn exec_semantic_search(
    disp: &Dispatcher,
    _envelope: &IpcEnvelope,
    args: &SemanticSearchArgs,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    let format = args.response_format;

    let top_k = args.top_k.unwrap_or(10).clamp(1, 30);
    let offset = args.offset.unwrap_or(0);

    // Use the search backend when available. A Complete engine answer
    // stands even when empty (a converged no-match is legitimate); anything
    // else routes to the labelled fallback below.
    let mut scored: Vec<(Memory, f64)> = Vec::new();
    let mut engine_complete = false;
    let mut engine_explanation: Option<ltmrs_search::retrieval::explain::RetrievalExplanation> =
        None;
    if let Some(sb) = disp.search() {
        // hybrid:false forces lexical-only (upstream parity on demand);
        // absent/true runs dense only when this backend serves vectors.
        let use_dense = args.hybrid != Some(false);
        let req = ltmrs_search::retrieval::engine::RetrievalRequest {
            query: args.query.clone(),
            scope: ltmrs_domain::command::Scope {
                project: args.project.clone(),
                all_projects: false,
                ..Default::default()
            },
            model_fingerprint: if use_dense {
                sb.model_fingerprint()
            } else {
                None
            },
            result_limit: top_k + offset,
            ..Default::default()
        };
        if let Ok(result) = sb.retrieve_sync(&req) {
            // Map each result to its engine score (native calibrated score,
            // falling back to the legacy reference score). Not a claim of
            // identical TF-IDF — the legacy `score` field is a display value.
            engine_complete = !result.explanation.partial;
            let scores = &result.explanation.candidates;
            for r in result.results {
                let s = scores
                    .get(&r.memory.id)
                    .map(|c| c.scores.native_score.max(c.scores.legacy_reference))
                    .unwrap_or(0.5);
                scored.push((r.memory, s));
            }
            engine_explanation = Some(result.explanation);
        }
    }

    /// Explain how the answer was produced when requested: the effective
    /// mode (hybrid only when the dense leg ran) plus engine readiness,
    /// or the fallback mode otherwise. The fallback candidate count is
    /// threaded in (the engine arm reports its own examined pool).
    fn explain_search(
        explanation: &Option<ltmrs_search::retrieval::explain::RetrievalExplanation>,
        fallback_candidates: usize,
    ) -> serde_json::Value {
        match explanation {
            Some(exp) => serde_json::json!({
                "mode": if exp.model_fingerprint.is_some() { "hybrid" } else { "lexical" },
                "dense_ready": exp.dense_ready,
                "fts_ready": exp.fts_ready,
                "partial": exp.partial,
                "no_match": exp.no_match,
                "candidates": exp.candidates.len(),
                "conflict_notice": exp.conflict_notice,
            }),
            None => serde_json::json!({
                "mode": "lexical-fallback",
                "dense_ready": false,
                // Substring scan over a canonical snapshot, not the FTS
                // index: never claim FTS readiness here.
                "fts_ready": false,
                "partial": false,
                "no_match": false,
                "candidates": fallback_candidates,
                "conflict_notice": null,
            }),
        }
    }

    // Lexical fallback when there is no Complete engine answer: no backend,
    // a failed call, or a Partial result. A Complete empty answer stays empty.
    if !engine_complete && scored.is_empty() {
        let export = repo.export_snapshot()?;
        let q = args.query.to_lowercase();
        let mut candidates: Vec<(Memory, f64)> = export
            .memories
            .iter()
            .filter(|m| m.lifecycle.is_recallable())
            .filter(|m| {
                if let Some(p) = &args.project {
                    m.project.as_deref() == Some(p.as_str()) || m.project.is_none()
                } else {
                    true
                }
            })
            .map(|m| (m.clone(), super::recall::relevance(m, &q)))
            .filter(|(_, s)| *s > 0.0)
            .collect();
        candidates.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.id.cmp(&b.0.id))
        });
        scored = candidates;
        // The served rows come from the substring fallback, not the engine:
        // drop the engine explanation so explain:true reports the effective
        // fallback mode instead of a hybrid that produced nothing. (When the
        // fallback also finds nothing, the engine no-match is preserved.)
        if !scored.is_empty() {
            engine_explanation = None;
        }
    }

    let total = scored.len();
    let page: Vec<(Memory, f64)> = scored
        .iter()
        .skip(offset)
        .take(top_k)
        .map(|(m, s)| (m.clone(), *s))
        .collect();
    let has_more = offset + top_k < total;
    let next_offset = if has_more { offset + top_k } else { 0 };

    if page.is_empty() {
        let text = format!(
            "No semantically similar memories found for: \"{}\"",
            args.query
        );
        let mut data = json!({
            "count": 0,
            "total": total,
            "results": [],
            "has_more": has_more,
            "next_offset": if has_more { Some(next_offset) } else { None },
        });
        if args.explain {
            data["explanation"] = explain_search(&engine_explanation, scored.len());
        }
        return Ok(format_result(text, data, format));
    }

    let mut text = format!(
        "=== SEMANTIC SEARCH RESULTS ===\nQuery: \"{}\"\nFound {} similar memories:\n\n",
        args.query,
        page.len()
    );
    let mut results_json: Vec<Value> = Vec::new();
    for (m, score) in &page {
        let preview: String = m.fragment.chars().take(100).collect();
        text.push_str(&format!(
            "  [{}%] [{}] \"{}\"\n      {}...\n",
            (*score * 100.0).round() as u64,
            legacy_id_of(repo, m),
            m.title,
            preview
        ));
        results_json.push(json!({
            "id": legacy_id_of(repo, m),
            "title": m.title,
            "score": score,
            "fragment_preview": m.fragment.chars().take(200).collect::<String>(),
        }));
    }
    if has_more {
        text.push_str(&format!(
            "\nMore results available. Pass offset={next_offset} for the next page."
        ));
    }

    let mut data = json!({
        "count": page.len(),
        "total": total,
        "results": results_json,
        "has_more": has_more,
        "next_offset": if has_more { Some(next_offset) } else { None },
    });
    if args.explain {
        data["explanation"] = explain_search(&engine_explanation, scored.len());
    }
    Ok(format_result(text, data, format))
}

#[cfg(test)]
/// Publish one projection row directly (no worker): lets explain tests
/// drive the engine instead of falling through to the substring
/// fallback on an empty table.
async fn publish_search_row(
    repo: &CanonicalRepository,
    table: &ltmrs_search::search::table::SearchTable,
    lexical_text: &str,
    fingerprint: ltmrs_domain::id::ModelFingerprint,
) {
    use ltmrs_domain::id::{ChunkId, DocumentRevision, StoreGeneration};
    let memory_id = repo.export_snapshot().unwrap().memories[0].id;
    table
        .publish_rows(&[ltmrs_search::search::row::SearchRow {
            store_generation: StoreGeneration::FIRST,
            memory_id,
            document_revision: DocumentRevision::new(1),
            model_fingerprint: fingerprint,
            chunk_id: ChunkId::new(0),
            chunker_version: "v1".to_string(),
            lexical_text: lexical_text.to_string(),
            char_start: 0,
            char_end: lexical_text.len() as u64,
            project: None,
            fragment_type: "fact".to_string(),
            created_at_millis: 1000,
            confidence: 0.9,
            updated_at_millis: 1000,
            embedding: Some(vec![0.0; 384]),
        }])
        .await
        .unwrap();
}
#[cfg(test)]
use super::test_support::*;
#[cfg(test)]
use ltmrs_compat::lemma::tool_args::ToolArgs;
#[cfg(test)]
use ltmrs_domain::clock::FrozenClock;
#[cfg(test)]
use ltmrs_service::repository::CanonicalRepository;
#[cfg(test)]
use std::sync::Arc;

#[test]
fn semantic_search_finds_relevant() {
    let (disp, _dir) = test_dispatcher();
    add_fragment(
        &disp,
        1,
        "## Rust Testing Patterns\n\n### Context\nHow to write tests in Rust with cargo test.",
    );
    add_fragment(
        &disp,
        2,
        "## Cooking Recipes\n\n### Context\nHow to bake bread at home.",
    );
    let ss_args = |q: &str| SemanticSearchArgs {
        query: q.to_string(),
        project: None,
        top_k: None,
        offset: None,
        hybrid: None,
        explain: false,
        response_format: None,
    };
    let env = tool_call(3, ToolArgs::SemanticSearch(ss_args("rust testing cargo")));
    let result = run(
        &disp,
        &env,
        &ToolArgs::SemanticSearch(ss_args("rust testing cargo")),
    );
    assert!(!result_is_error(&result));
    let structured = result_structured(&result).unwrap();
    assert!(structured["count"].as_u64().unwrap() >= 1);
    assert!(result_text(&result).contains("SEMANTIC SEARCH RESULTS"));
}

#[test]
fn semantic_search_empty_result() {
    let (disp, _dir) = test_dispatcher();
    add_fragment(
        &disp,
        1,
        "## Unrelated Content\n\n### Context\nNothing matching here.",
    );
    let ss_args = |q: &str| SemanticSearchArgs {
        query: q.to_string(),
        project: None,
        top_k: None,
        offset: None,
        hybrid: None,
        explain: false,
        response_format: None,
    };
    let env = tool_call(
        2,
        ToolArgs::SemanticSearch(ss_args("quantum chromodynamics lattice")),
    );
    let result = run(
        &disp,
        &env,
        &ToolArgs::SemanticSearch(ss_args("quantum chromodynamics lattice")),
    );
    assert!(!result_is_error(&result));
    let structured = result_structured(&result).unwrap();
    assert_eq!(structured["count"].as_u64().unwrap(), 0);
    assert!(result_text(&result).contains("No semantically similar memories found"));
}

/// Semantic fallback: with a backend attached but an empty table, the
/// dense leg runs and finds nothing, and the lexical snapshot fallback
/// still answers (mirrors the browse fallback above).
#[tokio::test]
async fn semantic_search_falls_back_on_empty_backend() {
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
        "## Semantic Fallback\n\n### Context\nLexical fallback must answer.",
    );

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
    // leg runs before the fallback answers.
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
    let args = SemanticSearchArgs {
        query: "fallback".to_string(),
        project: None,
        top_k: None,
        offset: None,
        hybrid: None,
        explain: false,
        response_format: None,
    };
    let env = tool_call(2, ToolArgs::SemanticSearch(args.clone()));
    // Same sync-context rule as the dispatcher: bridge from blocking code.
    let tool = ToolArgs::SemanticSearch(args);
    let result = tokio::task::spawn_blocking(move || run(&disp, &env, &tool))
        .await
        .unwrap();
    assert!(!result_is_error(&result));
    let structured = result_structured(&result).unwrap();
    assert!(
        structured["count"].as_u64().unwrap() >= 1,
        "lexical fallback must answer on an empty backend"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "dense leg must have run before falling back"
    );
}

/// hybrid:false is honored as lexical-only: the dense leg never runs,
/// and lexical results still answer (upstream parity on demand).
#[tokio::test]
async fn semantic_search_hybrid_false_skips_dense_leg() {
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
        "## Lexical Only\n\n### Context\nDense must stay silent.",
    );

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
    let backend = Arc::new(SearchBackend::new(
        Arc::clone(&repo),
        table,
        Arc::new(ltmrs_search::search::backend::embedders::QueryEmbedderAdapter::new(embedder)),
    ));
    let disp =
        Dispatcher::new(repo, crate::registry::FrontendRegistry::new(), clock).with_search(backend);
    let args = SemanticSearchArgs {
        query: "lexical silent".to_string(),
        project: None,
        top_k: None,
        offset: None,
        hybrid: Some(false),
        explain: false,
        response_format: None,
    };
    let env = tool_call(2, ToolArgs::SemanticSearch(args.clone()));
    let tool = ToolArgs::SemanticSearch(args);
    let result = tokio::task::spawn_blocking(move || run(&disp, &env, &tool))
        .await
        .unwrap();
    assert!(!result_is_error(&result));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "hybrid:false must not invoke the embedder"
    );
    let structured = result_structured(&result).unwrap();
    assert!(
        structured["count"].as_u64().unwrap() >= 1,
        "lexical results must answer"
    );
}

/// explain:true reports how the answer was produced: engine readiness
/// when the backend ran, fallback mode otherwise.
#[tokio::test]
async fn semantic_search_explain_reports_engine_explanation() {
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
        "## Explained Search\n\n### Context\nRecall with reasons.",
    );

    let lance_dir = tempfile::tempdir().unwrap();
    let table = SearchTable::open(lance_dir.path().to_str().unwrap())
        .await
        .unwrap();
    // Publish one row so the engine (not the fallback) answers.
    publish_search_row(
        &repo,
        &table,
        "explained reasons recall",
        ltmrs_embeddings::e5_small::E5_SMALL_FINGERPRINT,
    )
    .await;
    table.create_fts_index().await.unwrap();
    let embedder = Arc::new(ClosureEmbedder::new(|_| Ok(vec![0.0; 384])));
    // Dense-capable test double: declare the fingerprint so the hybrid
    // engine path (not lexical-only) answers.
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
    let args = SemanticSearchArgs {
        query: "explained reasons".to_string(),
        project: None,
        top_k: None,
        offset: None,
        hybrid: None,
        explain: true,
        response_format: None,
    };
    let env = tool_call(2, ToolArgs::SemanticSearch(args.clone()));
    let tool = ToolArgs::SemanticSearch(args);
    let result = tokio::task::spawn_blocking(move || run(&disp, &env, &tool))
        .await
        .unwrap();
    assert!(!result_is_error(&result));
    let structured = result_structured(&result).unwrap();
    assert_eq!(
        structured["explanation"]["mode"], "hybrid",
        "backend path must report hybrid mode"
    );
    assert!(
        structured["explanation"]["dense_ready"].is_boolean(),
        "readiness must be reported"
    );
}

/// explain:true with hybrid:false reports the lexical mode actually
/// run, not hybrid: the mode names the effective legs, and the
/// no-backend fallback does not claim a ready FTS index it never used.
#[tokio::test]
async fn semantic_search_explain_reports_effective_mode() {
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
        "## Explained Search\n\n### Context\nRecall with reasons.",
    );

    let lance_dir = tempfile::tempdir().unwrap();
    let table = SearchTable::open(lance_dir.path().to_str().unwrap())
        .await
        .unwrap();
    // Publish one row so the engine serves lexically (hybrid:false
    // skips the dense leg): the mode must name the effective legs.
    publish_search_row(
        &repo,
        &table,
        "explained reasons recall",
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
    let args = SemanticSearchArgs {
        query: "explained reasons".to_string(),
        project: None,
        top_k: None,
        offset: None,
        hybrid: Some(false),
        explain: true,
        response_format: None,
    };
    let env = tool_call(2, ToolArgs::SemanticSearch(args.clone()));
    let tool = ToolArgs::SemanticSearch(args);
    let result = tokio::task::spawn_blocking(move || run(&disp, &env, &tool))
        .await
        .unwrap();
    assert!(!result_is_error(&result));
    let structured = result_structured(&result).unwrap();
    assert_eq!(
        structured["explanation"]["mode"], "lexical",
        "hybrid:false must report the lexical mode actually run"
    );
}

/// explain:true with an empty engine result served by the substring
/// fallback reports the fallback mode (never a hybrid that produced
/// nothing) and never claims FTS readiness for a scan that used no
/// index.
#[tokio::test]
async fn semantic_search_explain_reports_fallback_mode() {
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
        "## Explained Search\n\n### Context\nRecall with reasons.",
    );

    // Empty table: the engine finds nothing, the substring fallback
    // serves from the canonical snapshot.
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
    let args = SemanticSearchArgs {
        query: "explained reasons".to_string(),
        project: None,
        top_k: None,
        offset: None,
        hybrid: None,
        explain: true,
        response_format: None,
    };
    let env = tool_call(2, ToolArgs::SemanticSearch(args.clone()));
    let tool = ToolArgs::SemanticSearch(args);
    let result = tokio::task::spawn_blocking(move || run(&disp, &env, &tool))
        .await
        .unwrap();
    assert!(!result_is_error(&result));
    let structured = result_structured(&result).unwrap();
    assert!(
        structured["count"].as_u64().unwrap() >= 1,
        "fallback must serve the snapshot row"
    );
    assert_eq!(
        structured["explanation"]["mode"], "lexical-fallback",
        "fallback-served rows must not report hybrid"
    );
    assert_eq!(
        structured["explanation"]["fts_ready"], false,
        "substring scan must not claim FTS readiness"
    );
    assert_eq!(
        structured["explanation"]["candidates"], structured["total"],
        "fallback candidates must match the examined pool"
    );
}
