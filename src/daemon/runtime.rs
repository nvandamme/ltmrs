//! Private runtime directories, OS singleton lock, secure socket creation and
//! safe stale-socket recovery (design §7.1, RQ-20, T-SEC-01).
//!
//! On Linux the daemon uses a user-owned 0700 runtime directory, a 0600
//! socket, and an OS lock (flock) held for the daemon's lifetime — not just a
//! PID file. The lock is the source of truth for ownership: if we hold it, any
//! existing socket is stale and may be removed. A concurrent startup either
//! wins the lock (becomes the owner) or observes a live daemon (rejects).

use std::path::{Path, PathBuf};

use crate::domain::id::StoreGeneration;

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
    /// The 0700 runtime directory (sockets, lock).
    pub runtime_dir: PathBuf,
    /// The lock file path.
    pub lock_path: PathBuf,
    /// The socket path.
    pub socket_path: PathBuf,
}

impl RuntimePaths {
    /// Resolve runtime paths under a base dir for a given store identity.
    pub fn resolve(base_dir: &Path, store_identity: &str) -> Self {
        let runtime_dir = base_dir.join("ltmrs").join(store_identity);
        Self {
            lock_path: runtime_dir.join("daemon.lock"),
            socket_path: runtime_dir.join("daemon.sock"),
            runtime_dir,
        }
    }
}

/// The OS singleton lock, held for the daemon's lifetime. Dropping it (or the
/// process exiting) releases the lock.
pub struct DaemonLock {
    /// Keep the file open so the flock is held; released on drop.
    #[allow(dead_code)]
    file: std::fs::File,
}

/// The acquired runtime: the singleton lock and the bound 0600 socket
/// listener. Both must be kept alive for the daemon's lifetime; the lock
/// guards ownership and the listener serves connections.
pub struct DaemonRuntime {
    pub lock: DaemonLock,
    pub listener: tokio::net::UnixListener,
}

/// Acquire the singleton lock and bind a secure 0600 socket, recovering a
/// stale socket if the previous daemon died. Returns the lock and the bound
/// listener (both must be kept alive).
pub fn acquire_singleton(paths: &RuntimePaths) -> Result<DaemonRuntime, RuntimeError> {
    // Create the 0700 runtime directory.
    create_runtime_dir(&paths.runtime_dir)?;

    // Open/create the lock file.
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&paths.lock_path)?;

    // Try to acquire an exclusive, non-blocking flock.
    use std::os::unix::io::AsRawFd;
    let fd = lock_file.as_raw_fd();
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::AlreadyExists
            || err.kind() == std::io::ErrorKind::WouldBlock
        {
            // Another daemon holds the lock: a live daemon exists.
            return Err(RuntimeError::AlreadyRunning(paths.lock_path.clone()));
        }
        return Err(RuntimeError::Io(err));
    }

    // We own the lock. Any existing socket is stale (previous daemon died
    // without cleanup). Remove it safely.
    recover_stale_socket(&paths.socket_path)?;

    // Bind the 0600 socket listener (kept alive to serve connections).
    let listener = tokio::net::UnixListener::bind(&paths.socket_path)?;
    set_mode(&paths.socket_path, 0o600)?;

    Ok(DaemonRuntime {
        lock: DaemonLock { file: lock_file },
        listener,
    })
}

/// Create the 0700 runtime directory, refusing symlinked paths.
fn create_runtime_dir(dir: &Path) -> Result<(), RuntimeError> {
    if dir.exists() {
        // Refuse if it is a symlink (a symlinked runtime path is unsafe).
        let meta = std::fs::symlink_metadata(dir)?;
        if meta.file_type().is_symlink() {
            return Err(RuntimeError::UnsafePath(dir.to_path_buf()));
        }
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    // Set 0700.
    set_mode(dir, 0o700)?;
    Ok(())
}

/// Remove a stale socket if present. Safe because we hold the lock: no live
/// daemon can be listening on it.
fn recover_stale_socket(socket_path: &Path) -> Result<(), RuntimeError> {
    if socket_path.exists() {
        // Refuse to remove a symlink (unsafe takeover).
        let meta = std::fs::symlink_metadata(socket_path)?;
        if meta.file_type().is_symlink() {
            return Err(RuntimeError::UnsafePath(socket_path.to_path_buf()));
        }
        std::fs::remove_file(socket_path)?;
    }
    Ok(())
}

/// Set a file's permission mode (Unix).
fn set_mode(path: &Path, mode: u32) -> Result<(), RuntimeError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
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
        assert_ne!(a.socket_path, b.socket_path);
        assert!(a.socket_path.starts_with(&a.runtime_dir));
    }

    #[tokio::test]
    async fn acquire_singleton_succeeds_on_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");
        let runtime = acquire_singleton(&paths).unwrap();
        // Lock held; socket bound.
        assert!(paths.socket_path.exists());
        drop(runtime);
    }

    #[tokio::test]
    async fn second_acquire_rejects_while_first_holds_lock() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");
        let runtime_a = acquire_singleton(&paths).unwrap();

        // A second attempt on the same paths must fail (live daemon).
        let result = acquire_singleton(&paths);
        assert!(matches!(result, Err(RuntimeError::AlreadyRunning(_))));

        drop(runtime_a);
    }

    #[tokio::test]
    async fn stale_socket_recovered_after_lock_released() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");

        // First daemon acquires, binds socket, then dies (drops lock + listener).
        let runtime = acquire_singleton(&paths).unwrap();
        assert!(paths.socket_path.exists());
        drop(runtime);

        // A new daemon acquires the now-free lock and recovers the stale socket.
        let runtime2 = acquire_singleton(&paths).unwrap();
        assert!(
            paths.socket_path.exists(),
            "socket recreated after recovery"
        );
        drop(runtime2);
    }

    #[test]
    fn symlinked_runtime_dir_refused() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");
        // Create the parent so the symlink location is valid, then symlink the
        // runtime dir itself to a real directory.
        let parent = paths.runtime_dir.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&parent).unwrap();
        let target = dir.path().join("real-dir");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, &paths.runtime_dir).unwrap();

        let result = acquire_singleton(&paths);
        assert!(matches!(result, Err(RuntimeError::UnsafePath(_))));
    }
}
