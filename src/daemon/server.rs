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
use crate::domain::command::{DomainError, DomainErrorCode};
use crate::embeddings::artifacts::ArtifactCache;
use crate::search::backend::SearchBackend;
use crate::search::maintenance::{MaintenanceConfig, MaintenanceScheduler};
use crate::search::table::SearchTable;
use crate::service::repository::CanonicalRepository;

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
    quotas: Arc<QuotaTracker>,
    /// The singleton lock + bound 0600 socket listener, kept alive for the
    /// daemon's lifetime.
    runtime: DaemonRuntime,
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
        let (registry, sessions_path) = if config.sessions_path.is_empty() {
            (FrontendRegistry::new(), None)
        } else {
            let p = std::path::PathBuf::from(&config.sessions_path);
            let registry = FrontendRegistry::load(&p).map_err(DaemonError::Io)?;
            (registry, Some(p))
        };
        let (dispatcher, embedding) = match &config.embedding {
            EmbeddingMode::Disabled => (Arc::new(Dispatcher::new(repo, registry, clock)), None),
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
                )
            }
        };

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
            quotas,
            runtime,
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

    /// Coordinate shutdown: persist durable session history, abort the embedding
    /// scheduler and stop the maintenance worker. Committed receipts already live
    /// in the store and survive independently; this ensures session state and
    /// background jobs are cleaned up so a restart restores history and leaks no workers.
    pub fn shutdown(&mut self) {
        if let Some(p) = &self.sessions_path {
            let _ = self.dispatcher.registry().persist(p);
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

    /// Run the accept loop until the socket is closed, an error occurs, or
    /// the idle timeout elapses with no connections (design §7.2).
    /// Uses the 0600 listener bound at startup (already permission-locked).
    pub async fn serve(&self) -> Result<(), DaemonError> {
        // Start background maintenance under its explicit budgets, if configured.
        self.start_maintenance().await;

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

    // Released at connection end on every path below: assigned after a
    // successful handshake, dropped when this function returns.
    let _client_guard: Option<ClientGuard>;
    // Handshake-authenticated frontend; every later frame must carry it.
    let authed: Option<crate::domain::id::FrontendId>;

    // ---- Handshake: the first frame on every connection ----
    let first = read_wire_frame(&mut stream, &mut buf).await?;
    match first {
        WireMessage::Handshake(req) => {
            let frontend = req.frontend_id;
            let dispatcher = Arc::clone(&dispatcher);
            let result = tokio::task::spawn_blocking(move || dispatcher.handle_handshake(&req))
                .await
                .unwrap_or_else(|e| Err(IpcError::from(std::io::Error::other(e.to_string()))));
            match result {
                Ok(hs) => {
                    // Admit the client to the quota table; a full table
                    // rejects instead of over-admitting (RQ-22).
                    if let Err(qe) = quotas.register_client(frontend) {
                        let err = WireError {
                            kind: "client_limit_reached".into(),
                            message: qe.to_string(),
                        };
                        write_reply(&mut stream, &WireReply::Error(err), &quotas).await?;
                        return Ok(());
                    }
                    // Free the client's quota slot at connection end on every
                    // path below. Bound to the handshake-authenticated ID, not
                    // request-claimed ones; parallel connections of one
                    // frontend share a slot (approximate by design: the first
                    // disconnect frees it while a sibling is still active).
                    _client_guard = Some(ClientGuard::new(Arc::clone(&quotas), frontend));
                    authed = Some(frontend);
                    let reply = WireReply::Handshake(hs);
                    write_reply(&mut stream, &reply, &quotas).await?;
                }
                Err(e) => {
                    // Rejected handshake: send a wire error and close.
                    let err = WireError {
                        kind: "handshake_rejected".into(),
                        message: e.to_string(),
                    };
                    write_reply(&mut stream, &WireReply::Error(err), &quotas).await?;
                    return Ok(());
                }
            }
        }
        // A request before a handshake is a protocol violation.
        WireMessage::Request(_) => {
            let err = WireError {
                kind: "handshake_required".into(),
                message: "first frame must be a handshake".into(),
            };
            write_reply(&mut stream, &WireReply::Error(err), &quotas).await?;
            return Ok(());
        }
    }

    // Free the client's quota slot at connection end on every path below.
    // (Guard created in the handshake arm above.)
    // ---- Request loop ----
    loop {
        let msg = match read_wire_frame(&mut stream, &mut buf).await {
            Ok(m) => m,
            Err(DaemonError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let envelope = match msg {
            WireMessage::Request(env) => env,
            // A second handshake is a protocol violation.
            WireMessage::Handshake(_) => {
                let err = WireError {
                    kind: "unexpected_handshake".into(),
                    message: "handshake already completed".into(),
                };
                write_reply(&mut stream, &WireReply::Error(err), &quotas).await?;
                continue;
            }
        };
        // Bind every frame to the handshake identity (RQ-05): a frame
        // claiming another frontend is a protocol violation, never routed
        // into its session namespace or quota bucket.
        if Some(envelope.frontend_id) != authed {
            let err = WireError {
                kind: "frontend_mismatch".into(),
                message: "frame frontend differs from handshake identity".into(),
            };
            write_reply(&mut stream, &WireReply::Error(err), &quotas).await?;
            continue;
        }
        // (Client slot was bound to the handshake ID by the guard above.)

        // Enforce the per-client quota: visible backpressure, not unbounded work.
        if let Err(qe) = quotas.try_enqueue(envelope.frontend_id) {
            let busy = DomainError::new(DomainErrorCode::Validation, format!("backpressure: {qe}"));
            let resp = IpcResponse::error(envelope.operation_id, &busy);
            write_reply(&mut stream, &WireReply::Response(resp), &quotas).await?;
            continue;
        }

        // In-flight storage bound: refuse visibly instead of piling up
        // blocking work beyond the configured concurrency.
        if let Err(qe) = quotas.try_start_storage() {
            quotas.dequeue(envelope.frontend_id);
            let busy = DomainError::new(DomainErrorCode::Validation, format!("backpressure: {qe}"));
            let resp = IpcResponse::error(envelope.operation_id, &busy);
            write_reply(&mut stream, &WireReply::Response(resp), &quotas).await?;
            continue;
        }

        // Dispatch on a blocking thread: Fjall write transactions + fsync
        // must not block a Tokio core I/O worker (§7.3).
        let op_id = envelope.operation_id;
        let fe_id = envelope.frontend_id;
        let dispatcher = Arc::clone(&dispatcher);
        let dispatch_result = tokio::task::spawn_blocking(move || dispatcher.handle(&envelope))
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

/// Unregisters the connection's frontend from the quota table on drop, so a
/// disconnect frees its client slot on every return path above. Bound to the
/// handshake-authenticated ID at construction (never request-claimed IDs).
struct ClientGuard {
    quotas: Arc<QuotaTracker>,
    frontend: crate::domain::id::FrontendId,
}

impl ClientGuard {
    fn new(quotas: Arc<QuotaTracker>, frontend: crate::domain::id::FrontendId) -> Self {
        Self { quotas, frontend }
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.quotas.unregister_client(self.frontend);
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
        }
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
            .register_client(FrontendId::new(Uuid::from_u128(99)))
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

    /// Shutdown persists session state and aborts background jobs so a
    /// restart restores durable history (design §7.2, §7.3).
    #[tokio::test]
    async fn shutdown_persists_sessions_and_aborts_scheduler() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");
        let sessions_path = dir.path().join("sessions.json");
        let config = DaemonConfig {
            store_path: dir.path().join("store").to_str().unwrap().to_string(),
            sessions_path: sessions_path.to_str().unwrap().to_string(),
            ..Default::default()
        };
        let mut daemon = Daemon::start(&paths, config).await.unwrap();
        // Start a session through the dispatcher so there is state to persist.
        daemon
            .dispatcher()
            .registry()
            .start_session(fe(1), ch(1), Some("proj".into()), None, 100);

        // Shutdown: persist sessions + abort the scheduler worker.
        daemon.shutdown();

        // The sessions file exists and restores the started session.
        assert!(sessions_path.exists(), "shutdown must persist sessions");
        let restored = FrontendRegistry::load(&sessions_path).unwrap();
        assert_eq!(restored.channel_count(), 1);
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
}
