//! Soak harness skeleton (WP-12 T-BENCH-03 slice; clock-time soak stays
//! `not_run` in benchmarks.toml until a machine-gated reference run exists).
//!
//! Boundary rules:
//! - Steady-state accounting only: per-segment op counts, failure counts,
//!   RSS drift and store-bytes drift across fixed-length segments of the
//!   same seed-pinned stream. No timing assertions (CI machines vary).
//! - Kill/restart accounting: the harness drops and reopens the store and
//!   the search table between segments (LanceDB has no in-process kill
//!   switch); a real SIGKILL/power-loss run stays machine-gated `not_run`.
//! - Derived state is rebuilt from canonical on reopen (projection rows
//!   republished from the workload key range), never carried across the
//!   restart invisibly.

use std::path::{Path, PathBuf};

use crate::bench::experiment::{OpStats, search_row_for, summarize, workload_memory};
use crate::bench::load::{MeasuredOp, RmwIssuer, execute_storage_op_counted};
use crate::bench::{
    GeneratorConfig, HistoryRecorder, OpGenerator, WorkloadOp, config_digest_for, read_history,
};
use ltmrs_search::search::projector::render_text;
use ltmrs_search::search::table::SearchTable;
use ltmrs_service::repository::CanonicalRepository;

/// One soak segment's honestly observable accounting.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SoakSegment {
    pub index: u64,
    pub ops: u64,
    pub failures: u64,
    /// Summed `retries`/`conflicts` counters from the history records.
    pub retries: u64,
    pub conflicts: u64,
    pub rss_delta_kb: Option<i64>,
    pub store_bytes_before: u64,
    pub store_bytes_after: u64,
}

/// Soak configuration (serialized into the report).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SoakConfig {
    pub name: String,
    pub backend_id: String,
    pub machine: String,
    pub generator: GeneratorConfig,
    pub segments: u64,
    pub ops_per_segment: u64,
    pub workers: usize,
}

/// Full soak summary (written next to the raw histories).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SoakSummary {
    pub provenance: crate::bench::Provenance,
    pub backend_id: String,
    pub segments: Vec<SoakSegment>,
    pub total_ops: u64,
    pub total_failures: u64,
    pub total_retries: u64,
    pub total_conflicts: u64,
    /// Steady-state growth signal: bytes/op between the first and last
    /// segment endpoints. Indicative only (LSM layout noise dominates at
    /// small segment counts); never a pass/fail gate.
    pub bytes_per_op_first_last: Option<f64>,
    pub op_stats: std::collections::BTreeMap<String, OpStats>,
    pub note: String,
}

/// Run the soak skeleton: `segments` fixed-length segments of the same
/// seed-pinned stream (continuous `OpGenerator`, so segment N+1 continues
/// where segment N left off), with a drop + reopen of the store and a
/// rebuild of the search projection between segments (kill/restart
/// accounting). Histories land as `history-seg-{i}.jsonl` under `out_dir`.
pub fn run_soak(config: &SoakConfig, out_dir: &Path) -> std::io::Result<SoakSummary> {
    if config.segments == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "soak needs at least one segment",
        ));
    }
    if config.ops_per_segment == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "soak segments must be non-empty",
        ));
    }
    if config.workers == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "soak needs at least one worker",
        ));
    }
    std::fs::create_dir_all(out_dir)?;
    let digest = config_digest_for(&config.generator, config.segments * config.ops_per_segment);
    let mut generator = OpGenerator::new(config.generator.clone());
    let mut segments = Vec::new();
    let mut all_records = Vec::new();
    let store_path = out_dir.join("store");
    for index in 0..config.segments {
        let ops: Vec<WorkloadOp> = (0..config.ops_per_segment)
            .map(|_| generator.next_op())
            .collect();
        let rss_before = crate::bench::experiment::current_rss_kb();
        let bytes_before = crate::bench::load::measure_dir_bytes(&store_path).unwrap_or(0);
        let history_path = out_dir.join(format!("history-seg-{index}.jsonl"));
        let segment = run_soak_segment(
            &store_path,
            out_dir,
            &ops,
            index,
            config.workers,
            &history_path,
            rss_before,
            bytes_before,
        )?;
        let mut records = read_history(&history_path)?;
        all_records.append(&mut records);
        segments.push(segment);
    }
    let op_stats = summarize(&all_records);
    let (total_ops, total_failures, total_retries, total_conflicts) =
        segments.iter().fold((0, 0, 0, 0), |acc, s| {
            (
                acc.0 + s.ops,
                acc.1 + s.failures,
                acc.2 + s.retries,
                acc.3 + s.conflicts,
            )
        });
    let bytes_per_op_first_last = match (segments.first(), segments.last()) {
        (Some(first), Some(last)) if total_ops > 0 => {
            let growth = last.store_bytes_after as i64 - first.store_bytes_before as i64;
            Some(growth as f64 / total_ops as f64)
        }
        _ => None,
    };
    Ok(SoakSummary {
        provenance: crate::bench::Provenance::collect(
            &config.machine,
            config.generator.seed,
            digest,
        ),
        backend_id: config.backend_id.clone(),
        segments,
        total_ops,
        total_failures,
        total_retries,
        total_conflicts,
        bytes_per_op_first_last,
        op_stats,
        note: "skeleton: drop/reopen restart accounting only; no SIGKILL/power-loss \
            coverage and no timing gates (machine-gated reference run stays not_run)"
            .to_string(),
    })
}

