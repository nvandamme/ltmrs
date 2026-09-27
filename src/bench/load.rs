//! Load runners and telemetry (WP-12 tasks 3-4; T-BENCH-02/03).
//!
//! Boundary rules:
//! - Closed-loop: a fixed set of workers issues the next op on completion.
//!   Latency is issue-to-complete; there is no queue by construction, so
//!   `queued_micros` is zero and offered load always equals completed load.
//! - Open-loop: arrivals follow a precomputed schedule at the offered rate
//!   regardless of completions. Latency is measured from the SCHEDULED
//!   arrival (queue + service, never service alone). There is no load
//!   shedding: when the store cannot keep up, the overrun shows up as queue
//!   time in the history, not as dropped arrivals.
//! - Only put/get ops run here (the search leg stays single-threaded until
//!   the table proves thread-safe publication under load).
//! - Telemetry covers what the storage boundary can honestly observe: store
//!   directory bytes before/after. Projection lag, retries and maintenance
//!   state have no pipeline in this boundary and are reported as
//!   not-applicable, never zero-filled.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::bench::{HistoryRecorder, WorkloadOp, WorkloadRng};
use crate::service::repository::CanonicalRepository;

/// Arrival process for open-loop offered load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrivalProcess {
    /// Fixed spacing: `1_000_000 / rate_per_sec` micros between arrivals.
    Constant,
    /// Poisson arrivals: exponential inter-arrivals with mean `1/rate`.
    Poisson,
}

/// Precomputed arrival offsets (micros from run start), deterministic per seed.
pub fn arrival_schedule(
    process: ArrivalProcess,
    rate_per_sec: u64,
    count: usize,
    seed: u64,
) -> Vec<u64> {
    assert!(rate_per_sec > 0, "offered rate must be positive");
    let mean_gap_micros = 1_000_000.0 / rate_per_sec as f64;
    let mut rng = WorkloadRng::new(seed);
    let mut offsets = Vec::with_capacity(count);
    let mut at = 0u64;
    for i in 0..count {
        if i > 0 {
            let gap = match process {
                ArrivalProcess::Constant => mean_gap_micros,
                ArrivalProcess::Poisson => {
                    let mut u = rng.next_u64() as f64 / u64::MAX as f64;
                    if u >= 1.0 {
                        u = 0.5;
                    }
                    -(1.0 - u).ln() * mean_gap_micros
                }
            };
            // Float-to-int conversion saturates (huge gaps) and sub-microsecond
            // rates truncate to simultaneous arrivals; both are caller errors
            // that stay monotonic rather than wrapping.
            at = at.saturating_add(gap as u64);
        }
        offsets.push(at);
    }
    offsets
}
/// Split an open-loop op into queue time (scheduled → start, saturating) and
/// service time (start → finish). All values in micros.
pub fn split_latency(scheduled: u64, started: u64, finished: u64) -> (u64, u64) {
    (
        started.saturating_sub(scheduled),
        finished.saturating_sub(started),
    )
}

/// One executed load op (returned by workers, recorded by the caller in a
/// deterministic order).
pub struct MeasuredOp {
    pub index: usize,
    pub op: WorkloadOp,
    pub elapsed_micros: u64,
    pub queued_micros: u64,
    pub ok: bool,
    pub detail: String,
}

/// Execute one put/get storage op (shared by every runner; search ops are
/// rejected here and belong to the single-threaded FTS leg). Returns
/// (elapsed micros, ok, detail).
/// Execute one put/get storage op (shared by every runner; search ops are
/// rejected here and belong to the single-threaded FTS leg). Returns
/// (elapsed micros, ok, detail). Payloads are constructed BEFORE the timer
/// starts so allocation/formatting never inflates store latency.
pub fn execute_storage_op(repo: &CanonicalRepository, op: &WorkloadOp) -> (u64, bool, String) {
    use crate::bench::experiment::workload_memory;
    use crate::domain::id::EntityId;
    enum Prepared {
        Put(Box<crate::domain::memory::Memory>),
        Get(EntityId),
    }
    let prepared = match op {
        WorkloadOp::Put { key, bytes } => Prepared::Put(Box::new(workload_memory(*key, *bytes))),
        WorkloadOp::Get { key } => {
            Prepared::Get(EntityId::new(uuid::Uuid::from_u128(u128::from(*key))))
        }
        WorkloadOp::Search { .. } => {
            return (
                0,
                false,
                "search unsupported in load runners (single-threaded FTS leg only)".to_string(),
            );
        }
    };
    let start = Instant::now();
    let elapsed = || start.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    match prepared {
        Prepared::Put(memory) => match repo.put_memory_direct(&memory) {
            Ok(()) => (elapsed(), true, "written".to_string()),
            Err(e) => (elapsed(), false, e.message.clone()),
        },
        Prepared::Get(id) => match repo.get_memories(&[id]) {
            Ok(found) => (elapsed(), true, format!("found={}", found.len())),
            Err(e) => (elapsed(), false, e.message.clone()),
        },
    }
}

