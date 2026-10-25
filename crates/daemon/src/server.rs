//! Daemon lifecycle (design §7.1, §7.2). Ties the singleton lock, secure
//! socket, dispatcher, scheduler and quotas into a running daemon with a
//! bounded accept loop and graceful shutdown.

use std::sync::Arc;

use tokio::task::JoinHandle;

use crate::dispatcher::Dispatcher;
use crate::envelope::IpcError;
use crate::limits::{QuotaTracker, ResourceLimits};
use crate::registry::FrontendRegistry;
use crate::runtime::{DaemonRuntime, RuntimeError, RuntimePaths, acquire_singleton};
use crate::scheduler::{EmbeddingScheduler, SchedulerConfig};
use ltmrs_domain::clock::{Clock, SystemClock};
use ltmrs_domain::command::{DomainError, DomainErrorCode};
use ltmrs_embeddings::artifacts::ArtifactCache;
use ltmrs_search::search::backend::SearchBackend;
use ltmrs_search::search::maintenance::MaintenanceConfig;
use ltmrs_search::search::table::SearchTable;
use ltmrs_service::repository::CanonicalRepository;

pub mod connection;
mod guards;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod projection_tests;
#[cfg(test)]
mod test_support;
mod workers;

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
    embedding: Option<ltmrs_embeddings::service::EmbeddingService>,
    maintenance_worker: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    maintenance_config: MaintenanceConfig,
    /// Projection worker (dense with E5, lexical-only otherwise): drives
    /// pending projection jobs to the Lance table. Aborted on shutdown
    /// like maintenance.
    projection_worker: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    /// Canonical housekeeping worker: periodically collects expired retry
    /// namespaces and their receipts via `gc_expired`. Independent from
    /// search maintenance so lexical-only deployments (no Lance path)
    /// still collect. Aborted on shutdown.
    housekeeping_worker: tokio::sync::Mutex<Option<JoinHandle<()>>>,
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
                // Lexical projection stays available without a model: FTS
                // rows publish, dense simply has no vectors. No search path
                // means no table (tests and minimal setups keep the
                // degraded snapshot path).
                if config.search_path.is_empty() {
                    (Arc::new(Dispatcher::new(repo, registry, clock)), None, None)
                } else {
                    let table = SearchTable::open(&config.search_path)
                        .await
                        .map_err(DaemonError::from)?;
                    let embedder: std::sync::Arc<
                        dyn ltmrs_search::retrieval::engine::QueryEmbedder,
                    > = std::sync::Arc::new(
                        ltmrs_search::search::backend::embedders::NoDenseEmbedder,
                    );
                    let backend = Arc::new(SearchBackend::new(Arc::clone(&repo), table, embedder));
                    (
                        Arc::new(Dispatcher::new(repo, registry, clock).with_search(backend)),
                        None,
                        None,
                    )
                }
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
                    ltmrs_embeddings::service::EmbeddingService::load_e5_small_from_cache(&cache)
                        .map_err(|e| DaemonError::Embedding {
                        message: e.to_string(),
                    })?;
                let table = SearchTable::open(&config.search_path)
                    .await
                    .map_err(DaemonError::from)?;
                let embedder: std::sync::Arc<dyn ltmrs_search::retrieval::engine::QueryEmbedder> =
                    std::sync::Arc::new(
                        ltmrs_search::search::backend::e5::ServiceQueryEmbedder::new(
                            service.clone(),
                        ),
                    );
                let backend = Arc::new(
                    SearchBackend::new(Arc::clone(&repo), table, embedder)
                        .with_model_fingerprint(ltmrs_embeddings::e5_small::E5_SMALL_FINGERPRINT),
                );
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
            Box::new(crate::scheduler::FixedDimAdapter { dim: 384 }),
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
            housekeeping_worker: tokio::sync::Mutex::new(None),
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

    /// The IPC endpoint for frontends to connect to (Unix socket path,
    /// Windows named-pipe name).
    pub fn endpoint(&self) -> &std::path::Path {
        &self.paths.endpoint
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
}
