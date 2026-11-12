//! Ablation leg implementations, easiest-first (WP-12 §8.2; T-QUALITY-01).
//!
//! Landed legs (model-free, deterministic, label-clean — each leg is one
//! [`evaluate`](super::evaluate) call on the frozen corpus at the cases'
//! fixed budgets):
//! - DenseExact: exact brute-force cosine over harness-stored hash vectors
//!   (same [`hash_embed_vec`](ltmrs_search::search::projector::hash_embed_vec)
//!   derivation for docs and queries, so the spaces align with no model).
//! - HybridRrf: FTS lexical (canonical scope pushdown) + dense exact fused
//!   with one-based RRF (the engine's [`rrf_fuse`](ltmrs_search::retrieval::ranking::rrf_fuse)).
//! - Priority: hybrid fused scores through
//!   [`native_score`](ltmrs_search::retrieval::ranking::native_score) with
//!   uniform priority — the frozen corpus carries no feedback/confidence
//!   signals, so uniform priority preserves hybrid order (asserted, not
//!   assumed; a future per-doc signal changes the order test).
//! - Mmr: native-scored candidates diversified with
//!   [`mmr_select`](ltmrs_search::retrieval::mmr::mmr_select) (frozen
//!   default lambda, empty protected set: no bundle info without labels).
//!
//! Stopping point: Graph is NOT landed. The frozen corpus carries no
//! relation edges, and building edges from evaluation labels
//! (`obsolete_ids`, `Conflict` kind) would leak answers into the leg,
//! while inferring edges from text similarity would invent matching
//! semantics. UpstreamReference stays unlanded behind that stop (its
//! identity — the Lemma-faithful TF-IDF port versus a provisioned
//! upstream service — is an owner decision, not a derivation).

use super::{Corpus, QueryCase};
use ltmrs_domain::id::EntityId;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use uuid::Uuid;

/// Embedding width for the harness hash vectors. Docs and queries both
/// derive here (single owner), so a dim change cannot misalign the spaces.
/// Matches the `BENCH_EMBED_DIM` precedent (384).
pub const ABLATION_EMBED_DIM: usize = 384;

/// Neutral priority when the corpus carries no feedback/confidence
/// signals. Uniform, so [`native_scored_order`] preserves RRF order.
pub const UNIFORM_PRIORITY: f64 = 0.5;

/// Exact-dense index: harness-stored hash vectors parsed once at the
/// boundary, then pure cosine ranking (no I/O, no model, no threshold).
pub struct DenseIndex {
    dim: usize,
    vectors: HashMap<String, Vec<f32>>,
}

impl DenseIndex {
    /// Parse the corpus pool into trusted vectors (fails fast on a bad dim).
    pub fn build(corpus: &Corpus, dim: usize) -> Self {
        assert!(dim > 0, "ablation embed dim must be positive, got {dim}");
        let vectors = corpus
            .documents
            .iter()
            .map(|doc| {
                (
                    doc.id.clone(),
                    ltmrs_search::search::projector::hash_embed_vec(&doc.text, dim),
                )
            })
            .collect();
        Self { dim, vectors }
    }

    /// Stored vectors (MMR diversity needs the whole document vector).
    pub fn vectors(&self) -> &HashMap<String, Vec<f32>> {
        &self.vectors
    }

    /// Exact cosine rank over the in-scope pool (project + declared time
    /// bounds — the same dimensions the lexical leg pushes down). Returns
    /// the full pool; truncation to `top_k` happens in the leg closure.
    /// Deterministic: score descending, doc-id ascending tiebreak.
    pub fn rank(&self, corpus: &Corpus, case: &QueryCase) -> Vec<String> {
        if corpus.documents.is_empty() {
            return Vec::new();
        }
        let query_vec = ltmrs_search::search::projector::hash_embed_vec(&case.query, self.dim);
        let mut scored: Vec<(String, f64)> = corpus
            .documents
            .iter()
            .filter(|doc| in_scope(doc, case))
            .map(|doc| {
                let sim = ltmrs_search::retrieval::ranking::cosine(
                    Some(&query_vec),
                    self.vectors.get(&doc.id).map(Vec::as_slice),
                );
                (doc.id.clone(), sim)
            })
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        scored.into_iter().map(|(id, _)| id).collect()
    }
}

/// Scope predicate for the dense leg (project equality + declared time
/// bounds). Obsolete status is a label, not scope: superseded docs stay
/// rankable and the metric counts the hit.
fn in_scope(doc: &super::DocLabel, case: &QueryCase) -> bool {
    if doc.project != case.project {
        return false;
    }
    if let Some(after) = case.after_millis
        && doc.created_at_millis < after
    {
        return false;
    }
    if let Some(before) = case.before_millis
        && doc.created_at_millis > before
    {
        return false;
    }
    true
}

