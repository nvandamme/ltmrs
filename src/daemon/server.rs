//! Daemon lifecycle (design §7.1, §7.2). Ties the singleton lock, secure
//! socket, dispatcher, scheduler and quotas into a running daemon with a
//! bounded accept loop and graceful shutdown.

use std::sync::Arc;

use tokio::io::AsyncReadExt;
use tokio::task::JoinHandle;

use crate::daemon::dispatcher::Dispatcher;
use crate::daemon::envelope::{
    IpcError, IpcResponse, MAX_FRAME_BYTES, WireError, WireMessage, WireReply,
    write_response_payload,
};
use crate::daemon::limits::{QuotaTracker, ResourceLimits};
use crate::daemon::registry::FrontendRegistry;
use crate::daemon::runtime::{DaemonRuntime, RuntimeError, RuntimePaths, acquire_singleton};
use crate::daemon::scheduler::{EmbeddingScheduler, SchedulerConfig};
use crate::domain::clock::{Clock, SystemClock};
use crate::domain::command::{DomainError, DomainErrorCode, DomainResult};
use crate::embeddings::artifacts::ArtifactCache;
use crate::search::backend::SearchBackend;
use crate::search::maintenance::{MaintenanceConfig, MaintenanceScheduler};
use crate::search::table::SearchTable;
use crate::service::repository::CanonicalRepository;

/// Commit-to-search wake-up (P2-1 projection latency).
///
/// The projection worker used to sleep a full maintenance interval between
/// passes, so a newly saved memory stayed invisible to dense retrieval for
/// minutes. Now every committed canonical mutation fires the commit hook,
/// which wakes this trigger; the worker drives the new job immediately in a
/// bounded batch. The interval remains as the maintenance fallback (repair /
/// retry of failed passes) — the wake-up is a latency optimization only, and
/// the durable pending-job records stay the recovery mechanism.
#[derive(Debug, Clone, Default)]
pub struct ProjectionTrigger {
    notify: Arc<tokio::sync::Notify>,
}

