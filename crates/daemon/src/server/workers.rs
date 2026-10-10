//! Background workers: socket server, shutdown, health, maintenance, housekeeping, projection, serve (moved verbatim from `server.rs`).

use std::sync::Arc;

use super::Daemon;
use super::DaemonError;
use super::connection::handle_connection;
use super::guards::wall_now_millis;
use ltmrs_domain::command::DomainResult;
use ltmrs_domain::command::{DomainError, DomainErrorCode};
use ltmrs_embeddings::artifacts::ArtifactCache;
use ltmrs_search::search::maintenance::MaintenanceScheduler;
use ltmrs_search::search::table::SearchTable;
use ltmrs_service::repository::CanonicalRepository;

impl Daemon {
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
                    Ok(stream) => {
                        let dispatcher = Arc::clone(&dispatcher);
                        let quotas = Arc::clone(&quotas);
                        tokio::spawn(async move {
                            let _ = handle_connection(stream, dispatcher, quotas).await;
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    // Unix: a rejected peer uid is skipped, the loop keeps
                    // serving (the listener stays alive).
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => continue,
                    // Windows: byte-mode pipe with no client yet; accept()
                    // already paces the retry, so just keep serving.
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
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
        // Same best-effort abort for the projection worker (dense and
        // lexical modes alike).
        let paborted = self
            .projection_worker
            .try_lock()
            .ok()
            .and_then(|mut guard| guard.take());
        if let Some(pworker) = paborted {
            pworker.abort();
        }
        // Same best-effort abort for the housekeeping worker.
        let haborted = self
            .housekeeping_worker
            .try_lock()
            .ok()
            .and_then(|mut guard| guard.take());
        if let Some(hworker) = haborted {
            hworker.abort();
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
        // Windows pipes have no filesystem residue: the name is gone with
        // the last handle.
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.paths.endpoint);
    }

    /// Build a health/doctor report (no memory contents). The served
    /// generation is read live; when it is unreadable the report degrades
    /// explicitly (`ready: false`) instead of inventing generation 1.
    pub fn health_report(
        &self,
        ready: bool,
        projection_current: bool,
    ) -> crate::health::HealthReport {
        let (generation, ready) = match self.dispatcher.repo_arc().store_generation() {
            Ok(generation) => (generation, ready),
            Err(_) => (ltmrs_domain::id::StoreGeneration::FIRST, false),
        };
        crate::health::build_health_report(
            ready,
            generation,
            crate::dispatcher::Dispatcher::protocol_version(),
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

    /// Canonical housekeeping cadence: expired namespaces/retries are a
    /// slow leak (24h TTL), so an hourly bounded pass is plenty; errors are
    /// loud and retried next tick, never fatal to serving.
    const HOUSEKEEPING_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3600);

    /// Spawn the canonical housekeeping worker if not already running.
    /// Idempotent — safe to call from both serve() and tests. Independent
    /// from search maintenance: runs with no search path, no models, and
    /// no Lance table (lexical-only deployments collect too). The first
    /// pass runs immediately so short-lived daemons still collect.
    pub async fn start_housekeeping(&self) {
        let mut hw = self.housekeeping_worker.lock().await;
        if hw.is_some() {
            return;
        }
        let repo = self.dispatcher.repo_arc();
        let clock = Arc::clone(self.dispatcher.clock());
        *hw = Some(tokio::spawn(async move {
            loop {
                let collected = tokio::task::spawn_blocking({
                    let repo = Arc::clone(&repo);
                    let clock = Arc::clone(&clock);
                    move || repo.gc_expired(clock.now_millis())
                })
                .await;
                match collected {
                    Ok(Ok(0)) => {}
                    Ok(Ok(n)) => eprintln!("ltmrs: housekeeping collected {n} expired receipts"),
                    Ok(Err(e)) => eprintln!(
                        "ltmrs: housekeeping pass failed ({}); retrying next tick",
                        e.message
                    ),
                    Err(join_err) => eprintln!(
                        "ltmrs: housekeeping pass panicked ({join_err}); retrying next tick"
                    ),
                }
                tokio::time::sleep(Self::HOUSEKEEPING_INTERVAL).await;
            }
        }));
    }

    /// Whether the housekeeping worker is currently running (diagnostics/tests).
    pub async fn housekeeping_worker_running(&self) -> bool {
        self.housekeeping_worker.lock().await.is_some()
    }

    /// Spawn the projection worker: dense when E5 embedding is configured,
    /// lexical-only otherwise (text rows with NULL vectors; the dense leg
    /// excludes them explicitly). Idempotent. Each tick rebuilds the
    /// projector at the repo's current generation (a cutover can never
    /// strand it refusing publishes) and drives pending jobs off the Tokio
    /// I/O workers via `spawn_blocking` (candle inference is synchronous
    /// CPU; the single outer bridge contains no nested blocking). Embed
    /// failures stay pending and retry next tick; lexical rows publish
    /// regardless. A commit wake drives the new job immediately (P2-1); the
    /// interval is the maintenance fallback. Without a search path the
    /// worker parks (tests and minimal setups keep the degraded path).
    pub async fn start_projection(&self) {
        let mut pw = self.projection_worker.lock().await;
        if pw.is_some() {
            return;
        }
        let repo = self.dispatcher.repo_arc();
        let table = match SearchTable::open(&self.search_path).await {
            Ok(table) => table,
            Err(e) => {
                eprintln!(
                    "ltmrs: projection table unavailable ({}); indexing parked",
                    e.message
                );
                return;
            }
        };
        /// What each worker tick embeds with: loaded E5 weights, or nothing
        /// (lexical rows only, jobs resolve instead of retrying vectors).
        enum EmbedderSource {
            E5(std::sync::Arc<std::sync::Mutex<ltmrs_embeddings::e5_small::E5SmallAdapter>>),
            Lexical,
        }
        let source = match self.models_dir.clone() {
            Some(models_dir) => {
                let cache = ArtifactCache::new(&models_dir);
                // Digest-hash + weight load (~1GB with the query service) runs on
                // the blocking pool: holding the worker guard across it is fine
                // (async yield only), but Tokio I/O workers must never hash.
                let adapter = match tokio::task::spawn_blocking(move || {
                    ltmrs_embeddings::e5_small::E5SmallAdapter::load_from_cache(&cache)
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
                // Upgrade backfill: rows projected while lexical-only carry
                // NULL vectors; requeue them for dense embedding now.
                Self::backfill_null_vectors(&repo, &table).await;
                EmbedderSource::E5(adapter)
            }
            None => {
                eprintln!("ltmrs: no embedding models; lexical-only projection");
                EmbedderSource::Lexical
            }
        };
        let lexical = matches!(source, EmbedderSource::Lexical);
        let interval = self.maintenance_config.interval;
        let trigger = self.projection_trigger.clone();
        *pw = Some(tokio::spawn(async move {
            loop {
                let embedder: Box<dyn ltmrs_search::search::projector::Embedder + Send> =
                    match &source {
                        EmbedderSource::E5(adapter) => Box::new(std::sync::Arc::clone(adapter)),
                        EmbedderSource::Lexical => {
                            Box::new(ltmrs_search::search::projector::StalledEmbedder)
                        }
                    };
                let (driven, fts) = Self::drive_projection_batch(
                    &repo,
                    &table,
                    embedder,
                    Self::MAX_PROJECTION_JOBS_PER_TICK,
                    lexical,
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

    /// Requeue memories whose projected rows lack vectors (lexical-era rows)
    /// for dense embedding now that E5 is available. Skips memories with
    /// pending jobs (already covered) and non-recallable ones. Best-effort:
    /// failures log loudly and the next start retries. Upgrade path for
    /// daemons that ran lexical-only.
    pub(crate) async fn backfill_null_vectors(
        repo: &Arc<CanonicalRepository>,
        table: &SearchTable,
    ) {
        let rows = match table.rows_where("embedding IS NULL").await {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!(
                    "ltmrs: dense backfill scan failed ({}); retry on next start",
                    e.message
                );
                return;
            }
        };
        let mut ids: Vec<ltmrs_domain::id::EntityId> = rows.iter().map(|r| r.memory_id).collect();
        ids.sort();
        ids.dedup();
        if ids.is_empty() {
            return;
        }
        let mut queued = 0usize;
        for memory in repo.get_memories(&ids).unwrap_or_default() {
            if !memory.lifecycle.is_recallable() {
                continue;
            }
            match repo.enqueue_projection_job_if_absent(memory.id, memory.document_revision, 1) {
                Ok(true) => queued += 1,
                Ok(false) => {}
                Err(e) => eprintln!(
                    "ltmrs: dense backfill enqueue failed ({}); retry on next start",
                    e.message
                ),
            }
        }
        if queued > 0 {
            eprintln!("ltmrs: dense backfill requeued {queued} lexical-era memories");
        }
    }

    /// One bounded projection pass: drive pending jobs off the Tokio I/O
    /// workers (inference is synchronous CPU; the spawn_blocking bridge
    /// keeps it there), then rebuild the FTS index after a successful
    /// drive. Returns jobs resolved plus the FTS outcome. Extracted so
    /// drain behavior is unit-testable without the E5 adapter. `lexical`
    /// drives text-only rows to resolution instead of retrying vectors.
    pub(crate) async fn drive_projection_batch(
        repo: &Arc<CanonicalRepository>,
        table: &SearchTable,
        embedder: Box<dyn ltmrs_search::search::projector::Embedder + Send>,
        max_jobs: usize,
        lexical: bool,
    ) -> (DomainResult<usize>, Option<DomainResult<bool>>) {
        let repo = Arc::clone(repo);
        let table = table.clone();
        let table_fts = table.clone();
        let driven = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async {
                if lexical {
                    ltmrs_search::search::projector::Projector::project_pending_lexical(
                        &repo, &table, max_jobs,
                    )
                    .await
                } else {
                    ltmrs_search::search::projector::Projector::project_pending(
                        &repo, &table, embedder, max_jobs,
                    )
                    .await
                }
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
        // Start projection (dense with E5, lexical-only otherwise).
        self.start_projection().await;
        // Start canonical housekeeping (namespace/GC collection).
        self.start_housekeeping().await;

        let listener = &self.runtime.listener;

        if self.idle_timeout_millis == 0 {
            loop {
                match listener.accept().await {
                    Ok(stream) => {
                        let dispatcher = Arc::clone(&self.dispatcher);
                        let quotas = Arc::clone(&self.quotas);
                        tokio::spawn(async move {
                            let _ = handle_connection(stream, dispatcher, quotas).await;
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => continue,
                    // Windows: a byte-mode pipe with no client data reports
                    // `WouldBlock` once the connect-data gate times out; keep
                    // serving (the idle-exit loop below treats it as a tick).
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(e) => return Err(e.into()),
                }
            }
        }

        // Idle-exit mode: bound each accept wait by the timeout, then exit
        // once no connection has been active for the window. Precision is
        // one timeout granularity — fine for a hygiene shutdown.
        let tracker = std::sync::Arc::new(std::sync::Mutex::new(
            crate::idle::IdleExitTracker::new(wall_now_millis()),
        ));
        loop {
            let wait = tokio::time::timeout(
                std::time::Duration::from_millis(self.idle_timeout_millis),
                listener.accept(),
            )
            .await;
            let idle_tick = match wait {
                Ok(Ok(stream)) => {
                    tracker
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .note_connect(wall_now_millis());
                    let dispatcher = Arc::clone(&self.dispatcher);
                    let quotas = Arc::clone(&self.quotas);
                    let tracker = std::sync::Arc::clone(&tracker);
                    tokio::spawn(async move {
                        // Disconnect is noted via Drop so a panicking
                        // connection cannot wedge the counter (which would
                        // disable idle-exit forever — fail-safe is to exit).
                        struct DropNote {
                            tracker: std::sync::Arc<std::sync::Mutex<crate::idle::IdleExitTracker>>,
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
                    false
                }
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::PermissionDenied => continue,
                // Windows: a byte-mode pipe with no client data reports
                // `WouldBlock` once the connect-data gate times out; treat it
                // exactly like a timeout tick so idle-exit still fires
                // instead of erroring out.
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::WouldBlock => true,
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => true,
            };
            if idle_tick {
                let t = tracker.lock().unwrap_or_else(|e| e.into_inner());
                if t.should_exit(wall_now_millis(), self.idle_timeout_millis) {
                    // Persist session history before the idle exit: the
                    // alternative silently discards everything since start.
                    // A failure is loud (previous file intact via
                    // tmp+rename: loss bounded to this run's sessions).
                    if let Some(p) = &self.sessions_path
                        && let Err(e) = self.dispatcher.registry().persist(p)
                    {
                        eprintln!("ltmrs: failed to persist session history at idle exit: {e:?}");
                    }
                    return Ok(());
                }
            }
        }
    }
}
