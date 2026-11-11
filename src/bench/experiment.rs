//! Storage-only experiments (WP-12 tasks 2-4; T-BENCH-01/02/03).
//!
//! Boundary rules (plan §7.5/§7.6):
//! - Only canonical store ops run inside the measured boundary
//!   (`put_memory_direct`, `get_memories`); no model inference, no daemon,
//!   no frontend. Backend identity and workload digest ride the report so
//!   comparisons stay apples-to-apples (T-BENCH-01).
//! - Durability mode (declared, not equated): each put commits one canonical
//!   write transaction (durable ACK per op, no group commit). fsync/barrier
//!   costs are not separately instrumented; a matched T-BENCH-01 comparison
//!   must equate this mode explicitly rather than assume it.
//! - Every op is timed and recorded to the raw history with its outcome;
//!   summaries derive from the history file, never from live counters.
//! - A missed point read is a successful read with `found=0`, not a
//!   failure: `ok` means the op executed without error.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use crate::bench::{
    GeneratorConfig, Histogram, HistoryRecord, HistoryRecorder, OpGenerator, WorkloadOp,
    config_digest, read_history,
};
use ltmrs_domain::id::{ChunkId, DocumentRevision, EntityId, ModelFingerprint, StoreGeneration};
use ltmrs_domain::memory::{
    FragmentType, Instant as DomainInstant, Memory, MemoryLifecycle, MemorySource,
};
use ltmrs_search::search::table::SearchTable;
use ltmrs_service::repository::CanonicalRepository;

/// Storage experiment configuration (serialized into the report).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExperimentConfig {
    pub name: String,
    pub backend_id: String,
    pub machine: String,
    pub ops: u64,
    pub generator: GeneratorConfig,
}

/// Per-op latency statistics derived from a raw history.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OpStats {
    pub count: u64,
    pub failures: u64,
    pub p50_micros: u64,
    pub p95_micros: u64,
    pub p99_micros: u64,
    pub mean_micros: f64,
    /// Tail-sufficiency marker: tail percentiles rest on few observations
    /// at small n (plan acceptance: insufficient tail samples are marked
    /// insufficient evidence, never silently reported).
    pub note: String,
}

/// Minimum samples for reporting tail percentiles without a caveat.
pub const TAIL_SAMPLE_FLOOR: u64 = 1000;

/// Full experiment summary (written next to the raw history).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExperimentSummary {
    pub provenance: crate::bench::Provenance,
    pub backend_id: String,
    pub op_stats: BTreeMap<String, OpStats>,
    pub total_ops: u64,
    pub total_failures: u64,
    pub wall_micros: u64,
    /// Resident-set delta around the run (Linux procfs; `None` off-Linux
    /// or when the kernel table is unreadable — declared, not fabricated).
    #[serde(default)]
    pub rss_delta_kb: Option<i64>,
}

/// Current process RSS in KiB (Linux `/proc/self/statm` field 2 × page
/// size; `None` off-Linux or on any read/parse failure — the caller
/// records absence, never a zero).
pub fn current_rss_kb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        let page_kb = libc_page_size()? / 1024;
        resident_pages.checked_mul(page_kb)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// OS page size in bytes via libc (already a direct root edge — no new
/// crates). Linux-only caller.
#[cfg(target_os = "linux")]
fn libc_page_size() -> Option<u64> {
    // SAFETY: sysconf(_SC_PAGESIZE) takes no pointers and installs no
    // handlers; it returns -1 only for an invalid name, impossible here.
    let pagesize = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(pagesize).ok()
}

/// RSS delta between two samples. `None` when either endpoint is
/// unavailable — absence is declared, never zero-filled.
pub fn rss_delta_kb(before: Option<u64>, after: Option<u64>) -> Option<i64> {
    match (before, after) {
        (Some(b), Some(a)) => Some(a.saturating_sub(b) as i64),
        _ => None,
    }
}

/// Deterministic workload memory: id and title derive from the key, the
/// fragment pads to `bytes` so value sizes stay workload-controlled.
/// With `salt = Some(seq)`, a unique per-seq trailer rides the fragment
/// (the lemma-comparison salt: same-key payloads differ, so the upstream
/// dedup gate cannot collapse the write path). `None` is the unsalted
/// contract: same key gives byte-identical content.
pub fn salted_workload_memory(key: u64, bytes: usize, salt: Option<u64>) -> Memory {
    let mut memory = workload_memory(key, bytes);
    if let Some(seq) = salt {
        memory.fragment.push_str(&format!(" Salt seq {seq}."));
    }
    memory
}

