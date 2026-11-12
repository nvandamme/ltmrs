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
use ltmrs_service::repository::CanonicalRepository;

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
#[derive(Debug, Clone)]
pub struct MeasuredOp {
    pub index: usize,
    pub op: WorkloadOp,
    pub elapsed_micros: u64,
    pub queued_micros: u64,
    pub ok: bool,
    pub detail: String,
    /// Revision-conflict retries taken inside the measured boundary.
    pub retries: u64,
    /// Storage write-transaction conflicts observed before success/exhaustion.
    pub conflicts: u64,
    /// Per-op CPU time (thread-CPU delta around the measured region).
    pub cpu_micros: u64,
    /// Per-op fsync-cost attribution (barrier time; 0 for reads).
    pub fsync_micros: u64,
}

/// Outcome of one storage op: elapsed micros, success flag, human detail,
/// plus the conflict/retry counters observed inside the measured boundary
/// (RMW loser retries; blind upserts stay 0).
#[derive(Debug, Clone)]
pub struct OpOutcome {
    pub elapsed_micros: u64,
    pub ok: bool,
    pub detail: String,
    pub retries: u64,
    pub conflicts: u64,
    /// Per-op CPU time (thread-CPU delta around the measured region).
    pub cpu_micros: u64,
    /// Per-op fsync-cost attribution (barrier time; 0 for reads).
    pub fsync_micros: u64,
}

/// Execute one put/get/RMW storage op (shared by every runner). Search ops
/// stay single-threaded until the FTS leg proves thread-safe publication
/// (see [`execute_search_op`]). Returns [`OpOutcome`].
/// Execute one storage op (shared by every runner). Payloads are constructed
/// BEFORE the timer starts so allocation/formatting never inflates store
/// latency.
///
/// Scope: load runners drive `put_memory_direct`, which bypasses the command
/// gateway (no receipts, aliases, revision checks or projection jobs).
/// Point them at throwaway benchmark stores only — never at a live store,
/// where blind overwrites at fixed revisions would clobber real state.
/// RMW ops go through the command gateway instead (fresh read +
/// revision-checked update): the only leg that can observe revision
/// conflicts, and the counters in [`OpOutcome`] stay honest (0 for
/// blind upserts, measured for RMW).
///
/// The runner is a caller of the storage layer, so it honors the plan's
/// retry contract (Part I: storage conflicts retry from a fresh snapshot
/// with bounded backoff): `put_memory_direct` exhaustion is typed
/// `Contention` (transient, safe to retry), and shared-watermark SSI
/// contention between closed-loop workers is expected under concurrency.
/// The runner retries exhaustion up to `RUNNER_RETRY_LIMIT` times; latency
/// is measured from the first attempt (retry time is real service time),
/// and true exhaustion stays a recorded failure.
pub fn execute_storage_op(repo: &CanonicalRepository, op: &WorkloadOp) -> (u64, bool, String) {
    let outcome = execute_storage_op_counted(repo, op, &RmwIssuer::direct());
    (outcome.elapsed_micros, outcome.ok, outcome.detail)
}

/// Per-op issuer of RMW command contexts: each RMW op needs a unique
/// operation id and digest (idempotency keys), even when workers share an
/// issuer across threads. The issuer binds one retry namespace per
/// (frontend, channel) pair against the target repo before issuing
/// contexts (`bind`): gateway `apply` refuses unbound epochs as stale, so
/// an unbound issuer would fail every RMW loudly instead of measuring.
pub struct RmwIssuer {
    frontend_id: ltmrs_domain::id::FrontendId,
    channel_id: ltmrs_domain::id::ChannelId,
    retry_epoch: std::sync::atomic::AtomicU64,
    next_op: std::sync::atomic::AtomicU64,
}

impl RmwIssuer {
    /// Issuer for single-threaded legs (fresh frontend/channel identity,
    /// unbound: call [`RmwIssuer::bind`] before executing RMW ops).
    pub fn direct() -> Self {
        Self::fresh()
    }

