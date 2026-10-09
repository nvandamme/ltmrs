//! stdio serving: daemon lifecycle + MCP over stdin/stdout (WP-10; T-CLI-01).
//!
//! Two modes, matching the CLI surface: `--socket <path>` attaches the
//! frontend to an already-running daemon (fail fast when unreachable);
//! without `--socket` the frontend attaches to the managed-home daemon if
//! one is reachable (connect-or-spawn), otherwise it starts the daemon
//! in-process under the managed home (`$HOME/.ltmrs` on unix,
//! `%USERPROFILE%/.ltmrs` on Windows) and serves its IPC endpoint
//! alongside the bridged duplex pair. Either way the MCP boundary is
//! `LtmrsFrontend` served over rmcp stdio. Losing the startup lock race
//! retries the connection instead of failing; the owning frontend stays
//! alive serving socket clients after its own stdio closes.

use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::cli::CliError;
use crate::frontend::mcp::{FrontendIdentity, LtmrsFrontend};
use ltmrs_daemon::client::IpcClient;
use ltmrs_daemon::envelope::{HandshakeRequest, PROTOCOL_VERSION};
use ltmrs_daemon::runtime::RuntimePaths;
use ltmrs_daemon::server::{Daemon, DaemonConfig, EmbeddingMode, handle_connection};
use ltmrs_domain::id::{ChannelId, FrontendId, StoreGeneration};

/// Managed home directory name under the home dir (`$HOME` on unix,
/// `%USERPROFILE%` on Windows; upstream Lemma uses `~/.lemma`, so this
/// path is ltmrs-native by design).
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
    /// The managed home (`$HOME/.ltmrs` on unix, `%USERPROFILE%/.ltmrs`
    /// on Windows).
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
        _ => Err(CliError::Runtime(format!(
            "stdio serving needs a home directory: set {} (no store location invented)",
            home_env_name()
        ))),
    }
}

/// The home-directory environment variable per platform: `$HOME` on unix,
/// `%USERPROFILE%` on Windows.
pub fn home_dir() -> Option<String> {
    home_dir_from(
        std::env::var("HOME").ok(),
        std::env::var(home_env_var()).ok(),
    )
}

/// Precedence core (pure for deterministic tests): an explicit, non-empty
/// HOME wins; otherwise the platform fallback (`USERPROFILE` on Windows).
fn home_dir_from(home: Option<String>, fallback: Option<String>) -> Option<String> {
    home.filter(|h| !h.is_empty())
        .or_else(|| fallback.filter(|h| !h.is_empty()))
}

pub const fn home_env_var() -> &'static str {
    #[cfg(unix)]
    {
        "HOME"
    }
    #[cfg(windows)]
    {
        "USERPROFILE"
    }
}