#[allow(dead_code)]
/// Unsalted constructor (dedup-collapsible by design): id and title derive
/// from the key, the fragment pads to `bytes`.
pub fn workload_memory(key: u64, bytes: usize) -> Memory {
    use ltmrs_domain::id::{DocumentRevision, EligibilityRevision, EntityRevision};
    let title = format!("Workload {key}");
    let mut fragment = format!("{title} body text. ");
    while fragment.len() < bytes {
        fragment.push_str("Workload filler sentence. ");
    }
    Memory {
        id: EntityId::new(uuid::Uuid::from_u128(u128::from(key))),
        external_alias: None,
        title,
        fragment,
        description: String::new(),
        fragment_type: FragmentType::Fact,
        project: None,
        project_id: None,
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
    }
}

/// Exported op for cross-implementation runs: the exact stream both sides
/// execute (consumed by tools/bench_against_lemma.mjs). Put payloads are
/// embedded so the upstream side needs no Rust code; search queries are
/// pre-rendered so term derivation cannot drift between sides. RMW ops
/// export as revision-checked updates (`kind: "rmw"`); the driver maps
/// them onto the freshest read + conditional patch of the same key.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ExportedOp {
    pub seq: u64,
    pub kind: String,
    pub key: u64,
    pub title: String,
    pub fragment: String,
    pub query: String,
    pub top_k: usize,
}

/// Write `n` stream ops as JSONL (one object per line). Returns the count.
/// Unsalted export: put/RMW payloads derive from the key alone, so every
/// seq touching the same key carries byte-identical content (dedup-gated
/// upstream comparisons refuse the repeats; see the salted variant).
pub fn export_workload_ops(
    generator: &GeneratorConfig,
    n: u64,
    path: &Path,
) -> std::io::Result<u64> {
    export_workload_ops_salted(generator, n, path, false)
}

/// Salted export: when `salted`, every put/RMW fragment carries a
/// per-seq unique trailer (`seq {seq}`), so same-key payloads differ and
/// upstream dedup gates cannot collapse the write path. Unsalted output
/// is byte-identical to [`export_workload_ops`] with `salted = false`.
/// Both sides of a comparison must agree on the flag: salt changes the
/// payload bytes, so salted and unsalted streams are NOT comparable.
pub fn export_workload_ops_salted(
    generator: &GeneratorConfig,
    n: u64,
    path: &Path,
    salted: bool,
) -> std::io::Result<u64> {
    use std::io::Write;
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut stream = OpGenerator::new(generator.clone());
    for seq in 0..n {
        let exported = match stream.next_op() {
            WorkloadOp::Put { key, bytes } => {
                let memory = salted_workload_memory(key, bytes, salted.then_some(seq));
                ExportedOp {
                    seq,
                    kind: "put".to_string(),
                    key,
                    title: memory.title,
                    fragment: memory.fragment,
                    query: String::new(),
                    top_k: 0,
                }
            }
            WorkloadOp::Rmw { key, bytes } => {
                let memory = salted_workload_memory(key, bytes, salted.then_some(seq));
                ExportedOp {
                    seq,
                    kind: "rmw".to_string(),
                    key,
                    title: memory.title,
                    fragment: memory.fragment,
                    query: String::new(),
                    top_k: 0,
                }
            }
            WorkloadOp::Get { key } => ExportedOp {
                seq,
                kind: "get".to_string(),
                key,
                title: String::new(),
                fragment: String::new(),
                query: String::new(),
                top_k: 0,
            },
            WorkloadOp::Search { probe, top_k } => ExportedOp {
                seq,
                kind: "search".to_string(),
                key: probe,
                title: String::new(),
                fragment: String::new(),
                query: format!("workload {probe}"),
                top_k,
            },
        };
        let mut line = serde_json::to_string(&exported)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        out.write_all(line.as_bytes())?;
    }
    out.flush()?;
    Ok(n)
}

/// Short op name for grouping (`put`, `get`, `rmw`, `search`).
pub fn op_name(op: &WorkloadOp) -> &'static str {
    match op {
        WorkloadOp::Put { .. } => "put",
        WorkloadOp::Get { .. } => "get",
        WorkloadOp::Rmw { .. } => "rmw",
        WorkloadOp::Search { .. } => "search",
    }
}

