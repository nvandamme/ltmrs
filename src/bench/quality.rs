//! Retrieval quality harness (WP-12 tasks 6-9; RQ-24, T-QUALITY-01/02).
//!
//! Boundary rules (plan §8):
//! - Labels are frozen data: development and held-out splits share no
//!   topics or projects (leakage is a structural property, checked by
//!   [`validate_corpus`], not by reviewer discipline).
//! - Metrics are pure functions over (labels, ranked ids). Conventional IR
//!   scores (recall, MRR, nDCG) sit alongside the safety measures the plan
//!   demands: no-answer false positives, obsolete-advice rate, conflict
//!   coverage and cross-scope leakage.
//! - Zero-tolerance invariants (scope leakage, obsolete advice presented as
//!   current) are fixture failures regardless of aggregate scores.
//! - The ablation runner takes a retrieval closure, so every leg (lexical,
//!   dense, hybrid, graph, MMR, upstream reference) runs the same cases at
//!   the same budgets. Legs without an implementation stay `not_run` with a
//!   named reason — never zero-filled.
//! - Calibration uses the development split only; held-out cases are
//!   evaluated, never tuned on.

use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
mod quality_fixture_tests;
#[cfg(test)]
mod quality_tests;

/// Corpus version (bumped when labels change; reports pin it).
pub const CORPUS_VERSION: u32 = 1;

/// Evaluation split. Held-out cases are evaluated, never tuned on.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Split {
    Dev,
    Heldout,
}

/// Case kind (determines which safety measures apply).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseKind {
    Exact,
    Paraphrase,
    CrossLanguage,
    NoAnswer,
    Obsolete,
    Conflict,
    ScopeTrap,
    TimeFilter,
}

/// One labeled document in the corpus pool.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DocLabel {
    pub id: String,
    pub topic: String,
    pub project: String,
    pub text: String,
    pub obsolete: bool,
    pub created_at_millis: u64,
}

/// One labeled query case.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QueryCase {
    pub id: String,
    pub kind: CaseKind,
    pub topic: String,
    /// Query scope (project the caller may see).
    pub project: String,
    pub split: Split,
    pub query: String,
    /// Ids that answer the query.
    pub relevant: Vec<String>,
    /// Ids that must appear in the top-k (companions, current advice).
    pub must_include: Vec<String>,
    /// Ids that must never appear (other scopes).
    pub must_exclude: Vec<String>,
    /// Superseded advice: presenting it as current is a fixture failure
    /// (distinct from scope leakage).
    pub obsolete_ids: Vec<String>,
    pub top_k: usize,
    pub no_answer: bool,
    /// Declared time bounds (millis since epoch, inclusive): runners apply
    /// them through the canonical scope, never by hand-rolled predicates.
    pub after_millis: Option<u64>,
    pub before_millis: Option<u64>,
}

/// Frozen labeled corpus.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Corpus {
    pub version: u32,
    pub documents: Vec<DocLabel>,
    pub cases: Vec<QueryCase>,
}

/// Structural validation: split integrity (no shared topics/projects across
/// splits) plus reference integrity (every cited id exists). Returns the
/// list of violations (empty = valid).
pub fn validate_corpus(corpus: &Corpus) -> Vec<String> {
    let mut errors = Vec::new();
    if corpus.version != CORPUS_VERSION {
        errors.push(format!(
            "corpus version {} does not match harness {CORPUS_VERSION}",
            corpus.version
        ));
    }
    let known: BTreeSet<&str> = corpus.documents.iter().map(|d| d.id.as_str()).collect();
    let mut seen_docs = BTreeSet::new();
    for doc in &corpus.documents {
        if !seen_docs.insert(doc.id.as_str()) {
            errors.push(format!("duplicate document id {}", doc.id));
        }
    }
    let mut seen_cases = BTreeSet::new();
    for case in &corpus.cases {
        for id in case
            .relevant
            .iter()
            .chain(&case.must_include)
            .chain(&case.must_exclude)
            .chain(&case.obsolete_ids)
        {
            if !known.contains(id.as_str()) {
                errors.push(format!("case {} cites unknown id {id}", case.id));
            }
        }
    }
    let mut topics: BTreeMap<&str, BTreeSet<Split>> = BTreeMap::new();
    let mut projects: BTreeMap<&str, BTreeSet<Split>> = BTreeMap::new();
    if corpus.cases.is_empty() {
        errors.push("corpus has no cases".to_string());
    }
    for case in &corpus.cases {
        if !seen_cases.insert(case.id.as_str()) {
            errors.push(format!("duplicate case id {}", case.id));
        }
        if case.top_k == 0 {
            errors.push(format!("case {} has top_k 0", case.id));
        }
        topics
            .entry(case.topic.as_str())
            .or_default()
            .insert(case.split);
        projects
            .entry(case.project.as_str())
            .or_default()
            .insert(case.split);
    }
    for (topic, splits) in &topics {
        if splits.len() > 1 {
            errors.push(format!("topic {topic} appears in both splits"));
        }
    }
    for (project, splits) in &projects {
        if splits.len() > 1 {
            errors.push(format!("project {project} appears in both splits"));
        }
    }
    errors
}