impl ProjectionTrigger {
    pub fn new() -> Self {
        Self {
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Wake the worker: a canonical commit just landed.
    pub fn wake(&self) {
        self.notify.notify_one();
    }

    /// Wait for the next drive: returns on a commit wake or when `interval`
    /// elapses, whichever comes first.
    pub async fn wait(&self, interval: std::time::Duration) {
        tokio::select! {
            _ = self.notify.notified() => {},
            _ = tokio::time::sleep(interval) => {},
        }
    }
}

/// Which embedding backend the daemon runs (design §9; RQ-09).
#[derive(Debug, Clone, Default)]
pub enum EmbeddingMode {
    /// No dense embedding: lexical/canonical retrieval only. Tools fall back
    /// to the canonical snapshot scan when no search backend is attached.
    #[default]
    Disabled,
    /// Load the pinned E5-small adapter from a model cache dir at startup
    /// (fail fast when artifacts are missing) and attach the query-side
    /// dense bridge to the dispatcher. Requires `search_path` for the
    /// projection table. Wiring only: per-request fingerprint plumbing and
    /// the projection loop that writes dense vectors land separately, so a
    /// fresh E5 start still serves lexical/canonical recall.
    E5SmallCached { cache_dir: String },
}

/// Configuration for the daemon.
#[derive(Debug, Clone, Default)]
pub struct DaemonConfig {
    /// The Fjall store path.
    pub store_path: String,
    /// The session-state path (persisted on shutdown, loaded on start).
    pub sessions_path: String,
    /// The Lance search-projection directory. Empty disables the maintenance
    /// scheduler (the projection is still rebuildable from canonical state).
    pub search_path: String,
    /// Resource limits.
    pub limits: ResourceLimits,
    /// Idle-exit timeout: serve() returns once no connection has been active
    /// for this long. 0 (default) serves forever, preserving current behavior.
    pub idle_timeout_millis: u64,
    /// Which embedding backend to run. Disabled by default (lexical-only).
    pub embedding: EmbeddingMode,
    /// Scheduler configuration.
    pub scheduler: SchedulerConfig,
    /// Maintenance schedule + explicit budgets for optimization/retention.
    pub maintenance: MaintenanceConfig,
}

/// Error from the daemon.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("runtime: {0}")]
    Runtime(#[from] RuntimeError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("domain: {code:?}: {message}")]
    Domain {
        code: DomainErrorCode,
        message: String,
    },
    #[error("ipc: {0}")]
    Ipc(IpcError),
    #[error("embedding: {message}")]
    Embedding { message: String },
}

impl From<DomainError> for DaemonError {
    fn from(e: DomainError) -> Self {
        Self::Domain {
            code: e.code,
            message: e.message,
        }
    }
}

impl From<IpcError> for DaemonError {
    fn from(e: IpcError) -> Self {
        match e {
            IpcError::Io(io) => DaemonError::Io(io),
            other => DaemonError::Ipc(other),
        }
    }
}

/// The running daemon, holding the singleton lock for its lifetime.
pub struct Daemon {
    dispatcher: Arc<Dispatcher>,
    scheduler: EmbeddingScheduler,
    scheduler_worker: Option<JoinHandle<()>>,
    /// Owned embedding worker (E5 mode): shut down explicitly so no
    /// inference thread outlives daemon shutdown (the backend holds its
    /// own clone for queries; Drop would only fire at full teardown).
    embedding: Option<crate::embeddings::service::EmbeddingService>,
    maintenance_worker: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    maintenance_config: MaintenanceConfig,
    /// Dense-projection worker (E5 mode only): drives pending projection
    /// jobs to the Lance table. Aborted on shutdown like maintenance.
    projection_worker: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    /// Verified model-cache directory (E5 mode only): the projection
    /// worker loads its passage adapter from here. `None` parks
    /// projection (lexical-only daemon).
    models_dir: Option<String>,
    quotas: Arc<QuotaTracker>,
    /// Commit-to-search wake-up (P2-1): fired by the repository commit hook,
    /// waited on by the projection worker. The interval stays the fallback.
    projection_trigger: ProjectionTrigger,
    /// The singleton lock + bound 0600 socket listener, kept alive for the
    /// daemon's lifetime.
    runtime: DaemonRuntime,
    /// Background socket accept loop for stdio-owned daemons (P1 shared
    /// lifecycle): bound sockets must accept, otherwise a second frontend
    /// dials a dead listener. Aborted on shutdown.
    socket_server: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    paths: RuntimePaths,
    sessions_path: Option<std::path::PathBuf>,
    search_path: String,
    idle_timeout_millis: u64,
}

impl Daemon {
    /// Start the daemon: acquire the singleton lock, open the store, wire the
    /// components. Async because E5 mode opens the Lance projection table.
    /// The returned daemon must be kept alive to hold the lock.
    pub async fn start(paths: &RuntimePaths, config: DaemonConfig) -> Result<Self, DaemonError> {
        let runtime = acquire_singleton(paths)?;

        let clock: Arc<dyn Clock + Send + Sync> = Arc::new(SystemClock);
        let repo = Arc::new(
            CanonicalRepository::open_with_clock(&config.store_path, Arc::clone(&clock))
                .map_err(DaemonError::from)?,
        );
        // Restore durable session history from a previous run (if any).
        // Traced sessions + receipts live in the canonical store: a
        // pre-migration sessions.json splits its traced state into a
        // legacy payload, imported once here (idempotent by handle and
        // operation ID). Bindings and virtual sessions stay in the file.
        let (registry, sessions_path) = if config.sessions_path.is_empty() {
            (FrontendRegistry::new(), None)
        } else {
            let p = std::path::PathBuf::from(&config.sessions_path);
            let (registry, legacy) = FrontendRegistry::load(&p).map_err(DaemonError::Io)?;
            let imported = repo
                .import_legacy_sessions(legacy.sessions, legacy.receipts)
                .map_err(DaemonError::from)?;
            if imported > 0 {
                eprintln!("ltmrs: imported {imported} legacy session record(s) into the store");
            }
            (registry, Some(p))
        };
        let (dispatcher, embedding, models_dir) = match &config.embedding {
            EmbeddingMode::Disabled => {
                (Arc::new(Dispatcher::new(repo, registry, clock)), None, None)
            }
            EmbeddingMode::E5SmallCached { cache_dir } => {
                if config.search_path.is_empty() {
                    return Err(DaemonError::Embedding {
                        message: "E5 embedding requires search_path for the projection table"
                            .into(),
                    });
                }
                let cache = ArtifactCache::new(cache_dir);
                let service =
                    crate::embeddings::service::EmbeddingService::load_e5_small_from_cache(&cache)
                        .map_err(|e| DaemonError::Embedding {
                            message: e.to_string(),
                        })?;
                let table = SearchTable::open(&config.search_path)
                    .await
                    .map_err(DaemonError::from)?;
                let embedder: std::sync::Arc<dyn crate::retrieval::engine::QueryEmbedder> =
                    std::sync::Arc::new(crate::search::backend::ServiceQueryEmbedder::new(
                        service.clone(),
                    ));
                let backend = Arc::new(SearchBackend::new(Arc::clone(&repo), table, embedder));
                (
                    Arc::new(Dispatcher::new(repo, registry, clock).with_search(backend)),
                    Some(service),
                    Some(cache_dir.clone()),
                )
            }
        };
        // Eager durability (P1): session mutations persist before ack.
        dispatcher.set_sessions_path(sessions_path.clone());
        // P2-1 wake-up: every committed mutation wakes the projection
        // worker so new memories become searchable without waiting out
        // the maintenance interval.
        let projection_trigger = ProjectionTrigger::new();
        {
            let waker = projection_trigger.clone();
            dispatcher
                .repo_arc()
                .set_commit_hook(std::sync::Arc::new(move || {
                    waker.wake();
                }));
        }

        let (scheduler, scheduler_worker) = EmbeddingScheduler::spawn(
            Box::new(crate::daemon::scheduler::FixedDimAdapter { dim: 384 }),
            config.scheduler,
        );
        let quotas = Arc::new(QuotaTracker::new(config.limits));

        Ok(Self {
            dispatcher,
            scheduler,
            scheduler_worker: Some(scheduler_worker),
            embedding,
            maintenance_worker: tokio::sync::Mutex::new(None),
            maintenance_config: config.maintenance,
            projection_worker: tokio::sync::Mutex::new(None),
            models_dir,
            quotas,
            runtime,
            projection_trigger,
            socket_server: tokio::sync::Mutex::new(None),
            paths: paths.clone(),
            sessions_path,
            search_path: config.search_path,
            idle_timeout_millis: config.idle_timeout_millis,
        })
    }

    /// The Lance search-projection directory (empty disables maintenance).
    pub fn search_path(&self) -> &str {
        &self.search_path
    }

    /// The resolver for the socket path (for frontends to connect).
    pub fn socket_path(&self) -> &std::path::Path {
        &self.paths.socket_path
    }

    /// The sessions snapshot path for eager persistence (P1 owned-daemon
    /// linger): empty when sessions are disabled.
    pub fn sessions_path_display(&self) -> String {
        self.sessions_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Access the dispatcher (for session operations, diagnostics).
    pub fn dispatcher(&self) -> &Dispatcher {
        &self.dispatcher
    }

    /// Access the quota tracker (for the accept loop, diagnostics).
    pub fn quotas(&self) -> Arc<QuotaTracker> {
        Arc::clone(&self.quotas)
    }

    /// A clone of the dispatcher handle (for the accept loop, tests).
    pub fn dispatcher_arc(&self) -> Arc<Dispatcher> {
        Arc::clone(&self.dispatcher)
    }

    /// Whether the scheduler worker has been aborted (post-shutdown).
    pub fn scheduler_worker_aborted(&self) -> bool {
        self.scheduler_worker.is_none()
    }

    /// Spawn the background socket accept loop (P1 shared-daemon lifecycle).
    /// The stdio path owns a bound 0600 listener but never called `serve()`,
    /// so a second frontend dialing the managed socket parked forever. This
    /// serves that listener with the same `handle_connection` path as `serve`,
    /// idempotently: a second call is a no-op. The loop lives until `shutdown`
    /// aborts it or the daemon drops (lock release).
    pub async fn spawn_socket_server(&self) {
        let mut guard = self.socket_server.lock().await;
        if guard.is_some() {
            return;
        }
        let listener = Arc::clone(&self.runtime.listener);
        let dispatcher = Arc::clone(&self.dispatcher);
        let quotas = Arc::clone(&self.quotas);
        *guard = Some(tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let dispatcher = Arc::clone(&dispatcher);
                        let quotas = Arc::clone(&quotas);
                        tokio::spawn(async move {
                            let _ = handle_connection(stream, dispatcher, quotas).await;
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        }));
    }

    /// Whether the background socket server is running (diagnostics/tests).
    pub async fn socket_server_running(&self) -> bool {
        self.socket_server.lock().await.is_some()
    }

    /// Coordinate shutdown: persist durable session history, abort the embedding
    /// scheduler and stop the maintenance worker. Committed receipts already live
    /// in the store and survive independently; this ensures session state and
    /// background jobs are cleaned up so a restart restores history and leaks no workers.
    pub fn shutdown(&mut self) {
        if let Some(p) = &self.sessions_path
            && let Err(e) = self.dispatcher.registry().persist(p)
        {
            // Loud, not silent: session history since start is lost, but
            // the previous file stays intact (tmp+rename), so the loss is
            // bounded to this run's sessions.
            eprintln!("ltmrs: failed to persist session history at shutdown: {e:?}");
        }
        if let Some(svc) = self.embedding.take() {
            svc.shutdown();
        }
        if let Some(worker) = self.scheduler_worker.take() {
            worker.abort();
        }
        // The maintenance handle lives behind a tokio Mutex (set from serve's async
        // context); poison is harmless here — we still abort on best effort.
        let aborted = self
            .maintenance_worker
            .try_lock()
            .ok()
            .and_then(|mut guard| guard.take());
        if let Some(mworker) = aborted {
            mworker.abort();
        }
        // Same best-effort abort for the projection worker (E5 mode only).
        let paborted = self
            .projection_worker
            .try_lock()
            .ok()
            .and_then(|mut guard| guard.take());
        if let Some(pworker) = paborted {
            pworker.abort();
        }
        // Same best-effort abort for the background socket server (stdio path).
        let saborted = self
            .socket_server
            .try_lock()
            .ok()
            .and_then(|mut guard| guard.take());
        if let Some(server) = saborted {
            server.abort();
        }
        // Unlink our socket while still holding the singleton lock: no new
        // owner can have bound it yet (binding requires the lock), so this
        // cannot delete a successor's socket. Graceful exits then leave no
        // stale file behind, and "socket gone" observably means "daemon
        // gone". Best-effort: crashes keep the old stale-recovery path.
        let _ = std::fs::remove_file(&self.paths.socket_path);
    }

    /// Build a health/doctor report (no memory contents).
    pub fn health_report(
        &self,
        ready: bool,
        projection_current: bool,
    ) -> crate::daemon::health::HealthReport {
        crate::daemon::health::build_health_report(
            ready,
            crate::domain::id::StoreGeneration::FIRST,
            crate::daemon::dispatcher::Dispatcher::protocol_version(),
            projection_current,
            &self.scheduler,
            &self.quotas,
        )
    }

    /// Spawn the maintenance worker if enabled (search_path set) and not already
    /// running. Idempotent — safe to call from both serve() and tests.
    pub async fn start_maintenance(&self) {
        let mut mw = self.maintenance_worker.lock().await;
        if mw.is_some() || self.search_path.is_empty() {
            return;
        }
        match SearchTable::open(&self.search_path).await {
            Ok(table) => {
                let sched = MaintenanceScheduler::new(table, self.maintenance_config)
                    .with_repo(self.dispatcher.repo_arc());
                *mw = Some(sched.spawn());
            }
            Err(e) => eprintln!(
                "ltmrs: failed to open search table for maintenance: {} ({})",
                e.code.as_str(),
                e.message
            ),
        }
    }

    /// Whether the maintenance worker is currently running (diagnostics/tests).
    pub async fn maintenance_worker_running(&self) -> bool {
        self.maintenance_worker.lock().await.is_some()
    }

    /// Spawn the dense-projection worker when E5 embedding is configured.
    /// Idempotent. Each tick rebuilds the projector at the repo's current
    /// generation (a cutover can never strand it refusing publishes) and
    /// drives pending jobs off the Tokio I/O workers via `spawn_blocking`
    /// (candle inference is synchronous CPU; the single outer bridge
    /// contains no nested blocking). Embed failures stay pending and retry
    /// next tick; lexical rows publish regardless. A commit wake drives the
    /// new job immediately (P2-1); the interval is the maintenance fallback.
    /// Without embedding this parks silently:
    /// lexical-only daemons have nothing to index densely.
    pub async fn start_projection(&self) {
        let mut pw = self.projection_worker.lock().await;
        if pw.is_some() {
            return;
        }
        let Some(models_dir) = self.models_dir.clone() else {
            return;
        };
        let repo = self.dispatcher.repo_arc();
        let table = match SearchTable::open(&self.search_path).await {
            Ok(table) => table,
            Err(e) => {
                eprintln!(
                    "ltmrs: projection table unavailable ({}); dense indexing parked",
                    e.message
                );
                return;
            }
        };
        let cache = ArtifactCache::new(&models_dir);
        // Digest-hash + weight load (~1GB with the query service) runs on
        // the blocking pool: holding the worker guard across it is fine
        // (async yield only), but Tokio I/O workers must never hash.
        let adapter = match tokio::task::spawn_blocking(move || {
            crate::embeddings::e5_small::E5SmallAdapter::load_from_cache(&cache)
        })
        .await
        {
            Ok(Ok(adapter)) => std::sync::Arc::new(std::sync::Mutex::new(adapter)),
            Ok(Err(e)) => {
                eprintln!(
                    "ltmrs: projection adapter failed ({e}); dense indexing parked; \
                     run `ltmrs --provision-models` to repair"
                );
                return;
            }
            Err(join_err) => {
                eprintln!("ltmrs: projection adapter load panicked ({join_err}); parked");
                return;
            }
        };
        let interval = self.maintenance_config.interval;
        let trigger = self.projection_trigger.clone();
        *pw = Some(tokio::spawn(async move {
            loop {
                let (driven, fts) = Self::drive_projection_batch(
                    &repo,
                    &table,
                    Box::new(std::sync::Arc::clone(&adapter)),
                    Self::MAX_PROJECTION_JOBS_PER_TICK,
                )
                .await;
                let resolved = match driven {
                    Err(e) => {
                        eprintln!(
                            "ltmrs: projection pass failed ({}); retrying next tick",
                            e.message
                        );
                        0
                    }
                    Ok(n) => {
                        if n > 0 {
                            eprintln!("ltmrs: projection converged {n} job(s)");
                        }
                        n
                    }
                };
                match fts {
                    Some(Err(fe)) => eprintln!(
                        "ltmrs: fts index build failed ({}); retrying next tick",
                        fe.message
                    ),
                    Some(Ok(true)) => eprintln!("ltmrs: fts index built"),
                    _ => {}
                }
                // Drain-while-full (re-review R6): a pass that hit the batch
                // cap likely leaves runnable backlog, so drive again after
                // yielding instead of sleeping through the interval. A pass
                // below the cap converged (only stale/failing work remains),
                // as does any failed pass — those wait on wake-or-interval.
                // No busy loop: resolutions acknowledge-and-clear their jobs,
                // so consecutive full passes each retire a full batch.
                if resolved < Self::MAX_PROJECTION_JOBS_PER_TICK {
                    // Commit wake or maintenance interval, whichever comes
                    // first (P2-1): new memories drive immediately in a
                    // bounded batch; the interval covers repair/retries.
                    trigger.wait(interval).await;
                } else {
                    tokio::task::yield_now().await;
                }
            }
        }));
    }

    /// One bounded projection pass: drive pending jobs off the Tokio I/O
    /// workers (inference is synchronous CPU; the spawn_blocking bridge
    /// keeps it there), then rebuild the FTS index after a successful
    /// drive. Returns jobs resolved plus the FTS outcome. Extracted so
    /// drain behavior is unit-testable without the E5 adapter.
    async fn drive_projection_batch(
        repo: &Arc<CanonicalRepository>,
        table: &SearchTable,
        embedder: Box<dyn crate::search::projector::Embedder + Send>,
        max_jobs: usize,
    ) -> (DomainResult<usize>, Option<DomainResult<bool>>) {
        let repo = Arc::clone(repo);
        let table = table.clone();
        let table_fts = table.clone();
        let driven = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async {
                crate::search::projector::Projector::project_pending(
                    &repo, &table, embedder, max_jobs,
                )
                .await
            })
        })
        .await;
        match driven {
            Err(join_err) => (
                Err(DomainError::new(
                    DomainErrorCode::Validation,
                    format!("projection drive panicked ({join_err}); retrying next tick"),
                )),
                None,
            ),
            Ok(Err(e)) => (Err(e), None),
            Ok(Ok(resolved)) => {
                // FTS only after a successful drive: a failing table needs
                // repair, not an index build.
                let fts = Some(table_fts.ensure_fts_index().await);
                (Ok(resolved), fts)
            }
        }
    }

