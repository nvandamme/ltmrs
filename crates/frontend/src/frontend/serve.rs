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

pub mod daemon;
#[cfg(test)]
mod serve_tests;
#[cfg(test)]
mod test_support;

use crate::cli::CliError;
use ltmrs_daemon::server::EmbeddingMode;

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
pub(crate) fn home_dir_from(home: Option<String>, fallback: Option<String>) -> Option<String> {
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
pub fn resolve_daemon_embedding(layout: &StdioLayout) -> EmbeddingMode {
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
