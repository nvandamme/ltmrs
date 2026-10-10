//! Managed daemon lifecycle: spawn, connect, serve (moved verbatim from `serve.rs`).

use std::path::{Path, PathBuf};

use super::{RUNTIME_IDENTITY, StdioLayout, resolve_daemon_embedding, resolve_home, stdio_layout};
use crate::cli::CliError;
use crate::frontend::mcp::{FrontendIdentity, LtmrsFrontend};
use ltmrs_daemon::client::IpcClient;
use ltmrs_daemon::envelope::{HandshakeRequest, PROTOCOL_VERSION};
use ltmrs_daemon::runtime::RuntimePaths;
use ltmrs_daemon::server::connection::handle_connection;
use ltmrs_daemon::server::{Daemon, DaemonConfig};
use ltmrs_domain::id::{ChannelId, FrontendId, StoreGeneration};
use uuid::Uuid;

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
