//! Serve tests (moved verbatim from `serve.rs`).

use std::path::{Path, PathBuf};

use super::daemon::{connect_remote, daemon_idle_ms, start_local_daemon};
use super::test_support::*;
use super::{
    RUNTIME_IDENTITY, home_dir_from, resolve_daemon_embedding, resolve_home, stdio_layout,
};
use crate::cli::CliError;
use crate::frontend::mcp::FrontendIdentity;
use ltmrs_daemon::client::IpcClient;
use ltmrs_daemon::envelope::{DomainRequest, HandshakeRequest, IpcEnvelope, PROTOCOL_VERSION};
use ltmrs_daemon::runtime::RuntimePaths;
use ltmrs_daemon::server::EmbeddingMode;
use ltmrs_domain::command::Scope;
use ltmrs_domain::id::{ChannelId, FrontendId, OperationId, StoreGeneration};
use uuid::Uuid;

/// Missing or empty HOME fails explicitly (exit 1 via Runtime).
#[test]
fn resolve_home_requires_home() {
    let err = resolve_home(None).unwrap_err();
    assert!(matches!(err, CliError::Runtime(_)));
    assert!(
        err.to_string()
            .contains(crate::frontend::serve::home_env_var()),
        "got: {err}"
    );
    assert!(matches!(
        resolve_home(Some(String::new())),
        Err(CliError::Runtime(_))
    ));
}

/// A given home resolves under the managed `.ltmrs` directory.
#[test]
fn resolve_home_appends_managed_dir() {
    assert_eq!(
        resolve_home(Some("/tmp/x".to_string())).unwrap(),
        PathBuf::from("/tmp/x/.ltmrs")
    );
}

/// HOME wins over USERPROFILE (tests and msys-style shells set HOME on
/// Windows); empty HOME falls back; both missing resolves to nothing.
#[test]
fn home_dir_prefers_home_over_userprofile() {
    assert_eq!(
        home_dir_from(Some("C:/t".to_string()), Some("C:/u".to_string())),
        Some("C:/t".to_string())
    );
    assert_eq!(
        home_dir_from(None, Some("C:/u".to_string())),
        Some("C:/u".to_string())
    );
    assert_eq!(
        home_dir_from(Some(String::new()), Some("C:/u".to_string())),
        Some("C:/u".to_string()),
        "empty HOME falls back"
    );
    assert_eq!(home_dir_from(None, None), None);
}

/// Layout sub-paths are pinned (store/sessions/search/runtime wiring).
/// Unix path literals; Windows layout is pinned by `resolve_home` +
/// the runtime endpoint tests.
#[cfg(unix)]
#[test]
fn stdio_layout_pins_subpaths() {
    let base = Path::new("/tmp/x/.ltmrs");
    let layout = stdio_layout(base);
    assert_eq!(layout.base, base);
    assert_eq!(layout.store_path, "/tmp/x/.ltmrs/store");
    assert_eq!(layout.sessions_path, "/tmp/x/.ltmrs/sessions.json");
    assert_eq!(layout.search_path, "/tmp/x/.ltmrs/search");
    assert_eq!(layout.runtime_base, base);
    // The runtime (lock + socket) resolves under the managed home.
    let runtime = RuntimePaths::resolve(&layout.runtime_base, RUNTIME_IDENTITY);
    assert!(runtime.endpoint.starts_with(&layout.base));
}

