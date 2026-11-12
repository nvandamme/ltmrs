//! Opt-in silent-capture hooks (Slice B).
//!
//! Two CLI subcommands (`hook-inject`, `hook-extract`) plus an
//! `install-hooks` generator for host JSON snippets. Hooks are opt-in
//! and default OFF: missing or unparseable config means disabled, and
//! every runtime path fails open (exit 0, empty stdout) so a hook never
//! blocks a host session.

use std::path::{Path, PathBuf};

/// Max hint chars sent to recall (hosts may pass long prompts).
const MAX_HINT_CHARS: usize = 2000;
/// Max inject output chars (host `additionalContext` budget).
const MAX_OUTPUT_CHARS: usize = 4000;
/// End-to-end hook budget: exceeding it fails open, never blocks a session.
const HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
    /// Existing file is not ours (no marker): left untouched.
    RefusedForeign,
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
    // Strip our managed marker line (JSON parser rejects comments).
    let body: String = raw
        .lines()
        .filter(|l| !l.starts_with("// ltmrs-hooks v"))
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    let value: serde_json::Value = match serde_json::from_str(&body) {
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

/// Run hook-inject: lexical recall against the managed daemon, fail-open.
/// Never blocks a session: any failure (disabled, unreachable, budget,
/// timeout) returns exit 0 with empty stdout plus one stderr line.
pub async fn run_hook_inject(home: Option<String>, input: &HookInjectInput) -> HookOutput {
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
    match tokio::time::timeout(HOOK_TIMEOUT, inject_inner(&home, input)).await {
        Ok(Ok(text)) => HookOutput {
            exit_code: 0,
            stdout: truncate_chars(&text, MAX_OUTPUT_CHARS),
        },
        Ok(Err(e)) => {
            eprintln!("ltmrs hook-inject: {e}");
            HookOutput::empty()
        }
        Err(_) => {
            eprintln!("ltmrs hook-inject: timed out after 5s");
            HookOutput::empty()
        }
    }
}

/// Attach to the RUNNING managed daemon over its socket (never spawn a
/// second daemon — the store lock is singleton). Returns a handshaken
/// frontend bound to ephemeral hook ids. Unreachable daemon = Err
/// (callers fail open).
async fn hook_frontend(home: &str) -> Result<crate::frontend::mcp::LtmrsFrontend, String> {
    use crate::frontend::mcp::{FrontendIdentity, LtmrsFrontend};
    use crate::frontend::serve::daemon::{connect_remote, daemon_socket_path};
    use crate::frontend::serve::{resolve_home, stdio_layout};
    use ltmrs_daemon::envelope::{HandshakeRequest, PROTOCOL_VERSION};
    use ltmrs_domain::id::{ChannelId, FrontendId, StoreGeneration};
    use uuid::Uuid;

    let base = resolve_home(Some(home.to_string())).map_err(|e| e.to_string())?;
    let layout = stdio_layout(&base);
    let socket = daemon_socket_path(&layout);
    let mut client = connect_remote(&socket.to_string_lossy())
        .await
        .map_err(|e| format!("daemon unavailable: {e}"))?;
    // The SAME ids must back the frontend below: envelopes carry the
    // frontend's stored ids, and a mismatch bricks the call
    // (frontend_mismatch is a hard identity error, not retryable).
    let frontend_id = FrontendId::new(Uuid::now_v7());
    let channel_id = ChannelId::new(Uuid::now_v7());
    client
        .handshake(&HandshakeRequest {
            protocol_version: PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id,
            channel_id,
            resume_retry_epoch: None,
        })
        .await
        .map_err(|e| format!("daemon handshake failed: {e}"))?;
    Ok(LtmrsFrontend::new(
        FrontendIdentity::new(frontend_id, channel_id),
        client,
    ))
}

/// Inject worker: lexical `semantic_search` (no embeddings, no network).
/// Unreachable daemon = fail-open upstream.
async fn inject_inner(home: &str, input: &HookInjectInput) -> Result<String, String> {
    use ltmrs_compat::lemma::tool_args::ToolArgs;
    use ltmrs_daemon::envelope::{DomainPayload, DomainRequest, IpcResult};

    let fe = hook_frontend(home).await?;
    let hint: String = input.query_hint.chars().take(MAX_HINT_CHARS).collect();
    let tool = ToolArgs::SemanticSearch(ltmrs_compat::lemma::tool_args::SemanticSearchArgs {
        query: hint,
        project: input.project.clone(),
        top_k: Some(5),
        offset: None,
        hybrid: Some(false),
        explain: false,
        response_format: None,
    });
    let resp = fe
        .call_daemon_tool(DomainRequest::ToolCall { tool }, input.project.is_some())
        .await
        .map_err(|e| format!("recall failed: {e}"))?;
    match resp.result {
        IpcResult::Success {
            payload: DomainPayload::ToolResult { text, is_error, .. },
            ..
        } if !is_error => Ok(text),
        IpcResult::Success { .. } => Err("recall returned an error result".to_string()),
        IpcResult::Error { message, .. } => Err(format!("recall refused: {message}")),
    }
}

/// Char-boundary truncation (byte slicing would panic on multi-byte text).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Run hook-extract: store transcript-tail findings, fail-open.
/// Exit 0 always; stdout empty (hosts ignore it).
pub async fn run_hook_extract(home: Option<String>, input: &HookExtractInput) -> HookOutput {
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
    match tokio::time::timeout(HOOK_TIMEOUT, extract_inner(&home, input)).await {
        Ok(Ok(())) => HookOutput::empty(),
        Ok(Err(e)) => {
            eprintln!("ltmrs hook-extract: {e}");
            HookOutput::empty()
        }
        Err(_) => {
            eprintln!("ltmrs hook-extract: timed out after 5s");
            HookOutput::empty()
        }
    }
}

/// Max transcript chars considered (tail only; hosts send bulk).
const MAX_TAIL_CHARS: usize = 8192;
/// Max candidate fragments per extract run.
const MAX_CANDIDATES: usize = 5;

/// Extract worker: tail the transcript, split durable-looking lines into
/// candidates, store each via `memory_add` (write-only-add; the server
/// Jaccard gate dedups reruns). Per-call failure is logged, never fatal.
async fn extract_inner(home: &str, input: &HookExtractInput) -> Result<(), String> {
    use ltmrs_compat::lemma::tool_args::{MemoryAddArgs, ToolArgs};
    use ltmrs_daemon::envelope::{DomainPayload, DomainRequest, IpcResult};

    let fe = hook_frontend(home).await?;
    let tail: String = input
        .transcript_tail
        .chars()
        .rev()
        .take(MAX_TAIL_CHARS)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let mut stored = 0;
    for candidate in split_candidates(&tail).into_iter().take(MAX_CANDIDATES) {
        let tool = ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: candidate.clone(),
            title: Some(derive_title(&candidate)),
            description: None,
            project: input.project.clone(),
            source: Some("hook-extract".to_string()),
            confirm: false,
            fragment_type: Some("pattern".to_string()),
            evidence: None,
        });
        match fe
            .call_daemon_tool(DomainRequest::ToolCall { tool }, input.project.is_some())
            .await
        {
            Ok(resp) => match resp.result {
                IpcResult::Success {
                    payload:
                        DomainPayload::ToolResult {
                            is_error: false, ..
                        },
                    ..
                } => stored += 1,
                other => eprintln!("ltmrs hook-extract: add refused: {other:?}"),
            },
            Err(e) => eprintln!("ltmrs hook-extract: add failed: {e}"),
        }
    }
    if stored == 0 {
        return Err("no candidates stored".to_string());
    }
    Ok(())
}