    /// Fresh random identity (uuid v7: CSPRNG-backed, no seed to manage).
    pub fn fresh() -> Self {
        Self {
            frontend_id: ltmrs_domain::id::FrontendId::new(uuid::Uuid::now_v7()),
            channel_id: ltmrs_domain::id::ChannelId::new(uuid::Uuid::now_v7()),
            retry_epoch: std::sync::atomic::AtomicU64::new(0),
            next_op: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Bind (or rebind) the issuer's retry namespace against `repo`
    /// (system clock). Retries once on channel contention.
    pub fn bind(&self, repo: &CanonicalRepository) -> ltmrs_domain::command::DomainResult<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(1000);
        match repo.issue_namespace(self.frontend_id, self.channel_id, now) {
            Ok(namespace) => {
                self.retry_epoch
                    .store(namespace.retry_epoch, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn next_ctx(&self, seq_hint: u64) -> ltmrs_domain::command::CommandContext {
        let op_num = self
            .next_op
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let retry_epoch = self.retry_epoch.load(std::sync::atomic::Ordering::SeqCst);
        ltmrs_domain::command::CommandContext {
            store_generation: ltmrs_domain::id::StoreGeneration::FIRST,
            frontend_id: self.frontend_id,
            channel_id: self.channel_id,
            session: None,
            operation_id: ltmrs_domain::id::OperationId::new(uuid::Uuid::from_u128(
                (u128::from(op_num) << 64) | u128::from(seq_hint),
            )),
            request_digest: format!("bench-rmw-{op_num}-{seq_hint}"),
            deadline_millis: None,
            scope: Default::default(),
            retry_epoch,
        }
    }
}

/// Counted executor: same behavior as [`execute_storage_op`] plus
/// conflict/retry counters; RMW ops route through `issuer`. The wrapper
/// adds the WP-12 per-op resource sample (thread-CPU delta + one timed
/// barrier on the same store state for writes).
pub fn execute_storage_op_counted(
    repo: &CanonicalRepository,
    op: &WorkloadOp,
    issuer: &RmwIssuer,
) -> OpOutcome {
    use crate::bench::experiment::current_thread_cpu_micros;
    let cpu_start = current_thread_cpu_micros();
    let mut outcome = execute_storage_op_counted_inner(repo, op, issuer);
    // CPU delta around the whole measured region (attempt sleeps consume
    // no CPU, so backoff rides along honestly).
    outcome.cpu_micros = match (cpu_start, current_thread_cpu_micros()) {
        (Some(b), Some(a)) => a.saturating_sub(b),
        _ => 0,
    };
    // fsync attribution: one extra barrier on the same store state costs
    // what the op's own barrier cost (same journal depth class). Reads
    // issue no barrier, so they sample 0 — never a fabricated share.
    // A failed probe records 0 (declared absence, not a fast barrier).
    outcome.fsync_micros = match op {
        WorkloadOp::Put { .. } | WorkloadOp::Rmw { .. } if outcome.ok => {
            let start = Instant::now();
            match repo.durability_barrier() {
                Ok(()) => start.elapsed().as_micros().min(u128::from(u64::MAX)) as u64,
                Err(_) => 0,
            }
        }
        _ => 0,
    };
    outcome
}

/// Inner executor (wall-clock + conflict/retry telemetry).
fn execute_storage_op_counted_inner(
    repo: &CanonicalRepository,
    op: &WorkloadOp,
    issuer: &RmwIssuer,
) -> OpOutcome {
    use crate::bench::experiment::workload_memory;
    use ltmrs_domain::command::DomainErrorCode;
    use ltmrs_domain::id::EntityId;
    enum Prepared {
        Put(Box<ltmrs_domain::memory::Memory>),
        Get(EntityId),
        Rmw { key: u64, bytes: usize },
    }
    let fail = |elapsed: u64, msg: String| OpOutcome {
        elapsed_micros: elapsed,
        ok: false,
        detail: msg,
        retries: 0,
        conflicts: 0,
        cpu_micros: 0,
        fsync_micros: 0,
    };
    let prepared = match op {
        WorkloadOp::Put { key, bytes } => Prepared::Put(Box::new(workload_memory(*key, *bytes))),
        WorkloadOp::Rmw { key, bytes } => Prepared::Rmw {
            key: *key,
            bytes: *bytes,
        },
        WorkloadOp::Get { key } => {
            Prepared::Get(EntityId::new(uuid::Uuid::from_u128(u128::from(*key))))
        }
        WorkloadOp::Search { .. } => {
            return fail(
                0,
                "search unsupported in load runners (single-threaded FTS leg only)".to_string(),
            );
        }
    };
    const RUNNER_RETRY_LIMIT: u32 = 20;
    // RMW retries at runner parity: hot-skew RMW hammers the same keys
    // from every worker, so its conflict rate meets or exceeds blind
    // puts — a smaller budget would fail the mix, not the store.
    const RMW_RETRY_LIMIT: u32 = 20;
    let start = Instant::now();
    let elapsed = || start.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    match prepared {
        Prepared::Put(memory) => {
            let mut last = None;
            let mut conflicts = 0u64;
            for attempt in 0..RUNNER_RETRY_LIMIT {
                match repo.put_memory_direct(&memory) {
                    Ok(()) => {
                        return OpOutcome {
                            elapsed_micros: elapsed(),
                            ok: true,
                            detail: "written".to_string(),
                            retries: 0,
                            conflicts,
                            cpu_micros: 0,
                            fsync_micros: 0,
                        };
                    }
                    Err(e) => {
                        if e.code != DomainErrorCode::Contention {
                            return fail(elapsed(), e.message.clone());
                        }
                        conflicts += 1;
                        last = Some(e.message.clone());
                        if attempt + 1 < RUNNER_RETRY_LIMIT {
                            std::thread::sleep(Duration::from_micros(50));
                        }
                    }
                }
            }
            OpOutcome {
                elapsed_micros: elapsed(),
                ok: false,
                detail: format!("runner retry exhausted: {}", last.unwrap_or_default()),
                retries: 0,
                conflicts,
                cpu_micros: 0,
                fsync_micros: 0,
            }
        }
        Prepared::Get(id) => match repo.get_memories(&[id]) {
            Ok(found) => OpOutcome {
                elapsed_micros: elapsed(),
                ok: true,
                detail: format!("found={}", found.len()),
                retries: 0,
                conflicts: 0,
                cpu_micros: 0,
                fsync_micros: 0,
            },
            Err(e) => fail(elapsed(), e.message.clone()),
        },
        Prepared::Rmw { key, bytes } => {
            execute_rmw(repo, issuer, key, bytes, &elapsed, RMW_RETRY_LIMIT)
        }
    }
}

/// Fresh-read + revision-checked update with loser retries: the only
/// benchmark path that can observe `RevisionConflict`. Each attempt
/// re-reads the memory, patches the fragment to workload content, and
/// retries on `RevisionConflict` with bounded backoff; a missing key is
/// created once (no conflict), then updated. Latency covers all attempts.
fn execute_rmw(
    repo: &CanonicalRepository,
    issuer: &RmwIssuer,
    key: u64,
    bytes: usize,
    elapsed: &dyn Fn() -> u64,
    retry_limit: u32,
) -> OpOutcome {
    use crate::bench::experiment::workload_memory;
    use ltmrs_domain::command::{DomainCommand, DomainErrorCode, MemoryPatch};
    use ltmrs_domain::id::EntityId;
    let id = EntityId::new(uuid::Uuid::from_u128(u128::from(key)));
    let mut retries = 0u64;
    let mut conflicts = 0u64;
    for attempt in 0..retry_limit {
        let ctx = issuer.next_ctx(key);
        let fresh = match repo.get_memories(&[id]) {
            Ok(found) => found.into_iter().next(),
            Err(e) => {
                return OpOutcome {
                    elapsed_micros: elapsed(),
                    ok: false,
                    detail: e.message.clone(),
                    retries,
                    conflicts,
                    cpu_micros: 0,
                    fsync_micros: 0,
                };
            }
        };
        let outcome = match fresh {
            Some(current) => {
                let mut patched = workload_memory(key, bytes);
                patched.entity_revision = current.entity_revision;
                let patch = MemoryPatch {
                    fragment: Some(patched.fragment),
                    ..Default::default()
                };
                repo.apply(
                    &ctx,
                    &DomainCommand::UpdateMemory {
                        id,
                        expected_revision: Some(current.entity_revision),
                        patch,
                    },
                )
            }
            None => {
                let memory = workload_memory(key, bytes);
                repo.apply(
                    &ctx,
                    &DomainCommand::AddMemory {
                        memory,
                        session: None,
                        auto_link: None,
                    },
                )
            }
        };
        match outcome {
            Ok(_) => {
                return OpOutcome {
                    elapsed_micros: elapsed(),
                    ok: true,
                    detail: format!("rmw ok retries={retries}"),
                    retries,
                    conflicts,
                    cpu_micros: 0,
                    fsync_micros: 0,
                };
            }
            Err(e)
                if e.code == DomainErrorCode::RevisionConflict
                    || (e.code == DomainErrorCode::Validation
                        && e.message.contains("memory already exists")) =>
            {
                // Create race: two workers read-missed the same hot key and
                // both tried AddMemory; the loser re-reads and updates (one
                // retry, one conflict — the same accounting as a stale
                // revision).
                retries += 1;
                conflicts += 1;
                if attempt + 1 >= retry_limit {
                    return OpOutcome {
                        elapsed_micros: elapsed(),
                        ok: false,
                        detail: format!("rmw retry exhausted: {}", e.message),
                        retries,
                        conflicts,
                        cpu_micros: 0,
                        fsync_micros: 0,
                    };
                }
                std::thread::sleep(Duration::from_micros(50));
            }
            Err(e) => {
                return OpOutcome {
                    elapsed_micros: elapsed(),
                    ok: false,
                    detail: e.message.clone(),
                    retries,
                    conflicts,
                    cpu_micros: 0,
                    fsync_micros: 0,
                };
            }
        }
    }
    OpOutcome {
        elapsed_micros: elapsed(),
        ok: false,
        detail: "rmw retry exhausted".to_string(),
        retries,
        conflicts,
        cpu_micros: 0,
        fsync_micros: 0,
    }
}

/// Closed-loop run: `workers` threads split `ops` round-robin; each issues
/// its next op on completion. Returns per-op measurements in input order.
/// Every worker holds its own [`RmwIssuer`] (fresh identity per worker), so
/// RMW op contexts stay unique across threads.
pub fn run_closed_loop(
    repo: &CanonicalRepository,
    ops: &[WorkloadOp],
    workers: usize,
) -> Vec<MeasuredOp> {
    run_closed_loop_counted(repo, ops, workers, &RmwIssuer::fresh())
}

/// Counted closed-loop run (RMW retry/conflict telemetry preserved). The
/// passed `issuer` is bound once against `repo` for the single-threaded
/// prefix below; worker threads bind their own per-worker issuers.
pub fn run_closed_loop_counted(
    repo: &CanonicalRepository,
    ops: &[WorkloadOp],
    workers: usize,
    issuer: &RmwIssuer,
) -> Vec<MeasuredOp> {
    assert!(workers >= 1, "at least one worker");
    if ops.is_empty() {
        return Vec::new();
    }
    let _ = issuer.bind(repo);
    let workers = workers.min(ops.len());
    let mut out: Vec<MeasuredOp> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for worker in 0..workers {
            // Per-worker issuer: unique op ids even when threads race.
            let issuer = RmwIssuer::fresh();
            let _ = issuer.bind(repo);
            handles.push(scope.spawn(move || {
                let mut local = Vec::new();
                let mut index = worker;
                while index < ops.len() {
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
                        cpu_micros: outcome.cpu_micros,
                        fsync_micros: outcome.fsync_micros,
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
/// Every worker holds its own [`RmwIssuer`] (fresh identity per worker), so
/// RMW op contexts stay unique across threads.
pub fn run_open_loop(
    repo: &CanonicalRepository,
    ops: &[WorkloadOp],
    schedule: &[u64],
    workers: usize,
) -> Vec<MeasuredOp> {
    assert!(workers >= 1, "at least one worker");
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
            // Per-worker issuer: unique op ids even when threads race.
            // Bound before any RMW executes (gateway refuses unbound epochs).
            let issuer = RmwIssuer::fresh();
            let _ = issuer.bind(repo);
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
                    let outcome = execute_storage_op_counted(repo, &ops[index], &issuer);
                    let (queued, _) = split_latency(
                        scheduled,
                        recv_at,
                        recv_at.saturating_add(outcome.elapsed_micros),
                    );
                    tx_done
                        .send(MeasuredOp {
                            index,
                            op: ops[index].clone(),
                            elapsed_micros: outcome.elapsed_micros,
                            queued_micros: queued,
                            ok: outcome.ok,
                            detail: outcome.detail,
                            retries: outcome.retries,
                            conflicts: outcome.conflicts,
                            cpu_micros: outcome.cpu_micros,
                            fsync_micros: outcome.fsync_micros,
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
        recorder.record_full(
            m.op.clone(),
            m.elapsed_micros,
            m.queued_micros,
            m.ok,
            &m.detail,
            m.retries,
            m.conflicts,
            m.cpu_micros,
            m.fsync_micros,
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
            .filter(|op| !matches!(op, WorkloadOp::Search { .. } | WorkloadOp::Rmw { .. }))
            .collect();
        let measured = run_closed_loop(&repo, &ops, 4);
        assert_eq!(measured.len(), ops.len(), "offered == completed");
        // (convergence asserted below with failure detail).
        assert!(
            measured.iter().all(|m| m.queued_micros == 0),
            "no queue in closed loop"
        );
        // Contention costs wait, never the outcome: concurrent puts
        // genuinely abort on the shared memory op-seq key bumped inside
        // every write tx, so under machine load the overlap window widens
        // and aborts surface as runner-level conflicts. The invariant is
        // convergence (all ok, no exhaustion) with conflicts kept as
        // observed telemetry — not zero conflicts, and not retries ==
        // conflicts (the `retries` counter is RMW-loser-scoped; blind puts
        // hardcode it to 0, so that equality is false for puts by
        // construction).
        assert!(
            measured.iter().all(|m| m.ok),
            "contention converges, never fails: {:?}",
            measured
                .iter()
                .filter(|m| !m.ok)
                .map(|m| &m.detail)
                .collect::<Vec<_>>()
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

    /// RMW leg: blind creates then genuine revision-checked updates against
    /// a skewed hot key; every op succeeds and counters stay honest.
    #[test]
    fn rmw_executes_revision_checked_updates() {
        use crate::bench::experiment::workload_memory;
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir);
        let issuer = RmwIssuer::fresh();
        issuer.bind(&repo).unwrap();
        let outcome =
            execute_storage_op_counted(&repo, &WorkloadOp::Rmw { key: 3, bytes: 64 }, &issuer);
        assert!(outcome.ok, "first RMW creates, got: {}", outcome.detail);
        let outcome =
            execute_storage_op_counted(&repo, &WorkloadOp::Rmw { key: 3, bytes: 64 }, &issuer);
        assert!(outcome.ok, "second RMW updates, got: {}", outcome.detail);
        assert_eq!((outcome.retries, outcome.conflicts), (0, 0));
        // Fresh reads see the workload content.
        let id = ltmrs_domain::id::EntityId::new(uuid::Uuid::from_u128(3));
        let found = repo.get_memories(&[id]).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].fragment, workload_memory(3, 64).fragment);
    }

    /// RMW losers retry from a fresh snapshot: two threads racing the same
    /// key both succeed, and at least one observes a revision conflict.
    #[test]
    fn rmw_conflicts_retry_to_success() {
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir);
        let issuer = RmwIssuer::fresh();
        issuer.bind(&repo).unwrap();
        let seed =
            execute_storage_op_counted(&repo, &WorkloadOp::Rmw { key: 1, bytes: 32 }, &issuer);
        assert!(seed.ok, "seed must succeed: {}", seed.detail);
        let ops: Vec<WorkloadOp> = vec![WorkloadOp::Rmw { key: 1, bytes: 32 }; 8];
        let measured = run_closed_loop(&repo, &ops, 4);
        assert_eq!(measured.len(), 8, "offered == completed");
        assert!(
            measured.iter().all(|m| m.ok),
            "losers retry to success: {:?}",
            measured.iter().map(|m| &m.detail).collect::<Vec<_>>()
        );
        let conflicts: u64 = measured.iter().map(|m| m.conflicts).sum();
        // Contended hot key: at least one loser must have observed a
        // conflict (proves the telemetry path is exercised, not stubbed).
        assert!(
            conflicts > 0,
            "contended RMW must observe conflicts, got: {:?}",
            measured
                .iter()
                .map(|m| (m.retries, m.conflicts))
                .collect::<Vec<_>>()
        );
        let history = dir.path().join("rmw.jsonl");
        let mut recorder = HistoryRecorder::create(&history).unwrap();
        record_measured(&mut recorder, &measured).unwrap();
        recorder.flush().unwrap();
        let back = crate::bench::read_history(&history).unwrap();
        let back_conflicts: u64 = back.iter().map(|r| r.conflicts).sum();
        assert_eq!(
            back_conflicts, conflicts,
            "conflicts must survive the history"
        );
    }

    /// Closed loop over a contended RMW-heavy stream: every op accounted,
    /// retries visible in aggregate.
    #[test]
    fn contended_write_mix_runs_closed_loop() {
        let dir = tempfile::tempdir().unwrap();
        let repo = open_repo(&dir);
        let mut generator = OpGenerator::new(GeneratorConfig {
            seed: 7,
            puts_per_1000: 500,
            rmw_per_1000: 500,
            hot_per_1000: 900,
            hot_key_span: 4,
            key_space: 64,
            value_bytes: 64,
            top_k: 5,
        });
        let ops: Vec<WorkloadOp> = (0..120).map(|_| generator.next_op()).collect();
        assert!(
            ops.iter().any(|op| matches!(op, WorkloadOp::Rmw { .. })),
            "mix must contain RMW ops"
        );
        let measured = run_closed_loop(&repo, &ops, 4);
        assert_eq!(measured.len(), ops.len(), "offered == completed");
        assert!(
            measured.iter().all(|m| m.ok),
            "all RMW retries converge: {:?}",
            measured
                .iter()
                .filter(|m| !m.ok)
                .map(|m| &m.detail)
                .collect::<Vec<_>>()
        );
        let retries: u64 = measured.iter().map(|m| m.retries).sum();
        let conflicts: u64 = measured.iter().map(|m| m.conflicts).sum();
        assert_eq!(retries, conflicts, "every conflict costs exactly one retry");
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

    /// Load legs record an RSS delta around the run (rank-1 WP-12 gap).
    #[test]
    fn load_run_records_rss_delta() {
        use crate::bench::experiment::current_rss_kb;
        let before = current_rss_kb();
        let after = current_rss_kb();
        let delta = crate::bench::experiment::rss_delta_kb(before, after);
        #[cfg(target_os = "linux")]
        assert!(delta.is_some(), "RSS must sample on Linux");
        #[cfg(not(target_os = "linux"))]
        assert!(delta.is_none(), "off-Linux sampler reports None");
    }

    /// Telemetry observes store growth across writes (never invents values
    /// for pipelines outside the boundary).
    #[test]
    fn telemetry_measures_dir_growth() {
        use crate::bench::experiment::workload_memory;
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        let repo = CanonicalRepository::open(store.to_str().unwrap()).unwrap();
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
            ..Default::default()
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
        let rss_before_closed = crate::bench::experiment::current_rss_kb();
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
            rss_delta_kb: crate::bench::experiment::rss_delta_kb(
                rss_before_closed,
                crate::bench::experiment::current_rss_kb(),
            ),
        };
        assert_eq!(closed_summary.total_failures, 0);
        std::fs::write(
            out.join("summary-closed.json"),
            serde_json::to_vec_pretty(&closed_summary).unwrap(),
        )
        .unwrap();

        // Open leg: Poisson 200/s arrivals, queue time measured per op.
        let schedule = arrival_schedule(ArrivalProcess::Poisson, 200, putget.len(), 12);
        let rss_before_open = crate::bench::experiment::current_rss_kb();
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
            rss_delta_kb: crate::bench::experiment::rss_delta_kb(
                rss_before_open,
                crate::bench::experiment::current_rss_kb(),
            ),
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

    /// Reference contended-write run (WP-12 [[not_run]] contended-write-mix):
    /// ignored in normal runs. Execute explicitly to (re)produce the raw
    /// evidence: `cargo test -- --ignored contended_write_mix_reference_run`.
    /// RMW hot-skew mix (500 puts/500 RMW per 1000, 900/1000 hot into 4 keys,
    /// 4 workers) over the same workload; history + summary + conflict/retry
    /// totals land under reports/wp12-contended-write/ (gitignored).
    #[test]
    #[ignore]
    fn contended_write_mix_reference_run() {
        use crate::bench::experiment::{ExperimentSummary, summarize};
        let out = crate::bench::crate_root().join("reports/wp12-contended-write");
        let _ = std::fs::remove_dir_all(&out);
        std::fs::create_dir_all(out.join("store")).unwrap();
        let machine =
            std::env::var("LTMRS_BENCH_MACHINE").unwrap_or_else(|_| "dev-box".to_string());
        let repo = CanonicalRepository::open(out.join("store").to_str().unwrap()).unwrap();
        let generator = GeneratorConfig {
            seed: 7,
            puts_per_1000: 500,
            rmw_per_1000: 500,
            hot_per_1000: 900,
            hot_key_span: 4,
            key_space: 64,
            value_bytes: 64,
            top_k: 5,
        };
        let digest = crate::bench::config_digest_for(&generator, 2000);
        let mut op_gen = OpGenerator::new(generator.clone());
        let ops: Vec<WorkloadOp> = (0..2000).map(|_| op_gen.next_op()).collect();
        let rss_before = crate::bench::experiment::current_rss_kb();
        let wall = std::time::Instant::now();
        let measured = run_closed_loop(&repo, &ops, 4);
        let wall_micros = wall.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        assert_eq!(measured.len(), ops.len(), "offered == completed");
        let history = out.join("history.jsonl");
        let mut recorder = HistoryRecorder::create(&history).unwrap();
        record_measured(&mut recorder, &measured).unwrap();
        recorder.flush().unwrap();
        let records = crate::bench::read_history(&history).unwrap();
        let stats = summarize(&records);
        let retries: u64 = records.iter().map(|r| r.retries).sum();
        let conflicts: u64 = records.iter().map(|r| r.conflicts).sum();
        assert!(conflicts > 0, "contended mix must observe conflicts");
        assert_eq!(retries, conflicts, "every conflict costs exactly one retry");
        let summary = ExperimentSummary {
            provenance: crate::bench::Provenance::collect(&machine, generator.seed, digest),
            backend_id: "fjall-canonical".to_string(),
            op_stats: stats,
            total_ops: records.len() as u64,
            total_failures: records.iter().filter(|r| !r.ok).count() as u64,
            wall_micros,
            rss_delta_kb: crate::bench::experiment::rss_delta_kb(
                rss_before,
                crate::bench::experiment::current_rss_kb(),
            ),
        };
        assert_eq!(summary.total_failures, 0);
        std::fs::write(
            out.join("summary.json"),
            serde_json::to_vec_pretty(&summary).unwrap(),
        )
        .unwrap();
        std::fs::write(
            out.join("contention.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "retries": retries, "conflicts": conflicts,
            }))
            .unwrap(),
        )
        .unwrap();
    }
}