    /// Whether the projection worker is currently running (diagnostics/tests).
    pub async fn projection_worker_running(&self) -> bool {
        self.projection_worker.lock().await.is_some()
    }

    /// Max projection jobs resolved per tick (fairness §7.3): bounds one
    /// tick to minutes of CPU at measured embed rates so a bulk backfill
    /// converges over successive ticks instead of one unbounded pass
    /// starving interactive recall. Steady-state write rates never bind.
    const MAX_PROJECTION_JOBS_PER_TICK: usize = 100;

    /// Run the accept loop until the socket is closed, an error occurs, or
    /// the idle timeout elapses with no connections (design §7.2).
    /// Uses the 0600 listener bound at startup (already permission-locked).
    pub async fn serve(&self) -> Result<(), DaemonError> {
        // Start background maintenance under its explicit budgets, if configured.
        self.start_maintenance().await;
        // Start dense projection when E5 embedding is configured (no-op otherwise).
        self.start_projection().await;

        let listener = &self.runtime.listener;

        if self.idle_timeout_millis == 0 {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let dispatcher = Arc::clone(&self.dispatcher);
                        let quotas = Arc::clone(&self.quotas);
                        tokio::spawn(async move {
                            let _ = handle_connection(stream, dispatcher, quotas).await;
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e.into()),
                }
            }
        }

