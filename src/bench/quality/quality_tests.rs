//! Quality-harness unit tests (moved verbatim from `quality.rs`).

use super::*;

fn doc(id: &str, topic: &str, project: &str) -> DocLabel {
    DocLabel {
        id: id.to_string(),
        topic: topic.to_string(),
        project: project.to_string(),
        text: format!("text of {id}"),
        obsolete: false,
        created_at_millis: 1000,
    }
}

fn case(id: &str, kind: CaseKind, topic: &str, project: &str, split: Split) -> QueryCase {
    QueryCase {
        id: id.to_string(),
        kind,
        topic: topic.to_string(),
        project: project.to_string(),
        split,
        query: format!("query {id}"),
        relevant: vec![],
        must_include: vec![],
        must_exclude: vec![],
        obsolete_ids: vec![],
        top_k: 5,
        no_answer: false,
        after_millis: None,
        before_millis: None,
    }
}

/// Split integrity is structural: shared topics or projects across
/// splits fail validation, as do dangling id references.
#[test]
fn validate_corpus_catches_leakage_and_dangling_refs() {
    let mut corpus = Corpus {
        version: CORPUS_VERSION,
        documents: vec![doc("d1", "garden", "home"), doc("d2", "harbor", "work")],
        cases: vec![
            {
                let mut c = case("c1", CaseKind::Exact, "garden", "home", Split::Dev);
                c.relevant = vec!["d1".to_string()];
                c.must_include = vec!["d1".to_string()];
                c
            },
            {
                let mut c = case("c2", CaseKind::Exact, "harbor", "work", Split::Heldout);
                c.relevant = vec!["d2".to_string()];
                c
            },
        ],
    };
    assert!(
        validate_corpus(&corpus).is_empty(),
        "clean corpus validates"
    );
    // Topic shared across splits.
    corpus.cases[1].topic = "garden".to_string();
    let errors = validate_corpus(&corpus);
    assert!(
        errors.iter().any(|e| e.contains("garden")),
        "topic leak must be named, got: {errors:?}"
    );
    corpus.cases[1].topic = "harbor".to_string();
    // Dangling reference.
    corpus.cases[0].relevant = vec!["ghost".to_string()];
    let errors = validate_corpus(&corpus);
    assert!(
        errors.iter().any(|e| e.contains("ghost")),
        "dangling id must be named, got: {errors:?}"
    );
}

/// Metrics are exact on a hand-checkable ranking.
#[test]
fn score_case_matches_hand_computation() {
    let mut c = case("c", CaseKind::Exact, "t", "p", Split::Dev);
    c.relevant = vec!["a".to_string(), "b".to_string()];
    c.must_include = vec!["a".to_string()];
    c.must_exclude = vec!["x".to_string()];
    // Ranked: [x, a, c]: x leaks, a hits at rank 2, b missed.
    let m = score_case(&c, &["x".to_string(), "a".to_string(), "c".to_string()]);
    assert!((m.recall_at_k - 0.5).abs() < 1e-9, "1 of 2 relevant");
    assert!(
        (m.reciprocal_rank - 0.5).abs() < 1e-9,
        "first hit at rank 2"
    );
    // DCG = 0/log2(2) + 1/log2(3) ; IDCG = 1/log2(2) + 1/log2(3).
    let idcg = 1.0 + 1.0 / 3.0f64.log2();
    let dcg = 1.0 / 3.0f64.log2();
    assert!(
        (m.ndcg_at_k - dcg / idcg).abs() < 1e-9,
        "ndcg={}",
        m.ndcg_at_k
    );
    assert!(m.must_include_covered, "a present");
    assert_eq!(m.leakage_count, 1, "x must not appear");
    assert!(!m.no_answer_fp && !m.obsolete_hit);
}

/// No-answer false positives and obsolete hits are counted, not averaged away.
#[test]
fn score_case_counts_safety_outcomes() {
    let mut c = case("c", CaseKind::NoAnswer, "t", "p", Split::Dev);
    c.no_answer = true;
    let m = score_case(&c, &["a".to_string()]);
    assert!(
        m.no_answer_fp,
        "any hit on a no-answer case is a false positive"
    );
    let m = score_case(&c, &[]);
    assert!(!m.no_answer_fp, "empty is the correct answer");
    let mut c = case("c2", CaseKind::Obsolete, "t", "p", Split::Dev);
    c.obsolete_ids = vec!["old".to_string()];
    c.must_include = vec!["new".to_string()];
    let m = score_case(&c, &["new".to_string(), "old".to_string()]);
    assert!(m.obsolete_hit, "superseded advice presented");
    assert!(m.must_include_covered, "current advice present too");
    assert_eq!(m.leakage_count, 0, "obsolete is not scope leakage");
}

/// The runner aggregates per split with sample sizes attached.
#[test]
fn evaluate_aggregates_with_sample_sizes() {
    let corpus = Corpus {
        version: CORPUS_VERSION,
        documents: vec![doc("a", "t", "p"), doc("b", "t", "p")],
        cases: vec![
            {
                let mut c = case("c1", CaseKind::Exact, "t", "p", Split::Dev);
                c.relevant = vec!["a".to_string()];
                c.must_include = vec!["a".to_string()];
                c
            },
            {
                let mut c = case("c2", CaseKind::NoAnswer, "t", "p", Split::Dev);
                c.no_answer = true;
                c
            },
        ],
    };
    let retrieve = |case: &QueryCase| match case.id.as_str() {
        "c1" => vec!["a".to_string()],
        _ => vec!["zzz".to_string()],
    };
    let report = evaluate(Ablation::Lexical, &corpus, Split::Dev, &retrieve);
    assert_eq!(report.ablation, Ablation::Lexical);
    assert_eq!(report.cases, 2);
    assert_eq!(report.no_answer_n, 1);
    assert!(
        (report.no_answer_fp_rate - 1.0).abs() < 1e-9,
        "c2 hit on no-answer"
    );
    assert!((report.mrr - 0.5).abs() < 1e-9, "mean of 1.0 and 0.0");
    assert_eq!(report.conflict_n, 0);
}
