//! Server lifecycle / handshake / limits tests (moved verbatim from `server.rs`).

use std::sync::Arc;

use super::connection::handle_connection;
use super::test_support::*;
use super::{Daemon, DaemonConfig, DaemonError, EmbeddingMode};
use crate::dispatcher::Dispatcher;
use crate::envelope::{DomainRequest, IpcEnvelope};
use crate::limits::{QuotaTracker, ResourceLimits};
use crate::registry::FrontendRegistry;
use crate::runtime::RuntimePaths;
use ltmrs_domain::clock::{Clock, FrozenClock};
use ltmrs_domain::command::Scope;
use ltmrs_domain::id::{ChannelId, FrontendId, OperationId, StoreGeneration};
use ltmrs_service::repository::CanonicalRepository;
use uuid::Uuid;

#[test]
fn daemon_config_defaults() {
    let config = DaemonConfig::default();
    assert!(config.store_path.is_empty());
    assert!(config.limits.max_clients > 0);
    assert_eq!(config.idle_timeout_millis, 0, "serve forever by default");
}

/// Channel binding (RQ-05): frames must carry the handshake channel as
/// well as the frontend. A same-frontend frame naming another channel
/// is rejected, never routed into that channel's session.
#[tokio::test]
async fn channel_spoofed_frames_are_rejected() {
    use crate::envelope::{
        HandshakeRequest, PROTOCOL_VERSION, WireMessage, WireReply, read_response_payload,
    };
    use ltmrs_service::repository::CanonicalRepository;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn write_frame<S: AsyncWriteExt + Unpin>(stream: &mut S, msg: &WireMessage) {
        let payload = serde_json::to_vec(msg).unwrap();
        stream
            .write_all(&(payload.len() as u32).to_be_bytes())
            .await
            .unwrap();
        stream.write_all(&payload).await.unwrap();
        stream.flush().await.unwrap();
    }
    async fn read_reply<S: AsyncReadExt + Unpin>(stream: &mut S) -> WireReply {
        let bytes = read_response_payload(stream).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    let dir = tempfile::tempdir().unwrap();
    let clock: std::sync::Arc<dyn ltmrs_domain::clock::Clock + Send + Sync> =
        std::sync::Arc::new(FrozenClock::new(1000));
    let repo = std::sync::Arc::new(
        CanonicalRepository::open_with_clock(dir.path().to_str().unwrap(), clock.clone()).unwrap(),
    );
    let dispatcher = std::sync::Arc::new(Dispatcher::new(repo, FrontendRegistry::new(), clock));
    let quotas = std::sync::Arc::new(QuotaTracker::default());
    let (server_end, mut client_end) = tokio::io::duplex(65536);
    let server_handle = tokio::spawn(async move {
        let _ = handle_connection(Box::new(server_end), dispatcher, quotas).await;
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

/// Canonical housekeeping spawns without any search path (it collects
/// expired namespaces/receipts, not Lance state), is idempotent, and is
/// aborted on shutdown.
#[tokio::test]
async fn housekeeping_spawns_without_search_path_and_aborts_on_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::resolve(dir.path(), "house-store");
    let config = DaemonConfig {
        store_path: dir.path().join("store").to_str().unwrap().to_string(),
        ..Default::default()
    };
    let mut daemon = Daemon::start(&paths, config).await.unwrap();

    assert!(!daemon.housekeeping_worker_running().await);

    daemon.start_housekeeping().await;
    assert!(
        daemon.housekeeping_worker_running().await,
        "housekeeping must spawn with no search path configured"
    );

    // Idempotent: a second call does not stack another worker.
    daemon.start_housekeeping().await;
    assert!(daemon.housekeeping_worker_running().await);

    daemon.shutdown();
    assert!(
        !daemon.housekeeping_worker_running().await,
        "shutdown must clear the housekeeping worker"
    );
}

#[test]
fn embedding_disabled_by_default() {
    assert!(
        matches!(DaemonConfig::default().embedding, EmbeddingMode::Disabled),
        "the daemon must stay lexical-only unless embedding is configured"
    );
}

/// No embedding configured: a lexical-only projection worker still runs
/// (text rows converge without vectors) and shutdown aborts it cleanly.
#[tokio::test]
async fn projection_worker_runs_lexical_without_embedding() {
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
        daemon.projection_worker_running().await,
        "lexical-only daemons still project text rows"
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
    #[cfg(unix)]
    assert!(daemon.endpoint().ends_with("daemon.sock"));
    #[cfg(windows)]
    assert!(
        daemon
            .endpoint()
            .to_string_lossy()
            .starts_with("\\\\.\\pipe\\ltmrs-")
    );
    // The lock is held; a second start must fail.
    let result = Daemon::start(&paths, config).await;
    assert!(result.is_err());
}

/// P1-B: resuming a live namespace reissues the SAME epoch (no counter
/// bump); unknown epochs refuse as stale-typed errors, never a silent
/// fresh epoch.
#[test]
fn handshake_resume_reissues_same_epoch() {
    use crate::limits::{QuotaTracker, ResourceLimits};

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
        matches!(err, crate::envelope::IpcError::StaleNamespace(_)),
        "unknown epoch must refuse stale-typed, got: {err:?}"
    );
}

/// Channel isolation at the wire: resuming epoch 1 (issued to channel
/// 2) from sibling channel 3 refuses stale-typed — never adopted.
#[test]
fn handshake_resume_refuses_cross_channel() {
    use crate::limits::{QuotaTracker, ResourceLimits};

    let dir = tempfile::tempdir().unwrap();
    let quotas = Arc::new(QuotaTracker::new(ResourceLimits::default()));
    let (dispatcher, _quotas) = test_dispatcher_with_quotas(&dir, quotas);
    let mut resume = handshake_as(1);
    resume.resume_retry_epoch = Some(1);
    resume.channel_id = ChannelId::new(Uuid::from_u128(3));
    let err = dispatcher.handle_handshake(&resume).unwrap_err();
    assert!(
        matches!(err, crate::envelope::IpcError::StaleNamespace(_)),
        "cross-channel resume must refuse stale-typed, got: {err:?}"
    );
}

/// A full client table rejects new handshakes instead of over-admitting.
#[tokio::test]
async fn handshake_rejected_when_client_limit_reached() {
    use crate::client::IpcClient;
    use crate::limits::{QuotaTracker, ResourceLimits};

    let dir = tempfile::tempdir().unwrap();
    let quotas = Arc::new(QuotaTracker::new(ResourceLimits {
        max_clients: 1,
        ..Default::default()
    }));
    // Fill the single slot with another frontend.
    quotas
        .register_client(
            FrontendId::new(Uuid::from_u128(99)),
            ltmrs_domain::id::ChannelId::new(Uuid::from_u128(98)),
        )
        .unwrap();
    let (dispatcher, quotas) = test_dispatcher_with_quotas(&dir, quotas);

    let (client_stream, server_stream) = tokio::io::duplex(65536);
    let server = tokio::spawn(handle_connection(
        Box::new(server_stream),
        dispatcher,
        quotas,
    ));
    let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
    client.set_stream(Box::new(client_stream));
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
    use crate::client::IpcClient;
    use crate::limits::{QuotaTracker, ResourceLimits};

    let dir = tempfile::tempdir().unwrap();
    let quotas = Arc::new(QuotaTracker::new(ResourceLimits {
        max_clients: 1,
        ..Default::default()
    }));
    let (dispatcher, quotas) = test_dispatcher_with_quotas(&dir, quotas);

    // First client connects: slot held.
    let (a_stream, a_server) = tokio::io::duplex(65536);
    let a_task = tokio::spawn(handle_connection(
        Box::new(a_server),
        Arc::clone(&dispatcher),
        Arc::clone(&quotas),
    ));
    let mut client_a = IpcClient::new(std::path::PathBuf::from("unused"));
    client_a.set_stream(Box::new(a_stream));
    client_a.handshake(&handshake_as(1)).await.unwrap();
    assert_eq!(quotas.client_count(), 1);

    // Second client rejected while the first is live.
    let (b_stream, b_server) = tokio::io::duplex(65536);
    let b_task = tokio::spawn(handle_connection(
        Box::new(b_server),
        Arc::clone(&dispatcher),
        Arc::clone(&quotas),
    ));
    let mut client_b = IpcClient::new(std::path::PathBuf::from("unused"));
    client_b.set_stream(Box::new(b_stream));
    assert!(client_b.handshake(&handshake_as(2)).await.is_err());
    let _ = b_task.await;

    // First disconnects: slot freed, third client admitted.
    client_a.close();
    let _ = a_task.await;
    assert_eq!(quotas.client_count(), 0);
    let (c_stream, c_server) = tokio::io::duplex(65536);
    let c_task = tokio::spawn(handle_connection(
        Box::new(c_server),
        Arc::clone(&dispatcher),
        Arc::clone(&quotas),
    ));
    let mut client_c = IpcClient::new(std::path::PathBuf::from("unused"));
    client_c.set_stream(Box::new(c_stream));
    client_c.handshake(&handshake_as(3)).await.unwrap();
    client_c.close();
    let _ = c_task.await;
}

/// Frames claiming another frontend than the handshake are rejected
/// before dispatch (RQ-05 channel binding).
#[tokio::test]
async fn mismatched_frame_frontend_rejected() {
    use crate::client::IpcClient;

    let dir = tempfile::tempdir().unwrap();
    let quotas = Arc::new(crate::limits::QuotaTracker::new(
        crate::limits::ResourceLimits::default(),
    ));
    let (dispatcher, quotas) = test_dispatcher_with_quotas(&dir, quotas);

    let (client_stream, server_stream) = tokio::io::duplex(65536);
    let _server = tokio::spawn(handle_connection(
        Box::new(server_stream),
        dispatcher,
        quotas,
    ));
    let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
    client.set_stream(Box::new(client_stream));
    client.handshake(&handshake_as(1)).await.unwrap();
    // Same channel, forged frontend: must not route.
    let mut env = IpcEnvelope {
        protocol_version: crate::envelope::PROTOCOL_VERSION,
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
    use crate::client::IpcClient;
    use crate::envelope::IpcResult;
    use crate::limits::{QuotaTracker, ResourceLimits};

    let dir = tempfile::tempdir().unwrap();
    let quotas = Arc::new(QuotaTracker::new(ResourceLimits {
        max_in_flight_storage: 0,
        ..Default::default()
    }));
    let (dispatcher, quotas) = test_dispatcher_with_quotas(&dir, quotas);

    let (client_stream, server_stream) = tokio::io::duplex(65536);
    let _server = tokio::spawn(handle_connection(
        Box::new(server_stream),
        dispatcher,
        quotas,
    ));
    let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
    client.set_stream(Box::new(client_stream));
    client.handshake(&handshake_as(1)).await.unwrap();
    let env = DomainRequest::ListMemories;
    let envelope = IpcEnvelope {
        protocol_version: crate::envelope::PROTOCOL_VERSION,
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
    use crate::client::IpcClient;
    use crate::envelope::{HandshakeRequest, PROTOCOL_VERSION};
    use crate::limits::{QuotaTracker, ResourceLimits};

    let dir = tempfile::tempdir().unwrap();
    let quotas = Arc::new(QuotaTracker::new(ResourceLimits {
        max_response_bytes: 512,
        ..Default::default()
    }));
    let (dispatcher, quotas) = test_dispatcher_with_quotas(&dir, quotas);

    let (client_stream, server_stream) = tokio::io::duplex(65536);
    let server = tokio::spawn(handle_connection(
        Box::new(server_stream),
        dispatcher,
        quotas,
    ));
    let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
    client.set_stream(Box::new(client_stream));
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
        crate::envelope::IpcResult::Error { message, .. } => assert!(
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

/// Shutdown persists routing state and aborts background jobs so a
/// restart restores bindings while sessions live in the store
/// (design §7.2, §7.3).
#[tokio::test]
async fn shutdown_persists_sessions_and_aborts_scheduler() {
    use ltmrs_domain::session::SessionOp;

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
    let handle = ltmrs_domain::id::SessionHandle::new(uuid::Uuid::from_u128(77));
    // Namespace under the daemon's real clock (frozen test stamps
    // would be instantly expired against wall-clock validation).
    let now = daemon.dispatcher().clock().now_millis();
    let ns = daemon
        .dispatcher()
        .repo()
        .issue_namespace(fe(1), ch(1), now)
        .unwrap();
    let scope = ltmrs_domain::command::OperationScope {
        store_generation: ltmrs_domain::id::StoreGeneration::FIRST,
        frontend_id: fe(1),
        channel_id: ch(1),
        retry_epoch: ns.retry_epoch,
        operation_id: OperationId::new(uuid::Uuid::from_u128(1)),
        request_digest: "digest-1".to_string(),
    };
    match daemon
        .dispatcher()
        .repo()
        .session_start_tx(
            &scope,
            handle,
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
    use crate::client::IpcClient;
    use crate::envelope::{HandshakeRequest, PROTOCOL_VERSION};

    let dir = tempfile::tempdir().unwrap();
    let clock: Arc<dyn Clock + Send + Sync> = Arc::new(FrozenClock::new(1000));
    let store_path = dir.path().join("store").to_str().unwrap().to_string();
    let repo =
        Arc::new(CanonicalRepository::open_with_clock(&store_path, Arc::clone(&clock)).unwrap());
    let dispatcher = Arc::new(Dispatcher::new(
        Arc::clone(&repo),
        FrontendRegistry::new(),
        clock,
    ));
    let quotas = Arc::new(QuotaTracker::new(ResourceLimits::default()));

    let (client_stream, server) = tokio::io::duplex(65536);
    let handle = tokio::spawn(handle_connection(Box::new(server), dispatcher, quotas));

    // Client connects, handshakes (which issues the retry namespace),
    // then sends an AddMemory request and drops before reading the
    // response — simulating a crash after the request is in flight.
    let mut client = IpcClient::new(std::path::PathBuf::from("unused"));
    client.set_stream(Box::new(client_stream));
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
