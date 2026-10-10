//! Retrieval quality probes (moved verbatim from `engine.rs`).

use super::test_support::*;
use super::{Engine, RetrievalRequest};
use crate::search::projector::Projector;
use crate::search::table::SearchTable;
use ltmrs_domain::id::FrontendId;
use uuid::Uuid;

/// Threshold calibration on the 300-case synthetic corpus (ignored:
/// needs the ~500MB pinned artifacts + ~10 min). Gate:
/// `LTMRS_PROBE_MODELS=<models dir>`; skips without it. Design:
/// paraphrase queries (mean 0.63 shared tokens) over topic-disjoint
/// dev/heldout splits; dense-leg cosine similarities recorded once,
/// thresholds swept in-memory exactly as the engine filters
/// (`similarity = 1 - distance >= min`). Reports retention +
/// fallout per threshold; recommends max-t with full dev retention.
/// Structural assertions only — no quality gates baked in.
#[tokio::test]
#[ignore]
async fn calibrate_dense_threshold_on_synthetic_corpus() {
    use ltmrs_embeddings::artifacts::ArtifactCache;
    use ltmrs_embeddings::e5_small::{E5_SMALL_FINGERPRINT, E5SmallAdapter};
    use ltmrs_embeddings::recipe::Role;
    use ltmrs_embeddings::service::EmbeddingService;
    use std::sync::{Arc, Mutex as StdMutex};

    #[derive(serde::Deserialize)]
    struct FixtureMemory {
        key: String,
        title: String,
        fragment: String,
    }
    #[derive(serde::Deserialize)]
    struct FixtureCase {
        query: String,
        targets: Vec<String>,
        split: String,
    }
    #[derive(serde::Deserialize)]
    struct Fixture {
        memories: Vec<FixtureMemory>,
        cases: Vec<FixtureCase>,
    }

    let models = std::env::var("LTMRS_PROBE_MODELS").unwrap_or_default();
    if models.is_empty() || !std::path::Path::new(&models).exists() {
        eprintln!("SKIP: set LTMRS_PROBE_MODELS to a provisioned models dir");
        return;
    }
    let fixture: Fixture = serde_json::from_str(include_str!(
        "../../../../../experiments/quality/retrieval-calibration.json"
    ))
    .expect("calibration fixture parses");
    assert_eq!(fixture.memories.len(), 300);
    assert_eq!(fixture.cases.len(), 300);

    let dir = tempfile::tempdir().unwrap();
    let lance_dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
    let repo = Arc::new(
        ltmrs_service::repository::CanonicalRepository::open_with_clock(
            dir.path().to_str().unwrap(),
            clock,
        )
        .unwrap(),
    );
    {
        let fe = FrontendId::new(Uuid::from_u128(1));
        repo.issue_namespace(
            fe,
            ltmrs_domain::id::ChannelId::new(Uuid::from_u128(2)),
            1000,
        )
        .unwrap();
    }
    for (i, m) in fixture.memories.iter().enumerate() {
        add(&repo, (i + 1) as u64, &m.title, &m.fragment, None);
    }
    let table = SearchTable::open(lance_dir.path().to_str().unwrap())
        .await
        .unwrap();
    let cache = ArtifactCache::new(&models);
    let adapter = Arc::new(StdMutex::new(
        E5SmallAdapter::load_from_cache(&cache).expect("projection adapter loads"),
    ));
    let resolved = Projector::project_pending(&repo, &table, Box::new(Arc::clone(&adapter)), 1000)
        .await
        .unwrap();
    assert_eq!(resolved, 300, "all fixture memories must index");
    let svc = EmbeddingService::load_e5_small_from_cache(&cache).expect("query service loads");
    let filter = format!(
        "model_fingerprint = {} AND embedding IS NOT NULL",
        E5_SMALL_FINGERPRINT.as_u64()
    );
    // Per case: embed once, record every row similarity (limit covers
    // the whole table, so the sweep below is exact).
    let mut sims: Vec<(String, Vec<(String, f32)>)> = Vec::new();
    for case in &fixture.cases {
        let vec = svc.embed(&case.query, Role::Query).await.unwrap();
        assert_eq!(vec.len(), 384);
        assert!(vec.iter().all(|x| x.is_finite()));
        let rows = table.vector_query(&vec, 400, Some(&filter)).await.unwrap();
        assert_eq!(rows.len(), 300, "every row must be searchable");
        let ranked: Vec<(String, f32)> = rows
            .iter()
            .map(|(row, d)| {
                let n = row.memory_id.as_uuid().as_u128() as usize;
                (fixture.memories[n - 1].key.clone(), 1.0 - *d)
            })
            .collect();
        assert!(
            ranked.iter().all(|(_, s)| s.is_finite()),
            "similarities must be finite"
        );
        sims.push((case.targets[0].clone(), ranked));
    }
    // Sweep thresholds in-memory (same predicate the engine applies).
    eprintln!("thr | dev-retain | dev-fallout | held-retain | held-fallout");
    let mut recommended = 0.0f64;
    let mut t = 0.0f64;
    while t <= 0.9001 {
        let mut dev_ret = 0usize;
        let mut dev_n = 0usize;
        let mut dev_fall = 0usize;
        let mut held_ret = 0usize;
        let mut held_n = 0usize;
        let mut held_fall = 0usize;
        for (case, ranked) in fixture.cases.iter().zip(sims.iter()) {
            let kept: Vec<&(String, f32)> =
                ranked.1.iter().filter(|(_, s)| *s as f64 >= t).collect();
            let hit = kept.iter().any(|(k, _)| *k == case.targets[0]);
            let fall = kept.len().saturating_sub(if hit { 1 } else { 0 });
            if case.split == "dev" {
                dev_n += 1;
                dev_ret += hit as usize;
                dev_fall += fall;
            } else {
                held_n += 1;
                held_ret += hit as usize;
                held_fall += fall;
            }
        }
        let dr = dev_ret as f64 / dev_n as f64;
        let hr = held_ret as f64 / held_n as f64;
        eprintln!(
            "{t:.2} | {dr:.3} | {:.1} | {hr:.3} | {:.1}",
            dev_fall as f64 / dev_n as f64,
            held_fall as f64 / held_n as f64
        );
        if dr >= 1.0 {
            recommended = t;
        }
        t += 0.05;
    }
    eprintln!("recommended min_similarity (max-t, full dev retention): {recommended:.2}");
}

