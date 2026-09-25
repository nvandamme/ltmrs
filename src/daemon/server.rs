//! Daemon lifecycle (design §7.1, §7.2). Ties the singleton lock, secure
//! socket, dispatcher, scheduler and quotas into a running daemon with a
//! bounded accept loop and graceful shutdown.

use std::sync::{Arc, Mutex};

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
use crate::embeddings::e5_small::E5SmallAdapter;
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
    maintenance_worker: tokio::sync::Mutex<Option<JoinHandle<()>>>,
    maintenance_config: MaintenanceConfig,
    quotas: Arc<QuotaTracker>,
    /// The singleton lock + bound 0600 socket listener, kept alive for the
    /// daemon's lifetime.
    runtime: DaemonRuntime,
    paths: RuntimePaths,
    sessions_path: Option<std::path::PathBuf>,
    search_path: String,
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
        let dispatcher = match &config.embedding {
            EmbeddingMode::Disabled => Arc::new(Dispatcher::new(repo, registry, clock)),
            EmbeddingMode::E5SmallCached { cache_dir } => {
                if config.search_path.is_empty() {
                    return Err(DaemonError::Embedding {
                        message: "E5 embedding requires search_path for the projection table"
                            .into(),
                    });
                }
                let cache = ArtifactCache::new(cache_dir);
                let adapter = E5SmallAdapter::load_from_cache(&cache).map_err(|e| {
                    DaemonError::Embedding {
                        message: e.to_string(),
                    }
                })?;
                let table = SearchTable::open(&config.search_path)
                    .await
                    .map_err(DaemonError::from)?;
                let backend = Arc::new(SearchBackend::new(
                    Arc::clone(&repo),
                    table,
                    Arc::new(Mutex::new(adapter)),
                ));
                Arc::new(Dispatcher::new(repo, registry, clock).with_search(backend))
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
            maintenance_worker: tokio::sync::Mutex::new(None),
            maintenance_config: config.maintenance,
            quotas,
            runtime,
            paths: paths.clone(),
            sessions_path,
            search_path: config.search_path,
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
                let sched = MaintenanceScheduler::new(table, self.maintenance_config);
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

    /// Run the accept loop until the socket is closed or an error occurs.
    /// Uses the 0600 listener bound at startup (already permission-locked).
    pub async fn serve(&self) -> Result<(), DaemonError> {
        // Start background maintenance under its explicit budgets, if configured.
        self.start_maintenance().await;

        let listener = &self.runtime.listener;

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
}

/// Handle a single client connection: read frames, dispatch, write responses.
/// The first frame MUST be a handshake; it authenticates the frontend and
/// issues its retry namespace. Subsequent frames are typed domain requests.
pub async fn handle_connection(
    mut stream: tokio::net::UnixStream,
    dispatcher: Arc<Dispatcher>,
    quotas: Arc<QuotaTracker>,
) -> Result<(), DaemonError> {
    let mut buf = Vec::with_capacity(4096);

    // ---- Handshake: the first frame on every connection ----
    let first = read_wire_frame(&mut stream, &mut buf).await?;
    match first {
        WireMessage::Handshake(req) => {
            let dispatcher = Arc::clone(&dispatcher);
            let result = tokio::task::spawn_blocking(move || dispatcher.handle_handshake(&req))
                .await
                .unwrap_or_else(|e| Err(IpcError::from(std::io::Error::other(e.to_string()))));
            match result {
                Ok(hs) => {
                    let reply = WireReply::Handshake(hs);
                    write_reply(&mut stream, &reply).await?;
                }
                Err(e) => {
                    // Rejected handshake: send a wire error and close.
                    let err = WireError {
                        kind: "handshake_rejected".into(),
                        message: e.to_string(),
                    };
                    write_reply(&mut stream, &WireReply::Error(err)).await?;
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
            write_reply(&mut stream, &WireReply::Error(err)).await?;
            return Ok(());
        }
    }

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
                write_reply(&mut stream, &WireReply::Error(err)).await?;
                continue;
            }
        };

        // Enforce the per-client quota: visible backpressure, not unbounded work.
        if let Err(qe) = quotas.try_enqueue(envelope.frontend_id) {
            let busy = DomainError::new(DomainErrorCode::Validation, format!("backpressure: {qe}"));
            let resp = IpcResponse::error(envelope.operation_id, &busy);
            write_reply(&mut stream, &WireReply::Response(resp)).await?;
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
        quotas.dequeue(fe_id);

        // Write the response.
        write_reply(&mut stream, &WireReply::Response(response)).await?;
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
) -> Result<(), DaemonError> {
    let payload = serde_json::to_vec(reply)?;
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