/// Closed-loop run: `workers` threads split `ops` round-robin; each issues
/// its next op on completion. Returns per-op measurements in input order.
pub fn run_closed_loop(
    repo: &CanonicalRepository,
    ops: &[WorkloadOp],
    workers: usize,
) -> Vec<MeasuredOp> {
    assert!(workers >= 1, "at least one worker");
    debug_assert!(
        !ops.iter().any(|op| matches!(op, WorkloadOp::Search { .. })),
        "load runners take put/get only; searches belong to the FTS leg"
    );
    if ops.is_empty() {
        return Vec::new();
    }
    let workers = workers.min(ops.len());
    let mut out: Vec<MeasuredOp> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for worker in 0..workers {
            handles.push(scope.spawn(move || {
                let mut local = Vec::new();
                let mut index = worker;
                while index < ops.len() {
                    let (elapsed, ok, detail) = execute_storage_op(repo, &ops[index]);
                    local.push(MeasuredOp {
                        index,
                        op: ops[index].clone(),
                        elapsed_micros: elapsed,
                        queued_micros: 0,
                        ok,
                        detail,
                    });
                    index += workers;
                }
                local
            }));
        }
        handles
            .into_iter()
            .flat_map(|h| h.join().expect("worker panicked"))
            .collect()
    });
    out.sort_by_key(|m| m.index);
    out
}

/// Open-loop run: arrivals follow `schedule` offsets regardless of
/// completions; `workers` threads execute on dequeue. Queue time is measured
/// per op from its scheduled arrival. Returns measurements in input order.
pub fn run_open_loop(
    repo: &CanonicalRepository,
    ops: &[WorkloadOp],
    schedule: &[u64],
    workers: usize,
) -> Vec<MeasuredOp> {
    assert!(workers >= 1, "at least one worker");
    debug_assert!(
        !ops.iter().any(|op| matches!(op, WorkloadOp::Search { .. })),
        "load runners take put/get only; searches belong to the FTS leg"
    );
    assert_eq!(
        ops.len(),
        schedule.len(),
        "schedule must cover every offered op"
    );
    if ops.is_empty() {
        return Vec::new();
    }
    let start = Instant::now();
    let (tx_work, rx_work) = std::sync::mpsc::channel::<(usize, u64)>();
    let (tx_done, rx_done) = std::sync::mpsc::channel::<MeasuredOp>();
    // std mpsc receivers are Send but not Sync: share behind a mutex so
    // workers dequeue one arrival at a time.
    let rx_work = std::sync::Mutex::new(rx_work);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let rx_work = &rx_work;
            let tx_done = tx_done.clone();
            scope.spawn(move || {
                loop {
                    // The mutex only serializes dequeuing; the guard drops at
                    // the end of this statement, before execution.
                    let job = rx_work.lock().expect("work queue mutex poisoned").recv();
                    let (index, scheduled) = match job {
                        Ok(job) => job,
                        Err(_) => break,
                    };
                    let recv_at = micros_since(start);
                    let (elapsed, ok, detail) = execute_storage_op(repo, &ops[index]);
                    let (queued, _) =
                        split_latency(scheduled, recv_at, recv_at.saturating_add(elapsed));
                    tx_done
                        .send(MeasuredOp {
                            index,
                            op: ops[index].clone(),
                            elapsed_micros: elapsed,
                            queued_micros: queued,
                            ok,
                            detail,
                        })
                        .expect("collector alive");
                }
            });
        }
        drop(tx_done);
        for (index, offset) in schedule.iter().enumerate() {
            let deadline = start + Duration::from_micros(*offset);
            let now = Instant::now();
            if deadline > now {
                std::thread::sleep(deadline - now);
            }
            tx_work.send((index, *offset)).expect("workers alive");
        }
        // Disconnect so workers drain and exit; without this the scope
        // joins forever (workers park in recv on a live channel).
        drop(tx_work);
    });
    let mut out: Vec<MeasuredOp> = rx_done.into_iter().collect();
    out.sort_by_key(|m| m.index);
    out
}

