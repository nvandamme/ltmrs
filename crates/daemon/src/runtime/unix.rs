//! Linux runtime: 0700 dir, 0600 socket, flock singleton, peer-credential
//! same-user boundary, symlink refusal (design §7.1, T-SEC-01).

use std::path::{Path, PathBuf};

use crate::envelope::{WireError, WireReply, write_response_payload};

use super::{DaemonListener, IpcStream, RuntimeError, RuntimePaths};

pub fn platform_endpoint(runtime_dir: &Path, _store_identity: &str) -> PathBuf {
    runtime_dir.join("daemon.sock")
}

/// Create the 0700 runtime directory, refusing symlinked paths.
pub fn create_runtime_dir(dir: &Path) -> Result<(), RuntimeError> {
    if dir.exists() {
        // Refuse if it is a symlink (a symlinked runtime path is unsafe).
        let meta = std::fs::symlink_metadata(dir)?;
        if meta.file_type().is_symlink() {
            return Err(RuntimeError::UnsafePath(dir.to_path_buf()));
        }
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    set_mode(dir, 0o700)?;
    Ok(())
}

/// Try to acquire an exclusive, non-blocking flock.
pub fn acquire_lock(file: &std::fs::File, lock_path: &Path) -> Result<(), RuntimeError> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: flock on our own open file descriptor; non-blocking, so no
    // deadlock, and the lock is released when the fd closes.
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::AlreadyExists
            || err.kind() == std::io::ErrorKind::WouldBlock
        {
            // Another daemon holds the lock: a live daemon exists.
            return Err(RuntimeError::AlreadyRunning(lock_path.to_path_buf()));
        }
        return Err(RuntimeError::Io(err));
    }
    Ok(())
}

/// Remove a stale socket if present. Safe because we hold the lock: no live
/// daemon can be listening on it.
pub fn recover_stale_endpoint(endpoint: &Path) -> Result<(), RuntimeError> {
    if endpoint.exists() {
        // Refuse to remove a symlink (unsafe takeover).
        let meta = std::fs::symlink_metadata(endpoint)?;
        if meta.file_type().is_symlink() {
            return Err(RuntimeError::UnsafePath(endpoint.to_path_buf()));
        }
        std::fs::remove_file(endpoint)?;
    }
    Ok(())
}

/// Bind the 0600 socket listener (kept alive to serve connections).
pub fn build_listener(paths: &RuntimePaths) -> Result<DaemonListener, RuntimeError> {
    let listener = tokio::net::UnixListener::bind(&paths.endpoint)?;
    set_mode(&paths.endpoint, 0o600)?;
    Ok(DaemonListener {
        listener: std::sync::Arc::new(listener),
    })
}

/// Side-effect-free probe: shared non-blocking flock, released immediately.
pub fn lock_held_platform(paths: &RuntimePaths) -> Option<bool> {
    use std::os::unix::io::AsRawFd;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .open(&paths.lock_path)
        .ok()?;
    // SAFETY: flock on our own open fd; shared non-blocking, released right
    // after the probe.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
    if rc == 0 {
        unsafe {
            libc::flock(file.as_raw_fd(), libc::LOCK_UN);
        }
        return Some(false);
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::AlreadyExists
        || err.kind() == std::io::ErrorKind::WouldBlock
    {
        return Some(true);
    }
    None
}

/// Set a file's permission mode (Unix).
fn set_mode(path: &Path, mode: u32) -> Result<(), RuntimeError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Same-user IPC boundary (design §7.1): the peer's UID must equal ours.
/// The 0600 socket already restricts access; this closes the remainder
/// (permissive umask at bind, fd passing). Raw UIDs, no exceptions — not
/// even root connecting elsewhere.
fn peer_authorized(peer_uid: u32, own_uid: u32) -> bool {
    peer_uid == own_uid
}

/// Reject connections from other UIDs before reading any frame.
fn check_peer_cred(stream: &tokio::net::UnixStream) -> bool {
    let Ok(peer) = stream.peer_cred() else {
        return false;
    };
    // SAFETY: geteuid takes no arguments and only reads process state.
    peer_authorized(peer.uid(), unsafe { libc::geteuid() })
}

impl DaemonListener {
    /// Accept one connection; the same-user check runs here, before any
    /// frame is read, and a rejected peer gets the wire error it expects.
    pub async fn accept(&self) -> Result<IpcStream, std::io::Error> {
        let (stream, _) = self.listener.accept().await?;
        if !check_peer_cred(&stream) {
            let err = WireError {
                kind: "unauthorized".into(),
                message: "peer uid differs from daemon uid".into(),
            };
            let payload = serde_json::to_vec(&WireReply::Error(err))
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let mut stream = &stream;
            let _ = write_response_payload(stream, &payload).await;
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unauthorized peer uid",
            ));
        }
        Ok(Box::new(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{RuntimePaths, acquire_singleton};

    #[tokio::test]
    async fn acquire_singleton_succeeds_on_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");
        let runtime = acquire_singleton(&paths).unwrap();
        // Lock held; socket bound.
        assert!(paths.endpoint.exists());
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
        assert!(paths.endpoint.exists());
        drop(runtime);

        // A new daemon acquires the now-free lock and recovers the stale socket.
        let runtime2 = acquire_singleton(&paths).unwrap();
        assert!(paths.endpoint.exists(), "socket recreated after recovery");
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

    /// The side-effect-free lock probe reports held while an owner lives,
    /// free after it drops, and unknown when nothing exists to inspect.
    #[tokio::test]
    async fn lock_probe_distinguishes_held_free_and_missing() {
        // Nothing on disk yet: unknown, never a bare false that could
        // mislead a spawner into failing fast.
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");
        assert_eq!(lock_held_platform(&paths), None);
        let runtime = acquire_singleton(&paths).unwrap();
        assert_eq!(lock_held_platform(&paths), Some(true));
        drop(runtime);
        assert_eq!(lock_held_platform(&paths), Some(false));
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
        assert!(check_peer_cred(&a));
    }
}
