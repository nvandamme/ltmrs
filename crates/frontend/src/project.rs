//! Project bind files and identity resolution (Wave A3b, Part I 01:§6.3).
//!
//! The frontend never decides identity: it resolves the env override,
//! else the checkout's bind file, else nothing (the daemon falls back to
//! the cwd clue it also forwards). Binding is a local file write;
//! registration happens daemon-side on first acknowledged write.

use std::path::{Path, PathBuf};

/// Bind directory inside a checkout.
pub const BIND_DIR: &str = ".ltmrs";
/// Bind file name inside [`BIND_DIR`].
pub const BIND_FILE: &str = "project.json";
/// Env override for the stable project id (wins over the bind file).
pub const ENV_PROJECT_ID: &str = "LTMRS_PROJECT_ID";

/// The checkout's bound identity, as stored in `project.json`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Binding {
    pub project_id: String,
    pub name: String,
    pub created_at: u64,
}

/// The bind file path for a checkout directory.
pub fn bind_path(checkout: &Path) -> PathBuf {
    checkout.join(BIND_DIR).join(BIND_FILE)
}

/// Read a checkout's bind file. `Ok(None)` when absent; `Err` on corrupt
/// content (surfaced by status, never silently ignored).
pub fn read_bind(checkout: &Path) -> Result<Option<Binding>, String> {
    let path = bind_path(checkout);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| format!("corrupt bind file {}: {e}", path.display()))
}