/// BEIR SciFact hybrid quality (ignored: ~5K real embeds + 300 queries,
/// needs pinned artifacts + BEIR dir). Gates: `LTMRS_PROBE_MODELS` and
/// `LTMRS_BEIR_DIR` (containing corpus.jsonl, queries.jsonl,
/// qrels/test.tsv); skips without both. Production-shape run: real
/// E5 index via `project_pending`, FTS built, hybrid retrieve at the
/// default threshold. Reports nDCG@10, MAP, Recall@100, MRR@10 on the
/// human test qrels. Structural assertions only.
#[tokio::test]
#[ignore]
async fn beir_scifact_hybrid_quality() {
    use crate::search::backend::e5::ServiceQueryEmbedder;
    use ltmrs_embeddings::artifacts::ArtifactCache;
    use ltmrs_embeddings::e5_small::{E5_SMALL_FINGERPRINT, E5SmallAdapter};
    use ltmrs_embeddings::service::EmbeddingService;
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex as StdMutex};

    let models = std::env::var("LTMRS_PROBE_MODELS").unwrap_or_default();
    let beir = std::env::var("LTMRS_BEIR_DIR").unwrap_or_default();
    if models.is_empty()
        || !std::path::Path::new(&models).exists()
        || beir.is_empty()
        || !std::path::Path::new(&beir).exists()
    {
        eprintln!("SKIP: set LTMRS_PROBE_MODELS and LTMRS_BEIR_DIR");
        return;
    }
    // BEIR JSONL/TSV corpus: _id/title/text docs, test qrels.
    let corpus: Vec<serde_json::Value> =
        std::fs::read_to_string(std::path::Path::new(&beir).join("corpus.jsonl"))
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
    let queries: Vec<serde_json::Value> =
        std::fs::read_to_string(std::path::Path::new(&beir).join("queries.jsonl"))
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
    let qtext: HashMap<String, String> = queries
        .iter()
        .map(|q| {
            (
                q["_id"].as_str().unwrap().to_string(),
                q["text"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let mut qrels: HashMap<String, HashSet<String>> = HashMap::new();
    for line in std::fs::read_to_string(std::path::Path::new(&beir).join("qrels/test.tsv"))
        .unwrap()
        .lines()
        .skip(1)
    {
        let mut c = line.split('\t');
        let (qid, did, score) = (
            c.next().unwrap().to_string(),
            c.next().unwrap().to_string(),
            c.next().unwrap().parse::<i64>().unwrap(),
        );
        if score > 0 {
            qrels.entry(qid).or_default().insert(did);
        }
    }
    eprintln!(
        "scifact: {} docs, {} test queries",
        corpus.len(),
        qrels.len()
    );

    let dir = tempfile::tempdir().unwrap();
    let lance_dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(ltmrs_domain::clock::FrozenClock::new(1000));
    let repo = Arc::new(
        ltmrs_service::repository::CanonicalRepository::open_with_clock(
            dir.path().to_str().unwrap(),
            clock,
        )
        .unwrap(),
    );
    {
        let fe = FrontendId::new(Uuid::from_u128(1));
        repo.issue_namespace(
            fe,
            ltmrs_domain::id::ChannelId::new(Uuid::from_u128(2)),
            1000,
        )
        .unwrap();
    }
    let mut doc_n = HashMap::new();
    for (i, doc) in corpus.iter().enumerate() {
        let n = (i + 1) as u64;
        doc_n.insert(doc["_id"].as_str().unwrap().to_string(), n);
        add(
            &repo,
            n,
            doc["title"].as_str().unwrap_or(""),
            doc["text"].as_str().unwrap_or(""),
            None,
        );
    }
    let table = SearchTable::open(lance_dir.path().to_str().unwrap())
        .await
        .unwrap();
    let cache = ArtifactCache::new(&models);
    let adapter = Arc::new(StdMutex::new(
        E5SmallAdapter::load_from_cache(&cache).expect("projection adapter loads"),
    ));
    let resolved =
        Projector::project_pending(&repo, &table, Box::new(Arc::clone(&adapter)), 100000)
            .await
            .unwrap();
    assert_eq!(resolved, corpus.len(), "all BEIR docs must index");
    table.create_fts_index().await.unwrap();
    let svc = EmbeddingService::load_e5_small_from_cache(&cache).expect("query service loads");
    let embedder = Arc::new(ServiceQueryEmbedder::new(svc));

    // Ranked retrieval per test query at the production default.
    let mut ndcg = 0.0f64;
    let mut ap_sum = 0.0f64;
    let mut recall = 0.0f64;
    let mut mrr = 0.0f64;
    let mut n = 0usize;
    for (qid, rel) in qrels.iter() {
        let Some(q) = qtext.get(qid) else { continue };
        let req = RetrievalRequest {
            query: q.clone(),
            model_fingerprint: Some(E5_SMALL_FINGERPRINT),
            candidate_limit: 100,
            ..Default::default()
        };
        let out = Engine::new(repo.clone(), table.clone())
            .retrieve(&req, embedder.as_ref())
            .await
            .unwrap();
        assert!(!out.results.is_empty() || out.explanation.no_match);
        let ranked: Vec<String> = out
            .results
            .iter()
            .take(100)
            .map(|r| {
                let n = r.memory.id.as_uuid().as_u128() as usize;
                corpus[n - 1]["_id"].as_str().unwrap().to_string()
            })
            .collect();
        let top10: Vec<bool> = ranked.iter().take(10).map(|id| rel.contains(id)).collect();
        let dcg: f64 = top10
            .iter()
            .enumerate()
            .map(|(i, hit)| {
                if *hit {
                    1.0 / ((i + 2) as f64).log2()
                } else {
                    0.0
                }
            })
            .sum();
        let ideal: f64 = (0..rel.len().min(10))
            .map(|i| 1.0 / ((i + 2) as f64).log2())
            .sum();
        ndcg += if ideal > 0.0 { dcg / ideal } else { 0.0 };
        let mut ap = 0.0f64;
        let mut seen = 0usize;
        for (i, hit) in top10.iter().enumerate() {
            if *hit {
                seen += 1;
                ap += seen as f64 / (i + 1) as f64;
            }
        }
        ap_sum += ap / rel.len() as f64;
        // Recall over the full returned list.
        let hits_all = ranked.iter().filter(|id| rel.contains(*id)).count();
        recall += hits_all as f64 / rel.len() as f64;
        mrr += top10
            .iter()
            .position(|h| *h)
            .map(|i| 1.0 / (i + 1) as f64)
            .unwrap_or(0.0);
        n += 1;
    }
    assert!(n > 0);
    eprintln!(
        "BEIR scifact hybrid (n={n} queries): nDCG@10={:.3} MAP={:.3} Recall@100={:.3} MRR@10={:.3}",
        ndcg / n as f64,
        ap_sum / n as f64,
        recall / n as f64,
        mrr / n as f64
    );
}