/// The local-daemon bridge commits through `handle_connection`: a memory
/// written via the bridged client is listed back (write path, not just
/// an empty-list read).
#[tokio::test]
async fn local_daemon_bridge_roundtrips_memory_add() {
    let dir = tempfile::tempdir().unwrap();
    let layout = stdio_layout(&dir.path().join(".ltmrs"));
    let (mut daemon, mut client) = start_local_daemon(&layout).await.unwrap();
    let id = test_identity();
    let hs = client
        .handshake(&HandshakeRequest {
            protocol_version: PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id: id.frontend_id,
            channel_id: id.channel_id,
            resume_retry_epoch: None,
        })
        .await
        .unwrap();
    let env = IpcEnvelope {
        protocol_version: PROTOCOL_VERSION,
        store_generation: StoreGeneration::FIRST,
        frontend_id: id.frontend_id,
        channel_id: id.channel_id,
        operation_id: OperationId::new(Uuid::from_u128(7)),
        session: None,
        retry_epoch: hs.retry_epoch,
        deadline_millis: None,
        scope: Scope::default(),
        body: DomainRequest::AddMemory {
            memory: test_memory(),
        },
    };
    let resp = client.roundtrip(&env).await.unwrap();
    assert!(
        matches!(
            resp.result,
            ltmrs_daemon::envelope::IpcResult::Success { .. }
        ),
        "add must succeed, got: {:?}",
        resp.result
    );
    let list = IpcEnvelope {
        operation_id: OperationId::new(Uuid::from_u128(8)),
        body: DomainRequest::ListMemories,
        ..env
    };
    let listed = client.roundtrip(&list).await.unwrap();
    match listed.result {
        ltmrs_daemon::envelope::IpcResult::Success {
            payload: ltmrs_daemon::envelope::DomainPayload::Memories(m),
            ..
        } => {
            assert_eq!(m.len(), 1);
            assert_eq!(m[0].title, "stdio-bridge");
        }
        other => panic!("expected memories, got: {other:?}"),
    }
    daemon.shutdown();
}

/// Socket mode fails fast on an unreachable daemon (no silent hang).
#[tokio::test]
async fn socket_mode_fails_fast_on_missing_socket() {
    let err = match connect_remote("/nonexistent-dir-xyz/daemon.sock").await {
        Ok(_) => panic!("connect to a missing socket must fail"),
        Err(e) => e,
    };
    assert!(matches!(err, CliError::Runtime(_)), "got: {err}");
}

/// A generation-mismatched handshake keeps the connection open for a
/// retry (restore bumps the generation mid-session): same-connection
/// re-handshake at the live generation succeeds and serves.
#[tokio::test]
async fn mismatched_handshake_retry_on_same_connection() {
    use ltmrs_daemon::envelope::IpcError;

    let dir = tempfile::tempdir().unwrap();
    let layout = stdio_layout(&dir.path().join(".ltmrs"));
    let (mut daemon, mut client) = start_local_daemon(&layout).await.unwrap();
    daemon
        .dispatcher_arc()
        .repo_arc()
        .set_store_generation(ltmrs_domain::id::StoreGeneration::new(2))
        .unwrap();
    let id = test_identity();
    let bad = HandshakeRequest {
        protocol_version: PROTOCOL_VERSION,
        store_generation: StoreGeneration::FIRST,
        frontend_id: id.frontend_id,
        channel_id: id.channel_id,
        resume_retry_epoch: None,
    };
    assert!(
        matches!(
            client.handshake(&bad).await,
            Err(IpcError::GenerationMismatch { daemon: 2, .. })
        ),
        "stale handshake must be typed GenerationMismatch"
    );
    let good = HandshakeRequest {
        store_generation: ltmrs_domain::id::StoreGeneration::new(2),
        ..bad
    };
    let hs = client.handshake(&good).await.unwrap();
    assert_eq!(hs.store_generation.as_u64(), 2);
    let env = IpcEnvelope {
        protocol_version: PROTOCOL_VERSION,
        store_generation: ltmrs_domain::id::StoreGeneration::new(2),
        frontend_id: id.frontend_id,
        channel_id: id.channel_id,
        operation_id: OperationId::new(Uuid::from_u128(11)),
        session: None,
        retry_epoch: hs.retry_epoch,
        deadline_millis: None,
        scope: Scope::default(),
        body: DomainRequest::ListMemories,
    };
    let resp = client.roundtrip(&env).await.unwrap();
    assert!(
        matches!(
            resp.result,
            ltmrs_daemon::envelope::IpcResult::Success { .. }
        ),
        "post-retry call must serve, got: {:?}",
        resp.result
    );
    daemon.shutdown();
}

