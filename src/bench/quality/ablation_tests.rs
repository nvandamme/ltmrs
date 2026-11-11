//! Ablation legs, easiest-first (WP-12 §8.2; T-QUALITY-01).
//!
//! Landed (model-free, deterministic, label-clean — each leg is one
//! [`evaluate`](super::evaluate) call on the frozen corpus):
//! - DenseExact: exact brute-force cosine over harness-stored hash vectors.
//! - HybridRrf: FTS lexical + dense exact fused with one-based RRF.
//! - Priority: hybrid fused scores through `native_score` with uniform
//!   priority (the frozen corpus carries no feedback/confidence signals;
//!   uniform priority preserves hybrid order — asserted, not assumed).
//! - Mmr: native-scored candidates diversified with `mmr_select` (frozen
//!   default lambda, empty protected set: no bundle info without labels).
//!
//! Stopping point: Graph is NOT landed. The frozen corpus carries no
//! relation edges, and building edges from evaluation labels
//! (`obsolete_ids`, `Conflict` kind) would leak answers into the leg,
//! while inferring edges from text similarity would invent matching
//! semantics. UpstreamReference stays unlanded behind that stop (its
//! identity — TF-IDF port versus a provisioned upstream service — is an
//! owner decision).

use super::ablation::*;
use super::*;
use std::collections::{HashMap, HashSet};

fn doc(id: &str, project: &str, created_at_millis: u64) -> DocLabel {
    DocLabel {
        id: id.to_string(),
        topic: "t".to_string(),
        project: project.to_string(),
        text: format!("text of {id}"),
        obsolete: false,
        created_at_millis,
    }
}

fn tcase(id: &str, project: &str, top_k: usize) -> QueryCase {
    QueryCase {
        id: id.to_string(),
        kind: CaseKind::Exact,
        topic: "t".to_string(),
        project: project.to_string(),
        split: Split::Dev,
        query: format!("query {id}"),
        relevant: vec![],
        must_include: vec![],
        must_exclude: vec![],
        obsolete_ids: vec![],
        top_k,
        no_answer: false,
        after_millis: None,
        before_millis: None,
    }
}

fn safety_corpus() -> Corpus {
    let raw = std::fs::read_to_string(
        crate::bench::crate_root().join("experiments/quality/safety-cases.json"),
    )
    .unwrap();
    let corpus: Corpus = serde_json::from_str(&raw).unwrap();
    assert_eq!(corpus.version, CORPUS_VERSION);
    assert!(validate_corpus(&corpus).is_empty());
    corpus
}

/// RRF arithmetic is exact on a hand-checkable pair: `a` in both lists at
/// rank 1 scores `2/61`, `b` in one list at rank 2 scores `1/62`.
#[test]
fn rrf_fuse_orders_matches_hand_computation() {
    let fused = rrf_fuse_orders(&["a".to_string(), "b".to_string()], &["a".to_string()]);
    assert_eq!(fused.len(), 2);
    assert_eq!(fused[0].0, "a");
    assert!((fused[0].1 - 2.0 / 61.0).abs() < 1e-12);
    assert_eq!(fused[1].0, "b");
    assert!((fused[1].1 - 1.0 / 62.0).abs() < 1e-12);
}

/// The exact leg is deterministic, enforces scope, and returns the full
/// in-scope pool (truncation to `top_k` happens in the leg closure).
#[test]
fn dense_exact_rank_is_deterministic_scope_clean_and_full_pool() {
    let corpus = Corpus {
        version: CORPUS_VERSION,
        documents: vec![
            doc("d1", "p", 1000),
            doc("d2", "p", 2000),
            doc("d3", "other", 1000),
            doc("d4", "p", 100),
        ],
        cases: vec![],
    };
    let mut c = tcase("c", "p", 1);
    c.after_millis = Some(500);
    let dense = DenseIndex::build(&corpus, ABLATION_EMBED_DIM);
    let first = dense.rank(&corpus, &c);
    let second = dense.rank(&corpus, &c);
    assert_eq!(first, second, "exact cosine must be deterministic");
    assert_eq!(first.len(), 2, "only d1+d2 are in scope");
    assert!(first.contains(&"d1".to_string()) && first.contains(&"d2".to_string()));
}

