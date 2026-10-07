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

#[cfg(test)]
mod tests {
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

    /// Safety fixture through the lexical leg: the frozen safety cases seed
    /// a real FTS table; the executable subset (everything except currency
    /// judgment and cross-language matching, which need the engine leg)
    /// must pass with zero leakage and zero no-answer false positives.
    #[tokio::test]
    async fn safety_fixture_passes_lexical_subset() {
        use crate::bench::experiment::search_row_for;
        use ltmrs_search::search::table::SearchTable;
        use std::collections::HashMap;
        let raw = std::fs::read_to_string(
            crate::bench::crate_root().join("experiments/quality/safety-cases.json"),
        )
        .unwrap();
        let corpus: Corpus = serde_json::from_str(&raw).unwrap();
        assert_eq!(corpus.version, CORPUS_VERSION);
        assert!(validate_corpus(&corpus).is_empty());
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
        // Currency judgment needs the engine's relational supersession
        // (engine leg); cross-language matching needs real embedding
        // semantics (real-E5 leg). The lexical leg decides neither.
        let mut executed = 0usize;
        let mut skipped_ids = Vec::new();
        let mut leakage_total = 0usize;
        for case in &corpus.cases {
            if matches!(case.kind, CaseKind::Obsolete | CaseKind::CrossLanguage) {
                skipped_ids.push(case.id.clone());
                continue;
            }
            executed += 1;
            // The canonical scope builds the predicate (same builder the
            // engine uses: project scope plus declared time bounds).
            let scope = ltmrs_domain::command::Scope {
                project: Some(case.project.clone()),
                after: case.after_millis,
                before: case.before_millis,
                ..Default::default()
            };
            let filter = ltmrs_search::retrieval::scope::EffectiveScope::resolve(&scope)
                .unwrap()
                .to_lance_filter();
            let hits = table
                .fts_query(&case.query, case.top_k, filter.as_deref())
                .await
                .unwrap();
            let ranked: Vec<String> = hits
                .iter()
                .map(|row| id_by_row[&row.memory_id.as_uuid().to_string()].clone())
                .collect();
            // Non-vacuous trap: the excluded doc must match unfiltered, so
            // its absence above proves the scope filter, not a dead query.
            if case.kind == CaseKind::ScopeTrap {
                let unfiltered = table
                    .fts_query(&case.query, case.top_k, None)
                    .await
                    .unwrap();
                let unfiltered_ids: Vec<String> = unfiltered
                    .iter()
                    .map(|row| id_by_row[&row.memory_id.as_uuid().to_string()].clone())
                    .collect();
                for excluded in &case.must_exclude {
                    assert!(
                        unfiltered_ids.contains(excluded),
                        "case {}: trap dead, {excluded} matches nothing unfiltered",
                        case.id
                    );
                }
            }
            let metrics = score_case(case, &ranked);
            leakage_total += metrics.leakage_count;
            assert_eq!(metrics.leakage_count, 0, "case {} leaked", case.id);
            if case.no_answer {
                assert!(
                    !metrics.no_answer_fp,
                    "case {} false-positive on no-answer",
                    case.id
                );
            } else {
                assert!(
                    metrics.must_include_covered,
                    "case {} missed mandatory ids (ranked {ranked:?})",
                    case.id
                );
            }
        }
        assert!(
            executed >= 6,
            "subset must stay substantial, ran {executed}"
        );
        assert_eq!(
            skipped_ids,
            vec!["obsolete-g".to_string(), "cross-g1".to_string()]
        );
        assert_eq!(leakage_total, 0);
    }

    /// Deterministic hash embedder with the exact FixedEmbedder derivation
    /// (query/passage spaces align without a model).
    struct HashQueryEmbedder;

    /// Embedding width shared by the hash query embedder and the passage
    /// FixedEmbedder below: a dim change must move both together or
    /// query/passage spaces silently misalign.
    const BENCH_EMBED_DIM: usize = 384;

    impl HashQueryEmbedder {
        fn hash_vec(text: &str) -> Vec<f32> {
            ltmrs_search::search::projector::hash_embed_vec(text, BENCH_EMBED_DIM)
        }
    }

