//! Windows runtime: `LockFileEx` singleton, per-user DACL named pipe, reparse
//! refusal (design §2.2/2.3).
//!
//! Windows named pipes have no filesystem residue and no peer-credential
//! socket: the singleton is a mandatory `LockFileEx` region on the lock file
//! (released when the handle closes, exactly like flock), and the same-user
//! boundary is a DACL granting generic read/write/execute to the current
//! user's SID and SYSTEM only — the OS rejects any other user at connect
//! time, so no frame is ever read from a foreign connection.

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};

use sha2::Sha256;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE, LocalFree, TRUE};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    ACL, ACL_REVISION, AllocateAndInitializeSid, FreeSid, GetLengthSid, GetTokenInformation,
    InitializeSecurityDescriptor, PSID, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
    SECURITY_NT_AUTHORITY, SetSecurityDescriptorDacl, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx, UnlockFile,
};
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::Pipes::PeekNamedPipe;
use windows_sys::Win32::System::SystemServices::{
    ACCESS_ALLOWED_ACE_TYPE, SECURITY_DESCRIPTOR_REVISION, SECURITY_LOCAL_SYSTEM_RID,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::{DaemonListener, IpcStream, RuntimeError, RuntimePaths};

/// The pipe endpoint name: `\\.\pipe\ltmrs-<identity>-<sha256(sid ‖ runtime_dir)[12]>`.
/// The pipe namespace is system-global, so the token mixes the user SID
/// with the runtime directory: two users' stores cannot collide, and two
/// stores under different bases (tests, portable stores) get distinct
/// names (design §2.1). The SID comes from the process token, which is
/// valid for the process lifetime; the string the system allocates is
/// freed immediately after reading.
pub fn platform_endpoint(runtime_dir: &Path, store_identity: &str) -> PathBuf {
    use sha2::Digest as _;
    use std::os::windows::ffi::OsStrExt as _;
    let sid = user_sid_string().expect("cannot read the process token SID");
    let mut digest = Sha256::new();
    digest.update(sid.as_bytes());
    let dir_bytes: Vec<u8> = runtime_dir
        .as_os_str()
        .encode_wide()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    digest.update(&dir_bytes);
    PathBuf::from(format!(
        "\\\\.\\pipe\\ltmrs-{}-{}",
        store_identity,
        hex_lower(&digest.finalize()[..6])
    ))
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The current user's SID string (S-1-5-21-...).
pub fn user_sid_string() -> Result<String, RuntimeError> {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: process pseudo-handle is always valid; the token we open is
        // closed below; buffers are call-scoped.
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) != TRUE {
            return Err(RuntimeError::Io(std::io::Error::last_os_error()));
        }
        let bytes = match read_token_user_sid(&mut token) {
            Ok(b) => b,
            Err(e) => {
                CloseHandle(token);
                return Err(e);
            }
        };
        let s = sid_bytes_to_string(&bytes);
        CloseHandle(token);
        s
    }
}

/// Read the user SID from the process token as owned bytes. The token
/// buffer is local to this call, so the SID bytes must be copied out — a
/// pointer into it would dangle the moment we return.
unsafe fn read_token_user_sid(token: &mut HANDLE) -> Result<Vec<u8>, RuntimeError> {
    let mut buf = [0u8; 256];
    let mut ret = 0u32;
    unsafe {
        if GetTokenInformation(
            *token,
            TokenUser,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut ret,
        ) != TRUE
        {
            return Err(RuntimeError::Io(std::io::Error::last_os_error()));
        }
        // The token buffer is a byte array (align 1); the TOKEN_USER header
        // it carries is only DWORD-aligned, so read it unaligned.
        let tu: TOKEN_USER = std::ptr::read_unaligned(buf.as_ptr() as *const TOKEN_USER);
        let sid_ptr = tu.User.Sid;
        let len = GetLengthSid(sid_ptr) as usize;
        Ok(std::slice::from_raw_parts(sid_ptr as *const u8, len).to_vec())
    }
}

