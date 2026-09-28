//! stdio serving: daemon lifecycle + MCP over stdin/stdout (WP-10; T-CLI-01).
//!
//! Two modes, matching the CLI surface: `--socket <path>` attaches the
//! frontend to an already-running daemon (fail fast when unreachable);
//! without `--socket` an in-process daemon starts under the managed home
//! (`$HOME/.ltmrs`) and the frontend talks to it over a `UnixStream` pair
//! through the same `handle_connection` path as socket clients. Either way
//! the MCP boundary is `LtmrsFrontend` served over rmcp stdio.

use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::cli::CliError;
use crate::daemon::client::IpcClient;
use crate::daemon::runtime::RuntimePaths;
use crate::daemon::server::{Daemon, DaemonConfig, EmbeddingMode, handle_connection};
use crate::domain::id::{ChannelId, FrontendId};
use crate::frontend::mcp::{FrontendIdentity, LtmrsFrontend};

/// Managed home directory name under `$HOME` (native default; upstream
/// Lemma uses `~/.lemma`, so this path is ltmrs-native by design).
pub const MANAGED_HOME_DIR: &str = ".ltmrs";
/// Fjall canonical store directory name under the managed home.
pub const STORE_DIR_NAME: &str = "store";
/// Durable session-history file name under the managed home (JSON registry
/// snapshot, as written by `FrontendRegistry::persist`).
pub const SESSIONS_FILE_NAME: &str = "sessions.json";
/// Lance projection directory name under the managed home (empty disables
/// the maintenance scheduler; the projection stays rebuildable).
pub const SEARCH_DIR_NAME: &str = "search";
/// Verified E5 embedding-artifact directory name under the managed home
/// (provisioned by `--provision-models`; absent or unverifiable means the
/// daemon serves dense-disabled — never a partial model).
pub const MODELS_DIR_NAME: &str = "models";
/// Store identity used for the stdio daemon's runtime paths.
pub const RUNTIME_IDENTITY: &str = "daemon";

/// Resolved on-disk layout for in-process stdio serving.
#[derive(Debug, Clone)]
pub struct StdioLayout {
    /// The managed home (`$HOME/.ltmrs`).
    pub base: PathBuf,
    /// Canonical store path (string form for `DaemonConfig`).
    pub store_path: String,
    /// Session-history file path (string form for `DaemonConfig`).
    pub sessions_path: String,
    /// Projection directory path (string form for `DaemonConfig`).
    pub search_path: String,
    /// Verified embedding-artifact directory path (provision target for
    /// `--provision-models`; enablement source for the local daemon).
    pub models_path: String,
    /// Base dir handed to `RuntimePaths::resolve` (lock + socket live under
    /// `<base>/ltmrs/<identity>/`).
    pub runtime_base: PathBuf,
}

/// Resolve the managed home. Missing or empty HOME is a runtime error —
/// stdio serving never invents a store location silently.
pub fn resolve_home(home: Option<String>) -> Result<PathBuf, CliError> {
    match home {
        Some(h) if !h.is_empty() => Ok(Path::new(&h).join(MANAGED_HOME_DIR)),
        _ => Err(CliError::Runtime(
            "stdio serving needs a home directory: set $HOME (no store location invented)".into(),
        )),
    }
}

/// Derive the stdio layout from a managed-home base.
pub fn stdio_layout(base: &Path) -> StdioLayout {
    StdioLayout {
        base: base.to_path_buf(),
        store_path: base.join(STORE_DIR_NAME).to_string_lossy().into_owned(),
        sessions_path: base.join(SESSIONS_FILE_NAME).to_string_lossy().into_owned(),
        search_path: base.join(SEARCH_DIR_NAME).to_string_lossy().into_owned(),
        models_path: base.join(MODELS_DIR_NAME).to_string_lossy().into_owned(),
        runtime_base: base.to_path_buf(),
    }
}

