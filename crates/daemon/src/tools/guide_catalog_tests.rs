//! guide-catalog dense-leg tests (moved verbatim from `tools.rs`).

use super::guide_catalog::suggest_guides_dense;
use super::test_support::*;
use crate::dispatcher::Dispatcher;
use ltmrs_compat::lemma::tool_args::{GuideCreateArgs, GuideGetArgs, ToolArgs};
use ltmrs_domain::clock::FrozenClock;
use ltmrs_domain::guide::Guide;

// ---- WP-09: guide tools ----

/// Biased passage double: task text maps to TASK_VEC; guide catalog
/// texts map by guide-name substring (beta ~= task, everything else
/// orthogonal). Query role always returns the task vector.
struct BiasedGuideEmbedder {
    fail_passages: bool,
}

impl ltmrs_search::retrieval::engine::QueryEmbedder for BiasedGuideEmbedder {
    fn embed_query<'a>(
        &'a self,
        _query: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = ltmrs_domain::command::DomainResult<Vec<f32>>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move { Ok(vec![1.0f32, 0.0, 0.0]) })
    }

    fn embed_passages<'a>(
        &'a self,
        texts: &'a [String],
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = ltmrs_domain::command::DomainResult<Vec<Vec<f32>>>>
                + Send
                + 'a,
        >,
    > {
        let fail = self.fail_passages;
        Box::pin(async move {
            if fail {
                return Err(ltmrs_domain::command::DomainError::new(
                    ltmrs_domain::command::DomainErrorCode::Validation,
                    "boom".to_string(),
                ));
            }
            Ok(texts
                .iter()
                .map(|t| {
                    if t.to_lowercase().contains("beta") {
                        vec![1.0f32, 0.0, 0.0]
                    } else {
                        vec![0.0f32, 1.0, 0.0]
                    }
                })
                .collect())
        })
    }
}

fn dense_alpha_guide() -> Guide {
    Guide {
        name: "alpha".into(),
        category: "test".into(),
        description: "Alpha rendering protocols".into(),
        contexts: vec!["alpha pixels".into()],
        learnings: vec!["alpha compositing".into()],
        usage_count: 0,
        last_used: None,
        success_count: 0,
        failure_count: 0,
        anti_patterns: vec![],
        pitfalls: vec![],
        depends_on: vec![],
        enables: vec![],
        source_memories: vec![],
        validated_by: vec![],
        superseded_by: None,
        deprecated: false,
        entity_revision: ltmrs_domain::id::EntityRevision::new(1),
        created_at: ltmrs_domain::memory::Instant::new(0),
        updated_at: ltmrs_domain::memory::Instant::new(0),
    }
}

fn dense_beta_guide() -> Guide {
    let mut g = dense_alpha_guide();
    g.name = "beta".into();
    g.description = "Beta estimation protocols".into();
    g.contexts = vec!["covariance matrices".into()];
    g.learnings = vec!["kalman gain tuning".into()];
    g
}

async fn dense_test_backend(
    fail_passages: bool,
) -> (
    tempfile::TempDir,
    tempfile::TempDir,
    ltmrs_search::search::backend::SearchBackend,
) {
    let store_dir = tempfile::tempdir().unwrap();
    let repo = std::sync::Arc::new(
        ltmrs_service::repository::CanonicalRepository::open(
            store_dir.path().join("store").to_str().unwrap(),
        )
        .unwrap(),
    );
    let lance_dir = tempfile::tempdir().unwrap();
    let table = ltmrs_search::search::table::SearchTable::open(lance_dir.path().to_str().unwrap())
        .await
        .unwrap();
    let backend = ltmrs_search::search::backend::SearchBackend::new(
        repo,
        table,
        std::sync::Arc::new(BiasedGuideEmbedder { fail_passages }),
    );
    (store_dir, lance_dir, backend)
}

/// Dense leg appends the token-missed guide (beta ~= task) while
/// leaving the already-suggested alpha out.
#[tokio::test]
async fn dense_guide_leg_appends_token_missed_guide() {
    let (_s, _l, backend) = dense_test_backend(false).await;
    let guides = vec![dense_alpha_guide(), dense_beta_guide()];
    let seen = std::collections::BTreeSet::from(["alpha".to_string()]);
    let out = tokio::task::spawn_blocking(move || {
        suggest_guides_dense(&backend, "alpha zonkblat", &guides, &seen)
    })
    .await
    .unwrap();
    assert_eq!(out.len(), 1, "only beta is dense-new, got: {out:?}");
    assert_eq!(out[0].guide, "beta");
    assert!(out[0].tracked, "dense additions come from the catalog");
}