/// Layout pins the models directory (provision target + daemon
/// enablement source). Unix path-literal pin; the Windows layout is
/// pinned by the native-separator assertion below.
#[cfg(unix)]
#[test]
fn stdio_layout_pins_models_path() {
    let base = Path::new("/tmp/x/.ltmrs");
    let layout = stdio_layout(base);
    assert_eq!(layout.models_path, "/tmp/x/.ltmrs/models");
}

#[cfg(windows)]
#[test]
fn stdio_layout_pins_models_path() {
    let base = Path::new("C:\\Users\\x\\.ltmrs");
    let layout = stdio_layout(base);
    assert_eq!(layout.models_path, "C:\\Users\\x\\.ltmrs\\models");
    assert_eq!(layout.store_path, "C:\\Users\\x\\.ltmrs\\store");
}

/// Unprovisioned models dir resolves dense-disabled (offline-safe).
#[test]
fn resolve_embedding_disabled_without_models() {
    let dir = tempfile::tempdir().unwrap();
    let layout = stdio_layout(&dir.path().join(".ltmrs"));
    assert!(
        matches!(resolve_daemon_embedding(&layout), EmbeddingMode::Disabled),
        "no models must mean dense-disabled"
    );
}

/// Partial/corrupt cache resolves dense-disabled too — never half-enabled.
#[test]
fn resolve_embedding_disabled_on_partial_cache() {
    use ltmrs_embeddings::manifest::{E5_SMALL_ID, E5_SMALL_REVISION};

    let dir = tempfile::tempdir().unwrap();
    let layout = stdio_layout(&dir.path().join(".ltmrs"));
    let partial = Path::new(&layout.models_path)
        .join(E5_SMALL_ID)
        .join(E5_SMALL_REVISION);
    std::fs::create_dir_all(&partial).unwrap();
    std::fs::write(partial.join("model.safetensors"), b"not a model").unwrap();
    assert!(
        matches!(resolve_daemon_embedding(&layout), EmbeddingMode::Disabled),
        "a partial cache must mean dense-disabled"
    );
}

/// No provisioned models: the local daemon serves lexical-only through
/// the attached lexical backend (not the snapshot fallback) and says so
/// on the wire (mode + dense_ready), never a silent dense claim.
#[tokio::test]
async fn local_daemon_without_models_serves_lexical_engine() {
    use ltmrs_compat::lemma::tool_args::{SemanticSearchArgs, ToolArgs};
    use ltmrs_daemon::envelope::{DomainPayload, IpcResult};

    let dir = tempfile::tempdir().unwrap();
    let layout = stdio_layout(&dir.path().join(".ltmrs"));
    let (mut daemon, mut client) = start_local_daemon(&layout).await.unwrap();
    // Deterministic convergence: the lexical worker builds the FTS
    // index asynchronously; ensure it here (retrying the worker's own
    // concurrent build) so the search below meets a converged
    // (Complete) table instead of racing the worker.
    let mut table = ltmrs_search::search::table::SearchTable::open(daemon.search_path())
        .await
        .unwrap();
    let mut converged = false;
    for _ in 0..40 {
        // Refresh first: this handle snapshots at open and would
        // otherwise never observe the worker's concurrent index build.
        let _ = table.refresh().await;
        match table.ensure_fts_index().await {
            Ok(_) => {
                converged = true;
                break;
            }
            // The worker's own concurrent index build preempted ours.
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
    assert!(converged, "FTS index must converge");
    let id = test_identity();
    let hs = client
        .handshake(&HandshakeRequest {
            protocol_version: PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id: id.frontend_id,
            channel_id: id.channel_id,
            resume_retry_epoch: None,
        })
        .await
        .unwrap();
    let env = IpcEnvelope {
        protocol_version: PROTOCOL_VERSION,
        store_generation: StoreGeneration::FIRST,
        frontend_id: id.frontend_id,
        channel_id: id.channel_id,
        operation_id: OperationId::new(Uuid::from_u128(9)),
        session: None,
        retry_epoch: hs.retry_epoch,
        deadline_millis: None,
        scope: Scope::default(),
        body: DomainRequest::ToolCall {
            tool: ToolArgs::SemanticSearch(SemanticSearchArgs {
                query: "bridged write".into(),
                project: None,
                top_k: None,
                offset: None,
                hybrid: None,
                explain: true,
                response_format: None,
            }),
        },
    };
    let resp = client.roundtrip(&env).await.unwrap();
    match resp.result {
        IpcResult::Success {
            payload:
                DomainPayload::ToolResult {
                    structured: Some(v),
                    is_error: false,
                    ..
                },
            ..
        } => {
            assert_eq!(v["explanation"]["mode"], "lexical");
            assert_eq!(v["explanation"]["dense_ready"], false);
        }
        other => panic!("expected tool result, got: {other:?}"),
    }
    daemon.shutdown();
}

/// P1 shared lifecycle: the stdio daemon serves its bound socket, so a
/// second frontend dialing the managed socket handshakes (previously the
/// socket was bound-but-unaccepted and parked forever).
#[tokio::test]
async fn socket_second_client_handshakes_after_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let layout = stdio_layout(&dir.path().join(".ltmrs"));
    let (mut daemon, _bridged) = start_local_daemon(&layout).await.unwrap();
    assert!(
        daemon.socket_server_running().await,
        "stdio daemon must serve its socket"
    );
    let socket = RuntimePaths::resolve(&layout.runtime_base, RUNTIME_IDENTITY).endpoint;
    let mut second = IpcClient::new(socket);
    tokio::time::timeout(std::time::Duration::from_secs(2), second.connect())
        .await
        .expect("socket connect must not hang")
        .unwrap();
    let id = FrontendIdentity::new(
        FrontendId::new(Uuid::from_u128(101)),
        ChannelId::new(Uuid::from_u128(102)),
    );
    let hs = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        second.handshake(&HandshakeRequest {
            protocol_version: PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id: id.frontend_id,
            channel_id: id.channel_id,
            resume_retry_epoch: None,
        }),
    )
    .await
    .expect("socket handshake must not hang")
    .unwrap();
    assert_eq!(hs.store_generation, StoreGeneration::FIRST);
    daemon.shutdown();
}