/// Deterministic EntityId assignment over a sorted id union (index order =
/// lexicographic order, so engine tiebreaks stay doc-id ordered). The
/// engine's fusion/selection primitives key on EntityId; the mapping is an
/// injective relabeling, never a score input.
fn eid_union(ids: &[String]) -> (Vec<String>, BTreeMap<EntityId, String>) {
    let mut union: Vec<String> = ids.to_vec();
    union.sort();
    union.dedup();
    let reverse = union
        .iter()
        .enumerate()
        .map(|(i, id)| (EntityId::new(Uuid::from_u128(i as u128 + 1)), id.clone()))
        .collect();
    (union, reverse)
}

fn eid_of(reverse: &BTreeMap<EntityId, String>, id: &str) -> EntityId {
    reverse
        .iter()
        .find_map(|(eid, name)| (name == id).then_some(*eid))
        .unwrap_or_else(|| panic!("fused id {id} must come from the fused union"))
}

fn ranked_list(
    leg: &'static str,
    order: &[String],
    reverse: &BTreeMap<EntityId, String>,
) -> ltmrs_search::retrieval::ranking::RankedList {
    ltmrs_search::retrieval::ranking::RankedList {
        leg,
        weight: 1.0,
        entries: order
            .iter()
            .enumerate()
            .map(|(i, id)| (eid_of(reverse, id), i + 1))
            .collect(),
    }
}

/// Fuse lexical + dense orders with one-based RRF. Pure over the two
/// orders; score descending, doc-id ascending tiebreak.
pub fn rrf_fuse_orders(lexical: &[String], dense: &[String]) -> Vec<(String, f64)> {
    let all: Vec<String> = lexical.iter().chain(dense.iter()).cloned().collect();
    if all.is_empty() {
        return Vec::new();
    }
    let (_union, reverse) = eid_union(&all);
    let legs = [
        ranked_list("lexical", lexical, &reverse),
        ranked_list("dense", dense, &reverse),
    ];
    let fused = ltmrs_search::retrieval::ranking::rrf_fuse(&legs);
    let mut out: Vec<(String, f64)> = reverse
        .iter()
        .map(|(eid, id)| {
            (
                id.clone(),
                fused
                    .scores
                    .get(eid)
                    .copied()
                    .unwrap_or_else(|| panic!("fused score must exist for {id}")),
            )
        })
        .collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    out
}

/// Hybrid fused scores through the calibrated native scorer with uniform
/// priority and no graph contribution. Uniform priority is an affine shift
/// of RRF, so this preserves [`rrf_fuse_orders`] order exactly.
pub fn native_scored_order(lexical: &[String], dense: &[String]) -> Vec<(String, f64)> {
    let all: Vec<String> = lexical.iter().chain(dense.iter()).cloned().collect();
    if all.is_empty() {
        return Vec::new();
    }
    let (_union, reverse) = eid_union(&all);
    let legs = [
        ranked_list("lexical", lexical, &reverse),
        ranked_list("dense", dense, &reverse),
    ];
    let fused = ltmrs_search::retrieval::ranking::rrf_fuse(&legs);
    let mut out: Vec<(String, f64)> = reverse
        .iter()
        .map(|(eid, id)| {
            let raw = fused
                .scores
                .get(eid)
                .copied()
                .unwrap_or_else(|| panic!("fused score must exist for {id}"));
            let r = ltmrs_search::retrieval::ranking::normalize_rrf(raw, &legs);
            let s = ltmrs_search::retrieval::ranking::native_score(r, 0.0, UNIFORM_PRIORITY);
            (id.clone(), s)
        })
        .collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    out
}

/// MMR over native-scored candidates (frozen default lambda, empty
/// protected set). Deterministic; capped at `max_items` (≥1); only
/// selects from `scored`.
pub fn mmr_order(
    scored: &[(String, f64)],
    vectors: &HashMap<String, Vec<f32>>,
    max_items: usize,
) -> Vec<String> {
    if scored.is_empty() {
        return Vec::new();
    }
    let all: Vec<String> = scored.iter().map(|(id, _)| id.clone()).collect();
    let (_union, reverse) = eid_union(&all);
    let candidates: Vec<ltmrs_search::retrieval::mmr::MmrCandidate> = scored
        .iter()
        .map(|(id, score)| ltmrs_search::retrieval::mmr::MmrCandidate {
            id: eid_of(&reverse, id),
            score: *score,
            vector: vectors.get(id).cloned(),
        })
        .collect();
    let result = ltmrs_search::retrieval::mmr::mmr_select(
        &candidates,
        &BTreeSet::new(),
        &ltmrs_search::retrieval::mmr::MmrConfig {
            max_items: max_items.max(1),
            ..Default::default()
        },
    );
    result
        .selected
        .iter()
        .map(|eid| {
            reverse
                .get(eid)
                .cloned()
                .unwrap_or_else(|| panic!("mmr selected {eid:?} outside the fused union"))
        })
        .collect()
}