/// Write a bind file (`0600`), refusing when one already exists.
pub fn write_bind(checkout: &Path, binding: &Binding) -> Result<PathBuf, String> {
    let path = bind_path(checkout);
    if path.exists() {
        return Err(format!(
            "already bound ({} exists); remove it to rebind",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true).mode(0o600);
        let mut file = opts
            .open(&path)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        use std::io::Write;
        let raw = serde_json::to_vec_pretty(binding)
            .map_err(|e| format!("cannot encode binding: {e}"))?;
        file.write_all(&raw)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        let raw = serde_json::to_vec_pretty(binding)
            .map_err(|e| format!("cannot encode binding: {e}"))?;
        file.write_all(&raw)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    Ok(path)
}

/// Canonicalize a checkout root for identity (realpath, case preserved).
/// `None` when the path cannot be canonicalized.
pub fn canonical_root(path: &Path) -> Option<String> {
    std::fs::canonicalize(path)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Resolve the winning stable id: env override, else the bind file.
/// Malformed env values fail loudly (silent misattribution is worse).
pub fn resolve_project_id(
    cwd: Option<&Path>,
    env_override: Option<String>,
) -> Result<Option<Binding>, String> {
    if let Some(raw) = env_override.filter(|s| !s.trim().is_empty()) {
        let raw = raw.trim().to_string();
        let (name, _id) = ltmrs_domain::project_id::parse_project_id(&raw).ok_or_else(|| {
            format!("{ENV_PROJECT_ID} is malformed (want prj_<name>_<26-char ULID>), got: {raw}")
        })?;
        return Ok(Some(Binding {
            project_id: raw,
            name,
            created_at: 0,
        }));
    }
    match cwd {
        Some(dir) => read_bind(dir),
        None => Ok(None),
    }
}

/// Best-effort count of unattached legacy rows for a slug, via the
/// managed-home daemon. `None` when no daemon serves (offline bind).
pub async fn daemon_unattached_count(name: &str, home: Option<String>) -> Option<usize> {
    use ltmrs_daemon::client::IpcClient;
    use ltmrs_daemon::envelope::{DomainRequest, HandshakeRequest, PROTOCOL_VERSION};
    use ltmrs_domain::id::{ChannelId, FrontendId, OperationId, StoreGeneration};

    let home = home.filter(|h| !h.is_empty())?;
    let base = std::path::Path::new(&home).join(crate::frontend::serve::MANAGED_HOME_DIR);
    if !base.exists() {
        return None;
    }
    let layout = crate::frontend::serve::stdio_layout(&base);
    let socket = crate::frontend::serve::daemon::daemon_socket_path(&layout);
    let mut client = IpcClient::new(socket);
    tokio::time::timeout(std::time::Duration::from_secs(2), client.connect())
        .await
        .ok()?
        .ok()?;
    let hs = client
        .handshake(&HandshakeRequest {
            protocol_version: PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id: FrontendId::new(uuid::Uuid::now_v7()),
            channel_id: ChannelId::new(uuid::Uuid::now_v7()),
            resume_retry_epoch: None,
        })
        .await
        .ok()?;
    let env = ltmrs_daemon::envelope::IpcEnvelope {
        protocol_version: PROTOCOL_VERSION,
        store_generation: hs.store_generation,
        frontend_id: FrontendId::new(uuid::Uuid::now_v7()),
        channel_id: ChannelId::new(uuid::Uuid::now_v7()),
        operation_id: OperationId::new(uuid::Uuid::now_v7()),
        session: None,
        retry_epoch: hs.retry_epoch,
        deadline_millis: None,
        scope: Default::default(),
        identity: Default::default(),
        body: DomainRequest::ListMemories,
    };
    let resp = tokio::time::timeout(std::time::Duration::from_secs(5), client.roundtrip(&env))
        .await
        .ok()?
        .ok()?;
    match resp.result {
        ltmrs_daemon::envelope::IpcResult::Success {
            payload: ltmrs_daemon::envelope::DomainPayload::Memories(mems),
            ..
        } => Some(
            mems.iter()
                .filter(|m| m.project_id.is_none() && m.project.as_deref() == Some(name))
                .count(),
        ),
        _ => None,
    }
}

/// Adoption report carried back from the daemon over internal IPC.
#[derive(Debug, Clone, PartialEq)]
pub struct AdoptReport {
    pub attached: usize,
    pub already_attached: bool,
    pub created: bool,
    pub warning: Option<String>,
    pub record_name: String,
}

/// Execute a legacy-bucket adoption on the daemon's store handle via
/// internal IPC (never a direct CLI open: concurrent writers must not
/// dual-open the store). `Err` carries user-facing guidance.
pub async fn daemon_adopt_slug(
    home: Option<String>,
    project_id: &str,
    canonical_root: Option<String>,
    slug: &str,
) -> Result<AdoptReport, String> {
    use ltmrs_daemon::client::IpcClient;
    use ltmrs_daemon::envelope::{DomainRequest, HandshakeRequest, PROTOCOL_VERSION};
    use ltmrs_domain::id::{ChannelId, FrontendId, OperationId, StoreGeneration};

    let home = home
        .filter(|h| !h.is_empty())
        .ok_or_else(|| "HOME is not set".to_string())?;
    let base = std::path::Path::new(&home).join(crate::frontend::serve::MANAGED_HOME_DIR);
    let layout = crate::frontend::serve::stdio_layout(&base);
    let socket = crate::frontend::serve::daemon::daemon_socket_path(&layout);
    let sock_display = socket.display().to_string();
    let unreachable = move || {
        format!(
            "adopt needs the running daemon: cannot reach it at {sock_display} (start it with `ltmrs daemon`)"
        )
    };
    let mut client = IpcClient::new(socket);
    tokio::time::timeout(std::time::Duration::from_secs(2), client.connect())
        .await
        .map_err(|_| unreachable())?
        .map_err(|_| unreachable())?;
    let frontend_id = FrontendId::new(uuid::Uuid::now_v7());
    let channel_id = ChannelId::new(uuid::Uuid::now_v7());
    let hs = client
        .handshake(&HandshakeRequest {
            protocol_version: PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id,
            channel_id,
            resume_retry_epoch: None,
        })
        .await
        .map_err(|_| unreachable())?;
    let env = ltmrs_daemon::envelope::IpcEnvelope {
        protocol_version: PROTOCOL_VERSION,
        store_generation: hs.store_generation,
        frontend_id,
        channel_id,
        operation_id: OperationId::new(uuid::Uuid::now_v7()),
        session: None,
        retry_epoch: hs.retry_epoch,
        deadline_millis: None,
        scope: Default::default(),
        identity: Default::default(),
        body: DomainRequest::AdoptLegacySlug {
            project_id: project_id.to_string(),
            canonical_root,
            slug: slug.to_string(),
        },
    };
    let resp = tokio::time::timeout(std::time::Duration::from_secs(30), client.roundtrip(&env))
        .await
        .map_err(|_| unreachable())?
        .map_err(|_| unreachable())?;
    match resp.result {
        ltmrs_daemon::envelope::IpcResult::Success {
            payload:
                ltmrs_daemon::envelope::DomainPayload::Adopted {
                    attached,
                    already_attached,
                    created,
                    warning,
                    record_name,
                },
            ..
        } => Ok(AdoptReport {
            attached,
            already_attached,
            created,
            warning,
            record_name,
        }),
        ltmrs_daemon::envelope::IpcResult::Success { .. } => {
            Err("adopt got an unexpected daemon response".to_string())
        }
        ltmrs_daemon::envelope::IpcResult::Error { message, .. } => Err(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkout(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        (dir, root)
    }

    #[test]
    fn bind_round_trip_writes_0600_json() {
        let (_d, root) = checkout("shop");
        let binding = Binding {
            project_id: "prj_shop_01ARZ3NDEKTSV4RRFFQ69G5FAV2".into(),
            name: "shop".into(),
            created_at: 1_700_000_000_123,
        };
        let path = write_bind(&root, &binding).unwrap();
        assert_eq!(path, root.join(".ltmrs").join("project.json"));
        let back = read_bind(&root).unwrap().unwrap();
        assert_eq!(back, binding);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "bind file must be owner-only");
        }
    }

    #[test]
    fn bind_refuses_when_already_bound() {
        let (_d, root) = checkout("shop");
        let binding = Binding {
            project_id: "prj_shop_01ARZ3NDEKTSV4RRFFQ69G5FAV2".into(),
            name: "shop".into(),
            created_at: 1,
        };
        write_bind(&root, &binding).unwrap();
        let err = write_bind(&root, &binding).unwrap_err();
        assert!(err.contains("already bound"), "got: {err}");
    }

    #[test]
    fn corrupt_bind_file_errors_loudly() {
        let (_d, root) = checkout("shop");
        let dir = root.join(".ltmrs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("project.json"), "{not json").unwrap();
        assert!(read_bind(&root).is_err());
    }

    #[test]
    fn resolve_prefers_env_over_file() {
        let (_d, root) = checkout("shop");
        let file_binding = Binding {
            project_id: "prj_shop_01ARZ3NDEKTSV4RRFFQ69G5FAV2".into(),
            name: "shop".into(),
            created_at: 1,
        };
        write_bind(&root, &file_binding).unwrap();
        let env_id = ltmrs_domain::project_id::generate_project_id("other").0;
        let won = resolve_project_id(Some(&root), Some(env_id.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(won.project_id, env_id);
        let from_file = resolve_project_id(Some(&root), None).unwrap().unwrap();
        assert_eq!(from_file, file_binding);
    }

    #[test]
    fn malformed_env_fails_loudly() {
        let (_d, root) = checkout("shop");
        let err = resolve_project_id(Some(&root), Some("bogus".into())).unwrap_err();
        assert!(err.contains("LTMRS_PROJECT_ID"), "got: {err}");
    }

    #[test]
    fn canonical_root_resolves_to_absolute() {
        let (_d, root) = checkout("shop");
        let canon = canonical_root(&root).unwrap();
        assert!(
            Path::new(&canon).is_absolute(),
            "must be absolute, got: {canon}"
        );
    }
}