/// Idle resolution: explicit flag wins, then a valid env override,
/// then the default; garbage env falls back loudly to the default.
#[test]
fn daemon_idle_ms_prefers_flag_then_env_then_default() {
    use crate::cli::DEFAULT_DAEMON_IDLE_MS;
    assert_eq!(daemon_idle_ms(Some(1500), None), 1500);
    assert_eq!(
        daemon_idle_ms(Some(1500), Some("5".to_string())),
        1500,
        "flag wins over env"
    );
    assert_eq!(daemon_idle_ms(None, Some("2500".to_string())), 2500);
    assert_eq!(
        daemon_idle_ms(None, Some("  3000  ".to_string())),
        3000,
        "surrounding whitespace is tolerated"
    );
    assert_eq!(
        daemon_idle_ms(None, Some("forever".to_string())),
        DEFAULT_DAEMON_IDLE_MS
    );
    assert_eq!(daemon_idle_ms(None, None), DEFAULT_DAEMON_IDLE_MS);
    assert_eq!(daemon_idle_ms(Some(0), None), 0, "0 serves forever");
}

/// Windows shutdown handlers register successfully: Ctrl-C and
/// Ctrl-Break via tokio's console control handler. Programmatic
/// delivery is not testable under `cargo test` on this conhost:
/// group-0 events kill cargo (cargo registers no handler), and
/// targeted delivery to the daemon's own process group is delayed
/// tens of seconds and hangs `try_wait` after the daemon exits —
/// both verified by probe (2026-10-08, deviation ledger). The
/// foreground Ctrl-C path is proven clean in the same console group
/// (daemon fires, exits 0).
#[cfg(windows)]
#[test]
fn shutdown_handlers_register() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let c = tokio::signal::windows::ctrl_c();
        let b = tokio::signal::windows::ctrl_break();
        assert!(c.is_ok(), "ctrl_c handler must register, got {c:?}");
        assert!(b.is_ok(), "ctrl_break handler must register, got {b:?}");
    });
}