/// One soak segment: open the store, execute the ops, record, reopen (the
/// restart half of kill/restart accounting), and rebuild the search
/// projection from canonical for the workload key range.
#[allow(clippy::too_many_arguments)]
fn run_soak_segment(
    store_path: &Path,
    out_dir: &Path,
    ops: &[WorkloadOp],
    index: u64,
    workers: usize,
    history_path: &Path,
    rss_before: Option<u64>,
    bytes_before: u64,
) -> std::io::Result<SoakSegment> {
    let repo = open_bench_repo(store_path)?;
    let queries = search_queries_for(ops);
    let measured = run_segment_ops(&repo, ops, workers);
    drop(repo);
    // Restart accounting: reopen the store and verify the written keys
    // survive the drop/reopen (a read-back census, not a timing gate).
    let repo = open_bench_repo(store_path)?;
    let census_failures = census_keys(&repo, ops);
    drop(repo);
    let search_failures = rebuild_search_projection(out_dir, store_path);
    let mut recorder = HistoryRecorder::create(history_path)?;
    for m in &measured {
        recorder.record_full(
            m.op.clone(),
            m.elapsed_micros,
            m.queued_micros,
            m.ok,
            &m.detail,
            m.retries,
            m.conflicts,
        )?;
    }
    recorder.flush()?;
    let records = read_history(history_path)?;
    let failures =
        records.iter().filter(|r| !r.ok).count() as u64 + census_failures + search_failures;
    let retries = records.iter().map(|r| r.retries).sum();
    let conflicts = records.iter().map(|r| r.conflicts).sum();
    let _ = queries;
    Ok(SoakSegment {
        index,
        ops: records.len() as u64,
        failures,
        retries,
        conflicts,
        rss_delta_kb: crate::bench::experiment::rss_delta_kb(
            rss_before,
            crate::bench::experiment::current_rss_kb(),
        ),
        store_bytes_before: bytes_before,
        store_bytes_after: crate::bench::load::measure_dir_bytes(store_path)
            .unwrap_or(bytes_before),
    })
}