/// Derive per-op statistics from raw history records (pure: no I/O, no clock).
pub fn summarize(records: &[HistoryRecord]) -> BTreeMap<String, OpStats> {
    let mut latencies: BTreeMap<String, Histogram> = BTreeMap::new();
    let mut failures: BTreeMap<String, u64> = BTreeMap::new();
    for record in records {
        let name = op_name(&record.op).to_string();
        latencies
            .entry(name.clone())
            .or_default()
            .push(record.elapsed_micros);
        if !record.ok {
            *failures.entry(name).or_default() += 1;
        }
    }
    latencies
        .into_iter()
        .map(|(name, histogram)| {
            let count = histogram.len() as u64;
            let stats = OpStats {
                count,
                failures: failures.get(&name).copied().unwrap_or(0),
                p50_micros: histogram.percentile(50.0).unwrap_or(0),
                p95_micros: histogram.percentile(95.0).unwrap_or(0),
                p99_micros: histogram.percentile(99.0).unwrap_or(0),
                mean_micros: histogram.mean().unwrap_or(0.0),
                note: if count < TAIL_SAMPLE_FLOOR {
                    format!("tail percentiles indicative only (n={count} < {TAIL_SAMPLE_FLOOR})")
                } else {
                    String::new()
                },
            };
            (name, stats)
        })
        .collect()
}

/// Run a put/get workload against the canonical store, recording every
/// executed op. `Search` ops are skipped here (they need a populated search
/// table; see the FTS leg) and counted in the returned skip total.
pub fn run_put_get(
    repo: &CanonicalRepository,
    config: &ExperimentConfig,
    history_path: &Path,
) -> std::io::Result<(ExperimentSummary, u64)> {
    let rss_before = current_rss_kb();
    let wall = Instant::now();
    let mut recorder = HistoryRecorder::create(history_path)?;
    let mut generator = OpGenerator::new(config.generator.clone());
    let mut skipped = 0u64;
    for _ in 0..config.ops {
        match generator.next_op() {
            WorkloadOp::Search { .. } => {
                skipped += 1;
            }
            op => {
                let (elapsed, ok, detail) = crate::bench::load::execute_storage_op(repo, &op);
                recorder.record(op, elapsed, ok, &detail)?;
            }
        }
    }
    recorder.flush()?;
    let records = read_history(history_path)?;
    let op_stats = summarize(&records);
    let total_failures = op_stats.values().map(|s| s.failures).sum();
    let digest = config_digest(
        config.generator.seed,
        config.generator.puts_per_1000,
        config.generator.key_space,
        config.generator.value_bytes,
        config.generator.top_k,
        config.ops,
    );
    Ok((
        ExperimentSummary {
            provenance: crate::bench::Provenance::collect(
                &config.machine,
                config.generator.seed,
                digest,
            ),
            backend_id: config.backend_id.clone(),
            total_ops: records.len() as u64,
            total_failures,
            wall_micros: wall.elapsed().as_micros().min(u128::from(u64::MAX)) as u64,
            op_stats,
            rss_delta_kb: rss_delta_kb(rss_before, current_rss_kb()),
        },
        skipped,
    ))
}

/// Lexical search row for a workload key: deterministic ids and revisions,
/// embedding-free (storage-only: the FTS leg never runs inference).
pub fn search_row_for(key: u64, text: &str) -> ltmrs_search::search::row::SearchRow {
    ltmrs_search::search::row::SearchRow {
        store_generation: StoreGeneration::FIRST,
        memory_id: EntityId::new(uuid::Uuid::from_u128(u128::from(key))),
        document_revision: DocumentRevision::new(1),
        model_fingerprint: ModelFingerprint::new(1),
        chunk_id: ChunkId::new(0),
        chunker_version: "bench-single-chunk-v1".to_string(),
        lexical_text: text.to_string(),
        char_start: 0,
        char_end: text.len() as u64,
        project: None,
        project_id: None,
        fragment_type: "fact".to_string(),
        created_at_millis: 1000,
        confidence: 0.5,
        updated_at_millis: 1000,
        embedding: None,
    }
}