/// UpstreamReference leg via the live Lemma SQL shape (owner directive:
/// use Lemma's implementation, no port). Builds a lemma-shaped SQLite
/// store (memories + memory_fts) from the frozen safety corpus with the
/// `sqlite3` CLI — no rusqlite edge, no reimplementation — then runs the
/// upstream FTS query shape per case verbatim:
/// `memory_fts MATCH ?` + `bm25(memory_fts)` + project filter, LIKE
/// fallback on FTS error. Returns the dev+heldout `evaluate` reports.
/// Reports retention, never gates (strategy comparison, not a quality bar).
#[cfg(test)]
pub fn upstream_reference_report() -> Vec<super::AblationReport> {
    use super::{Ablation, Corpus, Split, evaluate};
    use std::process::Command;
    let raw = std::fs::read_to_string(
        crate::bench::crate_root().join("experiments/quality/safety-cases.json"),
    )
    .expect("safety corpus reads");
    let corpus: Corpus = serde_json::from_str(&raw).expect("safety corpus parses");
    let dir = tempfile::tempdir().expect("scratch dir");
    let db = dir.path().join("upstream.db");
    let sql_escape = |s: &str| s.replace('\'', "''");
    // Standalone FTS table keyed by doc id (ranking math is what we
    // compare, not schema fidelity).
    let mut setup = String::from(
        "CREATE TABLE memories(id TEXT PRIMARY KEY, project TEXT, title TEXT, fragment TEXT, confidence REAL);\
         CREATE VIRTUAL TABLE memory_fts USING fts5(doc_id UNINDEXED, project, title, fragment);",
    );
    for doc in &corpus.documents {
        setup.push_str(&format!(
            "INSERT INTO memories VALUES('{}','{}','{}','{}',0.5);\
             INSERT INTO memory_fts VALUES('{}','{}','{}','{}');",
            sql_escape(&doc.id),
            sql_escape(&doc.project),
            sql_escape(&doc.id),
            sql_escape(&doc.text),
            sql_escape(&doc.id),
            sql_escape(&doc.project.to_lowercase()),
            sql_escape(&doc.id),
            sql_escape(&doc.text),
        ));
    }
    let status = Command::new("sqlite3")
        .arg(&db)
        .arg(&setup)
        .status()
        .expect("sqlite3 CLI builds the lemma-shaped store");
    assert!(status.success(), "store build must succeed");
    // Upstream FTS sanitize: their sanitizeFtsQuery strips operators down
    // to plain terms; mirror the effect (alphanumeric + space runs).
    let sanitize = |q: &str| {
        q.split_whitespace()
            .filter(|w| w.chars().any(|c| c.is_alphanumeric()))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let retrieve = |case: &QueryCase| -> Vec<String> {
        let terms = sanitize(&case.query);
        let fts_sql = format!(
            "SELECT doc_id FROM memory_fts WHERE memory_fts MATCH '{}' AND project = '{}' ORDER BY bm25(memory_fts) LIMIT {};",
            sql_escape(&terms),
            sql_escape(&case.project.to_lowercase()),
            case.top_k.max(1)
        );
        let out = Command::new("sqlite3")
            .arg(&db)
            .arg(&fts_sql)
            .output()
            .expect("sqlite3 CLI runs the upstream query");
        if out.status.success() {
            let rows: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            if !rows.is_empty() {
                return rows;
            }
        }
        // LIKE fallback (their substring fallback, same shape).
        let like = format!("%{}%", sql_escape(case.query.trim()));
        let like_sql = format!(
            "SELECT id FROM memories WHERE project = '{}' AND (title LIKE '{}' OR fragment LIKE '{}') ORDER BY confidence DESC LIMIT {};",
            sql_escape(&case.project.to_lowercase()),
            like,
            like,
            case.top_k.max(1)
        );
        let out = Command::new("sqlite3")
            .arg(&db)
            .arg(&like_sql)
            .output()
            .expect("sqlite3 CLI runs the fallback");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    };
    [Split::Dev, Split::Heldout]
        .iter()
        .map(|split| evaluate(Ablation::UpstreamReference, &corpus, *split, &retrieve))
        .collect()
}