/// Resolve the local daemon's embedding mode from the managed models
/// directory. Verification-only (`load_cached`, no download): a full digest
/// match enables dense, anything else serves lexical-only with a stderr
/// diagnostic naming the cause and the `--provision-models` remedy. A
/// partial or corrupt cache therefore degrades loudly, never half-enabled.
///
/// Cost note: verification hashes the full ~470MB artifact set (seconds),
/// and the daemon hashes + loads twice more (query service, projection
/// adapter). Slow, loud boots beat fast, uncertain ones.
fn resolve_daemon_embedding(layout: &StdioLayout) -> EmbeddingMode {
    use crate::embeddings::artifacts::ArtifactCache;
    use crate::embeddings::manifest::e5_small_artifact;

    let cache = ArtifactCache::new(&layout.models_path);
    match cache.load_cached(&e5_small_artifact()) {
        Ok(_) => EmbeddingMode::E5SmallCached {
            cache_dir: layout.models_path.clone(),
        },
        Err(e) => {
            eprintln!(
                "ltmrs: dense embeddings disabled ({e}); run `ltmrs --provision-models` to enable hybrid retrieval"
            );
            EmbeddingMode::Disabled
        }
    }
}

/// Start the in-process daemon for stdio mode and return it together with a
/// frontend client bridged over a `UnixStream` pair. The daemon must be kept
/// alive (and `shutdown` at the end) to hold the singleton lock.
///
/// Dense enablement is auto-detect, never silent: when the managed models
/// directory holds the full verified E5 artifact set the daemon starts
/// dense-enabled; otherwise it serves lexical-only and names the reason plus
/// the `--provision-models` remedy on stderr. Either way every
/// `semantic_search` answer carries its effective mode (`hybrid` vs
/// `lexical-fallback`) with `dense_ready`, so callers never infer dense
/// from a provisioned directory alone.
pub async fn start_local_daemon(layout: &StdioLayout) -> Result<(Daemon, IpcClient), CliError> {
    let paths = RuntimePaths::resolve(&layout.runtime_base, RUNTIME_IDENTITY);
    let embedding = resolve_daemon_embedding(layout);
    let config = DaemonConfig {
        store_path: layout.store_path.clone(),
        sessions_path: layout.sessions_path.clone(),
        search_path: layout.search_path.clone(),
        embedding,
        ..Default::default()
    };
    let daemon = Daemon::start(&paths, config)
        .await
        .map_err(|e| CliError::Runtime(format!("cannot start local daemon: {e}")))?;
    // Dense projection when E5 embedding is configured (no-op otherwise):
    // the stdio path has no accept loop calling serve(), so workers that
    // serve() starts must start here too.
    daemon.start_projection().await;
    let (client_stream, server_stream) = tokio::net::UnixStream::pair()
        .map_err(|e| CliError::Runtime(format!("cannot bridge stdio daemon: {e}")))?;
    // Detached on purpose: the task lives until the client stream closes
    // (dropping the JoinHandle detaches; only `abort` would cancel it).
    let _connection = tokio::spawn(handle_connection(
        server_stream,
        daemon.dispatcher_arc(),
        daemon.quotas(),
    ));
    let mut client = IpcClient::new(paths.socket_path.clone());
    client.set_stream(client_stream);
    Ok((daemon, client))
}

/// Connect to an already-running daemon at `socket`. Connects now so a dead
/// listener fails fast; the handshake stays lazy in `LtmrsFrontend::call_tool`
/// so the memory-snapshot prefetch for the dynamic instructions runs in both
/// modes.
pub async fn connect_remote(socket: &str) -> Result<IpcClient, CliError> {
    let mut client = IpcClient::new(std::path::PathBuf::from(socket));
    client
        .connect()
        .await
        .map_err(|e| CliError::Runtime(format!("cannot connect to daemon at {socket}: {e}")))?;
    Ok(client)
}

