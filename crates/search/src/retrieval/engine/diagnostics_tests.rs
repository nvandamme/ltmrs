//! Diagnostics / partial-state tests (moved verbatim from `engine.rs`).

use super::test_support::*;
use super::{Engine, QueryEmbedder, RetrievalRequest};
use ltmrs_domain::command::{DomainCommand, DomainResult};
use ltmrs_domain::id::ModelFingerprint;

/// Explanation records leg ranks, score components, and protected status.
#[tokio::test]
async fn explanation_records_full_diagnostics() {
    let (repo, table, mut proj, _guard) = env().await;
    add(&repo, 1, "rust async", "tokio runtime details", None);
    proj.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();

    let req = RetrievalRequest {
        query: "tokio runtime".into(),
        ..base_req()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    assert!(!result.results.is_empty());

    let exp = &result.explanation;
    assert_eq!(
        exp.profile_version,
        crate::retrieval::explain::RETRIEVAL_PROFILE_VERSION
    );
    assert!(!exp.no_match);
    // The top result has a position and score components.
    let top_id = result.results[0].memory.id;
    let cand = &exp.candidates[&top_id];
    assert_eq!(cand.position, 1);
    assert!(cand.scores.native_score > 0.0);
    assert!(cand.scores.native_score <= 1.0);
    // Legacy reference is separate evidence.
    assert!(cand.scores.legacy_reference < 0.1);
}

/// Task 10: partial-result reporting. A query issued while the projection
/// is not yet converged must be flagged partial, not claim completeness.
#[tokio::test]
async fn partial_reported_when_projection_pending() {
    let (repo, table, _proj, _guard) = env().await;
    // Add a memory but do NOT run the projector: projection is pending.
    add(&repo, 1, "rust async", "tokio runtime details", None);
    assert!(
        repo.has_pending_projection(eid(1)).unwrap(),
        "projection must be pending before the worker runs"
    );

    let req = RetrievalRequest {
        query: "tokio runtime".into(),
        model_fingerprint: Some(ModelFingerprint::new(1)),
        ..Default::default()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    // No rows are projected yet, so the result is empty but flagged partial.
    assert!(result.results.is_empty());
    assert!(
        result.explanation.partial,
        "a query with pending projections must be flagged partial"
    );
    assert!(result.explanation.projection_lag >= 1);
    assert!(!result.explanation.dense_ready);
}

/// A failing dense leg degrades to lexical-only instead of failing the
/// whole recall: won lexical results survive, flagged partial.
#[tokio::test]
async fn dense_failure_degrades_to_lexical_partial() {
    let (repo, table, mut proj, _guard) = env().await;
    add(&repo, 1, "rust async", "tokio runtime details", None);
    proj.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();

    let req = RetrievalRequest {
        query: "tokio runtime".into(),
        model_fingerprint: Some(ModelFingerprint::new(1)),
        ..Default::default()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &FailingEmbedder)
        .await
        .unwrap();
    assert!(
        !result.results.is_empty(),
        "lexical hits must survive dense failure"
    );
    assert!(
        result.explanation.partial,
        "degraded recall must be flagged partial"
    );
    assert!(
        !result.explanation.dense_ready,
        "failed dense leg must not report ready"
    );
}

/// Query embedder returning the negation of the passage hash: cosine
/// against any FixedEmbedder passage is strictly negative (both sides
/// are non-negative by construction).
struct NegatedEmbedder;

impl QueryEmbedder for NegatedEmbedder {
    fn embed_query<'a>(
        &'a self,
        query: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = DomainResult<Vec<f32>>> + Send + 'a>>
    {
        let text = query.to_string();
        Box::pin(async move {
            Ok(crate::search::projector::hash_embed_vec(&text, 384)
                .into_iter()
                .map(|v| -v)
                .collect())
        })
    }
}

/// The default request filters anti-correlated dense noise: a query with
/// no lexical overlap and strictly negative dense similarity returns
/// nothing, while explicit opt-out (`None`) still admits it.
#[tokio::test]
async fn default_threshold_filters_negative_dense_noise() {
    let (repo, table, mut proj, _guard) = env().await;
    add(&repo, 1, "rust async", "tokio runtime details", None);
    proj.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();

    let gibberish = "zzqqxx unrelated terms".to_string();
    let default_req = RetrievalRequest {
        query: gibberish.clone(),
        ..base_req()
    };
    let default_result = Engine::new(repo.clone(), table.clone())
        .retrieve(&default_req, &NegatedEmbedder)
        .await
        .unwrap();
    assert!(
        default_result.results.is_empty(),
        "default floor must filter anti-correlated noise, got {:?}",
        default_result
            .results
            .iter()
            .map(|r| r.memory.id.as_uuid().to_string())
            .collect::<Vec<_>>()
    );
    let unfiltered_req = RetrievalRequest {
        query: gibberish,
        min_similarity: None,
        ..base_req()
    };
    let unfiltered_result = Engine::new(repo, table)
        .retrieve(&unfiltered_req, &NegatedEmbedder)
        .await
        .unwrap();
    assert!(
        !unfiltered_result.results.is_empty(),
        "explicit opt-out must still admit nearest noise"
    );
}

/// Task 10: once the projection converges, the same query is complete.
#[tokio::test]
async fn complete_after_projection_converges() {
    let (repo, table, mut proj, _guard) = env().await;
    add(&repo, 1, "rust async", "tokio runtime details", None);
    proj.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();

    let req = RetrievalRequest {
        query: "tokio runtime".into(),
        model_fingerprint: Some(ModelFingerprint::new(1)),
        ..Default::default()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    assert!(
        !result.explanation.partial,
        "a converged projection must not be flagged partial"
    );
    assert!(result.explanation.dense_ready);
    assert!(result.explanation.fts_ready);
}

/// A conflict pair split by the context budget must emit a conflict
/// notice naming both sides (never silently show only one claim). The
/// notice is computed on the FINAL budgeted context, not pre-budget
/// selection — and it is populated on the context itself.
#[tokio::test]
async fn conflict_notice_fires_when_budget_splits_pair() {
    let (repo, table, mut proj, _guard) = env().await;
    add(&repo, 1, "claim one", "the timeout is 30 seconds", None);
    add(&repo, 2, "claim two", "the timeout is 60 seconds", None);
    proj.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();

    repo.apply(
        &ctx(10),
        &DomainCommand::Relate {
            relation: ltmrs_domain::relation::Relation::new(
                eid(100),
                eid(1),
                eid(2),
                ltmrs_domain::relation::RelationType::Contradicts,
                None,
                ltmrs_domain::memory::Instant::new(1),
            ),
        },
    )
    .unwrap();

    let req = RetrievalRequest {
        query: "timeout seconds".into(),
        context_budget: crate::retrieval::context::ContextBudget {
            max_bytes: 10,
            has_tokenizer: false,
        },
        ..base_req()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    let notice = result.context.conflict_notice.as_deref().unwrap_or("");
    assert!(
        notice.contains(&eid(1).as_uuid().to_string())
            && notice.contains(&eid(2).as_uuid().to_string()),
        "budget-split conflict must name both sides, got context items {:?} notice {notice:?}",
        result.context.items.len()
    );
}

/// A missing FTS index degrades lexical to empty: a non-empty query must
/// still be flagged partial, never presented as complete no-match.
#[tokio::test]
async fn missing_fts_index_is_partial_not_complete() {
    let (repo, table, mut proj, _guard) = env().await;
    add(&repo, 1, "rust async", "tokio runtime details", None);
    proj.run_until_idle().await.unwrap();
    // Deliberately no FTS index: lexical degrades to [].

    let req = RetrievalRequest {
        query: "tokio runtime".into(),
        ..base_req()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    assert!(
        result.explanation.partial,
        "unready lexical leg must flag partial"
    );
    assert!(!result.explanation.fts_ready);
}

/// The no-answer path must apply the same degraded-leg rule as the
/// ranked path: lexical-only with a missing FTS index is partial, not
/// a complete no-match.
#[tokio::test]
async fn no_answer_with_unready_lexical_leg_is_partial() {
    let (repo, table, mut proj, _guard) = env().await;
    add(&repo, 1, "rust async", "tokio runtime details", None);
    proj.run_until_idle().await.unwrap();
    // No FTS index and lexical-only: both legs empty -> no-answer path.

    let req = RetrievalRequest {
        query: "zzqqxx completely unrelated terms".into(),
        model_fingerprint: None,
        ..Default::default()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    assert!(result.results.is_empty());
    assert!(
        result.explanation.partial,
        "no-answer on an unready lexical leg must flag partial"
    );
    assert!(!result.explanation.fts_ready);
}

/// The no-answer path must flag a failed dense leg: with no lexical
/// hits either, the empty result may be degradation, not true absence.
#[tokio::test]
async fn no_answer_with_failed_dense_leg_is_partial() {
    let (repo, table, mut proj, _guard) = env().await;
    add(&repo, 1, "rust async", "tokio runtime details", None);
    proj.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();

    let req = RetrievalRequest {
        query: "zzqqxx completely unrelated terms".into(),
        model_fingerprint: Some(ModelFingerprint::new(1)),
        ..Default::default()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &FailingEmbedder)
        .await
        .unwrap();
    assert!(result.results.is_empty());
    assert!(
        result.explanation.partial,
        "no-answer with a failed dense leg must flag partial"
    );
    assert!(!result.explanation.dense_ready);
}