/// Convert DWORD-aligned SID bytes to their string form (the system
/// allocates the buffer; freed immediately after reading).
fn sid_bytes_to_string(bytes: &[u8]) -> Result<String, RuntimeError> {
    let mut aligned = vec![0u8; (bytes.len() + 3) & !3];
    aligned[..bytes.len()].copy_from_slice(bytes);
    unsafe {
        let mut p: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(aligned.as_ptr() as *const c_void as *mut c_void, &mut p) != TRUE
        {
            return Err(RuntimeError::Io(std::io::Error::last_os_error()));
        }
        let mut len = 0;
        while *(p.add(len)) != 0 {
            len += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
        LocalFree(p as *mut c_void);
        Ok(s)
    }
}

/// Build the DACL as raw ACL bytes: exactly two ACCESS_ALLOWED ACEs
/// (generic read/write/execute = 0xE0000000), the user SID then SYSTEM, no
/// other ACE. Built programmatically because windows-sys exposes no SDDL
/// parser; the bytes are what `CreateNamedPipe` receives, so verifying them
/// verifies the descriptor the OS applies.
fn build_dacl(user_sid: &[u8], sys_sid: &[u8]) -> Vec<u8> {
    // ACCESS_ALLOWED_ACE: 4-byte header + 4-byte mask + SID bytes.
    let ace1 = 8 + user_sid.len();
    let ace2 = 8 + sys_sid.len();
    let size = 8 + ace1 + ace2;
    let mut acl = vec![0u8; size];
    acl[0] = ACL_REVISION as u8;
    acl[2..4].copy_from_slice(&(size as u16).to_le_bytes());
    acl[4..8].copy_from_slice(&2u32.to_le_bytes());
    let mut off = 8;
    for (bytes, ace) in [(user_sid, ace1), (sys_sid, ace2)] {
        acl[off] = ACCESS_ALLOWED_ACE_TYPE as u8;
        acl[off + 2..off + 4].copy_from_slice(&(ace as u16).to_le_bytes());
        acl[off + 4..off + 8].copy_from_slice(&0xE000_0000u32.to_le_bytes());
        acl[off + 8..off + 8 + bytes.len()].copy_from_slice(bytes);
        off += ace;
    }
    acl
}

/// Create one DACL-protected pipe instance. All structures (token buffer,
/// ACL, descriptor, attributes) stay alive for the duration of the call.
fn create_pipe(endpoint: &Path) -> Result<NamedPipeServer, RuntimeError> {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) != TRUE {
            return Err(RuntimeError::Io(std::io::Error::last_os_error()));
        }
        let user_bytes = match read_token_user_sid(&mut token) {
            Ok(b) => b,
            Err(e) => {
                CloseHandle(token);
                return Err(e);
            }
        };
        let mut sys_sid: PSID = std::ptr::null_mut();
        // SAFETY: SECURITY_NT_AUTHORITY is a valid static authority; the SID
        // we allocate is freed at the end of this call.
        if AllocateAndInitializeSid(
            &SECURITY_NT_AUTHORITY,
            1,
            SECURITY_LOCAL_SYSTEM_RID as u32,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            &mut sys_sid,
        ) != TRUE
        {
            CloseHandle(token);
            return Err(RuntimeError::Io(std::io::Error::last_os_error()));
        }
        let sys_len = GetLengthSid(sys_sid) as usize;
        // SAFETY: sys_sid is valid from allocation to FreeSid at the end of
        // this call.
        let sys_bytes = std::slice::from_raw_parts(sys_sid as *const u8, sys_len).to_vec();
        let mut acl = build_dacl(&user_bytes, &sys_bytes);
        let mut sd: SECURITY_DESCRIPTOR = std::mem::zeroed();
        let sd_ptr = &mut sd as *mut SECURITY_DESCRIPTOR as *mut c_void;
        if InitializeSecurityDescriptor(sd_ptr, SECURITY_DESCRIPTOR_REVISION) != TRUE {
            FreeSid(sys_sid);
            CloseHandle(token);
            return Err(RuntimeError::Io(std::io::Error::last_os_error()));
        }
        if SetSecurityDescriptorDacl(sd_ptr, TRUE, acl.as_mut_ptr() as *const ACL, FALSE) != TRUE {
            FreeSid(sys_sid);
            CloseHandle(token);
            return Err(RuntimeError::Io(std::io::Error::last_os_error()));
        }
        let sec = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd_ptr,
            bInheritHandle: 0,
        };
        // SAFETY: `sec` is initialized with a valid security descriptor and
        // ACL, and both outlive this call.
        let server = ServerOptions::new()
            .create_with_security_attributes_raw(endpoint, &raw const sec as *mut c_void);
        FreeSid(sys_sid);
        CloseHandle(token);
        server.map_err(RuntimeError::Io)
    }
}