pub fn home_env_name() -> &'static str {
    #[cfg(unix)]
    {
        "$HOME"
    }
    #[cfg(windows)]
    {
        "%USERPROFILE%"
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
    use ltmrs_embeddings::artifacts::ArtifactCache;
    use ltmrs_embeddings::manifest::e5_small_artifact;

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

/// The managed daemon IPC endpoint for a stdio layout (single definition so
/// spawners, frontends and status output never disagree on the path).
pub fn daemon_socket_path(layout: &StdioLayout) -> PathBuf {
    RuntimePaths::resolve(&layout.runtime_base, RUNTIME_IDENTITY).endpoint
}

/// Start an in-process daemon and return it together with a frontend
/// client bridged over a `tokio::io::duplex` pair. Single-process mode for
/// tests and embeddings: production frontends attach to a spawned daemon
/// process instead (see `ensure_daemon_process`). The daemon must be kept
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
    // Dense projection when E5 embedding is configured (no-op otherwise).
    daemon.start_projection().await;
    // Periodic maintenance (optimization/retention/repair) belongs to every
    // serving daemon, not just the `serve()` path (re-review R6): the timer
    // covers what commit wakes don't (failed-pass repair).
    daemon.start_maintenance().await;
    // Serve the bound socket too: without this a second frontend dials a
    // bound-but-unaccepted listener and parks forever (P1 shared lifecycle).
    daemon.spawn_socket_server().await;
    let (client_stream, server_stream) = tokio::io::duplex(65536);
    // Detached on purpose: the task lives until the client stream closes
    // (dropping the JoinHandle detaches; only `abort` would cancel it).
    let _connection = tokio::spawn(handle_connection(
        Box::new(server_stream),
        daemon.dispatcher_arc(),
        daemon.quotas(),
    ));
    let mut client = IpcClient::new(paths.endpoint.clone());
    client.set_stream(Box::new(client_stream));
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

/// Resolve the daemon idle shutdown for a spawned or served daemon:
/// explicit flag wins, then the `LTMRS_DAEMON_IDLE_MS` environment
/// override (site-local default without flags; also what tests use to
/// keep spawned daemons short-lived), then the CLI default. Garbage env
/// values fall back loudly to the default rather than failing startup.
pub fn daemon_idle_ms(flag: Option<u64>, env_raw: Option<String>) -> u64 {
    if let Some(ms) = flag {
        return ms;
    }
    if let Some(ref raw) = env_raw
        && let Ok(ms) = raw.trim().parse::<u64>()
    {
        return ms;
    }
    if env_raw.is_some() {
        eprintln!("ltmrs: ignoring invalid LTMRS_DAEMON_IDLE_MS; using default");
    }
    crate::cli::DEFAULT_DAEMON_IDLE_MS
}

/// Spawn a detached daemon child (`exe daemon --foreground ...`) that
/// outlives this process: stdin to null, stdout/stderr appended to a log
/// file in the runtime dir (mode-0600 on unix, default ACL on Windows; a
/// silent daemon is undebuggable; rotation is future work), detached from
/// our terminal signal group (new session on unix,
/// `CREATE_NEW_PROCESS_GROUP` on Windows), never waited on (reparented on
/// our exit; it idle-exits on its own).
pub fn spawn_daemon_child(
    exe: &Path,
    idle_ms: u64,
    log_path: &Path,
) -> Result<std::process::Child, CliError> {
    use std::process::Stdio;

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CliError::Runtime(format!("cannot create daemon runtime dir: {e}")))?;
    }
    let log = open_daemon_log(log_path)?;
    let log_err = log
        .try_clone()
        .map_err(|e| CliError::Runtime(format!("cannot duplicate daemon log: {e}")))?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .arg("--foreground")
        .arg("--daemon-idle-ms")
        .arg(idle_ms.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    detach_child(&mut cmd);
    cmd.spawn()
        .map_err(|e| CliError::Runtime(format!("cannot spawn daemon process: {e}")))
}

/// Open the daemon log: mode-0600 on unix (owner-only diagnostics); the
/// default ACL on Windows (per-user profile dir inherits protection).
#[cfg(unix)]
fn open_daemon_log(log_path: &Path) -> Result<std::fs::File, CliError> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log_path)
        .map_err(|e| CliError::Runtime(format!("cannot open daemon log: {e}")))
}

#[cfg(windows)]
fn open_daemon_log(log_path: &Path) -> Result<std::fs::File, CliError> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| CliError::Runtime(format!("cannot open daemon log: {e}")))
}