/// One `evaluate()` call per split on the frozen corpus: 8 dev + 2 heldout
/// cases, zero scope leakage (project + time bounds enforced).
#[test]
fn dense_exact_evaluates_frozen_corpus() {
    let corpus = safety_corpus();
    let dense = DenseIndex::build(&corpus, ABLATION_EMBED_DIM);
    for split in [Split::Dev, Split::Heldout] {
        let report = evaluate(Ablation::DenseExact, &corpus, split, &|case| {
            dense
                .rank(&corpus, case)
                .into_iter()
                .take(case.top_k.max(1))
                .collect()
        });
        let expected = if split == Split::Dev { 8 } else { 2 };
        assert_eq!(report.cases, expected);
        assert_eq!(report.leakage_total, 0, "scope predicate leaked");
    }
}

async fn hybrid_setup(
    corpus: &Corpus,
) -> (
    tempfile::TempDir,
    ltmrs_search::search::table::SearchTable,
    HashMap<String, String>,
    DenseIndex,
) {
    use crate::bench::experiment::search_row_for;
    use ltmrs_search::search::table::SearchTable;
    let dir = tempfile::tempdir().unwrap();
    let table = SearchTable::open(dir.path().join("search").to_str().unwrap())
        .await
        .unwrap();
    let mut id_by_row: HashMap<String, String> = HashMap::new();
    let mut rows = Vec::new();
    for (index, doc) in corpus.documents.iter().enumerate() {
        let mut row = search_row_for(index as u64, &doc.text);
        row.project = Some(doc.project.clone());
        row.created_at_millis = doc.created_at_millis;
        id_by_row.insert(row.memory_id.as_uuid().to_string(), doc.id.clone());
        rows.push(row);
    }
    table.publish_rows(&rows).await.unwrap();
    table.create_fts_index().await.unwrap();
    let dense = DenseIndex::build(corpus, ABLATION_EMBED_DIM);
    (dir, table, id_by_row, dense)
}

async fn lexical_rank(
    table: &ltmrs_search::search::table::SearchTable,
    id_by_row: &HashMap<String, String>,
    case: &QueryCase,
) -> Vec<String> {
    use ltmrs_domain::command::Scope;
    use ltmrs_search::retrieval::scope::EffectiveScope;
    let scope = Scope {
        project: Some(case.project.clone()),
        after: case.after_millis,
        before: case.before_millis,
        ..Default::default()
    };
    let filter = EffectiveScope::resolve(&scope).unwrap().to_lance_filter();
    let hits = table
        .fts_query(&case.query, 50, filter.as_deref())
        .await
        .unwrap();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for row in &hits {
        let id = id_by_row[&row.memory_id.as_uuid().to_string()].clone();
        if seen.insert(id.clone()) {
            out.push(id);
        }
    }
    out
}

fn truncate_top_k(mut ranked: Vec<String>, case: &QueryCase) -> Vec<String> {
    ranked.truncate(case.top_k.max(1));
    ranked
}

/// One `evaluate()` call per split: FTS lexical + dense exact fused with
/// RRF at the same budgets, zero scope leakage.
#[tokio::test]
async fn hybrid_rrf_evaluates_frozen_corpus() {
    let corpus = safety_corpus();
    let (_dir, table, id_by_row, dense) = hybrid_setup(&corpus).await;
    let mut fused: HashMap<String, Vec<String>> = HashMap::new();
    for case in &corpus.cases {
        let lexical = lexical_rank(&table, &id_by_row, case).await;
        let dense_rank = dense.rank(&corpus, case);
        fused.insert(
            case.id.clone(),
            truncate_top_k(
                rrf_fuse_orders(&lexical, &dense_rank)
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect(),
                case,
            ),
        );
    }
    for split in [Split::Dev, Split::Heldout] {
        let report = evaluate(Ablation::HybridRrf, &corpus, split, &|case| {
            fused[&case.id].clone()
        });
        let expected = if split == Split::Dev { 8 } else { 2 };
        assert_eq!(report.cases, expected);
        assert_eq!(report.leakage_total, 0, "fused legs leaked scope");
    }
}