pub fn create_runtime_dir(dir: &Path) -> Result<(), RuntimeError> {
    if dir.exists() {
        // Refuse reparse points (symlinks and junctions are both detected
        // by is_symlink on Windows).
        let meta = std::fs::symlink_metadata(dir)?;
        if meta.file_type().is_symlink() {
            return Err(RuntimeError::UnsafePath(dir.to_path_buf()));
        }
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// No endpoint residue to recover on Windows: a dead daemon's pipe name is
/// simply gone, and the next `accept` creates a fresh instance.
pub fn recover_stale_endpoint(_endpoint: &Path) -> Result<(), RuntimeError> {
    Ok(())
}

pub fn acquire_lock(file: &std::fs::File, lock_path: &Path) -> Result<(), RuntimeError> {
    use std::os::windows::io::AsRawHandle;
    let handle = file.as_raw_handle();
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    // SAFETY: `handle` is the live handle of a file we just opened; the
    // zeroed OVERLAPPED is call-scoped.
    let ok = unsafe {
        LockFileEx(
            handle,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut ov,
        )
    };
    if ok == 0 {
        // Lock conflicts surface as ERROR_LOCK_VIOLATION (32) or
        // ERROR_BLOCK_TYPE_NOT_MATCHED (33); std maps 33 to Uncategorized,
        // so the raw code is the reliable signal that a live owner exists.
        let err = std::io::Error::last_os_error();
        if matches!(err.raw_os_error(), Some(32) | Some(33)) {
            return Err(RuntimeError::AlreadyRunning(lock_path.to_path_buf()));
        }
        return Err(RuntimeError::Io(err));
    }
    Ok(())
}

/// Side-effect-free probe: a shared non-blocking lock succeeds only when no
/// exclusive owner exists; released immediately.
pub fn lock_held_platform(paths: &RuntimePaths) -> Option<bool> {
    use std::os::windows::io::AsRawHandle;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .open(&paths.lock_path)
        .ok()?;
    let handle = file.as_raw_handle();
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    // SAFETY: live handle of a file we opened; zeroed OVERLAPPED is
    // call-scoped; we unlock the region we just took.
    let ok = unsafe { LockFileEx(handle, LOCKFILE_FAIL_IMMEDIATELY, 0, 1, 0, &mut ov) };
    if ok == 0 {
        let err = std::io::Error::last_os_error();
        if matches!(err.raw_os_error(), Some(32) | Some(33)) {
            return Some(true);
        }
        return None;
    }
    unsafe { UnlockFile(handle, 0, 0, 0, 0) };
    Some(false)
}

pub fn build_listener(paths: &RuntimePaths) -> Result<DaemonListener, RuntimeError> {
    Ok(DaemonListener {
        endpoint: paths.endpoint.clone(),
    })
}

impl DaemonListener {
    /// Create one DACL-protected pipe instance, wait for a client, and hand
    /// over the duplex byte stream (standard named-pipe fan-out: one
    /// instance serves one connection).
    ///
    /// `ConnectNamedPipe` reports `ERROR_NO_DATA` when the instance is in
    /// byte mode and no client is attached, which tokio/mio mistake for a
    /// successful connect: the instance is listening but never connected,
    /// and reads return EOF at once. Accepting those spins the serve loop
    /// forever (and starves idle-exit). The connect itself stays inside
    /// tokio/mio — their `OVERLAPPED` lives in boxed, never-moved memory,
    /// which a future-stack `OVERLAPPED` cannot provide (the kernel holds
    /// its pointer across awaits) — and the result is gated on DATA: our
    /// protocol is client-writes-first (handshake), so bytes waiting prove
    /// a genuine peer while a never-connected instance stays at zero
    /// forever. The same instance is polled throughout so a racing client
    /// always finds a listener; the wait is bounded so idle-exit fires.
    pub async fn accept(&self) -> Result<IpcStream, std::io::Error> {
        let server =
            create_pipe(&self.endpoint).map_err(|e| std::io::Error::other(e.to_string()))?;
        server.connect().await?;
        let data = tokio::time::timeout(std::time::Duration::from_millis(1000), async {
            loop {
                if peek_available(server.as_raw_handle()) > 0 {
                    return;
                }
                // A client that landed since the last call needs another
                // `ConnectNamedPipe` to finish server-side; on an already
                // connected instance that is expected noise — only the byte
                // count above matters.
                let _ = server.connect().await;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
        if data.is_ok() {
            Ok(Box::new(server))
        } else {
            Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
        }
    }
}

/// Bytes waiting on the instance, without consuming them. `PeekNamedPipe`
/// itself succeeds on a never-connected instance — only the count
/// discriminates (`> 0` proves a peer sent its handshake).
fn peek_available(handle: std::os::windows::io::RawHandle) -> u32 {
    let mut avail = 0u32;
    // SAFETY: live pipe handle; a null buffer with zero size only queries.
    let ok = unsafe {
        PeekNamedPipe(
            handle,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut avail,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 { 0 } else { avail }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{RuntimePaths, acquire_singleton};

    /// The endpoint carries the per-user+per-store token and differs per
    /// store and per base directory.
    #[test]
    fn endpoint_carries_user_sid_token() {
        use sha2::Digest as _;
        use std::os::windows::ffi::OsStrExt as _;
        let sid = user_sid_string().unwrap();
        let a = RuntimePaths::resolve(Path::new("C:/base"), "store-a");
        let mut digest = Sha256::new();
        digest.update(sid.as_bytes());
        let dir_bytes: Vec<u8> = a
            .runtime_dir
            .as_os_str()
            .encode_wide()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        digest.update(&dir_bytes);
        let token = hex_lower(&digest.finalize()[..6]);
        let b = RuntimePaths::resolve(Path::new("C:/base"), "store-b");
        assert_ne!(a.endpoint, b.endpoint);
        let s = a.endpoint.to_string_lossy().into_owned();
        assert!(s.starts_with("\\\\.\\pipe\\ltmrs-store-a-"), "got: {s}");
        assert!(
            s.ends_with(&token),
            "endpoint must carry the token, got: {s}"
        );
    }

    /// LockFileEx exclusivity: a second acquire fails while the owner lives.
    #[tokio::test]
    async fn second_acquire_rejects_while_first_holds_lock() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");
        let runtime = acquire_singleton(&paths).unwrap();
        let result = acquire_singleton(&paths);
        assert!(matches!(result, Err(RuntimeError::AlreadyRunning(_))));
        drop(runtime);
    }

    /// The probe reports held while an owner lives, free after it drops,
    /// and unknown when nothing exists to inspect.
    #[tokio::test]
    async fn lock_probe_distinguishes_held_free_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");
        assert_eq!(lock_held_platform(&paths), None);
        let runtime = acquire_singleton(&paths).unwrap();
        assert_eq!(lock_held_platform(&paths), Some(true));
        drop(runtime);
        assert_eq!(lock_held_platform(&paths), Some(false));
    }

    /// A junctioned runtime dir is refused (reparse points read as symlinks).
    #[test]
    fn junction_runtime_dir_refused() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");
        let parent = paths.runtime_dir.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&parent).unwrap();
        let target = dir.path().join("real-dir");
        std::fs::create_dir_all(&target).unwrap();
        // mklink /J works unprivileged; symlinks need SeCreateLinkPrivilege.
        let ok = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &paths.runtime_dir.to_string_lossy(),
                &target.to_string_lossy(),
            ])
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "junction creation must succeed in tempdir");
        let result = acquire_singleton(&paths);
        assert!(matches!(result, Err(RuntimeError::UnsafePath(_))));
    }

    /// The DACL bytes the OS receives carry exactly the user + SYSTEM ACEs:
    /// two ACCESS_ALLOWED ACEs, generic RWX mask, no Everyone.
    #[test]
    fn dacl_grants_only_user_and_system() {
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token);
            let user_bytes = read_token_user_sid(&mut token).unwrap();
            let user_str = sid_bytes_to_string(&user_bytes).unwrap();

            let mut sys_sid: PSID = std::ptr::null_mut();
            AllocateAndInitializeSid(
                &SECURITY_NT_AUTHORITY,
                1,
                SECURITY_LOCAL_SYSTEM_RID as u32,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                &mut sys_sid,
            );
            let sys_len = GetLengthSid(sys_sid) as usize;
            let sys_bytes = std::slice::from_raw_parts(sys_sid as *const u8, sys_len).to_vec();
            let sys_str = sid_bytes_to_string(&sys_bytes).unwrap();
            assert_eq!(sys_str, "S-1-5-18", "SYSTEM SID is pinned");

            let acl = build_dacl(&user_bytes, &sys_bytes);
            assert_eq!(acl[0], ACL_REVISION as u8);
            let count = u32::from_le_bytes([acl[4], acl[5], acl[6], acl[7]]);
            assert_eq!(count, 2, "exactly two ACEs");

            let mut sids = Vec::new();
            let mut off = 8;
            for _ in 0..2 {
                assert_eq!(acl[off], ACCESS_ALLOWED_ACE_TYPE as u8, "allowed ACE");
                assert_eq!(
                    u32::from_le_bytes([acl[off + 4], acl[off + 5], acl[off + 6], acl[off + 7]]),
                    0xE000_0000,
                    "generic RWX mask"
                );
                let ace = u16::from_le_bytes([acl[off + 2], acl[off + 3]]) as usize;
                // The ACE's embedded SID bytes convert back to the granted SID.
                sids.push(sid_bytes_to_string(&acl[off + 8..off + ace]).unwrap());
                off += ace;
            }
            assert!(
                sids.contains(&user_str),
                "user SID must be granted, got: {sids:?}"
            );
            assert!(
                sids.contains(&"S-1-5-18".to_string()),
                "SYSTEM must be granted, got: {sids:?}"
            );
            assert!(
                !sids.contains(&"S-1-1-0".to_string()),
                "Everyone must not be granted, got: {sids:?}"
            );
            FreeSid(sys_sid);
            CloseHandle(token);
        }
    }

    /// The live Windows path: a daemon serves its pipe endpoint and a client
    /// connects, handshakes and round-trips over the real named pipe.
    #[tokio::test]
    async fn client_connects_to_live_named_pipe() {
        use crate::client::IpcClient;
        use crate::envelope::{HandshakeRequest, PROTOCOL_VERSION};
        use crate::server::{Daemon, DaemonConfig};
        use ltmrs_domain::id::{ChannelId, FrontendId, StoreGeneration};
        use uuid::Uuid;

        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::resolve(dir.path(), "test-store");
        let config = DaemonConfig {
            store_path: dir.path().join("store").to_str().unwrap().to_string(),
            ..Default::default()
        };
        let mut daemon = Daemon::start(&paths, config).await.unwrap();
        daemon.spawn_socket_server().await;

        let mut client = IpcClient::new(paths.endpoint.clone());
        tokio::time::timeout(std::time::Duration::from_secs(10), client.connect())
            .await
            .expect("connect must not hang")
            .unwrap();
        let hs = client
            .handshake(&HandshakeRequest {
                protocol_version: PROTOCOL_VERSION,
                store_generation: StoreGeneration::FIRST,
                frontend_id: FrontendId::new(Uuid::from_u128(51)),
                channel_id: ChannelId::new(Uuid::from_u128(52)),
                resume_retry_epoch: None,
            })
            .await
            .unwrap();
        assert_eq!(hs.store_generation, StoreGeneration::FIRST);
        daemon.shutdown();
    }
}