/// Serve MCP over stdio: `socket=None` starts the in-process daemon under
/// the managed home, `socket=Some` attaches to a running daemon. Runs until
/// stdin closes, then shuts the local daemon down (persisting sessions).
pub async fn serve_stdio(socket: Option<String>, home: Option<String>) -> Result<(), CliError> {
    let identity = FrontendIdentity::new(
        FrontendId::new(Uuid::now_v7()),
        ChannelId::new(Uuid::now_v7()),
    );
    let (mut daemon, client) = match socket {
        Some(path) => (None, connect_remote(&path).await?),
        None => {
            let base = resolve_home(home)?;
            let layout = stdio_layout(&base);
            let (daemon, client) = start_local_daemon(&layout).await?;
            (Some(daemon), client)
        }
    };
    let frontend = LtmrsFrontend::new(identity, client);
    let service = rmcp::serve_server(frontend, rmcp::transport::stdio())
        .await
        .map_err(|e| CliError::Runtime(format!("stdio transport failed: {e}")))?;
    let reason = service
        .waiting()
        .await
        .map_err(|e| CliError::Runtime(format!("stdio serving failed: {e}")))?;
    let result = match reason {
        rmcp::service::QuitReason::Closed | rmcp::service::QuitReason::Cancelled => Ok(()),
        other => Err(CliError::Runtime(format!(
            "stdio serving ended abnormally: {other:?}"
        ))),
    };
    // Shut down on every exit path so sessions persist even when serving
    // ends in an error; the singleton lock releases on drop either way.
    if let Some(daemon) = daemon.as_mut() {
        daemon.shutdown();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::envelope::{DomainRequest, IpcEnvelope};
    use crate::daemon::envelope::{HandshakeRequest, PROTOCOL_VERSION};
    use crate::domain::command::Scope;
    use crate::domain::id::{OperationId, StoreGeneration};

    fn test_identity() -> FrontendIdentity {
        FrontendIdentity::new(
            FrontendId::new(Uuid::from_u128(1)),
            ChannelId::new(Uuid::from_u128(2)),
        )
    }

    /// Missing or empty HOME fails explicitly (exit 1 via Runtime).
    #[test]
    fn resolve_home_requires_home() {
        let err = resolve_home(None).unwrap_err();
        assert!(matches!(err, CliError::Runtime(_)));
        assert!(err.to_string().contains("HOME"), "got: {err}");
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

    /// Layout sub-paths are pinned (store/sessions/search/runtime wiring).
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
        assert!(runtime.socket_path.starts_with(&layout.base));
    }

    fn test_memory() -> crate::domain::memory::Memory {
        use crate::domain::id::{DocumentRevision, EligibilityRevision, EntityId, EntityRevision};
        use crate::domain::memory::Instant;
        use crate::domain::memory::{FragmentType, Memory, MemoryLifecycle, MemorySource};
        Memory {
            id: EntityId::new(Uuid::from_u128(42)),
            external_alias: None,
            title: "stdio-bridge".into(),
            fragment: "bridged write".into(),
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
                crate::daemon::envelope::IpcResult::Success { .. }
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
            crate::daemon::envelope::IpcResult::Success {
                payload: crate::daemon::envelope::DomainPayload::Memories(m),
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

    /// Layout pins the models directory (provision target + daemon enablement source).
    #[test]
    fn stdio_layout_pins_models_path() {
        let base = Path::new("/tmp/x/.ltmrs");
        let layout = stdio_layout(base);
        assert_eq!(layout.models_path, "/tmp/x/.ltmrs/models");
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
        use crate::embeddings::manifest::{E5_SMALL_ID, E5_SMALL_REVISION};

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

    /// No provisioned models: the local daemon serves lexical-only and says so
    /// on the wire (mode + dense_ready), never a silent dense claim.
    #[tokio::test]
    async fn local_daemon_without_models_serves_lexical_fallback() {
        use crate::compatibility::lemma::tool_args::{SemanticSearchArgs, ToolArgs};
        use crate::daemon::envelope::{DomainPayload, IpcResult};

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
                assert_eq!(v["explanation"]["mode"], "lexical-fallback");
                assert_eq!(v["explanation"]["dense_ready"], false);
            }
            other => panic!("expected tool result, got: {other:?}"),
        }
        daemon.shutdown();
    }
}