/// Uniform priority is order-preserving: the Priority leg must rank every
/// frozen case exactly like HybridRrf, and its report must match on all
/// aggregate metrics. A future per-doc priority signal changes this test.
#[tokio::test]
async fn priority_without_signals_preserves_hybrid_order() {
    let corpus = safety_corpus();
    let (_dir, table, id_by_row, dense) = hybrid_setup(&corpus).await;
    let mut hybrid_ranks: HashMap<String, Vec<String>> = HashMap::new();
    let mut priority_ranks: HashMap<String, Vec<String>> = HashMap::new();
    for case in &corpus.cases {
        let lexical = lexical_rank(&table, &id_by_row, case).await;
        let dense_rank = dense.rank(&corpus, case);
        hybrid_ranks.insert(
            case.id.clone(),
            truncate_top_k(
                rrf_fuse_orders(&lexical, &dense_rank)
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect(),
                case,
            ),
        );
        priority_ranks.insert(
            case.id.clone(),
            truncate_top_k(
                native_scored_order(&lexical, &dense_rank)
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect(),
                case,
            ),
        );
        assert_eq!(
            hybrid_ranks[&case.id], priority_ranks[&case.id],
            "uniform priority must preserve hybrid order for {}",
            case.id
        );
    }
    let hybrid_report = evaluate(Ablation::HybridRrf, &corpus, Split::Dev, &|case| {
        hybrid_ranks[&case.id].clone()
    });
    let priority_report = evaluate(Ablation::Priority, &corpus, Split::Dev, &|case| {
        priority_ranks[&case.id].clone()
    });
    assert_eq!(priority_report.cases, hybrid_report.cases);
    assert_eq!(priority_report.recall_at_k, hybrid_report.recall_at_k);
    assert_eq!(priority_report.mrr, hybrid_report.mrr);
    assert_eq!(priority_report.ndcg_at_k, hybrid_report.ndcg_at_k);
    assert_eq!(priority_report.leakage_total, hybrid_report.leakage_total);
}

/// MMR is deterministic, stays within the case budget, only selects fused
/// candidates, and keeps the frozen corpus leakage-free on both splits.
#[tokio::test]
async fn mmr_selects_within_budget_deterministically() {
    let corpus = safety_corpus();
    let (_dir, table, id_by_row, dense) = hybrid_setup(&corpus).await;
    let mut mmr_ranks: HashMap<String, Vec<String>> = HashMap::new();
    for case in &corpus.cases {
        let lexical = lexical_rank(&table, &id_by_row, case).await;
        let scored = native_scored_order(&lexical, &dense.rank(&corpus, case));
        let union: HashSet<String> = scored.iter().map(|(id, _)| id.clone()).collect();
        let first = mmr_order(&scored, dense.vectors(), case.top_k);
        let second = mmr_order(&scored, dense.vectors(), case.top_k);
        assert_eq!(first, second, "mmr must be deterministic");
        assert!(
            first.len() <= case.top_k.max(1),
            "mmr exceeded the case budget"
        );
        assert!(
            first.iter().all(|id| union.contains(id)),
            "mmr selected outside the fused pool"
        );
        mmr_ranks.insert(case.id.clone(), first);
    }
    for split in [Split::Dev, Split::Heldout] {
        let report = evaluate(Ablation::Mmr, &corpus, split, &|case| {
            mmr_ranks[&case.id].clone()
        });
        let expected = if split == Split::Dev { 8 } else { 2 };
        assert_eq!(report.cases, expected);
        assert_eq!(report.leakage_total, 0, "mmr leg leaked scope");
    }
}