/// Detach a child from our terminal's signal group: a new session on unix
/// (setsid), `CREATE_NEW_PROCESS_GROUP` on Windows. Ctrl-C to our console
/// never reaches the child; Ctrl-Break still does (Task 6 pins it).
#[cfg(unix)]
pub fn detach_child(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: pre_exec runs post-fork/pre-exec; the closure calls only
    // async-signal-safe libc::setsid with no allocation.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
pub fn detach_child(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0020;
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

/// Daemon child log file: diagnostics a detached daemon would otherwise
/// lose to /dev/null (startup failures, projection errors, idle exits).
pub fn daemon_log_path(layout: &StdioLayout) -> PathBuf {
    RuntimePaths::resolve(&layout.runtime_base, RUNTIME_IDENTITY)
        .runtime_dir
        .join("daemon.log")
}

/// Ensure a daemon process serves the managed home, spawning a detached
/// one when nothing verifiably serves. At most one spawn: whoever loses
/// the startup race finds the winner serving on retry (the loser's child
/// exits `AlreadyRunning` on its own). Reaps an exited child best-effort
/// on the way out so losing spawns never linger as zombies. Probe
/// handshakes use an ephemeral identity (their namespace is never used
/// for real calls).
pub async fn ensure_daemon_process(
    layout: &StdioLayout,
    exe: &Path,
    idle_ms: u64,
) -> Result<(), CliError> {
    use std::time::Duration;

    let socket = daemon_socket_path(layout);
    let probe_id = FrontendIdentity::new(
        FrontendId::new(Uuid::now_v7()),
        ChannelId::new(Uuid::now_v7()),
    );
    // Fast path: already serving (verified, not merely bound).
    if let Ok(mut client) = connect_remote(&socket.to_string_lossy()).await
        && probe_handshake(&mut client, probe_id.frontend_id, probe_id.channel_id)
            .await
            .is_ok()
    {
        return Ok(());
    }
    let mut child: Option<std::process::Child> =
        Some(spawn_daemon_child(exe, idle_ms, &daemon_log_path(layout))?);
    // Wait-ready: bounded verified attach. Refused dials poll cheaply
    // (covers slow first boot incl. model hashing); backlog-hung dials
    // burn a handshake timeout each, so cap consecutive silence — a
    // permanently mute owner fails loudly instead of parking forever.
    const MAX_ATTEMPTS: usize = 1200;
    const MAX_SILENT: usize = 24;
    const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
    let mut silent = 0usize;
    let mut last_err = String::from("daemon did not start serving");
    // Lock paths for the fast-fail probe below (no side effects).
    let lock_paths = RuntimePaths::resolve(&layout.runtime_base, RUNTIME_IDENTITY);
    for _ in 0..MAX_ATTEMPTS {
        // Our spawn may have lost the startup race: an exited child is
        // reaped (never waited on while live), and only fails fast when
        // NOBODY holds the lock — i.e. no winner exists to attach to.
        // (A loser child exiting while the winner serves is the normal
        // simultaneous-startup case, not a failure.)
        if let Some(c) = child.as_mut() {
            match c.try_wait() {
                Ok(Some(status)) => {
                    child = None;
                    if !status.success()
                        && ltmrs_daemon::runtime::lock_held(&lock_paths) == Some(false)
                    {
                        return Err(CliError::Runtime(format!(
                            "spawned daemon exited during startup: {status}"
                        )));
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    last_err = format!("cannot poll daemon child: {e}");
                    break;
                }
            }
        }
        match connect_remote(&socket.to_string_lossy()).await {
            Ok(mut client) => {
                match tokio::time::timeout(
                    PROBE_TIMEOUT,
                    probe_handshake_inner(&mut client, probe_id.frontend_id, probe_id.channel_id),
                )
                .await
                {
                    Ok(Ok(())) => {
                        // Reap a loser child that already exited; a live
                        // daemon child is left alone (never waited on).
                        if let Some(mut c) = child {
                            let _ = c.try_wait();
                        }
                        return Ok(());
                    }
                    // Transient handshake failures (busy/contention during
                    // concurrent startup, I/O on a dying socket, a
                    // generation that flipped mid-probe) retry: only
                    // deterministic rejections fail fast (same-user
                    // violation, protocol skew — retrying those is
                    // pointless).
                    Ok(Err(ltmrs_daemon::envelope::IpcError::Unauthorized))
                    | Ok(Err(ltmrs_daemon::envelope::IpcError::UnsupportedProtocol(_))) => {
                        last_err = "daemon refused frontend handshake".to_string();
                        break;
                    }
                    Ok(Err(e)) => {
                        // A responder answered (busy, I/O hiccup, flipped
                        // generation): alive, so reset the silence count.
                        silent = 0;
                        last_err = e.to_string();
                    }
                    Err(_) => {
                        silent += 1;
                        last_err = "daemon handshake timed out".to_string();
                        if silent >= MAX_SILENT {
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                // Nobody listening (yet): cheap poll, reset silence.
                silent = 0;
                last_err = e.to_string();
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(CliError::Runtime(format!(
        "daemon at {} never started serving: {last_err}",
        socket.display()
    )))
}

/// Bounded handshake-verified connection retries: losing a startup race
/// means another frontend just became the owner (or just spawned one),
/// so dial its socket instead of failing. Every attempt is
/// handshake-verified: a bare `connect` can succeed against a
/// bound-but-unaccepted listener (backlog), which would park the later
/// handshake forever. Attempts that connect but never handshake are
/// treated as unreachable. Worst case is bounded
/// (attempts × (dial + 1s handshake + 50ms)); a truly wedged owner fails
/// loudly instead of hanging.
pub async fn connect_with_retry(
    socket: &Path,
    attempts: usize,
    frontend_id: FrontendId,
    channel_id: ChannelId,
) -> Result<IpcClient, CliError> {
    let socket_str = socket.to_string_lossy().into_owned();
    let mut last_err = String::new();
    for _ in 0..attempts {
        match connect_remote(&socket_str).await {
            Ok(mut client) => match probe_handshake(&mut client, frontend_id, channel_id).await {
                Ok(()) => return Ok(client),
                Err(e) => last_err = e.to_string(),
            },
            Err(e) => last_err = e.to_string(),
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Err(CliError::Runtime(format!(
        "cannot connect to daemon at {} after {attempts} retries: {last_err}",
        socket.display()
    )))
}

/// Verify a connected client with a real handshake (bounded): proves the
/// listener actually serves instead of merely accepting into a backlog.
/// Follows the generation-mismatch retry the bridged path uses, so a
/// post-restore owner verifies on the first attempt that needs it.
async fn probe_handshake(
    client: &mut IpcClient,
    frontend_id: FrontendId,
    channel_id: ChannelId,
) -> Result<(), CliError> {
    let attempt = probe_handshake_inner(client, frontend_id, channel_id);
    tokio::time::timeout(std::time::Duration::from_secs(1), attempt)
        .await
        .map_err(|_| CliError::Runtime("daemon handshake timed out".into()))?
        .map_err(|e| CliError::Runtime(format!("daemon handshake failed: {e}")))
}

/// The handshake itself, without a timeout: callers that manage their own
/// budget (e.g. the spawn wait loop, whose silence cap is the bound) use
/// this directly under their own `timeout`. Returns the raw IPC error so
/// callers can distinguish deterministic rejections (unauthorized,
/// protocol skew — fail fast) from transient ones (busy, I/O, generation
/// flips — retry).
async fn probe_handshake_inner(
    client: &mut IpcClient,
    frontend_id: FrontendId,
    channel_id: ChannelId,
) -> Result<(), ltmrs_daemon::envelope::IpcError> {
    let first = HandshakeRequest {
        protocol_version: PROTOCOL_VERSION,
        store_generation: StoreGeneration::FIRST,
        frontend_id,
        channel_id,
        resume_retry_epoch: None,
    };
    match client.handshake(&first).await {
        Ok(_) => Ok(()),
        Err(ltmrs_daemon::envelope::IpcError::GenerationMismatch { daemon, .. }) => {
            let retry = HandshakeRequest {
                store_generation: StoreGeneration::new(daemon),
                ..first
            };
            client.handshake(&retry).await.map(|_| ())
        }
        Err(e) => Err(e),
    }
}

/// Run the shared daemon in this process until idle shutdown or SIGTERM:
/// the `daemon --foreground` implementation and the shape supervisors use.
/// Builds the managed-home config exactly like the spawned path (same
/// store, sessions, search, embedding detection), serves the bound socket
/// with maintenance and projection, persists on every shutdown path.
pub async fn run_daemon_foreground(layout: &StdioLayout, idle_ms: u64) -> Result<(), CliError> {
    let paths = RuntimePaths::resolve(&layout.runtime_base, RUNTIME_IDENTITY);
    let embedding = resolve_daemon_embedding(layout);
    let config = DaemonConfig {
        store_path: layout.store_path.clone(),
        sessions_path: layout.sessions_path.clone(),
        search_path: layout.search_path.clone(),
        embedding,
        idle_timeout_millis: idle_ms,
        ..Default::default()
    };
    let mut daemon = Daemon::start(&paths, config)
        .await
        .map_err(|e| CliError::Runtime(format!("cannot start daemon: {e}")))?;
    // SIGTERM (unix supervisors/hosts) and Ctrl-C shut down gracefully:
    // persist routing state, stop workers, exit 0. SIGKILL needs no
    // handling (store writes are barrier-flushed per commit, so
    // acknowledged work survives it). Windows: Ctrl-C on the console; a
    // detached daemon is Ctrl-C-protected by its new process group and
    // shuts down via Ctrl-Break (Task 6 pins the break path).

    let result = tokio::select! {
        r = daemon.serve() => r,
        _ = shutdown_signal() => {
            eprintln!("ltmrs: daemon received shutdown signal, shutting down");
            Ok(())
        }
    };
    // Shutdown on every exit path (including accept-loop errors): persist
    // routing state and stop workers before releasing the lock.
    daemon.shutdown();
    result.map_err(|e| CliError::Runtime(format!("daemon serve failed: {e}")))?;
    Ok(())
}

/// The shutdown signal set per platform: SIGTERM + Ctrl-C on unix; Ctrl-C
/// and Ctrl-Break on Windows (a detached daemon is Ctrl-C-protected by its
/// new process group; Ctrl-Break reaches it via GenerateConsoleCtrlEvent).
#[cfg(unix)]
pub async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("cannot watch for SIGTERM");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

#[cfg(windows)]
pub async fn shutdown_signal() {
    let mut brk = tokio::signal::windows::ctrl_break().expect("cannot watch for Ctrl-Break");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = brk.recv() => {}
    }
}

/// Serve MCP over stdio: `socket=None` ensures the managed-home daemon
/// process serves and attaches as a pure client; `socket=Some` attaches
/// to a running daemon. Runs until stdin closes, then exits — the daemon
/// outlives every frontend by design (it idle-exits on its own), so a
/// SIGKILLed first frontend can never stop service for the second.
pub async fn serve_stdio(socket: Option<String>, home: Option<String>) -> Result<(), CliError> {
    let identity = FrontendIdentity::new(
        FrontendId::new(Uuid::now_v7()),
        ChannelId::new(Uuid::now_v7()),
    );
    let client = match socket {
        Some(path) => connect_remote(&path).await?,
        None => {
            let base = resolve_home(home)?;
            let layout = stdio_layout(&base);
            let exe = std::env::current_exe()
                .map_err(|e| CliError::Runtime(format!("cannot locate ltmrs binary: {e}")))?;
            let idle = daemon_idle_ms(None, std::env::var("LTMRS_DAEMON_IDLE_MS").ok());
            ensure_daemon_process(&layout, &exe, idle).await?;
            // The daemon is verified serving; attach (bounded, for the
            // stop-start race where it died between ensure and dial).
            let sock = daemon_socket_path(&layout);
            connect_with_retry(&sock, 30, identity.frontend_id, identity.channel_id).await?
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
    // Pure client: no daemon is owned here, so there is nothing to shut
    // down or linger for. The daemon process outlives every frontend and
    // idle-exits on its own.
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ltmrs_daemon::envelope::{DomainRequest, IpcEnvelope};
    use ltmrs_daemon::envelope::{HandshakeRequest, PROTOCOL_VERSION};
    use ltmrs_domain::command::Scope;
    use ltmrs_domain::id::{OperationId, StoreGeneration};

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

    fn test_memory() -> ltmrs_domain::memory::Memory {
        use ltmrs_domain::id::{DocumentRevision, EligibilityRevision, EntityId, EntityRevision};
        use ltmrs_domain::memory::Instant;
        use ltmrs_domain::memory::{FragmentType, Memory, MemoryLifecycle, MemorySource};
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
}
