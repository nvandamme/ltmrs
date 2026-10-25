//! Recall-leg tests (moved verbatim from `engine.rs`).

use super::test_support::*;
use super::{Engine, RetrievalRequest};
use crate::search::projector::{FixedEmbedder, Projector};
use ltmrs_domain::command::{DomainCommand, DomainErrorCode, Scope};
use ltmrs_domain::id::EntityId;
use ltmrs_domain::id::{ModelFingerprint, StoreGeneration};

/// T-RANK-04: a nonempty query with zero overlap must return a valid
/// no-answer, not nearest-neighbor noise presented as truth.
#[tokio::test]
async fn no_answer_when_nothing_matches() {
    let (repo, table, mut proj, _guard) = env().await;
    add(&repo, 1, "rust async", "tokio runtime details", None);
    proj.run_until_idle().await.unwrap();

    let req = RetrievalRequest {
        query: "zzqqxx completely unrelated terms".into(),
        min_similarity: Some(1.0),
        ..base_req()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    assert!(result.results.is_empty());
    assert!(result.explanation.no_match);
}

/// A genuine complete no-answer: all legs ready, unrelated query —
/// empty, flagged no-match, and NOT partial (partial is reserved for
/// degraded legs, never a hedge on a true no-answer).
#[tokio::test]
async fn complete_no_answer_when_all_legs_ready() {
    let (repo, table, mut proj, _guard) = env().await;
    add(&repo, 1, "rust async", "tokio runtime details", None);
    proj.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();

    let req = RetrievalRequest {
        query: "zzqqxx completely unrelated terms".into(),
        // Zero-overlap pin (T-RANK-04): hash-vector noise must not
        // turn a true no-answer into spurious hits.
        min_similarity: Some(1.0),
        ..base_req()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    assert!(result.results.is_empty());
    assert!(result.explanation.no_match);
    assert!(result.explanation.fts_ready);
    assert!(
        !result.explanation.partial,
        "ready legs with no hits is complete, not partial"
    );
}

/// A pinned retired generation fails loudly: the store keeps no
/// versioned canonical rows, so serving current data under an old pin
/// would lie. Unpinned requests (and pins matching live) work normally.
#[tokio::test]
async fn pinned_retired_generation_fails_loudly() {
    let (repo, table, mut proj, _guard) = env().await;
    add(&repo, 1, "gen one", "first generation body", None);
    proj.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();
    repo.set_store_generation(ltmrs_domain::id::StoreGeneration::new(2))
        .unwrap();

    let stale = RetrievalRequest {
        query: "generation".into(),
        store_generation: Some(ltmrs_domain::id::StoreGeneration::FIRST),
        ..base_req()
    };
    let err = Engine::new(repo.clone(), table.clone())
        .retrieve(&stale, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::StaleGeneration
    );

    let live = RetrievalRequest {
        query: "generation".into(),
        store_generation: Some(ltmrs_domain::id::StoreGeneration::new(2)),
        ..base_req()
    };
    assert!(
        Engine::new(repo, table)
            .retrieve(&live, &TestQueryEmbedder { prefix: "" })
            .await
            .is_ok()
    );
}

/// The retired-pin gate precedes all routing: direct-ID and list reads
/// under a retired pin fail loudly too, never serve current data.
#[tokio::test]
async fn non_ranked_paths_with_retired_pin_fail_loudly() {
    let (repo, table, mut proj, _guard) = env().await;
    add(&repo, 1, "gen one", "first generation body", None);
    proj.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();
    repo.set_store_generation(ltmrs_domain::id::StoreGeneration::new(2))
        .unwrap();

    let stale_pin = Some(ltmrs_domain::id::StoreGeneration::FIRST);
    let direct = RetrievalRequest {
        query: "generation".into(),
        direct_ids: vec![eid(1)],
        store_generation: stale_pin,
        ..base_req()
    };
    let err = Engine::new(repo.clone(), table.clone())
        .retrieve(&direct, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::StaleGeneration
    );
    let list = RetrievalRequest {
        query: "   ".into(),
        store_generation: stale_pin,
        ..base_req()
    };
    let err = Engine::new(repo, table)
        .retrieve(&list, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap_err();
    assert_eq!(
        err.code,
        ltmrs_domain::command::DomainErrorCode::StaleGeneration
    );
}

/// Cutover readers: a request without a pinned generation reads the
/// active pointer per call; an explicit generation stays pinned
/// (rollback reads). Uses the ranked path: list mode reads canonical
/// state directly and is generation-agnostic by design.
#[tokio::test]
async fn none_generation_resolves_active_pointer() {
    let (repo, table, _proj, _guard) = env().await;
    add(&repo, 1, "gen two", "second generation body", None);
    // Build generation 2 alongside generation 1, before activation.
    let gen2 = repo.stage_generation(ModelFingerprint::new(2)).unwrap();
    let mut proj2 = Projector::new(
        repo.clone(),
        table.clone(),
        Box::new(FixedEmbedder { dim: 384 }),
        ModelFingerprint::new(2),
        gen2,
    );
    proj2.run_until_idle().await.unwrap();
    repo.note_generation_progress(gen2, 1).unwrap();
    table.create_fts_index().await.unwrap();

    let req = RetrievalRequest {
        query: "generation".into(),
        ..Default::default()
    };
    assert!(req.store_generation.is_none());
    // Pre-activation: the converging build is invisible to default readers.
    let pre = Engine::new(repo.clone(), table.clone())
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    assert!(
        pre.results.is_empty(),
        "unactivated build must stay invisible"
    );

    repo.activate_generation(gen2).unwrap();
    // Unpinned request follows the active pointer to the gen-2 row.
    let post = Engine::new(repo.clone(), table.clone())
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    assert_eq!(post.results.len(), 1, "active pointer must resolve");

    // Explicit pin to the old generation fails loudly (no versioned
    // canonical rows exist to serve it): stability through refusal,
    // not through silently current data.
    let pinned = RetrievalRequest {
        query: "generation".into(),
        store_generation: Some(StoreGeneration::FIRST),
        ..Default::default()
    };
    let err = Engine::new(repo, table)
        .retrieve(&pinned, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap_err();
    assert_eq!(err.code, DomainErrorCode::StaleGeneration);
}

/// T-SEARCH-02: exact technical identifiers (flags, paths, underscores)
/// survive the lexical leg and return current canonical IDs + spans.
#[tokio::test]
async fn exact_identifiers_survive_lexical_leg() {
    let (repo, table, mut proj, _guard) = env().await;
    add(
        &repo,
        1,
        "flag note",
        "use --no-pager with git commands in /home/user/Work space",
        None,
    );
    proj.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();

    let req = RetrievalRequest {
        query: "--no-pager".into(),
        model_fingerprint: None,
        ..Default::default()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    assert_eq!(result.results.len(), 1);
    assert_eq!(result.results[0].memory.id, eid(1));
    assert!(result.results[0].matched_span.is_some());
}

/// T-SCOPE-01: out-of-scope memories (and their graph traps) are never
/// returned or traversed, on any leg.
#[tokio::test]
async fn scope_applies_to_all_legs_and_graph() {
    let (repo, table, mut proj, _guard) = env().await;
    add(&repo, 1, "in scope", "alpha beta gamma", Some("app"));
    add(
        &repo,
        2,
        "out of scope trap",
        "alpha beta gamma",
        Some("other"),
    );
    proj.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();

    repo.apply(
        &ctx(10),
        &DomainCommand::Relate {
            relation: ltmrs_domain::relation::Relation::new(
                eid(100),
                eid(1),
                eid(2),
                ltmrs_domain::relation::RelationType::RelatedTo,
                None,
                ltmrs_domain::memory::Instant::new(1),
            ),
        },
    )
    .unwrap();

    let req = RetrievalRequest {
        query: "alpha beta gamma".into(),
        scope: Scope {
            project: Some("app".into()),
            ..Default::default()
        },
        ..base_req()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    let ids: Vec<EntityId> = result.results.iter().map(|r| r.memory.id).collect();
    assert!(ids.contains(&eid(1)));
    assert!(
        !ids.contains(&eid(2)),
        "out-of-scope graph trap must not be traversed or returned"
    );
}

/// T-RANK-03: a superseded memory is excluded from the primary answer and
/// the current one is protected; lineage is preserved in the explanation.
#[tokio::test]
async fn superseded_memory_excluded_current_protected() {
    let (repo, table, mut proj, _guard) = env().await;
    add(
        &repo,
        1,
        "old advice",
        "use feature flag A for rollout",
        None,
    );
    add(
        &repo,
        2,
        "new advice",
        "use feature flag B for rollout",
        None,
    );
    proj.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();

    // 2 supersedes 1.
    repo.apply(
        &ctx(10),
        &DomainCommand::Relate {
            relation: ltmrs_domain::relation::Relation::new(
                eid(100),
                eid(2),
                eid(1),
                ltmrs_domain::relation::RelationType::Supersedes,
                None,
                ltmrs_domain::memory::Instant::new(1),
            ),
        },
    )
    .unwrap();

    let req = RetrievalRequest {
        query: "feature flag rollout".into(),
        ..base_req()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();

    let ids: Vec<EntityId> = result.results.iter().map(|r| r.memory.id).collect();
    assert!(
        ids.contains(&eid(2)),
        "the current memory must be in the primary answer"
    );
    assert!(
        !ids.contains(&eid(1)),
        "obsolete advice must be excluded from the primary answer"
    );
    // Lineage is preserved in the explanation.
    let exp = &result.explanation;
    assert!(
        exp.candidates
            .values()
            .any(|c| c.protected && c.id == eid(2))
    );
}

/// T-RANK-03: when ONLY the stale memory matches the query (the current
/// record uses different wording), the stale advice must still be excluded
/// and the current record redirected in — the chain is resolvable from the
/// full relation graph, not just among recalled candidates.
#[tokio::test]
async fn stale_only_recall_redirects_to_current() {
    let (repo, table, mut proj, _guard) = env().await;
    add(
        &repo,
        1,
        "old advice",
        "use feature flag A for rollout",
        None,
    );
    add(&repo, 2, "new advice", "use dark mode by default", None);
    proj.run_until_idle().await.unwrap();
    table.create_fts_index().await.unwrap();

    // 2 supersedes 1.
    repo.apply(
        &ctx(10),
        &DomainCommand::Relate {
            relation: ltmrs_domain::relation::Relation::new(
                eid(100),
                eid(2),
                eid(1),
                ltmrs_domain::relation::RelationType::Supersedes,
                None,
                ltmrs_domain::memory::Instant::new(1),
            ),
        },
    )
    .unwrap();

    // Query matches ONLY the stale memory's text.
    let req = RetrievalRequest {
        query: "feature flag A rollout".into(),
        ..base_req()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();

    let ids: Vec<EntityId> = result.results.iter().map(|r| r.memory.id).collect();
    assert!(
        !ids.contains(&eid(1)),
        "stale advice recalled alone must still be excluded"
    );
    assert!(
        ids.contains(&eid(2)),
        "the current record must be redirected into the primary answer"
    );
}

/// T-RANK-03 / T-GRAPH-02: an unresolved contradiction bundle preserves
/// both sides (retrieval-side graph semantics).
#[tokio::test]
async fn conflict_bundle_preserves_both_sides() {
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
        ..base_req()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    let ids: Vec<EntityId> = result.results.iter().map(|r| r.memory.id).collect();
    assert!(
        ids.contains(&eid(1)) && ids.contains(&eid(2)),
        "both sides of a contradiction must survive MMR"
    );
}

/// Direct-ID routing bypasses the ranker; scope still enforced.
#[tokio::test]
async fn direct_ids_bypass_ranker_scope_enforced() {
    let (repo, table, _proj, _guard) = env().await;
    add(&repo, 1, "a", "alpha", Some("app"));
    add(&repo, 2, "b", "beta", Some("other"));

    let req = RetrievalRequest {
        direct_ids: vec![eid(1), eid(2)],
        scope: Scope {
            project: Some("app".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    let ids: Vec<EntityId> = result.results.iter().map(|r| r.memory.id).collect();
    assert_eq!(ids, vec![eid(1)]);
}

/// Empty-query routing: list/priority behavior, no dense recall.
#[tokio::test]
async fn empty_query_is_list_mode() {
    let (repo, table, _proj, _guard) = env().await;
    add(&repo, 1, "a", "alpha", None);
    add(&repo, 2, "b", "beta", None);

    let req = RetrievalRequest {
        query: "   ".into(),
        ..Default::default()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    assert!(result.explanation.empty_query);
    assert_eq!(result.results.len(), 2);
}

/// List reads bypass the legs but not the lag rule: with a pending
/// projection the listing may be incomplete, so it reports partial
/// like every other path (never a false-complete listing).
#[tokio::test]
async fn list_with_pending_projection_is_partial() {
    let (repo, table, _proj, _guard) = env().await;
    add(&repo, 1, "a", "alpha", None);

    let req = RetrievalRequest {
        query: "   ".into(),
        ..Default::default()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    assert!(result.explanation.empty_query);
    assert!(result.explanation.partial, "pending job must flag partial");
    assert!(result.explanation.projection_lag > 0);
}

/// Direct reads bypass the ranker but not the lag rule: with a pending
/// projection the direct hit may be stale, so it reports partial like
/// every other path.
#[tokio::test]
async fn direct_with_pending_projection_is_partial() {
    let (repo, table, _proj, _guard) = env().await;
    add(&repo, 1, "a", "alpha", None);

    let req = RetrievalRequest {
        query: "a".into(),
        direct_ids: vec![eid(1)],
        ..Default::default()
    };
    let result = Engine::new(repo, table)
        .retrieve(&req, &TestQueryEmbedder { prefix: "" })
        .await
        .unwrap();
    assert_eq!(result.results.len(), 1);
    assert!(result.explanation.partial, "pending job must flag partial");
    assert!(result.explanation.projection_lag > 0);
}

/// Semantic recall: a query identical to a document's text retrieves it.
#[tokio::test]
async fn semantic_recall_finds_identical_text() {
    let (repo, table, mut proj, _guard) = env().await;
    add(
        &repo,
        1,
        "t1",
        "the quick brown fox jumps over the lazy dog",
        None,
    );
    add(&repo, 2, "t2", "rust borrow checker rules explained", None);
    proj.run_until_idle().await.unwrap();

    // The query embedder uses a prefix that reconstructs the exact
    // rendered document text, so the query vector equals the document
    // vector (mirroring the E5 query/passage prefix asymmetry).
    let req = RetrievalRequest {
        query: "the quick brown fox jumps over the lazy dog".into(),
        model_fingerprint: Some(ModelFingerprint::new(1)),
        ..Default::default()
    };
    let embedder = TestQueryEmbedder { prefix: "t1\n" };
    let result = Engine::new(repo, table)
        .retrieve(&req, &embedder)
        .await
        .unwrap();
    assert!(!result.results.is_empty());
    assert_eq!(result.results[0].memory.id, eid(1));
}
