//! Opt-in silent-capture hooks (Slice B).
//!
//! Two CLI subcommands (`hook-inject`, `hook-extract`) plus an
//! `install-hooks` generator for host JSON snippets. Hooks are opt-in
//! and default OFF: missing or unparseable config means disabled, and
//! every runtime path fails open (exit 0, empty stdout) so a hook never
//! blocks a host session.

use std::path::{Path, PathBuf};

/// Bump on any template or config-schema change.
pub const HOOKS_VERSION: &str = "0.1.0";

/// Hooks config file name under `<home>/.ltmrs/`.
pub const HOOKS_FILE: &str = "hooks.json";

/// Parsed hooks config: a single enabled flag (Phase 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HooksConfig {
    pub enabled: bool,
}

/// Outcome of a hooks install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    Installed,
    AlreadyCurrent,
}

/// Hooks install error.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("refusing: unmanaged file at {0} (back it up and remove it first)")]
    RefusedForeign(PathBuf),
    #[error("io: {0}")]
    Io(String),
}

/// Unknown-host error for snippet generation.
#[derive(Debug, thiserror::Error)]
pub enum HooksError {
    #[error("unknown host: {0}")]
    UnknownHost(String),
}

/// Inject input: a recall hint plus optional project scope.
#[derive(Debug, Clone, Default)]
pub struct HookInjectInput {
    pub query_hint: String,
    pub project: Option<String>,
}

/// Extract input: a transcript tail plus optional project scope.
#[derive(Debug, Clone, Default)]
pub struct HookExtractInput {
    pub transcript_tail: String,
    pub project: Option<String>,
}

/// Hook output: exit code plus stdout (empty on fail-open / extract).
#[derive(Debug, Clone)]
pub struct HookOutput {
    pub exit_code: i32,
    pub stdout: String,
}

impl HookOutput {
    /// Fail-open output: exit 0, empty stdout.
    pub fn empty() -> Self {
        Self {
            exit_code: 0,
            stdout: String::new(),
        }
    }
}

/// Path to the hooks config file.
pub fn hooks_config_path(home: &Path) -> PathBuf {
    home.join(".ltmrs").join(HOOKS_FILE)
}

/// Load hooks config: missing file, empty, or unparseable means disabled
/// (never invent a home, never throw from a hook gate).
pub fn load_hooks_config(home: &Path) -> HooksConfig {
    let path = hooks_config_path(home);
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    if raw.trim().is_empty() {
        return HooksConfig { enabled: false };
    }
    let value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return HooksConfig { enabled: false },
    };
    HooksConfig {
        enabled: value
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

/// Run hook-inject (Phase 1: gate only; recall lands in Phase 2).
pub async fn run_hook_inject(home: Option<String>, _input: &HookInjectInput) -> HookOutput {
    let Some(home) = home.filter(|h| !h.is_empty()) else {
        return HookOutput::empty();
    };
    if std::env::var("LTMRS_HOOKS_ENABLED").as_deref() == Ok("0") {
        return HookOutput::empty();
    }
    let config = load_hooks_config(Path::new(&home));
    if !config.enabled {
        return HookOutput::empty();
    }
    // Phase 2 implements recall; the gate above is the Phase 1 contract.
    HookOutput::empty()
}

/// Run hook-extract (Phase 1: gate only; extraction lands in Phase 3).
pub async fn run_hook_extract(home: Option<String>, _input: &HookExtractInput) -> HookOutput {
    let Some(home) = home.filter(|h| !h.is_empty()) else {
        return HookOutput::empty();
    };
    if std::env::var("LTMRS_HOOKS_ENABLED").as_deref() == Ok("0") {
        return HookOutput::empty();
    }
    let config = load_hooks_config(Path::new(&home));
    if !config.enabled {
        return HookOutput::empty();
    }
    // Phase 3 implements extraction; the gate above is the Phase 1 contract.
    HookOutput::empty()
}

/// Install hooks (Phase 1: writes `hooks.json`; snippets land in Phase 4).
pub fn install_hooks(_home: &Path, _exe: &Path) -> Result<InstallOutcome, InstallError> {
    // Phase 4 implements atomic install + snippet generation.
    Ok(InstallOutcome::AlreadyCurrent)
}

/// Host JSON snippet (Phase 1: host registry only; templates land in Phase 4).
pub fn hook_snippet(host: &str, _exe: &Path) -> Result<String, HooksError> {
    match host {
        "claude-code" => Err(HooksError::UnknownHost(format!(
            "{host} (templates land in Phase 4)"
        ))),
        other => Err(HooksError::UnknownHost(other.to_string())),
    }
}