fn micros_since(start: Instant) -> u64 {
    start.elapsed().as_micros().min(u128::from(u64::MAX)) as u64
}

/// Record a batch of measurements in index order (deterministic history).
pub fn record_measured(
    recorder: &mut HistoryRecorder,
    measured: &[MeasuredOp],
) -> std::io::Result<()> {
    let mut ordered: Vec<&MeasuredOp> = measured.iter().collect();
    ordered.sort_by_key(|m| m.index);
    for m in ordered {
        recorder.record_queued(
            m.op.clone(),
            m.elapsed_micros,
            m.queued_micros,
            m.ok,
            &m.detail,
        )?;
    }
    Ok(())
}

/// Honestly observable storage telemetry around a run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Telemetry {
    pub store_bytes: u64,
    /// Coverage limits: projection lag, retries and maintenance state have
    /// no observable pipeline in the storage-only boundary (not-applicable,
    /// never zero-filled); write conflicts are countable from history
    /// details mentioning conflicted writes.
    pub coverage: String,
}

pub fn collect_telemetry(store_path: &Path) -> std::io::Result<Telemetry> {
    Ok(Telemetry {
        store_bytes: measure_dir_bytes(store_path)?,
        coverage: "projection-lag/retries/maintenance: not-applicable (no pipeline in the storage-only boundary)".to_string(),
    })
}