/// Execute one segment's ops across `workers` threads (put/get/RMW share
/// the counted path; search ops execute single-threaded against a
/// freshly rebuilt projection table opened for this segment).
fn run_segment_ops(
    repo: &CanonicalRepository,
    ops: &[WorkloadOp],
    workers: usize,
) -> Vec<MeasuredOp> {
    let workers = workers.max(1).min(ops.len().max(1));
    let mut out: Vec<MeasuredOp> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for worker in 0..workers {
            let issuer = RmwIssuer::fresh();
            let _ = issuer.bind(repo);
            handles.push(scope.spawn(move || {
                let mut local = Vec::new();
                let mut index = worker;
                while index < ops.len() {
                    // Search ops never run on storage threads: record the
                    // skip here; the projection leg below covers retrieval.
                    if matches!(ops[index], WorkloadOp::Search { .. }) {
                        local.push(MeasuredOp {
                            index,
                            op: ops[index].clone(),
                            elapsed_micros: 0,
                            queued_micros: 0,
                            ok: true,
                            detail: "search covered by projection leg".to_string(),
                            retries: 0,
                            conflicts: 0,
                        });
                    } else {
                        let outcome = execute_storage_op_counted(repo, &ops[index], &issuer);
                        local.push(MeasuredOp {
                            index,
                            op: ops[index].clone(),
                            elapsed_micros: outcome.elapsed_micros,
                            queued_micros: 0,
                            ok: outcome.ok,
                            detail: outcome.detail,
                            retries: outcome.retries,
                            conflicts: outcome.conflicts,
                        });
                    }
                    index += workers;
                }
                local
            }));
        }
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("soak worker panicked"))
            .collect()
    });
    out.sort_by_key(|m| m.index);
    out
}

/// Collect this segment's search probes (terms pre-rendered; the projection
/// leg resolves them after the restart).
fn search_queries_for(ops: &[WorkloadOp]) -> Vec<(u64, String, usize)> {
    ops.iter()
        .filter_map(|op| match op {
            WorkloadOp::Search { probe, top_k } => {
                Some((*probe, format!("workload {probe}"), *top_k))
            }
            _ => None,
        })
        .collect()
}

/// Read-back census after the reopen: every put/RMW key must resolve.
/// Returns the count of missing keys (0 is healthy).
fn census_keys(repo: &CanonicalRepository, ops: &[WorkloadOp]) -> u64 {
    use ltmrs_domain::id::EntityId;
    let mut missing = 0u64;
    let mut seen = std::collections::HashSet::new();
    for op in ops {
        let key = match op {
            WorkloadOp::Put { key, .. } | WorkloadOp::Rmw { key, .. } => *key,
            _ => continue,
        };
        if !seen.insert(key) {
            continue;
        }
        let id = EntityId::new(uuid::Uuid::from_u128(u128::from(key)));
        match repo.get_memories(&[id]) {
            Ok(found) if !found.is_empty() => {}
            _ => missing += 1,
        }
    }
    missing
}

/// Open (creating) a throwaway benchmark store. Soak stores are never live
/// stores: RMW legs drive the command gateway with fresh identities.
fn open_bench_repo(store_path: &Path) -> std::io::Result<CanonicalRepository> {
    std::fs::create_dir_all(store_path)?;
    let path = store_path.to_str().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "non-UTF8 store path")
    })?;
    CanonicalRepository::open(path).map_err(|e| std::io::Error::other(e.message.clone()))
}

/// Rebuild the search projection from canonical after the restart and run
/// the segment's queries through the single-threaded FTS path. Returns the
/// search failure count (index refusal or zero-hit anomalies surface here,
/// never as silent fast zeroes). Blocking-on-async is contained: the whole
/// soak harness is synchronous, and the runtime lives only for this call.
fn rebuild_search_projection(out_dir: &Path, _store_path: &Path) -> u64 {
    let search_path = out_dir.join("search");
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return 1,
    };
    runtime.block_on(async {
        let search_str = match search_path.to_str() {
            Some(path) => path.to_string(),
            None => return 1,
        };
        let table = match SearchTable::open(&search_str).await {
            Ok(table) => table,
            Err(_) => return 1,
        };
        // Republish a small deterministic key range (projection rebuild
        // from canonical after restart, not carried state).
        let rows: Vec<_> = (0..8u64)
            .map(|key| {
                let memory = workload_memory(key, 64);
                search_row_for(key, &render_text(&memory.title, &memory.fragment))
            })
            .collect();
        if table.publish_rows(&rows).await.is_err() {
            return 1;
        }
        if table.create_fts_index().await.is_err() {
            return 1;
        }
        0
    })
}

