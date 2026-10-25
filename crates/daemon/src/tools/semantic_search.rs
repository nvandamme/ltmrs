//! semantic_search tool (moved verbatim from `tools.rs`).

use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainPayload, IpcEnvelope};
use ltmrs_compat::lemma::tool_args::SemanticSearchArgs;
use ltmrs_domain::command::DomainResult;
use ltmrs_domain::id::EntityId;
use ltmrs_domain::memory::Memory;
use serde_json::{Value, json};

use super::ids::legacy_id_of;
use super::recall::{
    RecallSelection, engine_selection_score, explain_recall, format_recall_explanation, relevance,
};

use super::{effective_for_read, format_result};
use ltmrs_domain::command::{DomainError, DomainErrorCode};

/// Maximum page size for semantic_search (frozen upstream contract).
pub(crate) const SEMANTIC_PAGE_MAX: usize = 30;

pub(crate) fn exec_semantic_search(
    disp: &Dispatcher,
    _envelope: &IpcEnvelope,
    args: &SemanticSearchArgs,
    resolved: &ltmrs_service::projects::ResolvedScope,
) -> DomainResult<DomainPayload> {
    let repo = disp.repo();
    let format = args.response_format;

    let top_k = args.top_k.unwrap_or(10);
    if top_k == 0 || top_k > SEMANTIC_PAGE_MAX {
        return Err(DomainError::new(
            DomainErrorCode::Validation,
            format!("semantic_search top_k {top_k} out of range 1..={SEMANTIC_PAGE_MAX}"),
        ));
    }
    let offset = args.offset.unwrap_or(0);
    let window_end = ltmrs_compat::lemma::pagination::validate_window(offset, top_k)?;

    // Use the search backend when available.
    let mut scored: Vec<(Memory, f64)> = Vec::new();
    let mut engine_total: Option<usize> = None;
    let mut engine_truncated = false;
    let mut engine_explanation: Option<ltmrs_search::retrieval::explain::RetrievalExplanation> =
        None;
    let mut fallback_count: Option<usize> = None;
    // hybrid:false serves the Lemma-faithful TF-IDF reference profile
    // (DEV-011): corpus-wide TF-IDF cosine over the served snapshot, with no
    // dense leg and no FTS index, so the flag is truthful about parity.
    let tfidf_served = args.hybrid == Some(false);
    if tfidf_served {
        let export = repo.export_snapshot()?;
        let scope = effective_for_read(resolved, None, None, None, false)?;
        let docs: Vec<(EntityId, String)> = export
            .memories
            .iter()
            .filter(|m| m.lifecycle.is_recallable())
            .filter(|m| scope.is_eligible(m))
            .map(|m| {
                (
                    m.id,
                    ltmrs_search::retrieval::tfidf::corpus_text(
                        &m.title,
                        &m.fragment,
                        &m.description,
                    ),
                )
            })
            .collect();
        let by_id: std::collections::HashMap<EntityId, &Memory> =
            export.memories.iter().map(|m| (m.id, m)).collect();
        scored = ltmrs_search::retrieval::tfidf::rank(&args.query, &docs)
            .into_iter()
            .filter_map(|(id, s)| by_id.get(&id).map(|m| ((*m).clone(), s)))
            .collect();
        fallback_count = Some(scored.len());
    }
    if !tfidf_served && let Some(sb) = disp.search() {
        // hybrid is absent/true here, so the dense leg runs when a backend
        // is attached (the enhanced hybrid default); lexical-only selection
        // now lives on the TF-IDF arm above.
        let use_dense = true;
        // One effective scope (Wave A3b): stable ids key eligibility in
        // every leg; the attached slugs travel for the engine's own
        // resolution.
        let scope = effective_for_read(resolved, None, None, None, false)?;
        let req = ltmrs_search::retrieval::engine::RetrievalRequest {
            query: args.query.clone(),
            scope: ltmrs_domain::command::Scope {
                project: scope.project.clone(),
                project_id: resolved.project_id_str.clone(),
                all_projects: scope.all_projects,
                ..Default::default()
            },
            attached_slugs: scope.attached.clone(),
            // Dense leg in the pinned E5 space when a backend is attached;
            // no backend (or no dense hits) falls back to lexical below.
            model_fingerprint: if use_dense {
                Some(ltmrs_embeddings::e5_small::E5_SMALL_FINGERPRINT)
            } else {
                None
            },
            candidate_limit: ltmrs_compat::lemma::pagination::MAX_PAGINATION_WINDOW,
            result_limit: window_end,
            mmr_config: ltmrs_search::retrieval::mmr::MmrConfig {
                max_items: window_end.max(1),
                ..Default::default()
            },
            ..Default::default()
        };
        if let Ok(result) = sb.retrieve_sync(&req) {
            // Map each result to its engine score (native calibrated score,
            // falling back to the legacy reference score). Not a claim of
            // identical TF-IDF — the legacy `score` field is a display value.
            let scores = &result.explanation.candidates;
            for r in result.results {
                let s = engine_selection_score(scores, r.memory.id);
                scored.push((r.memory, s));
            }
            engine_total = Some(result.total);
            engine_truncated = result.pool_truncated;
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
        tfidf_served: bool,
    ) -> serde_json::Value {
        if tfidf_served {
            // The TF-IDF reference arm uses neither the dense leg nor the
            // FTS index: readiness flags stay false, honestly.
            return serde_json::json!({
                "mode": "lexical",
                "dense_ready": false,
                "fts_ready": false,
                "partial": false,
                "no_match": false,
                "candidates": fallback_candidates,
                "conflict_notice": null,
            });
        }
        match explanation {
            Some(exp) => serde_json::json!({
                // Hybrid only when the dense leg actually ran: the request
                // always names the pinned E5 space, but a backend without
                // vectors (or a failed dense leg) serves lexically.
                "mode": if exp.model_fingerprint.is_some() && exp.dense_ready { "hybrid" } else { "lexical" },
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

    // Lexical fallback when the backend is unavailable or returned nothing.
    // The substring scan covers the full snapshot, so its total is exact;
    // the engine arm carries the pool total reported above. The TF-IDF arm
    // never falls through here: its empty result is the profile's own
    // abstention signal, not a trigger for a weaker scan.
    if scored.is_empty() && !tfidf_served {
        let export = repo.export_snapshot()?;
        let scope = effective_for_read(resolved, None, None, None, false)?;
        let q = args.query.to_lowercase();
        let mut candidates: Vec<(Memory, f64)> = export
            .memories
            .iter()
            .filter(|m| m.lifecycle.is_recallable())
            .filter(|m| scope.is_eligible(m))
            .map(|m| (m.clone(), relevance(m, &q)))
            .filter(|(_, s)| *s > 0.0)
            .collect();
        candidates.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.id.cmp(&b.0.id))
        });
        fallback_count = Some(candidates.len());
        scored = candidates;
        // The served rows come from the substring fallback, not the engine:
        // drop the engine explanation so explain:true reports the effective
        // fallback mode instead of a hybrid that produced nothing. (When the
        // fallback also finds nothing, the engine no-match is preserved.)
        if !scored.is_empty() {
            engine_explanation = None;
            engine_total = None;
            engine_truncated = false;
        }
    }

    // Page through the choke point: the total is the engine pool total or
    // the full fallback count, never the served vector's length.
    let total = engine_total.or(fallback_count).unwrap_or(0);
    let paged = ltmrs_compat::lemma::pagination::paginate_vec(
        scored,
        total,
        offset,
        top_k,
        engine_truncated,
    )?;
    let (page, has_more, next_offset) = (paged.items, paged.has_more, paged.next_offset);
    let fallback_candidates = fallback_count.unwrap_or(total);

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
            "next_offset": next_offset,
        });
        if args.explain {
            data["explanation"] =
                explain_search(&engine_explanation, fallback_candidates, tfidf_served);
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
            "\nMore results available. Pass offset={} for the next page.",
            next_offset.unwrap_or(offset),
        ));
    }

    let mut data = json!({
        "count": page.len(),
        "total": total,
        "results": results_json,
        "has_more": has_more,
        "next_offset": next_offset,
    });
    if args.explain {
        data["explanation"] =
            explain_search(&engine_explanation, fallback_candidates, tfidf_served);
    }
    // Upstream-shaped supplement (DEV-012): the legs that actually served
    // name the selection method; ranks are the effective served positions.
    let recall_method = if tfidf_served {
        "tfidf_cosine"
    } else if let Some(exp) = engine_explanation.as_ref() {
        if exp.model_fingerprint.is_some() && exp.dense_ready {
            "hybrid_rrf_mmr"
        } else {
            "fts5_bm25"
        }
    } else {
        "substring_fallback"
    };
    let selections: Vec<RecallSelection> = page
        .iter()
        .enumerate()
        .map(|(i, (m, s))| RecallSelection {
            memory: m,
            method: recall_method,
            rank: Some(offset.saturating_add(i).saturating_add(1) as u64),
            score: Some(*s),
            graph: None,
        })
        .collect();
    let recall_explanation = if args.explain && !selections.is_empty() {
        Some(explain_recall(
            repo,
            &selections,
            "project_and_global",
            args.project.as_deref(),
        ))
    } else {
        None
    };
    if let Some(exp) = &recall_explanation {
        text.push_str(&format_recall_explanation(exp));
    }
    if let Some(exp) = recall_explanation {
        data["recall_explanation"] = json!(exp);
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
            project_id: None,
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

#[tokio::test]
async fn semantic_search_engine_explain_adds_recall_explanation() {
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
    assert_eq!(structured["explanation"]["mode"], "hybrid");
    let exp = &structured["recall_explanation"];
    assert_eq!(exp["applies_to"], json!("this_call"));
    let items = exp["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["selection"]["method"], json!("hybrid_rrf_mmr"));
    assert_eq!(items[0]["selection"]["rank"], json!(1));
    assert!(items[0]["selection"]["score"].is_number());
}

#[test]
fn semantic_search_fallback_explain_adds_recall_explanation() {
    let (disp, _dir) = test_dispatcher();
    add_fragment(
        &disp,
        1,
        "## Rust Cargo Testing\n\n### Context\nHow to write tests in Rust with cargo test.",
    );
    let args = SemanticSearchArgs {
        query: "rust cargo".to_string(),
        project: None,
        top_k: None,
        offset: None,
        hybrid: None,
        explain: true,
        response_format: None,
    };
    let env = tool_call(2, ToolArgs::SemanticSearch(args.clone()));
    let result = run(&disp, &env, &ToolArgs::SemanticSearch(args));
    assert!(!result_is_error(&result));
    let structured = result_structured(&result).unwrap();
    assert_eq!(structured["explanation"]["mode"], "lexical-fallback");
    let exp = &structured["recall_explanation"];
    assert_eq!(exp["applies_to"], json!("this_call"));
    let items = exp["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["selection"]["method"], json!("substring_fallback"));
    assert_eq!(items[0]["selection"]["rank"], json!(1));
}

#[test]
fn semantic_search_hybrid_false_explain_adds_recall_explanation() {
    let (disp, _dir) = test_dispatcher();
    add_fragment(
        &disp,
        1,
        "## Rust Cargo Testing\n\n### Context\nHow to write tests in Rust with cargo test.",
    );
    add_fragment(
        &disp,
        2,
        "## Bread Baking Recipes\n\n### Context\nHow to bake bread at home.",
    );
    let args = SemanticSearchArgs {
        query: "rust cargo".to_string(),
        project: None,
        top_k: None,
        offset: None,
        hybrid: Some(false),
        explain: true,
        response_format: None,
    };
    let env = tool_call(3, ToolArgs::SemanticSearch(args.clone()));
    let result = run(&disp, &env, &ToolArgs::SemanticSearch(args));
    assert!(!result_is_error(&result));
    let structured = result_structured(&result).unwrap();
    assert_eq!(
        structured["explanation"]["mode"], "lexical",
        "native explanation stays, reporting the legs actually run"
    );
    let exp = &structured["recall_explanation"];
    assert_eq!(exp["applies_to"], json!("this_call"));
    assert!(exp["scope"].is_object());
    assert!(exp["notice"].is_string());
    assert!(exp["correction_tools"].is_array());
    let items = exp["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["selection"]["method"], json!("tfidf_cosine"));
    assert_eq!(items[0]["selection"]["rank"], json!(1));
    assert!(items[0]["selection"]["score"].as_f64().unwrap() > 0.0);
    assert!(result_text(&result).contains("## Why these memories?"));
}

#[test]
fn semantic_search_hybrid_false_uses_tfidf_profile() {
    let (disp, _dir) = test_dispatcher();
    add_fragment(
        &disp,
        1,
        "## Rust Cargo Testing\n\n### Context\nHow to write tests in Rust with cargo test.",
    );
    add_fragment(
        &disp,
        2,
        "## Bread Baking Recipes\n\n### Context\nHow to bake bread at home.",
    );
    let args = SemanticSearchArgs {
        query: "rust cargo".to_string(),
        project: None,
        top_k: None,
        offset: None,
        hybrid: Some(false),
        explain: false,
        response_format: None,
    };
    let env = tool_call(3, ToolArgs::SemanticSearch(args.clone()));
    let result = run(&disp, &env, &ToolArgs::SemanticSearch(args));
    assert!(!result_is_error(&result));
    let structured = result_structured(&result).unwrap();
    assert_eq!(
        structured["count"].as_u64().unwrap(),
        1,
        "bread memory shares no query term and must be excluded"
    );
    assert_eq!(structured["total"].as_u64().unwrap(), 1);
    let top = &structured["results"][0];
    assert!(
        top["title"].as_str().unwrap().contains("Rust"),
        "TF-IDF must rank the rust memory first"
    );
    let score = top["score"].as_f64().unwrap();
    assert!(
        score > 0.0 && score <= 1.0,
        "TF-IDF cosine lives in (0, 1]; substring counts exceed 1: {score}"
    );
}

#[tokio::test]
async fn semantic_search_pagination_honest_totals_hybrid() {
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
            let args = SemanticSearchArgs {
                query: "pagetoken".to_string(),
                project: None,
                top_k: None,
                offset: if offset == 0 {
                    None
                } else {
                    Some(offset as usize)
                },
                hybrid: None,
                explain: false,
                response_format: None,
            };
            let env = tool_call(1000 + offset, ToolArgs::SemanticSearch(args.clone()));
            let tool = ToolArgs::SemanticSearch(args);
            let d = std::sync::Arc::clone(&disp);
            let result = tokio::task::spawn_blocking(move || run(&d, &env, &tool))
                .await
                .unwrap();
            assert!(!result_is_error(&result), "offset {offset} of {n}");
            let structured = result_structured(&result).unwrap();
            expect_pagination_page(&structured, n as u64, 10, offset);
        }
        assert!(
            calls.load(Ordering::SeqCst) >= 1,
            "dense leg must run on the hybrid path"
        );
    }
}

#[test]
fn semantic_search_pagination_honest_totals_lexical() {
    for n in [31usize, 51, 100, 500] {
        let (disp, _dir) = test_dispatcher();
        seed_pagetoken_memories(&disp, 1, n);
        let offsets = [0u64, 7, n as u64 / 2, n as u64 - 1, n as u64, n as u64 + 50];
        for offset in offsets {
            let args = SemanticSearchArgs {
                query: "pagetoken".to_string(),
                project: None,
                top_k: None,
                offset: if offset == 0 {
                    None
                } else {
                    Some(offset as usize)
                },
                hybrid: None,
                explain: false,
                response_format: None,
            };
            let env = tool_call(1000 + offset, ToolArgs::SemanticSearch(args.clone()));
            let result = run(&disp, &env, &ToolArgs::SemanticSearch(args));
            assert!(!result_is_error(&result), "offset {offset} of {n}");
            let structured = result_structured(&result).unwrap();
            expect_pagination_page(&structured, n as u64, 10, offset);
        }
    }
}