/// Per-case metric outcomes.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CaseMetrics {
    pub recall_at_k: f64,
    pub reciprocal_rank: f64,
    pub ndcg_at_k: f64,
    pub no_answer_fp: bool,
    pub obsolete_hit: bool,
    pub must_include_covered: bool,
    pub leakage_count: usize,
}

/// Score one ranked list against its labels (pure).
pub fn score_case(case: &QueryCase, ranked: &[String]) -> CaseMetrics {
    let k = case.top_k.max(1);
    let top: &[String] = &ranked[..ranked.len().min(k)];
    let relevant: BTreeSet<&str> = case.relevant.iter().map(|s| s.as_str()).collect();
    let hits = top
        .iter()
        .filter(|id| relevant.contains(id.as_str()))
        .count();
    let recall_at_k = if relevant.is_empty() {
        1.0
    } else {
        hits as f64 / relevant.len() as f64
    };
    let reciprocal_rank = top
        .iter()
        .position(|id| relevant.contains(id.as_str()))
        .map(|pos| 1.0 / (pos as f64 + 1.0))
        .unwrap_or(0.0);
    let dcg: f64 = top
        .iter()
        .enumerate()
        .map(|(pos, id)| {
            if relevant.contains(id.as_str()) {
                1.0 / ((pos as f64 + 2.0).log2())
            } else {
                0.0
            }
        })
        .sum();
    let ideal_hits = relevant.len().min(k);
    let idcg: f64 = (0..ideal_hits)
        .map(|pos| 1.0 / ((pos as f64 + 2.0).log2()))
        .sum();
    let ndcg_at_k = if idcg == 0.0 { 1.0 } else { dcg / idcg };
    let top_set: BTreeSet<&str> = top.iter().map(|s| s.as_str()).collect();
    CaseMetrics {
        recall_at_k,
        reciprocal_rank,
        ndcg_at_k,
        no_answer_fp: case.no_answer && !ranked.is_empty(),
        obsolete_hit: case
            .obsolete_ids
            .iter()
            .any(|id| top_set.contains(id.as_str())),
        must_include_covered: case
            .must_include
            .iter()
            .all(|id| top_set.contains(id.as_str())),
        leakage_count: case
            .must_exclude
            .iter()
            .filter(|id| top_set.contains(id.as_str()))
            .count(),
    }
}

/// Retrieval leg under ablation (plan §8.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ablation {
    Lexical,
    DenseExact,
    HybridRrf,
    Priority,
    Graph,
    Mmr,
    UpstreamReference,
}

/// Aggregated ablation outcome (per split).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AblationReport {
    pub ablation: Ablation,
    pub split: Split,
    pub cases: usize,
    pub recall_at_k: f64,
    pub mrr: f64,
    pub ndcg_at_k: f64,
    pub no_answer_fp_rate: f64,
    pub no_answer_n: usize,
    pub obsolete_rate: f64,
    pub obsolete_n: usize,
    pub conflict_coverage: f64,
    pub conflict_n: usize,
    pub leakage_total: usize,
}

/// Evaluate one retrieval leg over a corpus split at the cases' fixed
/// budgets. `retrieve` maps a case to ranked document ids.
pub fn evaluate(
    ablation: Ablation,
    corpus: &Corpus,
    split: Split,
    retrieve: &dyn Fn(&QueryCase) -> Vec<String>,
) -> AblationReport {
    let cases: Vec<&QueryCase> = corpus.cases.iter().filter(|c| c.split == split).collect();
    let mut recall = 0.0;
    let mut mrr = 0.0;
    let mut ndcg = 0.0;
    let mut fp = 0usize;
    let mut no_answer_n = 0usize;
    let mut obsolete_hits = 0usize;
    let mut obsolete_n = 0usize;
    let mut conflict_covered = 0usize;
    let mut conflict_n = 0usize;
    let mut leakage_total = 0usize;
    for case in &cases {
        let metrics = score_case(case, &retrieve(case));
        recall += metrics.recall_at_k;
        mrr += metrics.reciprocal_rank;
        ndcg += metrics.ndcg_at_k;
        if case.no_answer {
            no_answer_n += 1;
            fp += usize::from(metrics.no_answer_fp);
        }
        if !case.obsolete_ids.is_empty() {
            obsolete_n += 1;
            obsolete_hits += usize::from(metrics.obsolete_hit);
        }
        if case.kind == CaseKind::Conflict {
            conflict_n += 1;
            conflict_covered += usize::from(metrics.must_include_covered);
        }
        leakage_total += metrics.leakage_count;
    }
    let n = cases.len().max(1) as f64;
    AblationReport {
        ablation,
        split,
        cases: cases.len(),
        recall_at_k: recall / n,
        mrr: mrr / n,
        ndcg_at_k: ndcg / n,
        no_answer_fp_rate: fp as f64 / no_answer_n.max(1) as f64,
        no_answer_n,
        obsolete_rate: obsolete_hits as f64 / obsolete_n.max(1) as f64,
        obsolete_n,
        conflict_coverage: conflict_covered as f64 / conflict_n.max(1) as f64,
        conflict_n,
        leakage_total,
    }
}