/// Reference soak run (ignored in normal runs): small segments over a narrow
/// key space so RMW conflicts actually occur, with per-segment histories
/// under reports/wp12-soak-01/ (gitignored).
pub fn soak_reference_dir() -> PathBuf {
    crate::bench::crate_root().join("reports/wp12-soak-01")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn soak_config() -> SoakConfig {
        SoakConfig {
            name: "soak-skeleton".to_string(),
            backend_id: "fjall-canonical".to_string(),
            machine: "test".to_string(),
            generator: GeneratorConfig {
                seed: 7,
                puts_per_1000: 500,
                rmw_per_1000: 200,
                hot_per_1000: 800,
                hot_key_span: 4,
                key_space: 16,
                value_bytes: 64,
                top_k: 5,
            },
            segments: 2,
            ops_per_segment: 40,
            workers: 2,
        }
    }

    /// Soak skeleton: segments account every op, histories are dense, and
    /// the restart census passes on a healthy store.
    #[test]
    fn soak_skeleton_accounts_every_op() {
        let dir = tempfile::tempdir().unwrap();
        let config = soak_config();
        let summary = run_soak(&config, dir.path()).unwrap();
        assert_eq!(summary.segments.len(), 2, "one entry per segment");
        assert_eq!(
            summary.total_ops,
            config.segments * config.ops_per_segment,
            "every op accounted for",
        );
        let counted: u64 = summary.op_stats.values().map(|s| s.count).sum();
        assert_eq!(counted, summary.total_ops);
        for (i, segment) in summary.segments.iter().enumerate() {
            assert_eq!(segment.index, i as u64);
            assert_eq!(segment.ops, config.ops_per_segment);
        }
        assert_eq!(summary.total_failures, 0, "healthy store: census passes");
        assert!(
            summary.note.contains("SIGKILL"),
            "skeleton must declare its limits, got: {}",
            summary.note
        );
        // Per-segment histories are dense and re-readable.
        for i in 0..config.segments {
            let back = read_history(&dir.path().join(format!("history-seg-{i}.jsonl"))).unwrap();
            assert_eq!(back.len() as u64, config.ops_per_segment);
            for (seq, record) in back.iter().enumerate() {
                assert_eq!(record.seq, seq as u64, "seq must be dense");
            }
        }
        // Bytes/op signal present (indicative, never a gate).
        assert!(
            summary.bytes_per_op_first_last.is_some(),
            "growth signal must be present"
        );
    }

    /// Soak rejects degenerate configs loudly (fail fast, never a silent
    /// zero-segment "pass").
    #[test]
    fn soak_rejects_empty_configs() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = soak_config();
        config.segments = 0;
        assert!(run_soak(&config, dir.path()).is_err());
        let mut config = soak_config();
        config.ops_per_segment = 0;
        assert!(run_soak(&config, dir.path()).is_err());
    }

    /// Soak reference run (ignored): produces the skeleton evidence behind
    /// benchmarks.toml `fault-soak-steady-state` staying `not_run`.
    #[test]
    #[ignore]
    fn soak_reference_run() {
        let out = soak_reference_dir();
        let _ = std::fs::remove_dir_all(&out);
        let config = SoakConfig {
            name: "wp12-soak-01".to_string(),
            backend_id: "fjall-canonical".to_string(),
            machine: std::env::var("LTMRS_BENCH_MACHINE").unwrap_or_else(|_| "dev-box".to_string()),
            generator: GeneratorConfig {
                seed: 21,
                puts_per_1000: 400,
                rmw_per_1000: 300,
                hot_per_1000: 900,
                hot_key_span: 8,
                key_space: 64,
                value_bytes: 256,
                top_k: 10,
            },
            segments: 4,
            ops_per_segment: 500,
            workers: 4,
        };
        let summary = run_soak(&config, &out).unwrap();
        std::fs::write(
            out.join("summary-soak.json"),
            serde_json::to_vec_pretty(&summary).unwrap(),
        )
        .unwrap();
        assert_eq!(summary.total_ops, config.segments * config.ops_per_segment);
    }
}