/// Split a transcript tail into durable-looking candidates.
fn split_candidates(tail: &str) -> Vec<String> {
    const MARKERS: &[&str] = &["DECISION:", "LESSON:", "FACT:", "NOTE:"];
    tail.lines()
        .map(str::trim)
        .filter(|l| l.len() >= 20 && l.len() <= 2000)
        .filter(|l| MARKERS.iter().any(|m| l.starts_with(m)) || l.len() >= 60)
        .filter(|l| !looks_secret(l))
        .map(|l| {
            if l.len() > 500 {
                l.chars().take(500).collect()
            } else {
                l.to_string()
            }
        })
        .collect()
}

/// Best-effort secret-carrier detection (daemon redaction authoritative).
fn looks_secret(line: &str) -> bool {
    let lower = line.to_lowercase();
    const NEEDLES: &[&str] = &[
        "sk-",
        "api_key",
        "apikey",
        "api key",
        "secret",
        "token",
        "password",
        "passwd",
        "bearer ",
        "private key",
    ];
    NEEDLES.iter().any(|n| lower.contains(n))
}

/// Derive a short title: first 40 chars, char-boundary safe.
fn derive_title(candidate: &str) -> String {
    const MAX: usize = 40;
    if candidate.chars().count() <= MAX {
        return candidate.to_string();
    }
    candidate.chars().take(MAX).collect::<String>() + "..."
}