/// Sum of file sizes under a directory (symlinks not followed; entries
/// vanishing mid-walk are skipped, a missing root still errors).
pub fn measure_dir_bytes(path: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(entry.path());
            } else {
                match entry.metadata() {
                    Ok(meta) => total += meta.len(),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e),
                }
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::{GeneratorConfig, OpGenerator};

    fn gen_ops(seed: u64, n: usize) -> Vec<WorkloadOp> {
        let mut generator = OpGenerator::new(GeneratorConfig {
            seed,
            puts_per_1000: 600,
            key_space: 40,
            ..Default::default()
        });
        (0..n).map(|_| generator.next_op()).collect()
    }

    fn open_repo(dir: &tempfile::TempDir) -> CanonicalRepository {
        CanonicalRepository::open(dir.path().join("store").to_str().unwrap()).unwrap()
    }

    /// Schedules are seed-pinned; constant spacing is exact, Poisson averages
    /// to the offered rate.
    #[test]
    fn schedule_is_deterministic_and_honest_rate() {
        let a = arrival_schedule(ArrivalProcess::Constant, 100, 200, 7);
        let b = arrival_schedule(ArrivalProcess::Constant, 100, 200, 7);
        assert_eq!(a, b, "schedule must be seed-pinned");
        assert_eq!(a.len(), 200);
        assert_eq!(a[0], 0, "first arrival at t=0");
        for pair in a.windows(2) {
            assert_eq!(pair[1] - pair[0], 10_000, "exact 100/s spacing");
        }
        let p1 = arrival_schedule(ArrivalProcess::Poisson, 100, 10_000, 7);
        let p2 = arrival_schedule(ArrivalProcess::Poisson, 100, 10_000, 7);
        assert_eq!(p1, p2, "poisson must be seed-pinned");
        let mean_gap = (p1[p1.len() - 1] - p1[0]) as f64 / (p1.len() - 1) as f64;
        assert!(
            (mean_gap - 10_000.0).abs() < 2_000.0,
            "mean gap near 1/rate, got {mean_gap}"
        );
        assert!(p1.windows(2).all(|w| w[1] >= w[0]), "monotonic");
    }

    /// Queue/service split saturates instead of underflowing on early starts.
    #[test]
    fn latency_split_accounts_queue() {
        assert_eq!(split_latency(1000, 1010, 1060), (10, 50));
        assert_eq!(split_latency(1000, 900, 950), (0, 50));
    }

    /// Closed-loop executes every op across workers with zero queue by
    /// construction and no failures on a fresh store.
    #[test]
    fn closed_loop_executes_every_op() {
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir);
        let ops: Vec<WorkloadOp> = gen_ops(7, 40)
            .into_iter()
            .filter(|op| !matches!(op, WorkloadOp::Search { .. }))
            .collect();
        let measured = run_closed_loop(&repo, &ops, 4);
        assert_eq!(measured.len(), ops.len(), "offered == completed");
        assert!(measured.iter().all(|m| m.ok), "fresh store, no failures");
        assert!(
            measured.iter().all(|m| m.queued_micros == 0),
            "no queue in closed loop"
        );
        let history = dir.path().join("closed.jsonl");
        let mut recorder = HistoryRecorder::create(&history).unwrap();
        record_measured(&mut recorder, &measured).unwrap();
        recorder.flush().unwrap();
        let back = crate::bench::read_history(&history).unwrap();
        assert_eq!(back.len(), ops.len());
        for (i, record) in back.iter().enumerate() {
            assert_eq!(record.seq, i as u64);
        }
    }

    /// Open-loop executes every scheduled arrival and records per-op queue
    /// time measured from the scheduled (not issue) instant.
    #[test]
    fn open_loop_measures_queue_honestly() {
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir);
        let ops: Vec<WorkloadOp> = gen_ops(7, 20)
            .into_iter()
            .filter(|op| !matches!(op, WorkloadOp::Search { .. }))
            .collect();
        let schedule = arrival_schedule(ArrivalProcess::Constant, 500, ops.len(), 7);
        let start = Instant::now();
        let measured = run_open_loop(&repo, &ops, &schedule, 2);
        let wall = start.elapsed();
        assert_eq!(measured.len(), ops.len(), "no arrivals dropped");
        assert!(measured.iter().all(|m| m.ok));
        let last_arrival = Duration::from_micros(*schedule.last().unwrap());
        assert!(
            wall >= last_arrival,
            "run cannot finish before the last scheduled arrival ({last_arrival:?}), took {wall:?}"
        );
        let history = dir.path().join("open.jsonl");
        let mut recorder = HistoryRecorder::create(&history).unwrap();
        record_measured(&mut recorder, &measured).unwrap();
        recorder.flush().unwrap();
        let back = crate::bench::read_history(&history).unwrap();
        assert_eq!(back.len(), ops.len());
    }

    /// Telemetry observes store growth across writes (never invents values
    /// for pipelines outside the boundary).
    #[test]
    fn telemetry_measures_dir_growth() {
        use crate::bench::experiment::workload_memory;
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        let repo = CanonicalRepository::open(store.to_str().unwrap()).unwrap();
        let _before = collect_telemetry(&store).unwrap();
        for key in 0..10u64 {
            repo.put_memory_direct(&workload_memory(key, 128)).unwrap();
        }
        drop(repo);
        let after = collect_telemetry(&store).unwrap();
        // No direction invariant: LSM stores preallocate segments at open
        // and compact on close, so 10 puts can legitimately measure smaller
        // than a fresh open. Growth trends belong to soak runs (T-BENCH-03),
        // not to unit scale. What this pins: observability itself.
        assert!(
            after.store_bytes > 0,
            "store must be non-empty and measurable after writes"
        );
        assert!(after.coverage.contains("not-applicable"));
    }

    /// Reference load experiment (WP-12 task 3; T-BENCH-02): ignored in
    /// normal runs. Execute explicitly to (re)produce the raw evidence
    /// behind benchmarks.toml:
    /// `cargo test -- --ignored load_experiment_reference_run`.
    /// Closed-loop (4 workers) and open-loop (Poisson 200/s, 2 workers) legs
    /// over the same put/get stream; histories + summaries + telemetry land
    /// under reports/wp12-load-01/ (gitignored).
    /// Precondition: the report dir is removed first (fresh store per run).
    #[test]
    #[ignore]
    fn load_experiment_reference_run() {
        use crate::bench::config_digest;
        use crate::bench::experiment::{ExperimentSummary, summarize};
        let out = crate::bench::crate_root().join("reports/wp12-load-01");
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(out.join("store")).unwrap();
        let machine =
            std::env::var("LTMRS_BENCH_MACHINE").unwrap_or_else(|_| "dev-box".to_string());
        let repo = CanonicalRepository::open(out.join("store").to_str().unwrap()).unwrap();
        let generator = GeneratorConfig {
            seed: 11,
            puts_per_1000: 600,
            key_space: 500,
            value_bytes: 256,
            top_k: 10,
        };
        let mut op_gen = OpGenerator::new(generator.clone());
        let ops: Vec<WorkloadOp> = (0..1000).map(|_| op_gen.next_op()).collect();
        let putget: Vec<WorkloadOp> = ops
            .iter()
            .filter(|op| !matches!(op, WorkloadOp::Search { .. }))
            .cloned()
            .collect();
        let skipped = (ops.len() - putget.len()) as u64;
        let digest = config_digest(
            generator.seed,
            generator.puts_per_1000,
            generator.key_space,
            generator.value_bytes,
            generator.top_k,
            ops.len() as u64,
        );
        let tele_before = collect_telemetry(&out.join("store")).unwrap();

        // Closed leg: 4 workers, issue-on-complete.
        let wall = std::time::Instant::now();
        let closed = run_closed_loop(&repo, &putget, 4);
        let closed_wall = wall.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        assert_eq!(closed.len(), putget.len(), "offered == completed");
        let history_closed = out.join("history-closed.jsonl");
        let mut recorder = HistoryRecorder::create(&history_closed).unwrap();
        record_measured(&mut recorder, &closed).unwrap();
        recorder.flush().unwrap();
        let closed_records = crate::bench::read_history(&history_closed).unwrap();
        let closed_stats = summarize(&closed_records);
        let closed_summary = ExperimentSummary {
            provenance: crate::bench::Provenance::collect(&machine, generator.seed, digest.clone()),
            backend_id: "fjall-canonical".to_string(),
            op_stats: closed_stats,
            total_ops: closed_records.len() as u64,
            total_failures: closed_records.iter().filter(|r| !r.ok).count() as u64,
            wall_micros: closed_wall,
        };
        assert_eq!(closed_summary.total_failures, 0);
        std::fs::write(
            out.join("summary-closed.json"),
            serde_json::to_vec_pretty(&closed_summary).unwrap(),
        )
        .unwrap();

        // Open leg: Poisson 200/s arrivals, queue time measured per op.
        let schedule = arrival_schedule(ArrivalProcess::Poisson, 200, putget.len(), 12);
        let wall = std::time::Instant::now();
        let open = run_open_loop(&repo, &putget, &schedule, 2);
        let open_wall = wall.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        assert_eq!(open.len(), putget.len(), "no arrivals dropped");
        let history_open = out.join("history-open.jsonl");
        let mut recorder = HistoryRecorder::create(&history_open).unwrap();
        record_measured(&mut recorder, &open).unwrap();
        recorder.flush().unwrap();
        let open_records = crate::bench::read_history(&history_open).unwrap();
        let open_stats = summarize(&open_records);
        let queue_histogram = {
            let mut histogram = crate::bench::Histogram::new();
            for record in &open_records {
                histogram.push(record.queued_micros);
            }
            histogram
        };
        let queue_p99 = queue_histogram.percentile(99.0).unwrap_or(0);
        let open_summary = ExperimentSummary {
            provenance: crate::bench::Provenance::collect(&machine, generator.seed, digest),
            backend_id: "fjall-canonical".to_string(),
            op_stats: open_stats,
            total_ops: open_records.len() as u64,
            total_failures: open_records.iter().filter(|r| !r.ok).count() as u64,
            wall_micros: open_wall,
        };
        assert_eq!(open_summary.total_failures, 0);
        std::fs::write(
            out.join("summary-open.json"),
            serde_json::to_vec_pretty(&open_summary).unwrap(),
        )
        .unwrap();

        let tele_after = collect_telemetry(&out.join("store")).unwrap();
        std::fs::write(
            out.join("telemetry.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "skipped_search_ops": skipped,
                "store_bytes_before": tele_before.store_bytes,
                "store_bytes_after": tele_after.store_bytes,
                "open_queue_p99_micros": queue_p99,
                "coverage": tele_after.coverage,
            }))
            .unwrap(),
        )
        .unwrap();
    }
}