/// A blank task proposes nothing dense (no noise vectors).
#[tokio::test]
async fn dense_guide_leg_ignores_blank_task() {
    let (_s, _l, backend) = dense_test_backend(false).await;
    let guides = vec![dense_beta_guide()];
    let seen = std::collections::BTreeSet::new();
    let out =
        tokio::task::spawn_blocking(move || suggest_guides_dense(&backend, "   ", &guides, &seen))
            .await
            .unwrap();
    assert!(out.is_empty(), "blank task must stay token-only");
}

/// Embedding failure degrades to no dense candidates (the caller
/// keeps the token-only suggestions byte-identically).
#[tokio::test]
async fn dense_guide_errors_fall_back_silently() {
    let (_s, _l, backend) = dense_test_backend(true).await;
    let guides = vec![dense_beta_guide()];
    let seen = std::collections::BTreeSet::new();
    let out = tokio::task::spawn_blocking(move || {
        suggest_guides_dense(&backend, "alpha zonkblat", &guides, &seen)
    })
    .await
    .unwrap();
    assert!(out.is_empty(), "failed dense leg must add nothing");
}

/// Build a dispatcher with the biased dense backend attached.
async fn dense_wiring_dispatcher() -> (tempfile::TempDir, tempfile::TempDir, Dispatcher) {
    let dir = tempfile::tempdir().unwrap();
    let clock: std::sync::Arc<dyn ltmrs_domain::clock::Clock + Send + Sync> =
        std::sync::Arc::new(FrozenClock::new(1000));
    let repo = std::sync::Arc::new(
        ltmrs_service::repository::CanonicalRepository::open_with_clock(
            dir.path().to_str().unwrap(),
            std::sync::Arc::clone(&clock),
        )
        .unwrap(),
    );
    repo.issue_namespace(fe(1), ch(1), 1000).unwrap();
    let disp = Dispatcher::new(
        std::sync::Arc::clone(&repo),
        crate::registry::FrontendRegistry::new(),
        std::sync::Arc::clone(&clock),
    );
    let lance_dir = tempfile::tempdir().unwrap();
    let table = ltmrs_search::search::table::SearchTable::open(lance_dir.path().to_str().unwrap())
        .await
        .unwrap();
    let backend = std::sync::Arc::new(ltmrs_search::search::backend::SearchBackend::new(
        std::sync::Arc::clone(&repo),
        table,
        std::sync::Arc::new(BiasedGuideEmbedder {
            fail_passages: false,
        }),
    ));
    let disp = disp.with_search(backend);
    (dir, lance_dir, disp)
}

fn create_alpha_beta(disp: &Dispatcher) {
    for (op, name, desc, ctx, learn) in [
        (
            1u64,
            "alpha",
            "Alpha rendering protocols",
            "alpha pixels",
            "alpha compositing",
        ),
        (
            2u64,
            "beta",
            "Beta estimation protocols",
            "covariance matrices",
            "kalman gain tuning",
        ),
    ] {
        let args = ToolArgs::GuideCreate(GuideCreateArgs {
            guide: name.to_string(),
            category: "test".to_string(),
            description: desc.to_string(),
            contexts: vec![ctx.to_string()],
            learnings: vec![learn.to_string()],
        });
        let env = tool_call(op, args.clone());
        let result = run(disp, &env, &args);
        assert!(!result_is_error(&result), "guide {name} must create");
    }
}

fn suggest_task_text(disp: &Dispatcher) -> String {
    let args = ToolArgs::GuideGet(GuideGetArgs {
        task: Some("alpha zonkblat".to_string()),
        ..Default::default()
    });
    let env = tool_call(9, args.clone());
    let result = run(disp, &env, &args);
    assert!(!result_is_error(&result));
    result_text(&result)
}

/// End to end: token path finds alpha, dense leg appends beta after it.
#[tokio::test]
async fn dense_guide_wiring_appends_after_token() {
    let (_d, _l, disp) = dense_wiring_dispatcher().await;
    // run() bridges onto the runtime (block_on), so the whole flow must
    // execute off the async worker like the recall_browse tests.
    let text = tokio::task::spawn_blocking(move || {
        create_alpha_beta(&disp);
        suggest_task_text(&disp)
    })
    .await
    .unwrap();
    let alpha = text.find("alpha").expect("alpha must be suggested");
    let beta = text.find("beta").expect("beta must be dense-suggested");
    assert!(
        alpha < beta,
        "token match first, dense addition after:\n{text}"
    );
}

/// Without a backend the same catalog stays token-only (beta absent):
/// the dense leg changes nothing when unavailable.
#[tokio::test]
async fn token_only_without_backend() {
    let (disp, _dir) = test_dispatcher();
    create_alpha_beta(&disp);
    let text = suggest_task_text(&disp);
    assert!(text.contains("alpha"), "alpha must be suggested:\n{text}");
    assert!(
        !text.contains("beta"),
        "beta must stay absent without dense:\n{text}"
    );
}