/// Install hooks: write `<home>/.ltmrs/hooks.json` (`{"enabled":true}`)
/// atomically (temp file + rename). Refuses on unmanaged content.
pub fn install_hooks(home: &Path, _exe: &Path) -> Result<InstallOutcome, InstallError> {
    let path = hooks_config_path(home);
    let current = std::fs::read_to_string(&path).ok();
    match current.as_deref() {
        None => write_atomic(&path, &render_config(true)).map(|()| InstallOutcome::Installed),
        Some(text) if is_managed_config(text) => {
            if load_hooks_config(home).enabled {
                Ok(InstallOutcome::AlreadyCurrent)
            } else {
                write_atomic(&path, &render_config(true)).map(|()| InstallOutcome::Installed)
            }
        }
        Some(_) => Ok(InstallOutcome::RefusedForeign),
    }
}

/// Marker line identifying our managed config (versioned for updates).
fn config_marker() -> String {
    format!("// ltmrs-hooks v{HOOKS_VERSION}\n")
}

/// Render managed config content.
fn render_config(enabled: bool) -> String {
    format!("{}{{\"enabled\":{enabled}}}\n", config_marker())
}

/// Our managed file starts with the marker line.
fn is_managed_config(text: &str) -> bool {
    text.lines()
        .next()
        .is_some_and(|l| l == config_marker().trim_end())
}

