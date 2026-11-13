//! Retrieval quality probes (moved verbatim from `engine.rs`).

use super::test_support::*;
use super::{Engine, RetrievalRequest};
use crate::search::projector::Projector;
use crate::search::table::SearchTable;
use ltmrs_domain::id::FrontendId;
use uuid::Uuid;

/// ANN IVF-PQ versus exact on synthetic fixed-seed vectors (T-QUALITY-02
/// probe, ignored: PQ training on 10k rows plus ~140 queries takes minutes).
/// Gate: `LTMRS_PROBE_MODELS` must point at an existing dir (suite run-intent
/// convention; the vectors here are synthetic, so the gate is a marker, not
/// a data dependency). Design: 10_000 unit-norm 384-d vectors from a local
/// xorshift64* (seeds stated below) published as search rows; exact ground
/// truth comes from `vector_query` BEFORE any vector index exists; then an
/// IVF-PQ index (Cosine, matching the query metric) is built and the same
/// queries re-run. Reports Recall@10 ANN-vs-exact (unfiltered + narrow
/// project filter), index build time, and fresh-insert visibility: rows added
/// AFTER the build must be visible to the exact leg; the probe reports
/// whether the ANN leg sees them (plan §8.3: an ANN mode that drops fresh
/// rows fails), then runs `optimize` (folds unindexed tails) and re-checks.
/// Scale verdict: 10k rows is below the plan §8.3 100k target, so this probe
/// is scale-limited evidence, never a T-QUALITY-02 pass. Structural asserts
/// only (row counts, exact-leg top-1 self-match); recall numbers are
/// reported, never gated.
/// Adversarial context: uniform-random vectors are the worst case for IVF-PQ (distances concentrate, no cluster structure) with deliberately untuned defaults as a floor-setting probe; real-E5 distribution is unmeasured.
#[tokio::test]
#[ignore]
async fn ann_ivf_pq_vs_exact_recall_and_freshness() {
    use crate::search::row::SearchRow;
    use ltmrs_domain::id::{
        ChunkId, DocumentRevision, EntityId, ModelFingerprint, StoreGeneration,
    };

    const N_DOCS: usize = 10_000;
    const DIM: usize = 384;
    const N_QUERIES: usize = 100;
    const N_FILTERED: usize = 20;
    const N_FRESH: usize = 100;
    const CORPUS_SEED: u64 = 0x9E3779B97F4A7C15;
    const QUERY_SEED: u64 = 0xBF58476D1CE4E5B9;

    fn next_u64(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        *state = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn unit_vector(state: &mut u64) -> Vec<f32> {
        let mut v: Vec<f64> = (0..DIM)
            .map(|_| next_u64(state) as f64 / u64::MAX as f64 * 2.0 - 1.0)
            .collect();
        let norm: f64 = v.iter().map(|x| x * x).sum::<f64>().sqrt();
        for x in &mut v {
            *x /= norm;
        }
        v.iter().map(|x| *x as f32).collect()
    }

    let models = std::env::var("LTMRS_PROBE_MODELS").unwrap_or_default();
    if models.is_empty() || !std::path::Path::new(&models).exists() {
        eprintln!("SKIP: set LTMRS_PROBE_MODELS to a provisioned models dir");
        return;
    }

    let lance_dir = tempfile::tempdir().unwrap();
    let table = SearchTable::open(lance_dir.path().to_str().unwrap())
        .await
        .unwrap();

    let mut rng = CORPUS_SEED;
    let mut rows = Vec::with_capacity(N_DOCS);
    for i in 0..N_DOCS {
        let text = format!("ann probe doc {i}");
        rows.push(SearchRow {
            store_generation: StoreGeneration::FIRST,
            memory_id: EntityId::new(Uuid::from_u128(i as u128)),
            document_revision: DocumentRevision::new(1),
            model_fingerprint: ModelFingerprint::new(9001),
            chunk_id: ChunkId::new(0),
            chunker_version: "ann-probe-v1".to_string(),
            lexical_text: text.clone(),
            char_start: 0,
            char_end: text.len() as u64,
            project: Some(if i % 2 == 0 {
                "probe-a".into()
            } else {
                "probe-b".into()
            }),
            project_id: None,
            fragment_type: "fact".into(),
            created_at_millis: 1000,
            updated_at_millis: 1000,
            confidence: 0.5,
            embedding: Some(unit_vector(&mut rng)),
        });
    }
    table.publish_rows(&rows).await.unwrap();
    assert_eq!(
        table.count_rows(None).await.unwrap(),
        N_DOCS as u64,
        "all probe rows must publish"
    );

    let mut qrng = QUERY_SEED;
    let queries: Vec<Vec<f32>> = (0..N_QUERIES).map(|_| unit_vector(&mut qrng)).collect();

    async fn top_ids(table: &SearchTable, q: &[f32], filter: Option<&str>) -> Vec<String> {
        table
            .vector_query(q, 10, filter)
            .await
            .unwrap()
            .iter()
            .map(|(row, _)| row.memory_id.as_uuid().to_string())
            .collect()
    }

    // Exact ground truth: no vector index exists yet, so every query is a
    // flat scan.
    let mut exact: Vec<Vec<String>> = Vec::with_capacity(N_QUERIES);
    for q in &queries {
        exact.push(top_ids(&table, q, None).await);
    }
    assert!(exact.iter().all(|r| r.len() == 10));
    let mut exact_f: Vec<Vec<String>> = Vec::with_capacity(N_FILTERED);
    for q in &queries[..N_FILTERED] {
        exact_f.push(top_ids(&table, q, Some("project = 'probe-a'")).await);
    }
    assert!(exact_f.iter().all(|r| r.len() == 10));

    // Build the IVF-PQ index (Cosine: the metric MUST match vector_query).
    use lancedb::index::{Index, vector::IvfPqIndexBuilder};
    let build_start = std::time::Instant::now();
    table
        .table()
        .create_index(
            &["embedding"],
            Index::IvfPq(IvfPqIndexBuilder::default().distance_type(lancedb::DistanceType::Cosine)),
        )
        .execute()
        .await
        .unwrap();
    let build_secs = build_start.elapsed().as_secs_f64();
    let indices = table.table().list_indices().await.unwrap();
    let kinds: Vec<String> = indices
        .iter()
        .map(|c| format!("{:?}", c.index_type))
        .collect();
    eprintln!("vector indices after build: {kinds:?}");
    assert!(
        kinds.iter().any(|k| k == "IvfPq"),
        "IVF-PQ index must be listed, got {kinds:?}"
    );

    fn recall_at_10(ann: &[String], truth: &[String]) -> f64 {
        ann.iter().filter(|id| truth.contains(id)).count() as f64 / 10.0
    }
    let mut recalls = Vec::with_capacity(N_QUERIES);
    for (q, truth) in queries.iter().zip(exact.iter()) {
        recalls.push(recall_at_10(&top_ids(&table, q, None).await, truth));
    }
    let mean: f64 = recalls.iter().sum::<f64>() / recalls.len() as f64;
    let min: f64 = recalls.iter().cloned().fold(1.0, f64::min);
    eprintln!("ANN unfiltered Recall@10: mean={mean:.3} min={min:.3} (n={N_QUERIES})");
    let mut recalls_f = Vec::with_capacity(N_FILTERED);
    for (q, truth) in queries[..N_FILTERED].iter().zip(exact_f.iter()) {
        recalls_f.push(recall_at_10(
            &top_ids(&table, q, Some("project = 'probe-a'")).await,
            truth,
        ));
    }
    let mean_f: f64 = recalls_f.iter().sum::<f64>() / recalls_f.len() as f64;
    eprintln!("ANN filtered Recall@10: mean={mean_f:.3} (n={N_FILTERED}, project = 'probe-a')");
    eprintln!("IVF-PQ build time: {build_secs:.1}s (default partitions/params, Cosine)");

    // Fresh-insert visibility: rows added AFTER the index build.
    let mut fresh_vecs = Vec::with_capacity(N_FRESH);
    for j in 0..N_FRESH {
        let v = unit_vector(&mut rng);
        fresh_vecs.push(v.clone());
        let text = format!("ann probe fresh {j}");
        table
            .publish_rows(&[SearchRow {
                store_generation: StoreGeneration::FIRST,
                memory_id: EntityId::new(Uuid::from_u128((N_DOCS + j) as u128)),
                document_revision: DocumentRevision::new(1),
                model_fingerprint: ModelFingerprint::new(9001),
                chunk_id: ChunkId::new(0),
                chunker_version: "ann-probe-v1".to_string(),
                lexical_text: text.clone(),
                char_start: 0,
                char_end: text.len() as u64,
                project: Some("probe-a".into()),
                project_id: None,
                fragment_type: "fact".into(),
                created_at_millis: 1000,
                updated_at_millis: 1000,
                confidence: 0.5,
                embedding: Some(v),
            }])
            .await
            .unwrap();
    }
    // Fresh-row top-1 check: both this and the ANN top-10 loop below run the
    // post-build ANN path via `vector_query` (no post-build flat-scan exact
    // leg exists; "flat scan" applies only to the pre-build ground-truth
    // phase). The §8.3 conclusion (no fresh-row drop via delta scan) holds
    // for that ANN path.
    let mut exact_fresh = 0usize;
    for (j, v) in fresh_vecs.iter().enumerate() {
        let top = top_ids(&table, v, None).await;
        if top.first().map(|s| s.as_str())
            == Some(Uuid::from_u128((N_DOCS + j) as u128).to_string().as_str())
        {
            exact_fresh += 1;
        }
    }
    assert_eq!(exact_fresh, N_FRESH, "exact leg must see all fresh rows");
    // ANN leg: report, never gate.
    let mut ann_fresh = 0usize;
    for (j, v) in fresh_vecs.iter().enumerate() {
        let top = top_ids(&table, v, None).await;
        if top.contains(&Uuid::from_u128((N_DOCS + j) as u128).to_string()) {
            ann_fresh += 1;
        }
    }
    eprintln!(
        "fresh-insert visibility (pre-optimize): exact={exact_fresh}/{N_FRESH} ann={ann_fresh}/{N_FRESH}"
    );
    table.optimize().await.unwrap();
    let mut ann_fresh_opt = 0usize;
    for (j, v) in fresh_vecs.iter().enumerate() {
        let top = top_ids(&table, v, None).await;
        if top.contains(&Uuid::from_u128((N_DOCS + j) as u128).to_string()) {
            ann_fresh_opt += 1;
        }
    }
    eprintln!("fresh-insert visibility (post-optimize): ann={ann_fresh_opt}/{N_FRESH}");
    eprintln!(
        "ANN probe verdict: scale-limited ({} rows vs 100k plan target); \
         recall and freshness above are evidence, not a T-QUALITY-02 pass",
        N_DOCS + N_FRESH
    );
}
/// Clustered-distribution ANN probe (ignored: needs the pinned E5
/// artifacts + ~300 live embeds). Gate: `LTMRS_PROBE_MODELS` (same
/// convention as the calibration probe). Design: the 300-case synthetic
/// fixture texts (topic-disjoint, paraphrase queries) embedded with real
/// E5-small — a clustered distribution, the complement of the
/// uniform-random floor probe above. Exact ground truth from `vector_query`
/// before any vector index; then IVF-PQ (Cosine, tuned partitions for the
/// 300-row scale) with the same queries re-run. Reports Recall@10
/// ANN-vs-exact + build time. Structural assertions only (row counts);
/// recall numbers are reported, never gated. Scale verdict: 300 rows is
/// far below the plan §8.3 100k target — a distribution data point, never
/// a T-QUALITY-02 pass.
#[tokio::test]
#[ignore]
async fn ann_ivf_pq_clustered_distribution_recall() {
    use crate::search::row::SearchRow;
    use ltmrs_domain::id::{
        ChunkId, DocumentRevision, EntityId, ModelFingerprint, StoreGeneration,
    };
    use ltmrs_embeddings::artifacts::ArtifactCache;
    use ltmrs_embeddings::recipe::Role;
    use ltmrs_embeddings::service::EmbeddingService;

    #[derive(serde::Deserialize)]
    struct FixtureMemory {
        title: String,
        fragment: String,
    }
    #[derive(serde::Deserialize)]
    struct FixtureCase {
        query: String,
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

    let cache = ArtifactCache::new(&models);
    let svc = EmbeddingService::load_e5_small_from_cache(&cache).expect("query service loads");

    let lance_dir = tempfile::tempdir().unwrap();
    let table = SearchTable::open(lance_dir.path().to_str().unwrap())
        .await
        .unwrap();
    // Real E5 passage embeddings: one forward per memory (clustered by
    // topic, unlike the uniform-random floor probe).
    let mut rows = Vec::with_capacity(fixture.memories.len());
    for (i, m) in fixture.memories.iter().enumerate() {
        let text = format!("{} {}", m.title, m.fragment);
        let vec = svc.embed(&text, Role::Passage).await.unwrap();
        assert_eq!(vec.len(), 384);
        assert!(vec.iter().all(|x| x.is_finite()));
        rows.push(SearchRow {
            store_generation: StoreGeneration::FIRST,
            memory_id: EntityId::new(Uuid::from_u128((i + 1) as u128)),
            document_revision: DocumentRevision::new(1),
            model_fingerprint: ModelFingerprint::new(9001),
            chunk_id: ChunkId::new(0),
            chunker_version: "ann-probe-v2".to_string(),
            lexical_text: text.clone(),
            char_start: 0,
            char_end: text.len() as u64,
            project: None,
            project_id: None,
            fragment_type: "fact".into(),
            created_at_millis: 1000,
            updated_at_millis: 1000,
            confidence: 0.5,
            embedding: Some(vec),
        });
    }
    table.publish_rows(&rows).await.unwrap();
    assert_eq!(
        table.count_rows(None).await.unwrap(),
        fixture.memories.len() as u64,
        "all clustered rows must publish"
    );

    // Queries: real E5 query embeddings of the fixture paraphrase queries.
    let mut queries = Vec::with_capacity(fixture.cases.len());
    for case in &fixture.cases {
        let vec = svc.embed(&case.query, Role::Query).await.unwrap();
        queries.push(vec);
    }

    async fn top_ids(table: &SearchTable, q: &[f32]) -> Vec<String> {
        table
            .vector_query(q, 10, None)
            .await
            .unwrap()
            .iter()
            .map(|(row, _)| row.memory_id.as_uuid().to_string())
            .collect()
    }

    // Exact ground truth: no vector index exists yet, flat scan.
    let mut exact: Vec<Vec<String>> = Vec::with_capacity(queries.len());
    for q in &queries {
        exact.push(top_ids(&table, q).await);
    }
    assert!(exact.iter().all(|r| r.len() == 10));

    // IVF-PQ tuned for the 300-row scale (Cosine MUST match vector_query).
    use lancedb::index::{Index, vector::IvfPqIndexBuilder};
    let build_start = std::time::Instant::now();
    table
        .table()
        .create_index(
            &["embedding"],
            Index::IvfPq(
                IvfPqIndexBuilder::default()
                    .distance_type(lancedb::DistanceType::Cosine)
                    .num_partitions(8),
            ),
        )
        .execute()
        .await
        .unwrap();
    let build_secs = build_start.elapsed().as_secs_f64();

    fn recall_at_10(ann: &[String], truth: &[String]) -> f64 {
        ann.iter().filter(|id| truth.contains(id)).count() as f64 / 10.0
    }
    let mut recalls = Vec::with_capacity(queries.len());
    for (q, truth) in queries.iter().zip(exact.iter()) {
        recalls.push(recall_at_10(&top_ids(&table, q).await, truth));
    }
    let mean: f64 = recalls.iter().sum::<f64>() / recalls.len() as f64;
    let min: f64 = recalls.iter().cloned().fold(1.0, f64::min);
    eprintln!(
        "ANN clustered Recall@10: mean={mean:.3} min={min:.3} (n={})",
        queries.len()
    );
    eprintln!("IVF-PQ build time (300 rows, 8 partitions, Cosine): {build_secs:.1}s");
    eprintln!(
        "ANN clustered verdict: distribution data point (300 real-E5 rows vs 10k uniform-random floor 0.176); scale-limited, not a T-QUALITY-02 pass"
    );
}

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
    use std::sync::Arc;

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
    // Production parallel embedder (same unit the daemon wires:
    // ParallelE5Embedder over a capped rayon pool, parity-proven against
    // the sequential bridge). The projector loop stays serial by design
    // (per-entity guards, generation checks); parallelism fans out chunk
    // forwards inside each memory's embed_texts call — identical vectors,
    // N-way throughput. Sequential bridge would take ~10x wall time here.
    let pool = std::sync::Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(8)
            .build()
            .expect("BEIR leg embed pool builds"),
    );
    let adapter = std::sync::Arc::new(
        E5SmallAdapter::load_from_cache(&cache).expect("projection adapter loads"),
    );
    let parallel = crate::search::backend::e5::ParallelE5Embedder::new(adapter, pool);
    // Chunked projection with progress: same one-shot semantics as a
    // single 100000-cap drive (jobs stay pending until resolved), but each
    // 200-doc slice reports elapsed + throughput so a 5k-doc index run is
    // observable instead of silent. Test-side only; production path untouched.
    let mut resolved = 0usize;
    let t_all = std::time::Instant::now();
    // Watchdog: the projector is serial by design, so a 5k-doc backfill is
    // minutes of silence without this. Reports pending-queue depth every
    // 15s from a side thread (test-side only, production path untouched);
    // distinguishes slow drain from a stuck retry loop while it runs.
    let repo_wd = repo.clone();
    std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(15));
            match repo_wd.projection_jobs() {
                Ok(jobs) => eprintln!(
                    "beir watchdog: {pending} jobs pending at {elapsed:.0}s",
                    pending = jobs.len(),
                    elapsed = t0.elapsed().as_secs_f32(),
                ),
                Err(e) => {
                    eprintln!("beir watchdog: queue unreadable ({e:?}); exiting");
                    break;
                }
            }
        }
    });
    while resolved < corpus.len() {
        let t_slice = std::time::Instant::now();
        let n = Projector::project_pending(&repo, &table, Box::new(parallel.clone()), 200)
            .await
            .unwrap();
        resolved += n;
        eprintln!(
            "beir index: {resolved}/{total} docs in {elapsed:.1}s ({rate:.1}/s this slice)",
            total = corpus.len(),
            elapsed = t_all.elapsed().as_secs_f32(),
            rate = n as f32 / t_slice.elapsed().as_secs_f32().max(1e-6),
        );
        if n == 0 {
            break;
        }
    }
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