/// Run lexical search ops against a populated table, recording every probe.
/// A zero-hit query is a successful search with `found=0`, not a failure.
/// Refuses when no FTS index exists: index-less graceful-empty results
/// would otherwise masquerade as fast zero-hit evidence.
pub async fn run_search(
    table: &SearchTable,
    queries: &[(u64, String, usize)],
    recorder: &mut HistoryRecorder,
) -> std::io::Result<u64> {
    if !table
        .fts_index_ready()
        .await
        .map_err(|e| std::io::Error::other(e.message.clone()))?
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no FTS index: create it before the search leg",
        ));
    }
    let mut executed = 0u64;
    for (probe, terms, top_k) in queries {
        let start = Instant::now();
        let outcome = table.fts_query(terms, *top_k, None).await;
        let elapsed = start.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        let (ok, detail) = match outcome {
            Ok(rows) => (true, format!("found={}", rows.len())),
            Err(e) => (false, e.message.clone()),
        };
        record_search_outcome(recorder, *probe, *top_k, elapsed, ok, &detail)?;
        executed += 1;
    }
    Ok(executed)
}

/// Shared per-probe record path (single-threaded [`run_search`] and the
/// threaded [`run_search_threaded`] leg record identically).
pub fn record_search_outcome(
    recorder: &mut HistoryRecorder,
    probe: u64,
    top_k: usize,
    elapsed_micros: u64,
    ok: bool,
    detail: &str,
) -> std::io::Result<()> {
    recorder.record(
        WorkloadOp::Search { probe, top_k },
        elapsed_micros,
        ok,
        detail,
    )
}