        // Idle-exit mode: bound each accept wait by the timeout, then exit
        // once no connection has been active for the window. Precision is
        // one timeout granularity — fine for a hygiene shutdown.
        let tracker = std::sync::Arc::new(std::sync::Mutex::new(
            crate::daemon::idle::IdleExitTracker::new(wall_now_millis()),
        ));
        loop {
            let wait = tokio::time::timeout(
                std::time::Duration::from_millis(self.idle_timeout_millis),
                listener.accept(),
            )
            .await;
            match wait {
                Ok(Ok((stream, _))) => {
                    tracker.lock().unwrap().note_connect(wall_now_millis());
                    let dispatcher = Arc::clone(&self.dispatcher);
                    let quotas = Arc::clone(&self.quotas);
                    let tracker = std::sync::Arc::clone(&tracker);
                    tokio::spawn(async move {
                        // Disconnect is noted via Drop so a panicking
                        // connection cannot wedge the counter (which would
                        // disable idle-exit forever — fail-safe is to exit).
                        struct DropNote {
                            tracker: std::sync::Arc<
                                std::sync::Mutex<crate::daemon::idle::IdleExitTracker>,
                            >,
                        }
                        impl Drop for DropNote {
                            fn drop(&mut self) {
                                // Never panic in Drop (would abort during
                                // unwinding): a poisoned mutex just skips.
                                if let Ok(mut t) = self.tracker.lock() {
                                    t.note_disconnect(wall_now_millis());
                                }
                            }
                        }
                        let _note = DropNote { tracker };
                        let _ = handle_connection(stream, dispatcher, quotas).await;
                    });
                }
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => {
                    let t = tracker.lock().unwrap();
                    if t.should_exit(wall_now_millis(), self.idle_timeout_millis) {
                        // Persist session history before the idle exit: the
                        // alternative silently discards everything since start.
                        // A failure is loud (previous file intact via
                        // tmp+rename: loss bounded to this run's sessions).
                        if let Some(p) = &self.sessions_path
                            && let Err(e) = self.dispatcher.registry().persist(p)
                        {
                            eprintln!(
                                "ltmrs: failed to persist session history at idle exit: {e:?}"
                            );
                        }
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Wall-clock millis for idle tracking (same shape as the maintenance
/// retention clock; serve has no injected clock by design).
fn wall_now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Same-user IPC boundary (design §7.1): the peer's UID must equal ours.
/// The 0600 socket already restricts access; this closes the remainder
/// (permissive umask at bind, fd passing). Raw UIDs, no exceptions — not
/// even root connecting elsewhere.
fn peer_authorized(peer_uid: u32, own_uid: u32) -> bool {
    peer_uid == own_uid
}

/// Reject connections from other UIDs before reading any frame.
fn check_peer_cred(stream: &tokio::net::UnixStream) -> Result<(), DaemonError> {
    let peer = stream.peer_cred().map_err(DaemonError::from)?.uid();
    // SAFETY: geteuid takes no arguments and only reads process state.
    if !peer_authorized(peer, unsafe { libc::geteuid() }) {
        return Err(DaemonError::Ipc(IpcError::Unauthorized));
    }
    Ok(())
}

/// Handle a single client connection: read frames, dispatch, write responses.
/// The first frame MUST be a handshake; it authenticates the frontend and
/// issues its retry namespace. Subsequent frames are typed domain requests.
pub async fn handle_connection(
    mut stream: tokio::net::UnixStream,
    dispatcher: Arc<Dispatcher>,
    quotas: Arc<QuotaTracker>,
) -> Result<(), DaemonError> {
    // Same-user boundary first: never read a frame from another UID.
    if let Err(e) = check_peer_cred(&stream) {
        let err = WireError {
            kind: "unauthorized".into(),
            message: e.to_string(),
        };
        write_reply(&mut stream, &WireReply::Error(err), &quotas).await?;
        return Ok(());
    }
    let mut buf = Vec::with_capacity(4096);

    // Live-connection tracking for restore readiness: counted while this
    // task lives (RAII decrement on every exit path, including panic
    // unwind). Persisted channel bindings outlive their runs and must
    // never stand in for liveness.
    let _live_guard = LiveConnGuard::new(&dispatcher);
    // Released at connection end on every path below: assigned after a
    // successful handshake, dropped when this function returns.
    let mut _client_guard: Option<ClientGuard> = None;
    // Handshake-authenticated frontend; every later frame must carry it.
    // None until the first accepted handshake (a generation-mismatched
    // first attempt keeps the connection open for a same-stream retry).
    let mut authed: Option<crate::domain::id::FrontendId> = None;
    // Handshake-authenticated channel; every later frame must carry it.
    // Frontend-only binding would let one channel's frames reach another
    // channel's session (RQ-05).
    let mut authed_channel: Option<crate::domain::id::ChannelId> = None;

    // ---- Frames: the first must be a handshake. A generation-mismatched
    // handshake keeps the connection open for a same-stream retry (restore
    // bumps the generation mid-session); every other rejection closes it.
    // ---- Request loop ----
    loop {
        let msg = match read_wire_frame(&mut stream, &mut buf).await {
            Ok(m) => m,
            Err(DaemonError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        match msg {
            WireMessage::Handshake(req) => {
                // RQ-05: on an authenticated connection only a same-identity
                // epoch refresh is accepted; a different identity stays a
                // protocol violation, never routed anywhere.
                if let (Some(af), Some(ac)) = (authed, authed_channel)
                    && (req.frontend_id != af || req.channel_id != ac)
                {
                    let err = WireError {
                        kind: "unexpected_handshake".into(),
                        message: "handshake already completed".into(),
                    };
                    write_reply(&mut stream, &WireReply::Error(err), &quotas).await?;
                    continue;
                }
                let frontend = req.frontend_id;
                let channel = req.channel_id;
                let dispatcher = Arc::clone(&dispatcher);
                let result = tokio::task::spawn_blocking(move || dispatcher.handle_handshake(&req))
                    .await
                    .unwrap_or_else(|e| Err(IpcError::from(std::io::Error::other(e.to_string()))));
                match result {
                    Ok(hs) => {
                        // Admit the client to the quota table on first success
                        // only (a refresh reuses the held slot); a full table
                        // rejects instead of over-admitting (RQ-22).
                        if _client_guard.is_none() {
                            if let Err(qe) = quotas.register_client(frontend, channel) {
                                let err = WireError {
                                    kind: "client_limit_reached".into(),
                                    message: qe.to_string(),
                                };
                                write_reply(&mut stream, &WireReply::Error(err), &quotas).await?;
                                return Ok(());
                            }
                            // Free the connection's quota slot at connection end
                            // on every path below. Bound to the
                            // handshake-authenticated (frontend, channel), not
                            // request-claimed ones: each connection holds
                            // exactly one slot.
                            _client_guard =
                                Some(ClientGuard::new(Arc::clone(&quotas), frontend, channel));
                        }
                        authed = Some(frontend);
                        authed_channel = Some(channel);
                        let reply = WireReply::Handshake(hs);
                        write_reply(&mut stream, &reply, &quotas).await?;
                    }
                    Err(e) => {
                        // Rejected handshake: send a wire error. A generation
                        // mismatch keeps the connection open for a same-stream
                        // retry (the frontend adopts the live generation);
                        // every other rejection closes it, as before.
                        let keep_open = matches!(&e, IpcError::GenerationMismatch { .. });
                        let kind = match &e {
                            IpcError::GenerationMismatch { .. } => "generation_mismatch",
                            IpcError::Busy(_) => "daemon_busy",
                            IpcError::StaleNamespace(_) => "stale_namespace",
                            _ => "handshake_rejected",
                        };
                        let err = WireError {
                            kind: kind.into(),
                            message: e.to_string(),
                        };
                        write_reply(&mut stream, &WireReply::Error(err), &quotas).await?;
                        if !keep_open {
                            return Ok(());
                        }
                        // De-authenticate so a failed epoch refresh retries
                        // from scratch (slot freed, re-registered on success).
                        _client_guard = None;
                        authed = None;
                        authed_channel = None;
                    }
                }
                continue;
            }
            WireMessage::Request(env) => {
                if authed.is_none() {
                    // A request before a handshake is a protocol violation.
                    let err = WireError {
                        kind: "handshake_required".into(),
                        message: "first frame must be a handshake".into(),
                    };
                    write_reply(&mut stream, &WireReply::Error(err), &quotas).await?;
                    return Ok(());
                }
                let envelope = env;
                // Bind every frame to the handshake identity (RQ-05): a frame
                // claiming another frontend OR another channel is a protocol
                // violation, never routed into its session namespace or quota
                // bucket. Channel is bound too: same-frontend frames must not
                // reach a sibling channel's session.
                if Some(envelope.frontend_id) != authed {
                    let err = WireError {
                        kind: "frontend_mismatch".into(),
                        message: "frame frontend differs from handshake identity".into(),
                    };
                    write_reply(&mut stream, &WireReply::Error(err), &quotas).await?;
                    continue;
                }
                if Some(envelope.channel_id) != authed_channel {
                    let err = WireError {
                        kind: "channel_mismatch".into(),
                        message: "frame channel differs from handshake identity".into(),
                    };
                    write_reply(&mut stream, &WireReply::Error(err), &quotas).await?;
                    continue;
                }
                // (Client slot was bound to the handshake ID by the guard above.)

                // Enforce the per-client quota: visible backpressure, not unbounded work.
                if let Err(qe) = quotas.try_enqueue(envelope.frontend_id) {
                    let busy = DomainError::new(
                        DomainErrorCode::Validation,
                        format!("backpressure: {qe}"),
                    );
                    let resp = IpcResponse::error(envelope.operation_id, &busy);
                    write_reply(&mut stream, &WireReply::Response(resp), &quotas).await?;
                    continue;
                }

                // In-flight storage bound: refuse visibly instead of piling up
                // blocking work beyond the configured concurrency.
                if let Err(qe) = quotas.try_start_storage() {
                    quotas.dequeue(envelope.frontend_id);
                    let busy = DomainError::new(
                        DomainErrorCode::Validation,
                        format!("backpressure: {qe}"),
                    );
                    let resp = IpcResponse::error(envelope.operation_id, &busy);
                    write_reply(&mut stream, &WireReply::Response(resp), &quotas).await?;
                    continue;
                }

                // Dispatch on a blocking thread: Fjall write transactions + fsync
                // must not block a Tokio core I/O worker (§7.3).
                let op_id = envelope.operation_id;
                let fe_id = envelope.frontend_id;
                let dispatcher = Arc::clone(&dispatcher);
                let dispatch_result =
                    tokio::task::spawn_blocking(move || dispatcher.handle(&envelope))
                        .await
                        .unwrap_or_else(|e| {
                            Err(DomainError::new(
                                DomainErrorCode::Validation,
                                format!("dispatch task failed: {e}"),
                            ))
                        });
                let response = match dispatch_result {
                    Ok(r) => r,
                    Err(e) => IpcResponse::error(op_id, &e),
                };
                quotas.finish_storage();
                quotas.dequeue(fe_id);

                // Write the response.
                write_reply(&mut stream, &WireReply::Response(response), &quotas).await?;
            }
        }
    }
}

/// Tracks one live IPC connection in the frontend registry (RAII decrement
/// on drop, mirroring `ClientGuard`). Restore readiness counts live
/// connections, never persisted channel history.
struct LiveConnGuard {
    dispatcher: Arc<Dispatcher>,
}

impl LiveConnGuard {
    fn new(dispatcher: &Arc<Dispatcher>) -> Self {
        dispatcher.registry().note_live_connect();
        Self {
            dispatcher: Arc::clone(dispatcher),
        }
    }
}

impl Drop for LiveConnGuard {
    fn drop(&mut self) {
        self.dispatcher.registry().note_live_disconnect();
    }
}

/// Unregisters the connection's frontend from the quota table on drop, so a
/// disconnect frees its client slot on every return path above. Bound to the
/// handshake-authenticated ID at construction (never request-claimed IDs).
struct ClientGuard {
    quotas: Arc<QuotaTracker>,
    frontend: crate::domain::id::FrontendId,
    channel: crate::domain::id::ChannelId,
}

impl ClientGuard {
    fn new(
        quotas: Arc<QuotaTracker>,
        frontend: crate::domain::id::FrontendId,
        channel: crate::domain::id::ChannelId,
    ) -> Self {
        Self {
            quotas,
            frontend,
            channel,
        }
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.quotas.unregister_client(self.frontend, self.channel);
    }
}

/// Read one tagged wire frame (a `WireMessage`) from the stream.
async fn read_wire_frame(
    stream: &mut tokio::net::UnixStream,
    buf: &mut Vec<u8>,
) -> Result<WireMessage, DaemonError> {
    buf.clear();
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(DaemonError::Ipc(IpcError::FrameTooLarge(len)));
    }
    buf.resize(len, 0);
    stream.read_exact(buf).await?;
    let msg: WireMessage = serde_json::from_slice(buf).map_err(|e| DaemonError::Domain {
        code: DomainErrorCode::Validation,
        message: format!("invalid wire frame: {e}"),
    })?;
    Ok(msg)
}

/// Write a tagged wire reply as a stream: an 8-byte total-length header
/// (u64 BE) followed by length-prefixed chunk frames, each bounded by
/// `MAX_FRAME_BYTES`. Uniform for small and large replies — large payloads
/// stream across multiple frames instead of being rejected or truncated.
async fn write_reply(
    stream: &mut tokio::net::UnixStream,
    reply: &WireReply,
    quotas: &QuotaTracker,
) -> Result<(), DaemonError> {
    let payload = serde_json::to_vec(reply)?;
    // Response byte budget (RQ-22): oversized data responses are refused
    // explicitly, never silently truncated. Control replies (handshake,
    // errors) always pass — they are small by construction and required
    // for the protocol to report failures at all.
    if matches!(reply, WireReply::Response(_)) && !quotas.allows_response(payload.len()) {
        let too_big = DomainError::new(
            DomainErrorCode::Validation,
            format!("response exceeds budget ({} bytes)", payload.len()),
        );
        let op_id = match reply {
            WireReply::Response(resp) => resp.operation_id,
            _ => unreachable!("checked above"),
        };
        let fallback = WireReply::Response(IpcResponse::error(op_id, &too_big));
        let payload = serde_json::to_vec(&fallback)?;
        write_response_payload(stream, &payload).await?;
        return Ok(());
    }
    write_response_payload(stream, &payload).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::envelope::{DomainRequest, IpcEnvelope};
    use crate::domain::clock::FrozenClock;
    use crate::domain::command::Scope;
    use crate::domain::id::{
        ChannelId, DocumentRevision, EligibilityRevision, EntityId, EntityRevision, FrontendId,
        OperationId, StoreGeneration,
    };
    use crate::domain::memory::{FragmentType, Instant, Memory, MemoryLifecycle, MemorySource};
    use uuid::Uuid;

    #[test]
    fn daemon_config_defaults() {
        let config = DaemonConfig::default();
        assert!(config.store_path.is_empty());
        assert!(config.limits.max_clients > 0);
        assert_eq!(config.idle_timeout_millis, 0, "serve forever by default");
    }

    /// Same-user IPC boundary: a connected peer with our UID passes.
    #[test]
    fn peer_authorized_matches_uids() {
        assert!(peer_authorized(1000, 1000));
        assert!(!peer_authorized(0, 1000));
        assert!(!peer_authorized(1000, 0));
    }

    /// A live socket pair shares our UID, so the peer check passes.
    #[tokio::test]
    async fn peer_cred_same_uid_passes() {
        let (a, _b) = tokio::net::UnixStream::pair().unwrap();
        assert!(check_peer_cred(&a).is_ok());
    }

    /// Channel binding (RQ-05): frames must carry the handshake channel as
    /// well as the frontend. A same-frontend frame naming another channel
    /// is rejected, never routed into that channel's session.
    #[tokio::test]
    async fn channel_spoofed_frames_are_rejected() {
        use crate::daemon::envelope::{
            HandshakeRequest, PROTOCOL_VERSION, WireMessage, WireReply, read_response_payload,
        };
        use crate::service::repository::CanonicalRepository;
        use tokio::io::AsyncWriteExt;

        async fn write_frame(stream: &mut tokio::net::UnixStream, msg: &WireMessage) {
            let payload = serde_json::to_vec(msg).unwrap();
            stream
                .write_all(&(payload.len() as u32).to_be_bytes())
                .await
                .unwrap();
            stream.write_all(&payload).await.unwrap();
            stream.flush().await.unwrap();
        }
        async fn read_reply(stream: &mut tokio::net::UnixStream) -> WireReply {
            let bytes = read_response_payload(stream).await.unwrap();
            serde_json::from_slice(&bytes).unwrap()
        }

        let dir = tempfile::tempdir().unwrap();
        let clock: std::sync::Arc<dyn crate::domain::clock::Clock + Send + Sync> =
            std::sync::Arc::new(FrozenClock::new(1000));
        let repo = std::sync::Arc::new(
            CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), clock.clone())
                .unwrap(),
        );
        let dispatcher = std::sync::Arc::new(Dispatcher::new(repo, FrontendRegistry::new(), clock));
        let quotas = std::sync::Arc::new(QuotaTracker::default());
        let (server_end, mut client_end) = tokio::net::UnixStream::pair().unwrap();
        let server_handle = tokio::spawn(async move {
            let _ = handle_connection(server_end, dispatcher, quotas).await;
        });

        let fe = FrontendId::new(Uuid::from_u128(1));
        write_frame(
            &mut client_end,
            &WireMessage::Handshake(HandshakeRequest {
                protocol_version: PROTOCOL_VERSION,
                store_generation: StoreGeneration::FIRST,
                frontend_id: fe,
                channel_id: ChannelId::new(Uuid::from_u128(2)),
                resume_retry_epoch: None,
            }),
        )
        .await;
        assert!(
            matches!(read_reply(&mut client_end).await, WireReply::Handshake(_)),
            "handshake must succeed first"
        );
        // Same frontend, another channel: must be rejected, not routed.
        let mut seq = 100u128;
        let mut spoofed = || {
            seq += 1;
            IpcEnvelope {
                protocol_version: PROTOCOL_VERSION,
                store_generation: StoreGeneration::FIRST,
                frontend_id: fe,
                channel_id: ChannelId::new(Uuid::from_u128(99)),
                operation_id: OperationId::new(Uuid::from_u128(seq)),
                session: None,
                retry_epoch: 1,
                deadline_millis: None,
                scope: Scope::default(),
                body: DomainRequest::ListMemories,
            }
        };
        write_frame(&mut client_end, &WireMessage::Request(Box::new(spoofed()))).await;
        match read_reply(&mut client_end).await {
            WireReply::Error(err) => assert_eq!(
                err.kind, "channel_mismatch",
                "spoofed channel must be refused as channel_mismatch, got {}: {}",
                err.kind, err.message
            ),
            other => panic!("spoofed channel frame must not be routed, got {other:?}"),
        }
        // The bound channel still works on the same connection.
        let mut legit = spoofed();
        legit.channel_id = ChannelId::new(Uuid::from_u128(2));
        write_frame(&mut client_end, &WireMessage::Request(Box::new(legit))).await;
        assert!(
            matches!(read_reply(&mut client_end).await, WireReply::Response(_)),
            "handshake channel must keep working"
        );
        server_handle.abort();
    }
    /// The maintenance worker spawns when a search path is configured and is
    /// aborted on shutdown (task 10 scheduling integration).
    #[tokio::test]
    async fn maintenance_worker_spawns_with_search_path_and_aborts_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "maint-store");
        let config = DaemonConfig {
            store_path: dir.path().join("store").to_str().unwrap().to_string(),
            search_path: dir.path().join("search").to_str().unwrap().to_string(),
            ..Default::default()
        };
        let mut daemon = Daemon::start(&paths, config).await.unwrap();

        // Not running before serve/start_maintenance.
        assert!(!daemon.maintenance_worker_running().await);

        daemon.start_maintenance().await;
        assert!(
            daemon.maintenance_worker_running().await,
            "maintenance worker must spawn when a search path is configured"
        );

        // Idempotent: a second call does not stack another worker.
        daemon.start_maintenance().await;
        assert!(daemon.maintenance_worker_running().await);

        // Shutdown takes the handle out (aborting it) so no orphan survives and
        // the daemon reports maintenance as stopped.
        daemon.shutdown();
        assert!(
            !daemon.maintenance_worker_running().await,
            "shutdown must clear the maintenance worker"
        );
    }

    #[test]
    fn embedding_disabled_by_default() {
        assert!(
            matches!(DaemonConfig::default().embedding, EmbeddingMode::Disabled),
            "the daemon must stay lexical-only unless embedding is configured"
        );
    }

    /// No embedding configured: projection stays parked (no worker, no
    /// crash) and shutdown is a clean no-op for it.
    #[tokio::test]
    async fn projection_worker_parked_without_embedding() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "proj-store");
        let config = DaemonConfig {
            store_path: dir.path().join("store").to_str().unwrap().to_string(),
            search_path: dir.path().join("search").to_str().unwrap().to_string(),
            ..Default::default()
        };
        let mut daemon = Daemon::start(&paths, config).await.unwrap();

        assert!(!daemon.projection_worker_running().await);
        daemon.start_projection().await;
        assert!(
            !daemon.projection_worker_running().await,
            "lexical-only daemons must not spawn a projection worker"
        );

        daemon.shutdown();
        assert!(!daemon.projection_worker_running().await);
    }

    /// E5 mode without model artifacts fails fast at startup (no silent
    /// lexical-only fallback that callers could mistake for dense search).
    #[tokio::test]
    async fn e5_mode_with_missing_cache_fails_fast() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "e5-store");
        let config = DaemonConfig {
            store_path: dir.path().join("store").to_str().unwrap().to_string(),
            search_path: dir.path().join("search").to_str().unwrap().to_string(),
            embedding: EmbeddingMode::E5SmallCached {
                cache_dir: dir
                    .path()
                    .join("no-models-here")
                    .to_str()
                    .unwrap()
                    .to_string(),
            },
            ..Default::default()
        };
        let err = match Daemon::start(&paths, config).await {
            Ok(_) => panic!("startup with a missing model cache must fail"),
            Err(e) => e,
        };
        assert!(
            matches!(err, DaemonError::Embedding { .. }),
            "missing model cache must fail fast with an embedding error, got: {err}"
        );
    }

    #[tokio::test]
    async fn start_and_serve_lifecycle() {
        // Verify the daemon starts, acquires the lock, and exposes the socket.
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");
        let config = DaemonConfig {
            store_path: dir.path().join("store").to_str().unwrap().to_string(),
            ..Default::default()
        };
        let daemon = Daemon::start(&paths, config.clone()).await.unwrap();
        assert!(daemon.socket_path().ends_with("daemon.sock"));
        // The lock is held; a second start must fail.
        let result = Daemon::start(&paths, config).await;
        assert!(result.is_err());
    }

    /// Test dispatcher with explicit quotas (bypasses Daemon::start, which
    /// always uses the configured limits).
    fn test_dispatcher_with_quotas(
        dir: &tempfile::TempDir,
        quotas: std::sync::Arc<crate::daemon::limits::QuotaTracker>,
    ) -> (
        std::sync::Arc<crate::daemon::dispatcher::Dispatcher>,
        std::sync::Arc<crate::daemon::limits::QuotaTracker>,
    ) {
        use crate::daemon::dispatcher::Dispatcher;
        use crate::daemon::registry::FrontendRegistry;
        use crate::service::repository::CanonicalRepository;

        let clock: Arc<dyn Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
        let repo = Arc::new(
            CanonicalRepository::open_with_clock(
                dir.path().join("store").to_str().unwrap(),
                Arc::clone(&clock),
            )
            .unwrap(),
        );
        repo.issue_namespace(FrontendId::new(Uuid::from_u128(1)), 1000)
            .unwrap();
        (
            Arc::new(Dispatcher::new(repo, FrontendRegistry::new(), clock)),
            quotas,
        )
    }

    fn handshake_as(fe_n: u64) -> crate::daemon::envelope::HandshakeRequest {
        crate::daemon::envelope::HandshakeRequest {
            protocol_version: crate::daemon::envelope::PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id: FrontendId::new(Uuid::from_u128(fe_n as u128)),
            channel_id: ChannelId::new(Uuid::from_u128(2)),
            resume_retry_epoch: None,
        }
    }

    /// P1-B: resuming a live namespace reissues the SAME epoch (no counter
    /// bump); unknown epochs refuse as stale-typed errors, never a silent
    /// fresh epoch.
    #[test]
    fn handshake_resume_reissues_same_epoch() {
        use crate::daemon::limits::{QuotaTracker, ResourceLimits};

        let dir = tempfile::tempdir().unwrap();
        let quotas = Arc::new(QuotaTracker::new(ResourceLimits::default()));
        let (dispatcher, _quotas) = test_dispatcher_with_quotas(&dir, quotas);
        // Epoch 1 was issued at setup.
        let mut resume = handshake_as(1);
        resume.resume_retry_epoch = Some(1);
        let hs = dispatcher.handle_handshake(&resume).unwrap();
        assert_eq!(hs.retry_epoch, 1);
        // No counter consumed: the next fresh handshake still yields 2.
        let fresh = dispatcher.handle_handshake(&handshake_as(1)).unwrap();
        assert_eq!(fresh.retry_epoch, 2);
        // Unknown epoch refuses typed.
        let mut unknown = handshake_as(1);
        unknown.resume_retry_epoch = Some(99);
        let err = dispatcher.handle_handshake(&unknown).unwrap_err();
        assert!(
            matches!(err, crate::daemon::envelope::IpcError::StaleNamespace(_)),
            "unknown epoch must refuse stale-typed, got: {err:?}"
        );
    }

    /// A full client table rejects new handshakes instead of over-admitting.
    #[tokio::test]
    async fn handshake_rejected_when_client_limit_reached() {
        use crate::daemon::client::IpcClient;
        use crate::daemon::limits::{QuotaTracker, ResourceLimits};

        let dir = tempfile::tempdir().unwrap();
        let quotas = Arc::new(QuotaTracker::new(ResourceLimits {
            max_clients: 1,
            ..Default::default()
        }));
        // Fill the single slot with another frontend.
        quotas
            .register_client(
                FrontendId::new(Uuid::from_u128(99)),
                crate::domain::id::ChannelId::new(Uuid::from_u128(98)),
            )
            .unwrap();
        let (dispatcher, quotas) = test_dispatcher_with_quotas(&dir, quotas);

        let (client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let server = tokio::spawn(handle_connection(server_stream, dispatcher, quotas));
        let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
        client.set_stream(client_stream);
        let err = client.handshake(&handshake_as(1)).await.unwrap_err();
        assert!(
            err.to_string().contains("limit")
                || err.to_string().contains("reject")
                || err.to_string().contains("handshake"),
            "full table must reject, got: {err}"
        );
        let _ = server.await;
    }

    /// The client slot is held for the whole connection: a second client is
    /// rejected while the first is connected, and admitted after it
    /// disconnects (no sleeps — EOF drives every transition).
    #[tokio::test]
    async fn client_slot_held_during_connection() {
        use crate::daemon::client::IpcClient;
        use crate::daemon::limits::{QuotaTracker, ResourceLimits};

        let dir = tempfile::tempdir().unwrap();
        let quotas = Arc::new(QuotaTracker::new(ResourceLimits {
            max_clients: 1,
            ..Default::default()
        }));
        let (dispatcher, quotas) = test_dispatcher_with_quotas(&dir, quotas);

        // First client connects: slot held.
        let (a_stream, a_server) = tokio::net::UnixStream::pair().unwrap();
        let a_task = tokio::spawn(handle_connection(
            a_server,
            Arc::clone(&dispatcher),
            Arc::clone(&quotas),
        ));
        let mut client_a = IpcClient::new(std::path::PathBuf::from("unused"));
        client_a.set_stream(a_stream);
        client_a.handshake(&handshake_as(1)).await.unwrap();
        assert_eq!(quotas.client_count(), 1);

        // Second client rejected while the first is live.
        let (b_stream, b_server) = tokio::net::UnixStream::pair().unwrap();
        let b_task = tokio::spawn(handle_connection(
            b_server,
            Arc::clone(&dispatcher),
            Arc::clone(&quotas),
        ));
        let mut client_b = IpcClient::new(std::path::PathBuf::from("unused"));
        client_b.set_stream(b_stream);
        assert!(client_b.handshake(&handshake_as(2)).await.is_err());
        let _ = b_task.await;

        // First disconnects: slot freed, third client admitted.
        client_a.close();
        let _ = a_task.await;
        assert_eq!(quotas.client_count(), 0);
        let (c_stream, c_server) = tokio::net::UnixStream::pair().unwrap();
        let c_task = tokio::spawn(handle_connection(
            c_server,
            Arc::clone(&dispatcher),
            Arc::clone(&quotas),
        ));
        let mut client_c = IpcClient::new(std::path::PathBuf::from("unused"));
        client_c.set_stream(c_stream);
        client_c.handshake(&handshake_as(3)).await.unwrap();
        client_c.close();
        let _ = c_task.await;
    }

    /// Frames claiming another frontend than the handshake are rejected
    /// before dispatch (RQ-05 channel binding).
    #[tokio::test]
    async fn mismatched_frame_frontend_rejected() {
        use crate::daemon::client::IpcClient;

        let dir = tempfile::tempdir().unwrap();
        let quotas = Arc::new(crate::daemon::limits::QuotaTracker::new(
            crate::daemon::limits::ResourceLimits::default(),
        ));
        let (dispatcher, quotas) = test_dispatcher_with_quotas(&dir, quotas);

        let (client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let _server = tokio::spawn(handle_connection(server_stream, dispatcher, quotas));
        let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
        client.set_stream(client_stream);
        client.handshake(&handshake_as(1)).await.unwrap();
        // Same channel, forged frontend: must not route.
        let mut env = IpcEnvelope {
            protocol_version: crate::daemon::envelope::PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id: FrontendId::new(Uuid::from_u128(1)),
            channel_id: ChannelId::new(Uuid::from_u128(2)),
            operation_id: OperationId::new(Uuid::from_u128(10)),
            session: None,
            retry_epoch: 1,
            deadline_millis: None,
            scope: Scope::default(),
            body: DomainRequest::ListMemories,
        };
        env.frontend_id = FrontendId::new(Uuid::from_u128(99));
        assert!(
            client.roundtrip(&env).await.is_err(),
            "forged frontend frame must be rejected"
        );
    }

    /// In-flight storage exhaustion answers busy instead of queueing
    /// unboundedly.
    #[tokio::test]
    async fn storage_busy_answers_backpressure() {
        use crate::daemon::client::IpcClient;
        use crate::daemon::envelope::IpcResult;
        use crate::daemon::limits::{QuotaTracker, ResourceLimits};

        let dir = tempfile::tempdir().unwrap();
        let quotas = Arc::new(QuotaTracker::new(ResourceLimits {
            max_in_flight_storage: 0,
            ..Default::default()
        }));
        let (dispatcher, quotas) = test_dispatcher_with_quotas(&dir, quotas);

        let (client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let _server = tokio::spawn(handle_connection(server_stream, dispatcher, quotas));
        let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
        client.set_stream(client_stream);
        client.handshake(&handshake_as(1)).await.unwrap();
        let env = DomainRequest::ListMemories;
        let envelope = IpcEnvelope {
            protocol_version: crate::daemon::envelope::PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id: FrontendId::new(Uuid::from_u128(1)),
            channel_id: ChannelId::new(Uuid::from_u128(2)),
            operation_id: OperationId::new(Uuid::from_u128(1)),
            session: None,
            retry_epoch: 1,
            deadline_millis: None,
            scope: Scope::default(),
            body: env,
        };
        let resp = client.roundtrip(&envelope).await.unwrap();
        match resp.result {
            IpcResult::Error { message, .. } => assert!(
                message.contains("backpressure") || message.contains("busy"),
                "must signal busy, got: {message}"
            ),
            other => panic!("expected busy error, got: {other:?}"),
        }
    }

    /// Responses beyond the byte budget are refused explicitly, never
    /// truncated: seed enough content to overflow a tiny budget, then a
    /// list read comes back as an explicit error (control replies such as
    /// the handshake itself always pass — they are small by construction).
    #[tokio::test]
    async fn oversized_response_is_refused_not_truncated() {
        use crate::daemon::client::IpcClient;
        use crate::daemon::envelope::{HandshakeRequest, PROTOCOL_VERSION};
        use crate::daemon::limits::{QuotaTracker, ResourceLimits};

        let dir = tempfile::tempdir().unwrap();
        let quotas = Arc::new(QuotaTracker::new(ResourceLimits {
            max_response_bytes: 512,
            ..Default::default()
        }));
        let (dispatcher, quotas) = test_dispatcher_with_quotas(&dir, quotas);

        let (client_stream, server_stream) = tokio::net::UnixStream::pair().unwrap();
        let server = tokio::spawn(handle_connection(server_stream, dispatcher, quotas));
        let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
        client.set_stream(client_stream);
        let hs = client
            .handshake(&HandshakeRequest {
                protocol_version: PROTOCOL_VERSION,
                store_generation: StoreGeneration::FIRST,
                frontend_id: fe(1),
                channel_id: ch(1),
                resume_retry_epoch: None,
            })
            .await
            .unwrap();
        // Seed five memories (~2KB of list output, far over the budget).
        for n in 1..=5u64 {
            let env = IpcEnvelope {
                protocol_version: PROTOCOL_VERSION,
                store_generation: StoreGeneration::FIRST,
                frontend_id: fe(1),
                channel_id: ch(1),
                operation_id: op(10 + n),
                session: None,
                retry_epoch: hs.retry_epoch,
                deadline_millis: None,
                scope: Scope::default(),
                body: DomainRequest::AddMemory {
                    memory: test_memory(n),
                },
            };
            client.roundtrip(&env).await.unwrap();
        }
        // The list response overflows the budget: explicit error, not a
        // silently truncated payload.
        let env = IpcEnvelope {
            protocol_version: PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id: fe(1),
            channel_id: ch(1),
            operation_id: op(99),
            session: None,
            retry_epoch: hs.retry_epoch,
            deadline_millis: None,
            scope: Scope::default(),
            body: DomainRequest::ListMemories,
        };
        let resp = client.roundtrip(&env).await.unwrap();
        match resp.result {
            crate::daemon::envelope::IpcResult::Error { message, .. } => assert!(
                message.contains("exceeds budget"),
                "over-budget list must be refused, got: {message}"
            ),
            other => panic!("expected budget error, got: {other:?}"),
        }
        drop(client);
        let _ = server.await;
    }

    /// serve() exits when the idle timeout elapses with no connections
    /// (outer timeout guards against hanging here forever).
    #[tokio::test]
    async fn serve_exits_on_idle_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "idle-store");
        let config = DaemonConfig {
            store_path: dir.path().join("store").to_str().unwrap().to_string(),
            idle_timeout_millis: 50,
            ..Default::default()
        };
        let daemon = Daemon::start(&paths, config).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), daemon.serve())
            .await
            .expect("serve must exit on idle timeout, not hang")
            .unwrap();
    }

    /// Idle exit persists session history instead of discarding it: the
    /// sessions file must exist and parse after the exit.
    #[tokio::test]
    async fn serve_persists_sessions_on_idle_exit() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "idle-store");
        let sessions = dir.path().join("sessions.json");
        let config = DaemonConfig {
            store_path: dir.path().join("store").to_str().unwrap().to_string(),
            sessions_path: sessions.to_str().unwrap().to_string(),
            idle_timeout_millis: 50,
            ..Default::default()
        };
        let daemon = Daemon::start(&paths, config).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), daemon.serve())
            .await
            .expect("serve must exit on idle timeout, not hang")
            .unwrap();
        let raw = std::fs::read_to_string(&sessions).expect("sessions file must exist");
        let snapshot: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(
            snapshot.get("sessions").is_some(),
            "persisted snapshot must carry sessions"
        );
    }

    // ---- Cancellation / receipt-survives-connection-loss (T-CONC-04) ----

    fn fe(n: u64) -> FrontendId {
        FrontendId::new(Uuid::from_u128(n as u128))
    }
    fn ch(n: u64) -> ChannelId {
        ChannelId::new(Uuid::from_u128(n as u128))
    }
    fn op(n: u64) -> OperationId {
        OperationId::new(Uuid::from_u128(n as u128))
    }

    fn test_memory(id_num: u64) -> Memory {
        Memory {
            id: EntityId::new(Uuid::from_u128(id_num as u128)),
            external_alias: None,
            title: format!("mem-{id_num}"),
            fragment: format!("frag-{id_num}"),
            description: String::new(),
            fragment_type: FragmentType::Fact,
            project: None,
            source: MemorySource::Ai,
            confidence: 0.5,
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
            created_at: Instant::new(1),
            updated_at: Instant::new(1),
            raw_created: None,
            unknown_fields: std::collections::BTreeMap::new(),
        }
    }

    /// Shutdown persists routing state and aborts background jobs so a
    /// restart restores bindings while sessions live in the store
    /// (design §7.2, §7.3).
    #[tokio::test]
    async fn shutdown_persists_sessions_and_aborts_scheduler() {
        use crate::domain::session::SessionOp;

        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");
        let sessions_path = dir.path().join("sessions.json");
        let config = DaemonConfig {
            store_path: dir.path().join("store").to_str().unwrap().to_string(),
            sessions_path: sessions_path.to_str().unwrap().to_string(),
            ..Default::default()
        };
        let mut daemon = Daemon::start(&paths, config).await.unwrap();
        // Start a session through the canonical store so there is durable
        // state, and bind the channel to it.
        let handle = crate::domain::id::SessionHandle::new(uuid::Uuid::from_u128(77));
        match daemon
            .dispatcher()
            .repo()
            .session_start_tx(
                "test-op-1",
                "digest-1",
                handle,
                ch(1),
                Some("proj".into()),
                None,
                vec![],
                None,
                None,
                100,
            )
            .unwrap()
        {
            SessionOp::Applied(h) | SessionOp::Replayed(h) => {
                daemon
                    .dispatcher()
                    .registry()
                    .bind_session(fe(1), ch(1), h, false);
            }
            SessionOp::Conflict => panic!("test setup conflict"),
        }

        // Shutdown: persist routing + abort the scheduler worker.
        daemon.shutdown();

        // The sessions file exists and restores the channel binding, while
        // the session itself lives in the reopened store.
        assert!(sessions_path.exists(), "shutdown must persist sessions");
        let (restored, _) = FrontendRegistry::load(&sessions_path).unwrap();
        assert_eq!(restored.channel_count(), 1);
        assert_eq!(restored.channel_session(fe(1), ch(1)), Some(handle));
        let session = daemon
            .dispatcher()
            .repo()
            .get_session(handle)
            .unwrap()
            .expect("session must live in the store");
        assert_eq!(session.project.as_deref(), Some("proj"));
        // The scheduler worker was aborted.
        assert!(daemon.scheduler_worker_aborted());
    }

    /// A committed request whose client connection disappears must still have
    /// its durable receipt available (T-CONC-04 / T-REC-01).
    #[tokio::test]
    async fn committed_receipt_survives_connection_drop() {
        use crate::daemon::client::IpcClient;
        use crate::daemon::envelope::{HandshakeRequest, PROTOCOL_VERSION};

        let dir = tempfile::tempdir().unwrap();
        let clock: Arc<dyn Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
        let store_path = dir.path().join("store").to_str().unwrap().to_string();
        let repo = Arc::new(
            CanonicalRepository::open_with_clock(&store_path, Arc::clone(&clock)).unwrap(),
        );
        let dispatcher = Arc::new(Dispatcher::new(
            Arc::clone(&repo),
            FrontendRegistry::new(),
            clock,
        ));
        let quotas = Arc::new(QuotaTracker::new(ResourceLimits::default()));

        let (client_stream, server) = tokio::net::UnixStream::pair().unwrap();
        let handle = tokio::spawn(handle_connection(server, dispatcher, quotas));

        // Client connects, handshakes (which issues the retry namespace),
        // then sends an AddMemory request and drops before reading the
        // response — simulating a crash after the request is in flight.
        let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
        client.set_stream(client_stream);
        let hs = client
            .handshake(&HandshakeRequest {
                protocol_version: PROTOCOL_VERSION,
                store_generation: StoreGeneration::FIRST,
                frontend_id: fe(1),
                channel_id: ch(1),
                resume_retry_epoch: None,
            })
            .await
            .unwrap();

        let env = IpcEnvelope {
            protocol_version: PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id: fe(1),
            channel_id: ch(1),
            operation_id: op(1),
            session: None,
            retry_epoch: hs.retry_epoch,
            deadline_millis: None,
            scope: Scope::default(),
            body: DomainRequest::AddMemory {
                memory: test_memory(1),
            },
        };
        client.write_request(&env).await.unwrap();
        drop(client);

        // The daemon processes the request (commits memory + receipt) then
        // observes the dropped connection.
        let _ = handle.await.unwrap();

        // The durable receipt must still be available after the connection
        // disappeared, and the memory must be present.
        let receipt = repo
            .lookup_receipt(StoreGeneration::FIRST, fe(1), hs.retry_epoch, op(1))
            .unwrap();
        assert!(
            receipt.is_some(),
            "committed receipt must survive connection drop"
        );
        let memories = repo.get_memories(&[test_memory(1).id]).unwrap();
        assert_eq!(memories.len(), 1, "committed memory must survive");
    }

    /// P2-1 wake-up: a commit notification returns from the wait immediately
    /// (no interval sleep); without one the wait spans the full interval.
    /// Paused clock: fully deterministic, no real-time sleeps.
    #[tokio::test(start_paused = true)]
    async fn projection_trigger_wakes_on_commit_not_interval() {
        let trigger = ProjectionTrigger::new();
        let mut wait = Box::pin(trigger.wait(std::time::Duration::from_secs(300)));
        // Probe at t=0 (starts the interval timer), then elapse 299s: the
        // wait must still be pending — zero-duration timeouts probe liveness
        // without moving the clock.
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(0), &mut wait)
                .await
                .is_err()
        );
        tokio::time::advance(std::time::Duration::from_secs(299)).await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(0), &mut wait)
                .await
                .is_err(),
            "without a wake the wait must span the full interval"
        );
        // The 300th second completes the interval wait.
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut wait)
            .await
            .expect("interval expiry must still fire the wait");
        // A commit wake fires a fresh wait without any clock advance.
        trigger.wake();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            trigger.wait(std::time::Duration::from_secs(300)),
        )
        .await
        .expect("commit wake must fire immediately");
    }

    /// Re-review R6: more than two batch limits of pending work keeps
    /// draining across back-to-back bounded passes with no new commits and
    /// no interval wait. Deterministic: no clock advance between batches.
    #[tokio::test]
    async fn projection_drains_past_one_batch_without_sleep() {
        use crate::domain::command::DomainCommand;
        use crate::search::projector::FixedEmbedder;
        use crate::search::table::SearchTable;

        let dir = tempfile::tempdir().unwrap();
        let clock: Arc<dyn Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
        let repo = Arc::new(
            CanonicalRepository::open_with_clock(
                dir.path().join("store").to_str().unwrap(),
                Arc::clone(&clock),
            )
            .unwrap(),
        );
        repo.issue_namespace(fe(1), 1000).unwrap();
        let table_dir = dir.path().join("table");
        std::fs::create_dir_all(&table_dir).unwrap();
        let table = SearchTable::open(table_dir.to_str().unwrap())
            .await
            .unwrap();
        // Seed 250 pending jobs (2.5 batch limits at MAX=100).
        for n in 1..=250u64 {
            let ctx = crate::domain::command::CommandContext {
                store_generation: StoreGeneration::FIRST,
                frontend_id: fe(1),
                channel_id: ch(1),
                session: None,
                operation_id: OperationId::new(Uuid::from_u128(1000 + n as u128)),
                request_digest: format!("drain-test-{n}"),
                deadline_millis: None,
                scope: Scope::default(),
                retry_epoch: 1,
            };
            repo.apply(
                &ctx,
                &DomainCommand::AddMemory {
                    memory: test_memory(n),
                    session: None,
                },
            )
            .unwrap();
        }
        assert_eq!(repo.projection_lag().unwrap(), 250);
        // Three consecutive bounded batches, no writes and no clock advance
        // between them: 100 + 100 + 50 proves the worker drains runnable
        // backlog instead of sleeping after the first batch.
        let (first, _) = Daemon::drive_projection_batch(
            &repo,
            &table,
            Box::new(FixedEmbedder { dim: 384 }),
            100,
        )
        .await;
        assert_eq!(first.unwrap(), 100);
        let (second, _) = Daemon::drive_projection_batch(
            &repo,
            &table,
            Box::new(FixedEmbedder { dim: 384 }),
            100,
        )
        .await;
        assert_eq!(second.unwrap(), 100);
        let (third, _) = Daemon::drive_projection_batch(
            &repo,
            &table,
            Box::new(FixedEmbedder { dim: 384 }),
            100,
        )
        .await;
        assert_eq!(third.unwrap(), 50);
        assert_eq!(repo.projection_lag().unwrap(), 0);
    }
}