    impl ltmrs_search::retrieval::engine::QueryEmbedder for HashQueryEmbedder {
        fn embed_query<'a>(
            &'a self,
            query: &'a str,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = ltmrs_domain::command::DomainResult<Vec<f32>>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move { Ok(Self::hash_vec(query)) })
        }
    }

    /// Safety fixture through the full engine (fusion + scope + currency):
    /// the same frozen cases run end-to-end with deterministic vectors, so
    /// currency judgment and scope enforcement are exercised with no model.
    /// Only cross-language matching needs real embedding semantics (E5 leg).
    #[tokio::test]
    async fn safety_fixture_passes_engine_leg() {
        use ltmrs_domain::command::{CommandContext, DomainCommand, Scope};
        use ltmrs_domain::id::{
            ChannelId, DocumentRevision, EligibilityRevision, EntityId, EntityRevision, FrontendId,
            ModelFingerprint, OperationId, StoreGeneration,
        };
        use ltmrs_domain::memory::{
            FragmentType, Instant as DomainInstant, Memory, MemoryLifecycle, MemorySource,
        };
        use ltmrs_domain::relation::{Relation, RelationType};
        use ltmrs_search::retrieval::engine::{Engine, RetrievalRequest};
        use ltmrs_search::search::projector::{FixedEmbedder, Projector};
        use ltmrs_search::search::table::SearchTable;
        use ltmrs_service::repository::CanonicalRepository;
        use std::sync::Arc;

        fn eid(n: u64) -> EntityId {
            EntityId::new(uuid::Uuid::from_u128(n as u128))
        }

        fn engine_memory(
            id: EntityId,
            title: &str,
            fragment: &str,
            project: Option<&str>,
            created_at_millis: u64,
        ) -> Memory {
            Memory {
                id,
                external_alias: None,
                title: title.to_string(),
                fragment: fragment.to_string(),
                description: String::new(),
                fragment_type: FragmentType::Fact,
                project: project.map(|s| s.to_string()),
                source: MemorySource::Ai,
                confidence: 1.0,
                quality_score: None,
                lifecycle: MemoryLifecycle::Live,
                tags: vec![],
                associated_with: vec![],
                relations: vec![],
                parent_id: None,
                child_ids: vec![],
                session_id: None,
                task_type: None,
                related_guides: vec![],
                evidence: vec![],
                access_count: 0,
                last_accessed_at: None,
                positive_feedback: 0,
                negative_feedback: 0,
                negative_hits: 0,
                refinement_count: 0,
                distill_candidate: false,
                entity_revision: EntityRevision::new(1),
                document_revision: DocumentRevision::new(1),
                eligibility_revision: EligibilityRevision::new(1),
                created_at: DomainInstant::new(created_at_millis),
                updated_at: DomainInstant::new(created_at_millis),
                raw_created: None,
                unknown_fields: std::collections::BTreeMap::new(),
            }
        }

        fn engine_ctx(op_num: u64) -> CommandContext {
            CommandContext {
                store_generation: StoreGeneration::FIRST,
                frontend_id: FrontendId::new(uuid::Uuid::from_u128(1)),
                channel_id: ChannelId::new(uuid::Uuid::from_u128(2)),
                session: None,
                operation_id: OperationId::new(uuid::Uuid::from_u128(u128::from(op_num))),
                request_digest: format!("d{op_num}"),
                deadline_millis: None,
                scope: Scope::default(),
                retry_epoch: 1,
            }
        }

        let raw = std::fs::read_to_string(
            crate::bench::crate_root().join("experiments/quality/safety-cases.json"),
        )
        .unwrap();
        let corpus: Corpus = serde_json::from_str(&raw).unwrap();
        assert!(validate_corpus(&corpus).is_empty());

        let dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        // Frozen clock: retry namespaces carry a 24h TTL, so a real-clock
        // store outlives a fixed issuance instant before the first apply.
        let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
        let repo =
            CanonicalRepository::open_with_clock(dir.path().join("store").to_str().unwrap(), clock)
                .unwrap();
        repo.issue_namespace(
            FrontendId::new(uuid::Uuid::from_u128(1)),
            ChannelId::new(uuid::Uuid::from_u128(2)),
            1000,
        )
        .unwrap();
        let repo = Arc::new(repo);
        let table = SearchTable::open(lance_dir.path().to_str().unwrap())
            .await
            .unwrap();
        let mut projector = Projector::new(
            repo.clone(),
            table.clone(),
            Box::new(FixedEmbedder {
                dim: BENCH_EMBED_DIM,
            }),
            ModelFingerprint::new(1),
            StoreGeneration::FIRST,
        );
        // Seed order fixes the eid mapping (index -> eid).
        let mut id_by_uuid = std::collections::HashMap::new();
        for (index, doc) in corpus.documents.iter().enumerate() {
            let id = eid(index as u64);
            id_by_uuid.insert(id.as_uuid().to_string(), doc.id.clone());
            repo.apply(
                &engine_ctx(index as u64),
                &DomainCommand::AddMemory {
                    memory: engine_memory(
                        id,
                        &doc.id,
                        &doc.text,
                        Some(&doc.project),
                        doc.created_at_millis,
                    ),
                    session: None,
                },
            )
            .unwrap();
        }
        projector.run_until_idle().await.unwrap();
        table.create_fts_index().await.unwrap();
        // The fixture's obsolete pair: g5 supersedes g4.
        let idx = |fid: &str| corpus.documents.iter().position(|d| d.id == fid).unwrap() as u64;
        repo.apply(
            &engine_ctx(9000),
            &DomainCommand::Relate {
                relation: Relation::new(
                    eid(9000),
                    eid(idx("g5")),
                    eid(idx("g4")),
                    RelationType::Supersedes,
                    None,
                    DomainInstant::new(1),
                ),
            },
        )
        .unwrap();

        let mut executed = 0usize;
        let mut skipped_ids = Vec::new();
        let mut leakage_total = 0usize;
        for case in &corpus.cases {
            // Two exclusions, both named:
            // - CrossLanguage: hash vectors are language-blind by
            //   construction; matching needs real embedding semantics.
            // - NoAnswer: the engine's no-answer rule is a calibrated
            //   similarity threshold, and thresholds calibrate on real
            //   similarity distributions (WP-12 task 9), not on hash noise.
            //   With no threshold the dense-hash nearest neighbor flows in.
            if matches!(case.kind, CaseKind::CrossLanguage | CaseKind::NoAnswer) {
                skipped_ids.push(case.id.clone());
                continue;
            }
            executed += 1;
            let req = RetrievalRequest {
                query: case.query.clone(),
                scope: Scope {
                    project: Some(case.project.clone()),
                    after: case.after_millis,
                    before: case.before_millis,
                    ..Default::default()
                },
                model_fingerprint: Some(ModelFingerprint::new(1)),
                // No threshold: hash similarities are uncalibrated by design
                // (see the NoAnswer exclusion above).
                min_similarity: None,
                result_limit: case.top_k,
                ..Default::default()
            };
            let result = Engine::new(repo.clone(), table.clone())
                .retrieve(&req, &HashQueryEmbedder)
                .await
                .unwrap();
            let ranked: Vec<String> = result
                .results
                .iter()
                .map(|r| id_by_uuid[&r.memory.id.as_uuid().to_string()].clone())
                .collect();
            let metrics = score_case(case, &ranked);
            leakage_total += metrics.leakage_count;
            assert_eq!(metrics.leakage_count, 0, "case {} leaked", case.id);
            // No-answer cases are excluded from this leg (see above), so
            // every executed case must cover its mandatory ids.
            assert!(
                metrics.must_include_covered,
                "case {} missed mandatory ids (ranked {ranked:?})",
                case.id
            );
            if !case.obsolete_ids.is_empty() {
                assert!(
                    !metrics.obsolete_hit,
                    "case {} presented superseded advice (ranked {ranked:?})",
                    case.id
                );
                // Non-vacuous: the superseded doc must have been considered
                // (a candidate) and then excluded from the primary answer.
                for obsolete in &case.obsolete_ids {
                    let position = corpus
                        .documents
                        .iter()
                        .position(|d| &d.id == obsolete)
                        .unwrap();
                    let uuid = eid(position as u64).as_uuid().to_string();
                    let considered = result
                        .explanation
                        .candidates
                        .keys()
                        .any(|id| id.as_uuid().to_string() == uuid);
                    assert!(
                        considered,
                        "case {}: superseded doc {obsolete} was never a candidate (vacuous exclusion)",
                        case.id
                    );
                }
            }
        }
        assert!(
            executed >= 7,
            "engine subset must stay substantial, ran {executed}"
        );
        assert_eq!(
            skipped_ids,
            vec![
                "noanswer-g".to_string(),
                "cross-g1".to_string(),
                "noanswer-h".to_string()
            ]
        );
        assert_eq!(leakage_total, 0);
    }

    /// Passage + query bridges: real E5 semantics behind both seams. One
    /// shared adapter (single model load per test), roles kept separate.
    struct E5SharedEmbedder {
        adapter: std::sync::Arc<std::sync::Mutex<ltmrs_embeddings::e5_small::E5SmallAdapter>>,
    }

    impl ltmrs_search::search::projector::Embedder for E5SharedEmbedder {
        fn embed(&mut self, text: &str) -> Result<Vec<f32>, String> {
            use ltmrs_embeddings::recipe::Role;
            self.adapter
                .lock()
                .expect("embedder alive")
                .embed(text, Role::Passage)
                .map(|seq| seq.vector)
                .map_err(|e| format!("{e:?}"))
        }

        /// Production chunking verbatim (same code path as the daemon
        /// projector): the cross-lang verdict covers multi-chunk documents.
        fn chunk_text(
            &self,
            title: &str,
            fragment: &str,
        ) -> Vec<ltmrs_search::search::projector::TextChunk> {
            let chunks = self
                .adapter
                .lock()
                .expect("embedder alive")
                .chunk_passage(title, fragment);
            ltmrs_search::search::backend::e5_chunks_to_text_chunks(title, &chunks)
        }

        fn chunker_version(&self) -> String {
            ltmrs_search::search::backend::E5_CHUNK_VERSION.to_string()
        }
    }

    impl ltmrs_search::retrieval::engine::QueryEmbedder for E5SharedEmbedder {
        fn embed_query<'a>(
            &'a self,
            query: &'a str,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = ltmrs_domain::command::DomainResult<Vec<f32>>>
                    + Send
                    + 'a,
            >,
        > {
            use ltmrs_embeddings::recipe::Role;
            Box::pin(async move {
                self.adapter
                    .lock()
                    .expect("embedder alive")
                    .embed(query, Role::Query)
                    .map(|seq| seq.vector)
                    .map_err(|e| {
                        ltmrs_domain::command::DomainError::new(
                            ltmrs_domain::command::DomainErrorCode::Validation,
                            format!("{e:?}"),
                        )
                    })
            })
        }
    }

    /// Cross-language through real E5-small (the embedding engine near
    /// WP-04 is the Candle E5 stack, not LanceDB — Lance is the vector
    /// table). Skips cleanly when the pinned artifacts are absent (CI
    /// without network), following the e5_small precedent.
    #[tokio::test]
    async fn safety_fixture_passes_real_e5_leg() {
        use ltmrs_domain::command::{CommandContext, DomainCommand, Scope};
        use ltmrs_domain::id::{
            ChannelId, DocumentRevision, EligibilityRevision, EntityId, EntityRevision, FrontendId,
            ModelFingerprint, OperationId, StoreGeneration,
        };
        use ltmrs_domain::memory::{
            FragmentType, Instant as DomainInstant, Memory, MemoryLifecycle, MemorySource,
        };
        use ltmrs_search::retrieval::engine::{Engine, RetrievalRequest};
        use ltmrs_search::search::projector::Projector;
        use ltmrs_search::search::table::SearchTable;
        use ltmrs_service::repository::CanonicalRepository;
        use std::sync::Arc;

        let artifacts_dir = crate::bench::crate_root().join("tmp/e5-artifacts");
        let artifacts = artifacts_dir.as_path();
        if !artifacts.join("model.safetensors").exists() {
            eprintln!("SKIP real-E5 leg: tmp/e5-artifacts absent (CI without network)");
            return;
        }
        let raw = std::fs::read_to_string(
            crate::bench::crate_root().join("experiments/quality/safety-cases.json"),
        )
        .unwrap();
        let corpus: Corpus = serde_json::from_str(&raw).unwrap();
        assert!(validate_corpus(&corpus).is_empty());

        let dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
        let repo =
            CanonicalRepository::open_with_clock(dir.path().join("store").to_str().unwrap(), clock)
                .unwrap();
        repo.issue_namespace(
            FrontendId::new(uuid::Uuid::from_u128(1)),
            ChannelId::new(uuid::Uuid::from_u128(2)),
            1000,
        )
        .unwrap();
        let repo = Arc::new(repo);
        let table = SearchTable::open(lance_dir.path().to_str().unwrap())
            .await
            .unwrap();
        let shared = std::sync::Arc::new(std::sync::Mutex::new(
            ltmrs_embeddings::e5_small::E5SmallAdapter::load_verified(artifacts)
                .expect("pinned artifacts load"),
        ));
        let mut projector = Projector::new(
            repo.clone(),
            table.clone(),
            Box::new(E5SharedEmbedder {
                adapter: shared.clone(),
            }),
            ModelFingerprint::new(1),
            StoreGeneration::FIRST,
        );
        let mut id_by_uuid = std::collections::HashMap::new();
        for (index, doc) in corpus.documents.iter().enumerate() {
            let id = EntityId::new(uuid::Uuid::from_u128(index as u128));
            id_by_uuid.insert(id.as_uuid().to_string(), doc.id.clone());
            repo.apply(
                &CommandContext {
                    store_generation: StoreGeneration::FIRST,
                    frontend_id: FrontendId::new(uuid::Uuid::from_u128(1)),
                    channel_id: ChannelId::new(uuid::Uuid::from_u128(2)),
                    session: None,
                    operation_id: OperationId::new(uuid::Uuid::from_u128(index as u128)),
                    request_digest: format!("e5-{index}"),
                    deadline_millis: None,
                    scope: Scope::default(),
                    retry_epoch: 1,
                },
                &DomainCommand::AddMemory {
                    memory: Memory {
                        id,
                        external_alias: None,
                        title: doc.id.clone(),
                        fragment: doc.text.clone(),
                        description: String::new(),
                        fragment_type: FragmentType::Fact,
                        project: Some(doc.project.clone()),
                        source: MemorySource::Ai,
                        confidence: 1.0,
                        quality_score: None,
                        lifecycle: MemoryLifecycle::Live,
                        tags: vec![],
                        associated_with: vec![],
                        relations: vec![],
                        parent_id: None,
                        child_ids: vec![],
                        session_id: None,
                        task_type: None,
                        related_guides: vec![],
                        evidence: vec![],
                        access_count: 0,
                        last_accessed_at: None,
                        positive_feedback: 0,
                        negative_feedback: 0,
                        negative_hits: 0,
                        refinement_count: 0,
                        distill_candidate: false,
                        entity_revision: EntityRevision::new(1),
                        document_revision: DocumentRevision::new(1),
                        eligibility_revision: EligibilityRevision::new(1),
                        created_at: DomainInstant::new(doc.created_at_millis),
                        updated_at: DomainInstant::new(doc.created_at_millis),
                        raw_created: None,
                        unknown_fields: std::collections::BTreeMap::new(),
                    },
                    session: None,
                },
            )
            .unwrap();
        }
        projector.run_until_idle().await.unwrap();
        table.create_fts_index().await.unwrap();

        let embedder = E5SharedEmbedder {
            adapter: shared.clone(),
        };
        // French query against English docs (plus an English anchor): real
        // cross-lingual similarity must surface the rose-pruning doc.
        for case_id in ["cross-g1", "exact-g1"] {
            let case = corpus.cases.iter().find(|c| c.id == case_id).unwrap();
            let req = RetrievalRequest {
                query: case.query.clone(),
                scope: Scope {
                    project: Some(case.project.clone()),
                    ..Default::default()
                },
                model_fingerprint: Some(ModelFingerprint::new(1)),
                min_similarity: None,
                result_limit: case.top_k,
                ..Default::default()
            };
            let result = Engine::new(repo.clone(), table.clone())
                .retrieve(&req, &embedder)
                .await
                .unwrap();
            let ranked: Vec<String> = result
                .results
                .iter()
                .map(|r| id_by_uuid[&r.memory.id.as_uuid().to_string()].clone())
                .collect();
            assert!(
                case.must_include.iter().all(|id| ranked.contains(id)),
                "case {case_id} missed mandatory ids (ranked {ranked:?})"
            );
        }
    }

    /// The harness must project through production chunking, not single-chunk
    /// override: a long document yields multiple rows (greedy units).
    #[tokio::test]
    async fn e5_leg_uses_production_chunking() {
        use ltmrs_domain::command::{CommandContext, DomainCommand, Scope};
        use ltmrs_domain::id::{
            ChannelId, DocumentRevision, EligibilityRevision, EntityId, EntityRevision, FrontendId,
            ModelFingerprint, OperationId, StoreGeneration,
        };
        use ltmrs_domain::memory::{
            FragmentType, Instant as DomainInstant, Memory, MemoryLifecycle, MemorySource,
        };
        use ltmrs_search::search::projector::Projector;
        use ltmrs_search::search::table::SearchTable;
        use ltmrs_service::repository::CanonicalRepository;

        let artifacts_dir = crate::bench::crate_root().join("tmp/e5-artifacts");
        let artifacts = artifacts_dir.as_path();
        if !artifacts.join("model.safetensors").exists() {
            eprintln!("SKIP real-E5 leg: tmp/e5-artifacts absent (CI without network)");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        let clock = std::sync::Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
        let repo =
            CanonicalRepository::open_with_clock(dir.path().join("store").to_str().unwrap(), clock)
                .unwrap();
        repo.issue_namespace(
            FrontendId::new(uuid::Uuid::from_u128(1)),
            ChannelId::new(uuid::Uuid::from_u128(2)),
            1000,
        )
        .unwrap();
        let repo = std::sync::Arc::new(repo);
        let table = SearchTable::open(lance_dir.path().to_str().unwrap())
            .await
            .unwrap();
        let shared = std::sync::Arc::new(std::sync::Mutex::new(
            ltmrs_embeddings::e5_small::E5SmallAdapter::load_verified(artifacts)
                .expect("pinned artifacts load"),
        ));
        let mut projector = Projector::new(
            repo.clone(),
            table.clone(),
            Box::new(E5SharedEmbedder {
                adapter: shared.clone(),
            }),
            ModelFingerprint::new(1),
            StoreGeneration::FIRST,
        );
        let id = EntityId::new(uuid::Uuid::from_u128(77));
        repo.apply(
            &CommandContext {
                store_generation: StoreGeneration::FIRST,
                frontend_id: FrontendId::new(uuid::Uuid::from_u128(1)),
                channel_id: ChannelId::new(uuid::Uuid::from_u128(2)),
                session: None,
                operation_id: OperationId::new(uuid::Uuid::from_u128(3)),
                request_digest: "e5-chunk".to_string(),
                deadline_millis: None,
                scope: Scope::default(),
                retry_epoch: 1,
            },
            &DomainCommand::AddMemory {
                memory: Memory {
                    id,
                    external_alias: None,
                    title: "Long doc".to_string(),
                    fragment: "word ".repeat(600),
                    description: String::new(),
                    fragment_type: FragmentType::Fact,
                    project: None,
                    source: MemorySource::Ai,
                    confidence: 1.0,
                    quality_score: None,
                    lifecycle: MemoryLifecycle::Live,
                    tags: vec![],
                    associated_with: vec![],
                    relations: vec![],
                    parent_id: None,
                    child_ids: vec![],
                    session_id: None,
                    task_type: None,
                    related_guides: vec![],
                    evidence: vec![],
                    access_count: 0,
                    last_accessed_at: None,
                    positive_feedback: 0,
                    negative_feedback: 0,
                    negative_hits: 0,
                    refinement_count: 0,
                    distill_candidate: false,
                    entity_revision: EntityRevision::new(1),
                    document_revision: DocumentRevision::new(1),
                    eligibility_revision: EligibilityRevision::new(1),
                    created_at: DomainInstant::new(1000),
                    updated_at: DomainInstant::new(1000),
                    raw_created: None,
                    unknown_fields: std::collections::BTreeMap::new(),
                },
                session: None,
            },
        )
        .unwrap();
        projector.run_until_idle().await.unwrap();
        let rows = table
            .rows_where(&format!("memory_id = '{}'", id.as_uuid()))
            .await
            .unwrap();
        assert!(
            rows.len() > 1,
            "long document must project to multiple chunks, got {}",
            rows.len()
        );
    }
}