/// Threaded search-load leg: `workers` OS threads share one [`SearchTable`]
/// handle and issue `queries` round-robin; each thread blocks on its own
/// single-thread Tokio runtime, so no query future ever crosses an executor
/// and the only shared state is the proved `Send + Sync` table handle
/// itself. Returns per-query measurements in input order. The
/// index-readiness gate runs first on the calling thread: without an FTS
/// index this refuses rather than recording fast zero-hit evidence.
pub fn run_search_threaded(
    table: &SearchTable,
    queries: &[(u64, String, usize)],
    workers: usize,
) -> std::io::Result<Vec<crate::bench::load::MeasuredOp>> {
    use crate::bench::load::MeasuredOp;
    if queries.is_empty() {
        return Ok(Vec::new());
    }
    assert!(workers >= 1, "at least one worker");
    let workers = workers.min(queries.len());
    let gate_table = table.clone();
    let gate = std::thread::Builder::new()
        .name("bench-search-gate".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(std::io::Error::other)?;
            runtime
                .block_on(async { gate_table.fts_index_ready().await })
                .map_err(|e| std::io::Error::other(e.message.clone()))
        })
        .map_err(std::io::Error::other)?;
    if !gate.join().expect("search gate thread panicked")? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no FTS index: create it before the search leg",
        ));
    }
    let mut out: Vec<MeasuredOp> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for worker in 0..workers {
            let table = table.clone();
            handles.push(
                std::thread::Builder::new()
                    .name(format!("bench-search-{worker}"))
                    .spawn_scoped(scope, move || {
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .expect("bench search worker runtime builds");
                        let mut local = Vec::new();
                        let mut index = worker;
                        while index < queries.len() {
                            let (probe, terms, top_k) = &queries[index];
                            let start = std::time::Instant::now();
                            let outcome = runtime.block_on(table.fts_query(terms, *top_k, None));
                            let elapsed =
                                start.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
                            let (ok, detail) = match outcome {
                                Ok(rows) => (true, format!("found={}", rows.len())),
                                Err(e) => (false, e.message.clone()),
                            };
                            local.push(MeasuredOp {
                                index,
                                op: WorkloadOp::Search {
                                    probe: *probe,
                                    top_k: *top_k,
                                },
                                elapsed_micros: elapsed,
                                queued_micros: 0,
                                ok,
                                detail,
                                retries: 0,
                                conflicts: 0,
                            });
                            index += workers;
                        }
                        local
                    })
                    .expect("bench search worker spawns"),
            );
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("search worker panicked"))
            .collect::<Vec<Vec<MeasuredOp>>>()
            .into_iter()
            .flatten()
            .collect()
    });
    out.sort_by_key(|m| m.index);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resource sampling reports RSS around a workload (Linux procfs;
    /// `None` off-Linux — declared, not fabricated).
    #[test]
    fn resource_sample_reports_rss_when_available() {
        let before = current_rss_kb();
        // Touch memory so the sampler has something real to observe.
        let v: Vec<u8> = vec![7u8; 1 << 20];
        assert!(!v.is_empty());
        let after = current_rss_kb();
        #[cfg(target_os = "linux")]
        {
            let (Some(b), Some(a)) = (before, after) else {
                panic!("RSS sampling must succeed on Linux: {before:?} -> {after:?}");
            };
            assert!(a >= b, "RSS must not shrink: {b} -> {a}");
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert!(
                before.is_none() && after.is_none(),
                "off-Linux sampler must report None"
            );
        }
    }
    fn record(op: WorkloadOp, elapsed_micros: u64, ok: bool) -> HistoryRecord {
        HistoryRecord {
            seq: 0,
            op,
            elapsed_micros,
            ok,
            detail: String::new(),
            queued_micros: 0,
            retries: 0,
            conflicts: 0,
        }
    }

    /// Summaries group by op with exact percentiles and failure counts.
    #[test]
    fn summarize_groups_ops_and_percentiles() {
        let records = vec![
            record(WorkloadOp::Put { key: 1, bytes: 8 }, 10, true),
            record(WorkloadOp::Put { key: 2, bytes: 8 }, 20, true),
            record(WorkloadOp::Put { key: 3, bytes: 8 }, 30, true),
            record(WorkloadOp::Get { key: 1 }, 5, false),
        ];
        let stats = summarize(&records);
        let put = &stats["put"];
        assert_eq!((put.count, put.failures), (3, 0));
        assert_eq!(
            (put.p50_micros, put.p95_micros, put.p99_micros),
            (20, 30, 30)
        );
        assert!((put.mean_micros - 20.0).abs() < 1e-9);
        let get = &stats["get"];
        assert_eq!((get.count, get.failures), (1, 1));
    }

    /// Workload memories are deterministic and size-controlled.
    #[test]
    fn workload_memory_is_deterministic() {
        let a = workload_memory(42, 256);
        let b = workload_memory(42, 256);
        assert_eq!(a, b, "same key must give the same record");
        assert_eq!(a.id, EntityId::new(uuid::Uuid::from_u128(42)));
        assert!(a.title.contains("42"), "title must name the key");
        assert!(
            a.fragment.len() >= 256,
            "fragment must honor the size floor, got {}",
            a.fragment.len()
        );
        let c = workload_memory(43, 256);
        assert_ne!(a.id, c.id);
    }

    /// The driver records every emitted op in order; totals match the summary.
    #[test]
    fn run_put_get_records_every_op() {
        let dir = tempfile::tempdir().unwrap();
        let repo = CanonicalRepository::open(dir.path().join("store").to_str().unwrap()).unwrap();
        let config = ExperimentConfig {
            name: "slice-b1".to_string(),
            backend_id: "test".to_string(),
            machine: "test".to_string(),
            ops: 50,
            generator: GeneratorConfig {
                seed: 7,
                puts_per_1000: 600,
                key_space: 25,
                ..Default::default()
            },
        };
        let history = dir.path().join("history.jsonl");
        let (summary, skipped) = run_put_get(&repo, &config, &history).unwrap();
        assert_eq!(summary.total_ops + skipped, 50, "every op accounted for");
        assert_eq!(
            summary.total_failures, 0,
            "fresh store, no conflicts expected"
        );
        let back = crate::bench::read_history(&history).unwrap();
        assert_eq!(back.len() as u64, summary.total_ops);
        for (i, record) in back.iter().enumerate() {
            assert_eq!(record.seq, i as u64, "seq must be dense");
        }
        let counted: u64 = summary.op_stats.values().map(|s| s.count).sum();
        assert_eq!(counted, summary.total_ops);
        assert_eq!(summary.backend_id, "test");
    }

    /// The FTS leg records every probe with its hit count; zero-hit queries
    /// succeed with `found=0`.
    #[tokio::test]
    async fn fts_leg_records_searches() {
        let dir = tempfile::tempdir().unwrap();
        let table = SearchTable::open(dir.path().join("search").to_str().unwrap())
            .await
            .unwrap();
        table
            .publish_rows(&[
                search_row_for(1, "alpha bravo charlie"),
                search_row_for(2, "delta echo foxtrot"),
            ])
            .await
            .unwrap();
        table.create_fts_index().await.unwrap();
        let history = dir.path().join("search.jsonl");
        let mut recorder = HistoryRecorder::create(&history).unwrap();
        let executed = run_search(
            &table,
            &[
                (0, "bravo".to_string(), 5),
                (1, "zzz-no-such-token".to_string(), 5),
            ],
            &mut recorder,
        )
        .await
        .unwrap();
        assert_eq!(executed, 2);
        recorder.flush().unwrap();
        let back = crate::bench::read_history(&history).unwrap();
        assert_eq!(back.len(), 2);
        assert!(back[0].ok, "hit query must succeed");
        let found: usize = back[0].detail["found=".len()..].parse().unwrap();
        assert!(
            found >= 1,
            "distinctive term must hit, got: {}",
            back[0].detail
        );
        assert!(back[1].ok, "zero-hit query still succeeds");
        assert!(
            back[1].detail.contains("found=0"),
            "got: {}",
            back[1].detail
        );
    }

    /// The exported stream is the contract with the upstream driver: same
    /// seed gives byte-identical files, seqs are dense, payloads match the
    /// in-process workload constructors.
    #[test]
    fn export_workload_ops_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let generator = GeneratorConfig {
            seed: 7,
            puts_per_1000: 600,
            key_space: 500,
            value_bytes: 64,
            top_k: 10,
            ..Default::default()
        };
        let a = dir.path().join("a.jsonl");
        let b = dir.path().join("b.jsonl");
        assert_eq!(export_workload_ops(&generator, 200, &a).unwrap(), 200);
        assert_eq!(export_workload_ops(&generator, 200, &b).unwrap(), 200);
        assert_eq!(
            std::fs::read(&a).unwrap(),
            std::fs::read(&b).unwrap(),
            "same seed must export byte-identical streams"
        );
        let raw = std::fs::read_to_string(&a).unwrap();
        let ops: Vec<ExportedOp> = raw
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(ops.len(), 200);
        for (i, op) in ops.iter().enumerate() {
            assert_eq!(op.seq, i as u64, "seqs must be dense");
        }
        let first_put = ops.iter().find(|op| op.kind == "put").unwrap();
        let expected = workload_memory(first_put.key, 64);
        assert_eq!(first_put.title, expected.title);
        assert_eq!(first_put.fragment, expected.fragment);
        for op in ops.iter().filter(|op| op.kind == "search") {
            assert_eq!(op.query, format!("workload {}", op.key));
            assert_eq!(op.top_k, 10);
        }
    }

    /// Salted exports defeat same-key dedup collapse: every put/RMW
    /// fragment carries its seq trailer; unsalted output is unchanged.
    #[test]
    fn salted_export_defeats_dedup_collapse() {
        let dir = tempfile::tempdir().unwrap();
        let generator = GeneratorConfig {
            seed: 7,
            puts_per_1000: 1000,
            key_space: 4,
            value_bytes: 64,
            top_k: 10,
            ..Default::default()
        };
        let salted = dir.path().join("salted.jsonl");
        assert_eq!(
            export_workload_ops_salted(&generator, 60, &salted, true).unwrap(),
            60
        );
        let raw = std::fs::read_to_string(&salted).unwrap();
        let ops: Vec<ExportedOp> = raw
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()
            .unwrap();
        let puts: Vec<&ExportedOp> = ops.iter().filter(|op| op.kind == "put").collect();
        assert!(!puts.is_empty(), "all-put stream must export puts");
        for op in &puts {
            assert!(
                op.fragment.contains(&format!("Salt seq {}.", op.seq)),
                "put seq {} must carry its salt trailer",
                op.seq
            );
        }
        // Same key, different seqs: fragments differ (dedup cannot collapse).
        let mut by_key: std::collections::BTreeMap<u64, Vec<&str>> =
            std::collections::BTreeMap::new();
        for op in &puts {
            by_key.entry(op.key).or_default().push(&op.fragment);
        }
        let repeated = by_key
            .values()
            .any(|fragments| fragments.len() > 1 && fragments.windows(2).any(|w| w[0] != w[1]));
        assert!(repeated, "salted same-key payloads must differ");
        // Unsalted export is byte-identical to the legacy path.
        let plain = dir.path().join("plain.jsonl");
        let legacy = dir.path().join("legacy.jsonl");
        assert_eq!(
            export_workload_ops_salted(&generator, 60, &plain, false).unwrap(),
            60
        );
        assert_eq!(export_workload_ops(&generator, 60, &legacy).unwrap(), 60);
        assert_eq!(
            std::fs::read(&plain).unwrap(),
            std::fs::read(&legacy).unwrap(),
            "unsalted export must match the legacy path byte-for-byte"
        );
        assert!(
            !std::fs::read_to_string(&plain)
                .unwrap()
                .contains("Salt seq"),
            "unsalted export must not salt"
        );
    }

    /// Salted helper: `None` is the unsalted contract (key-identical
    /// content); `Some(seq)` appends the unique trailer.
    #[test]
    fn salted_memory_helper_contract() {
        let a = salted_workload_memory(9, 64, None);
        let b = salted_workload_memory(9, 64, None);
        assert_eq!(a.fragment, b.fragment, "unsalted must be key-deterministic");
        assert_eq!(a.fragment, workload_memory(9, 64).fragment);
        let salted = salted_workload_memory(9, 64, Some(3));
        assert_ne!(salted.fragment, a.fragment);
        assert!(salted.fragment.contains("Salt seq 3."));
    }

    /// Threaded search leg matches single-threaded results hit-for-hit and
    /// preserves input order; without an index it refuses loudly.
    #[tokio::test]
    async fn threaded_search_matches_single_thread() {
        let dir = tempfile::tempdir().unwrap();
        let table = SearchTable::open(dir.path().join("search").to_str().unwrap())
            .await
            .unwrap();
        table
            .publish_rows(&[
                search_row_for(1, "alpha bravo charlie"),
                search_row_for(2, "delta echo foxtrot"),
            ])
            .await
            .unwrap();
        table.create_fts_index().await.unwrap();
        let queries = vec![
            (0u64, "bravo".to_string(), 5),
            (1u64, "zzz-no-such-token".to_string(), 5),
            (2u64, "delta".to_string(), 5),
        ];
        let history = dir.path().join("single.jsonl");
        let mut recorder = HistoryRecorder::create(&history).unwrap();
        let executed = run_search(&table, &queries, &mut recorder).await.unwrap();
        recorder.flush().unwrap();
        assert_eq!(executed, 3);
        drop(recorder);
        let single = read_history(&history).unwrap();
        let threaded = run_search_threaded(&table, &queries, 2).unwrap();
        assert_eq!(threaded.len(), 3, "every query measured");
        for (i, m) in threaded.iter().enumerate() {
            assert_eq!(m.index, i, "input order preserved");
            assert_eq!(m.op, single[i].op);
            assert_eq!(
                (m.ok, m.detail.clone()),
                (single[i].ok, single[i].detail.clone())
            );
        }
        // No index: loud refusal, never fast zero-hit evidence.
        let bare = SearchTable::open(dir.path().join("bare").to_str().unwrap())
            .await
            .unwrap();
        let err = run_search_threaded(&bare, &queries, 2).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    /// Op names group the new leg (`rmw` joins `put`/`get`/`search`).
    #[test]
    fn op_name_covers_rmw() {
        assert_eq!(op_name(&WorkloadOp::Rmw { key: 1, bytes: 8 }), "rmw");
        assert_eq!(op_name(&WorkloadOp::Put { key: 1, bytes: 8 }), "put");
        assert_eq!(op_name(&WorkloadOp::Get { key: 1 }), "get");
        assert_eq!(
            op_name(&WorkloadOp::Search { probe: 1, top_k: 5 }),
            "search"
        );
    }

    /// Export the reference comparison stream (slice E): same config as the
    /// wp12-storage-01 ltmrs run so the upstream comparison is stream-identical.
    /// Ignored in normal runs; invoke explicitly.
    #[test]
    #[ignore]
    fn export_reference_stream() {
        let out = crate::bench::crate_root().join("reports/wp12-lemma-01");
        std::fs::create_dir_all(&out).unwrap();
        let generator = GeneratorConfig {
            seed: 7,
            puts_per_1000: 600,
            key_space: 500,
            value_bytes: 256,
            top_k: 10,
            ..Default::default()
        };
        let n = export_workload_ops(&generator, 2000, &out.join("ops.jsonl")).unwrap();
        assert_eq!(n, 2000);
    }

    /// Reference storage experiment (WP-12 task 2): ignored in normal runs.
    /// Execute explicitly to (re)produce the raw evidence behind
    /// benchmarks.toml: `cargo test -- --ignored storage_experiment_reference_run`.
    /// Raw histories + summaries land under reports/wp12-storage-01/
    /// (gitignored); the committed benchmarks.toml specifies the workload.
    /// Precondition: the report dir is removed first, so every run measures
    /// a fresh store (re-runs never inherit a dirty LSM layout).
    #[tokio::test]
    #[ignore]
    async fn storage_experiment_reference_run() {
        use ltmrs_search::search::projector::render_text;
        let out = crate::bench::crate_root().join("reports/wp12-storage-01");
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(out.join("store")).unwrap();
        std::fs::create_dir_all(out.join("search")).unwrap();
        let machine =
            std::env::var("LTMRS_BENCH_MACHINE").unwrap_or_else(|_| "dev-box".to_string());
        let repo = CanonicalRepository::open(out.join("store").to_str().unwrap()).unwrap();
        let config = ExperimentConfig {
            name: "wp12-storage-01".to_string(),
            backend_id: "fjall-canonical".to_string(),
            machine: machine.clone(),
            ops: 2000,
            generator: GeneratorConfig {
                seed: 7,
                puts_per_1000: 600,
                key_space: 500,
                value_bytes: 256,
                top_k: 10,
                ..Default::default()
            },
        };
        let history_putget = out.join("history-putget.jsonl");
        let (summary, skipped) = run_put_get(&repo, &config, &history_putget).unwrap();
        assert_eq!(summary.total_ops + skipped, config.ops);
        std::fs::write(
            out.join("summary-putget.json"),
            serde_json::to_vec_pretty(&summary).unwrap(),
        )
        .unwrap();
        // Seed the FTS table by replaying the workload key range through the
        // canonical text rendering (lexical rows only, no embeddings).
        let table = SearchTable::open(out.join("search").to_str().unwrap())
            .await
            .unwrap();
        let mut rows = Vec::new();
        for key in 0..config.generator.key_space {
            let memory = workload_memory(key, config.generator.value_bytes);
            rows.push(search_row_for(
                key,
                &render_text(&memory.title, &memory.fragment),
            ));
        }
        table.publish_rows(&rows).await.unwrap();
        table.create_fts_index().await.unwrap();
        // Collect the Search ops from the same stream (skipped by run_put_get).
        let mut generator = OpGenerator::new(config.generator.clone());
        let mut queries = Vec::new();
        for _ in 0..config.ops {
            if let WorkloadOp::Search { probe, top_k } = generator.next_op() {
                queries.push((probe, format!("workload {probe}"), top_k));
            }
        }
        assert_eq!(
            queries.len() as u64,
            skipped,
            "search ops must match the skip count"
        );
        let history_search = out.join("history-search.jsonl");
        let mut recorder = HistoryRecorder::create(&history_search).unwrap();
        let wall = std::time::Instant::now();
        let executed = run_search(&table, &queries, &mut recorder).await.unwrap();
        let wall_micros = wall.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        recorder.flush().unwrap();
        let search_records = read_history(&history_search).unwrap();
        assert_eq!(search_records.len() as u64, executed);
        // Seed-pinned stream against the replayed key range: every probe
        // must hit (a zero-hit run would be latency without retrieval).
        for record in &search_records {
            assert!(
                record.detail != "found=0",
                "probe {} found nothing",
                match &record.op {
                    WorkloadOp::Search { probe, .. } => *probe,
                    _ => u64::MAX,
                }
            );
        }
        let op_stats = summarize(&search_records);
        let search_summary = ExperimentSummary {
            provenance: crate::bench::Provenance::collect(
                &machine,
                config.generator.seed,
                config_digest(
                    config.generator.seed,
                    config.generator.puts_per_1000,
                    config.generator.key_space,
                    config.generator.value_bytes,
                    config.generator.top_k,
                    config.ops,
                ),
            ),
            backend_id: "lance-fts".to_string(),
            total_ops: executed,
            total_failures: op_stats.values().map(|s| s.failures).sum(),
            wall_micros,
            op_stats,
            rss_delta_kb: None,
        };
        std::fs::write(
            out.join("summary-search.json"),
            serde_json::to_vec_pretty(&search_summary).unwrap(),
        )
        .unwrap();
    }
}