/// Atomic write: temp file in the target directory, then rename.
fn write_atomic(path: &Path, content: &str) -> Result<(), InstallError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| InstallError::Io(format!("mkdir {}: {e}", parent.display())))?;
    }
    let tmp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&tmp, content)
        .map_err(|e| InstallError::Io(format!("write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| InstallError::Io(format!("rename {}: {e}", path.display())))?;
    Ok(())
}

/// Host JSON snippet: the hooks block the host pastes into its settings.
/// Only `claude-code` ships a verified template; others stay
/// unknown-host until a host run proves them.
pub fn hook_snippet(host: &str, exe: &Path) -> Result<String, HooksError> {
    let exe = exe.display().to_string();
    match host {
        "claude-code" => Ok(format!(
            "{{\n  \"hooks\": {{\n    \"SessionStart\": [\n      {{ \"matcher\": \"\", \"hooks\": [{{ \"type\": \"command\", \"command\": \"{exe} hook-inject\" }}] }}\n    ],\n    \"Stop\": [\n      {{ \"matcher\": \"\", \"hooks\": [{{ \"type\": \"command\", \"command\": \"{exe} hook-extract\" }}] }}\n    ]\n  }}\n}}"
        )),
        other => Err(HooksError::UnknownHost(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Slice B Phase 3 RED: extract stores transcript findings as
    /// memories (write-only-add; rerun adds no duplicates; secrets
    /// never stored).
    #[tokio::test]
    async fn hook_extract_stores_findings_live() {
        use crate::frontend::serve::daemon::start_local_daemon;
        use crate::frontend::serve::{resolve_home, stdio_layout};

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap().to_string();
        let home_path = std::path::Path::new(&home);
        std::fs::create_dir_all(home_path.join(".ltmrs")).unwrap();
        std::fs::write(
            home_path.join(".ltmrs").join("hooks.json"),
            "{\"enabled\":true}",
        )
        .unwrap();

        let base = resolve_home(Some(home.clone())).unwrap();
        let layout = stdio_layout(&base);
        let (mut daemon, _client) = start_local_daemon(&layout).await.unwrap();

        let transcript = [
            "## Session notes",
            "",
            "DECISION: Shop Ledger uses Fjall for canonical storage.",
            "Some chatter here.",
            "LESSON: Lance projections rebuild after restore.",
            "",
            "apikey material must never land: sk-test-0000000000000000000000000000000000000000",
        ]
        .join("\n");
        let out = run_hook_extract(
            Some(home.clone()),
            &HookExtractInput {
                transcript_tail: transcript,
                project: None,
            },
        )
        .await;
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.is_empty());

        // Findings must be recallable via inject. Assert on a STORED
        // marker, not the echoed query: an empty store replies
        // `No semantically similar memories found for: "<hint>"` which
        // contains the hint words — only a result row proves storage.
        let found = run_hook_inject(
            Some(home.clone()),
            &HookInjectInput {
                query_hint: "Fjall canonical".to_string(),
                project: None,
            },
        )
        .await;
        assert!(
            found.stdout.contains("[m"),
            "extract must store a result row (not just echo the hint), got: {}",
            found.stdout.chars().take(300).collect::<String>()
        );
        // Rerun idempotency: the server Jaccard gate dedups, so a second
        // extract of the same tail must not grow the store.
        let before = count_memories(&home).await;
        let rerun = run_hook_extract(
            Some(home.clone()),
            &HookExtractInput {
                transcript_tail: "DECISION: Shop Ledger uses Fjall for canonical storage. LESSON: Lance projections rebuild after restore.".to_string(),
                project: None,
            },
        )
        .await;
        assert_eq!(rerun.exit_code, 0);
        let after = count_memories(&home).await;
        assert_eq!(
            before, after,
            "rerun must add no duplicates (server Jaccard gate)"
        );
        // Secret exclusion: the transcript's sk-test line must never be
        // stored (local pre-filter; daemon redaction authoritative).
        let secret_probe = run_hook_inject(
            Some(home.clone()),
            &HookInjectInput {
                query_hint: "sk-test-0000000000000000000000000000000000000000".to_string(),
                project: None,
            },
        )
        .await;
        assert!(
            !secret_probe.stdout.contains("[m"),
            "secret material must never be stored, got: {}",
            secret_probe.stdout.chars().take(300).collect::<String>()
        );
        daemon.shutdown();
    }

    /// Count memories via inject probe (empty hint lists; `[m` marks rows).
    async fn count_memories(home: &str) -> usize {
        let found = run_hook_inject(
            Some(home.to_string()),
            &HookInjectInput {
                query_hint: String::new(),
                project: None,
            },
        )
        .await;
        found.stdout.matches("[m").count()
    }

    /// Slice B Phase 2 RED: inject against a live seeded daemon returns
    /// the matching title (lexical recall, truncated to budget).
    #[tokio::test]
    async fn hook_inject_returns_matching_recall_live() {
        use crate::frontend::mcp::{FrontendIdentity, LtmrsFrontend};
        use crate::frontend::serve::daemon::start_local_daemon;
        use crate::frontend::serve::{resolve_home, stdio_layout};
        use ltmrs_compat::lemma::tool_args::{MemoryAddArgs, ToolArgs};
        use ltmrs_daemon::envelope::{DomainPayload, DomainRequest, IpcResult};
        use ltmrs_domain::id::{ChannelId, FrontendId};
        use uuid::Uuid;

        // Managed home + hooks enabled.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap().to_string();
        let home_path = std::path::Path::new(&home);
        std::fs::create_dir_all(home_path.join(".ltmrs")).unwrap();
        std::fs::write(
            home_path.join(".ltmrs").join("hooks.json"),
            "{\"enabled\":true}",
        )
        .unwrap();

        // Seed one memory through a frontend bridged to a live daemon.
        let base = resolve_home(Some(home.clone())).unwrap();
        let layout = stdio_layout(&base);
        let (mut daemon, client) = start_local_daemon(&layout).await.unwrap();
        let fe = LtmrsFrontend::new(
            FrontendIdentity::new(
                FrontendId::new(Uuid::from_u128(21)),
                ChannelId::new(Uuid::from_u128(22)),
            ),
            client,
        );
        let seed = ToolArgs::MemoryAdd(MemoryAddArgs {
            fragment: "## Shop Ledger\n\n### Context\nDurable shop fact.".to_string(),
            title: Some("Shop Ledger".to_string()),
            description: None,
            project: None,
            source: None,
            confirm: true,
            fragment_type: None,
            evidence: None,
        });
        let resp = fe
            .call_daemon_tool(DomainRequest::ToolCall { tool: seed }, false)
            .await
            .unwrap();
        assert!(
            matches!(
                resp.result,
                IpcResult::Success {
                    payload: DomainPayload::ToolResult {
                        is_error: false,
                        ..
                    },
                    ..
                }
            ),
            "seed must succeed, got: {:?}",
            resp.result
        );
        // Inject must surface the seeded title (the daemon stays alive
        // here — inject attaches over its socket; shutdown after asserts).
        let out = run_hook_inject(
            Some(home),
            &HookInjectInput {
                query_hint: "shop ledger".to_string(),
                project: None,
            },
        )
        .await;
        assert_eq!(out.exit_code, 0);
        assert!(
            out.stdout.contains("Shop Ledger"),
            "inject must surface seeded title, got: {}",
            out.stdout.chars().take(300).collect::<String>()
        );
        daemon.shutdown();
    }

    /// Slice B Phase 4 RED: install writes hooks.json; rerun is
    /// AlreadyCurrent; foreign content is refused.
    #[test]
    fn install_hooks_writes_and_refuses_foreign() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let exe = std::path::Path::new("/fake/ltmrs");
        // Fresh install.
        let first = install_hooks(home, exe).unwrap();
        assert_eq!(first, InstallOutcome::Installed);
        let raw = std::fs::read_to_string(hooks_config_path(home)).unwrap();
        assert!(raw.contains("\"enabled\""));
        assert!(load_hooks_config(home).enabled);
        // Rerun: already current.
        let second = install_hooks(home, exe).unwrap();
        assert_eq!(second, InstallOutcome::AlreadyCurrent);
        // Foreign content: refused.
        std::fs::write(hooks_config_path(home), "{\"enabled\":false,\"custom\":1}").unwrap();
        let third = install_hooks(home, exe).unwrap();
        assert_eq!(third, InstallOutcome::RefusedForeign);
    }

    /// Slice B Phase 4: snippet renders for claude-code, unknown host errors.
    #[test]
    fn hook_snippet_renders_claude_code() {
        let out = hook_snippet("claude-code", std::path::Path::new("/bin/ltmrs")).unwrap();
        assert!(out.contains("hook-inject"), "got: {out}");
        assert!(out.contains("hook-extract"), "got: {out}");
        assert!(out.contains("/bin/ltmrs"), "got: {out}");
        assert!(hook_snippet("nope", std::path::Path::new("/bin/ltmrs")).is_err());
    }

    /// Slice B Phase 2 RED: inject fails open on missing home (no daemon).
    #[tokio::test]
    async fn hook_inject_fails_open_without_home() {
        let out = run_hook_inject(
            Some("/nonexistent-home-xyz".to_string()),
            &HookInjectInput {
                query_hint: "shop".to_string(),
                project: None,
            },
        )
        .await;
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.is_empty());
    }
}
