//! Private runtime directories, OS singleton lock, secure endpoint creation
//! and safe stale-endpoint recovery (design §7.1, RQ-20, T-SEC-01).
//!
//! Linux: a user-owned 0700 runtime directory, a 0600 socket, and an OS lock
//! (flock) held for the daemon's lifetime. Windows: a per-user runtime
//! directory under the managed home, a `LockFileEx` singleton lock, and a
//! DACL-protected named pipe `\\.\pipe\ltmrs-<identity>-<user-token>` — the
//! per-user DACL is the same-user boundary (the OS rejects foreign users at
//! connect, so no frame is ever read from them). The lock is the source of
//! truth for ownership in both platforms: if we hold it, any existing
//! endpoint is stale and may be recovered.

use std::path::{Path, PathBuf};

use ltmrs_domain::id::StoreGeneration;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

/// A duplex IPC byte stream: the platform accept path hands over the
/// concrete connection (Unix socket, named pipe, duplex pair) as one
/// boxed transport so the wire protocol stays platform-neutral. A combined
/// trait is required: `dyn` cannot merge two non-auto traits directly.
pub trait IpcDuplex: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> IpcDuplex for T {}

pub type IpcStream = Box<dyn IpcDuplex>;

/// Error type for runtime setup.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("another daemon already owns the store lock ({0})")]
    AlreadyRunning(PathBuf),
    #[error("runtime path is a symlink or not a regular dir: {0}")]
    UnsafePath(PathBuf),
}

/// Resolved runtime paths for a store, keyed by store identity so a portable
/// store does not collide with the default store.
#[derive(Debug, Clone)]
pub struct RuntimePaths {
    /// The runtime directory (sockets, lock).
    pub runtime_dir: PathBuf,
    /// The lock file path.
    pub lock_path: PathBuf,
    /// The IPC endpoint: Unix socket path, or Windows named-pipe name.
    pub endpoint: PathBuf,
}

impl RuntimePaths {
    /// Resolve runtime paths under a base dir for a given store identity.
    pub fn resolve(base_dir: &Path, store_identity: &str) -> Self {
        let runtime_dir = base_dir.join("ltmrs").join(store_identity);
        Self {
            lock_path: runtime_dir.join("daemon.lock"),
            endpoint: platform_endpoint(&runtime_dir, store_identity),
            runtime_dir,
        }
    }
}

/// The OS singleton lock, held for the daemon's lifetime. Dropping it (or the
/// process exiting) releases the lock.
pub struct DaemonLock {
    /// Keep the file open so the lock is held; released on drop.
    #[allow(dead_code)]
    file: std::fs::File,
}

/// The accepted connection listener. Unix: a persistent bound socket
/// listener; Windows: the pipe endpoint, one instance created per accept.
pub struct DaemonListener {
    #[cfg(unix)]
    listener: std::sync::Arc<tokio::net::UnixListener>,
    #[cfg(windows)]
    endpoint: PathBuf,
}

/// The acquired runtime: the singleton lock and the endpoint listener. Both
/// must be kept alive for the daemon's lifetime; the lock guards ownership
/// and the listener serves connections. The listener is reference-counted so
/// the stdio path can spawn a background accept loop that outlives any
/// single borrow of the daemon.
pub struct DaemonRuntime {
    pub lock: DaemonLock,
    pub listener: std::sync::Arc<DaemonListener>,
}

#[cfg(unix)]
use unix::{
    acquire_lock, build_listener, create_runtime_dir, lock_held_platform, platform_endpoint,
    recover_stale_endpoint,
};
#[cfg(windows)]
use windows::{
    acquire_lock, build_listener, create_runtime_dir, lock_held_platform, platform_endpoint,
    recover_stale_endpoint,
};

/// Bind the endpoint listener without acquiring the singleton lock. Used by
/// frontend tests that drive the wire protocol with a custom server task.
pub fn bind_listener(paths: &RuntimePaths) -> Result<DaemonListener, RuntimeError> {
    build_listener(paths)
}

/// Acquire the singleton lock and secure the endpoint, recovering a stale
/// endpoint if the previous daemon died. Returns the lock and the listener
/// (both must be kept alive).
pub fn acquire_singleton(paths: &RuntimePaths) -> Result<DaemonRuntime, RuntimeError> {
    create_runtime_dir(&paths.runtime_dir)?;

    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&paths.lock_path)?;

    acquire_lock(&lock_file, &paths.lock_path)?;

    // We own the lock: any existing endpoint is stale (previous daemon died
    // without cleanup). Recover it safely.
    recover_stale_endpoint(&paths.endpoint)?;

    let listener = build_listener(paths)?;
    Ok(DaemonRuntime {
        lock: DaemonLock { file: lock_file },
        listener: std::sync::Arc::new(listener),
    })
}

/// Whether another process currently holds the singleton lock, probed
/// without side effects (shared non-blocking lock: succeeds only when
/// nobody owns it). Used by spawners to tell "our child died and nobody
/// serves" (fail fast) apart from "our child lost the race to a live
/// owner" (keep waiting). Returns `None` when the lock file cannot be
/// inspected at all (treat as unknown: keep waiting, never fail on it).
pub fn lock_held(paths: &RuntimePaths) -> Option<bool> {
    lock_held_platform(paths)
}

/// The store generation the daemon is serving (used in the handshake).
#[derive(Debug, Clone, Copy)]
pub struct DaemonIdentity {
    pub store_generation: StoreGeneration,
    pub protocol_version: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_resolve_per_store_identity() {
        let a = RuntimePaths::resolve(Path::new("/tmp"), "store-a");
        let b = RuntimePaths::resolve(Path::new("/tmp"), "store-b");
        assert_ne!(a.endpoint, b.endpoint);
        #[cfg(unix)]
        assert!(a.endpoint.starts_with(&a.runtime_dir));
    }
}
