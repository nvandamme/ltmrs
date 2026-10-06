//! Command-line surface (WP-10 task 1; T-CLI-01).
//!
//! Parses the native commands plus the exact claimed legacy aliases from the
//! Lemma 0.21.0 baseline (`-lib`, `-vis`, `--install-skill`, `-h`, `-V`).
//! Every alias, default, exit code and stdout/stderr rule is pinned by tests
//! below. Arms whose execution lands in later slices fail explicitly
//! (`Unimplemented`) — never a silent success.

/// A parsed command: what to do, not how. Execution lives in `main` (help,
/// version, dispatch) except `run_library`, which is testable here.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Serve MCP over stdio (default, no arguments).
    Stdio { socket: Option<String> },
    /// Run the shared daemon: bare form ensures it is serving and exits,
    /// `--foreground` serves inline until idle/SIGTERM (supervisors).
    Daemon {
        foreground: bool,
        idle_ms: Option<u64>,
    },
    /// Print a knowledge-base snapshot from a store.
    Library { store: Option<String> },
    /// Run the visualizer (wired in a later slice).
    Visualize { foreground: bool, port: Option<u16> },
    /// Install/update the managed skill (wired in a later slice).
    InstallSkill,
    /// Install the opt-in legacy `lemma` executable shim.
    InstallShim,
    /// Download + verify the pinned E5 embedding artifacts into the managed home.
    ProvisionModels,
    /// Print help to stdout.
    Help,
    /// Print the version to stdout.
    Version,
}

/// CLI failure with its process exit code.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum CliError {
    /// Usage/argument error (flags, values, combinations).
    #[error("{0}")]
    Usage(String),
    /// Parsed but not yet implemented (later slice, never silent).
    #[error("not implemented yet: {0}")]
    Unimplemented(&'static str),
    /// Runtime failure (IO, store open, rendering).
    #[error("{0}")]
    Runtime(String),
}

impl CliError {
    /// Process exit code: 0 success, 1 runtime failure, 2 usage error,
    /// 3 parsed-but-unimplemented slice.
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::Usage(_) => 2,
            CliError::Unimplemented(_) => 3,
            CliError::Runtime(_) => 1,
        }
    }
}

/// Parse an argument vector (excluding the program name).
/// Two phases: `-h`/`-V` short-circuit anywhere; otherwise collect every
/// flag, then reject conflicting commands and mismatched options
/// regardless of order (no silent precedence, no swallowed flags).
pub fn parse_args(argv: &[String]) -> Result<Command, CliError> {
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(Command::Help);
    }
    if argv.iter().any(|a| a == "-V" || a == "--version") {
        return Ok(Command::Version);
    }

    #[derive(PartialEq)]
    enum Selected {
        Library,
        Visualize,
        InstallSkill,
        InstallShim,
        ProvisionModels,
        Daemon,
    }
    let mut selected: Option<Selected> = None;
    let mut select = |next: Selected| -> Result<(), CliError> {
        if selected.as_ref().is_some_and(|s| *s != next) {
            return Err(CliError::Usage(
                "commands are mutually exclusive; pick one".to_string(),
            ));
        }
        selected = Some(next);
        Ok(())
    };

    let mut args = argv.iter().peekable();
    let mut store: Option<String> = None;
    let mut socket: Option<String> = None;
    let mut foreground = false;
    let mut daemon_foreground = false;
    let mut daemon_idle_ms: Option<u64> = None;
    let mut port: Option<u16> = None;
    let mut positional: Vec<String> = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-lib" | "--library" => select(Selected::Library)?,
            "-vis" | "--visualize" => select(Selected::Visualize)?,
            "--install-skill" => select(Selected::InstallSkill)?,
            "--install-shim" => select(Selected::InstallShim)?,
            "--provision-models" => select(Selected::ProvisionModels)?,
            "--daemon" => select(Selected::Daemon)?,
            "--foreground" => daemon_foreground = true,
            "--daemon-idle-ms" => {
                let raw = args.next().ok_or_else(|| {
                    CliError::Usage("--daemon-idle-ms requires a value in milliseconds".to_string())
                })?;
                daemon_idle_ms = Some(parse_daemon_idle_ms(raw)?);
            }
            "--fg" => foreground = true,
            "-p" | "--port" => {
                let raw = args.next().ok_or_else(|| {
                    CliError::Usage("-p/--port requires a value (1-65535)".to_string())
                })?;
                port = Some(parse_port(raw)?);
            }
            "--store" => {
                let raw = args
                    .next()
                    .ok_or_else(|| CliError::Usage("--store requires a path".to_string()))?;
                store = Some(reject_flag_value("--store", raw)?);
            }
            "--socket" => {
                let raw = args
                    .next()
                    .ok_or_else(|| CliError::Usage("--socket requires a path".to_string()))?;
                socket = Some(reject_flag_value("--socket", raw)?);
            }
            // Daemon subcommand (Part I §7): `ltmrs daemon [--foreground]`.
            "daemon" => select(Selected::Daemon)?,
            other if other.starts_with('-') => {
                return Err(CliError::Usage(format!("unknown flag: {other}")));
            }
            other => positional.push(other.to_string()),
        }
    }
    if !positional.is_empty() {
        return Err(CliError::Usage(format!(
            "unexpected arguments: {}",
            positional.join(" ")
        )));
    }
    if selected.as_ref().is_some_and(|s| *s != Selected::Daemon)
        && (daemon_foreground || daemon_idle_ms.is_some())
    {
        return Err(CliError::Usage(
            "--foreground/--daemon-idle-ms require daemon".to_string(),
        ));
    }
    match selected {
        Some(Selected::Daemon) => {
            if store.is_some() || socket.is_some() || foreground || port.is_some() {
                return Err(CliError::Usage(
                    "daemon takes only --foreground and --daemon-idle-ms".to_string(),
                ));
            }
            Ok(Command::Daemon {
                foreground: daemon_foreground,
                idle_ms: daemon_idle_ms,
            })
        }
        Some(Selected::Library) => {
            if socket.is_some() || foreground || port.is_some() {
                return Err(CliError::Usage("-lib takes only --store".to_string()));
            }
            Ok(Command::Library { store })
        }
        Some(Selected::Visualize) => {
            if store.is_some() || socket.is_some() {
                return Err(CliError::Usage(
                    "-vis takes only --fg and -p/--port".to_string(),
                ));
            }
            Ok(Command::Visualize { foreground, port })
        }
        Some(Selected::InstallSkill) => {
            if store.is_some() || socket.is_some() || foreground || port.is_some() {
                return Err(CliError::Usage(
                    "--install-skill takes no options".to_string(),
                ));
            }
            Ok(Command::InstallSkill)
        }
        Some(Selected::InstallShim) => {
            if store.is_some() || socket.is_some() || foreground || port.is_some() {
                return Err(CliError::Usage(
                    "--install-shim takes no options".to_string(),
                ));
            }
            Ok(Command::InstallShim)
        }
        Some(Selected::ProvisionModels) => {
            if store.is_some() || socket.is_some() || foreground || port.is_some() {
                return Err(CliError::Usage(
                    "--provision-models takes no options".to_string(),
                ));
            }
            Ok(Command::ProvisionModels)
        }
        None => {
            if store.is_some() {
                return Err(CliError::Usage(
                    "--store requires -lib/--library".to_string(),
                ));
            }
            if foreground || port.is_some() {
                return Err(CliError::Usage(
                    "--fg/--port require -vis/--visualize".to_string(),
                ));
            }
            Ok(Command::Stdio { socket })
        }
    }
}

/// Reject flag-like option values (a swallowed flag is never a path).
fn reject_flag_value(flag: &str, raw: &str) -> Result<String, CliError> {
    if raw.starts_with('-') {
        return Err(CliError::Usage(format!(
            "{flag} requires a path, got flag-like value: {raw}"
        )));
    }
    Ok(raw.to_string())
}

fn parse_port(raw: &str) -> Result<u16, CliError> {
    let port: u32 = raw
        .parse()
        .map_err(|_| CliError::Usage(format!("invalid port: {raw}")))?;
    if !(1..=65535).contains(&port) {
        return Err(CliError::Usage(format!("port out of range 1-65535: {raw}")));
    }
    Ok(port as u16)
}

/// Default daemon idle shutdown: 60s with no connections, then exit.
/// Zero means serve forever (matches DaemonConfig idle convention).
pub const DEFAULT_DAEMON_IDLE_MS: u64 = 60_000;

/// Parse `--daemon-idle-ms` (nonnegative integer milliseconds).
fn parse_daemon_idle_ms(raw: &str) -> Result<u64, CliError> {
    raw.parse().map_err(|_| {
        CliError::Usage(format!(
            "invalid --daemon-idle-ms (nonnegative integer milliseconds): {raw}"
        ))
    })
}

/// Help text: documents every flag (tested to stay complete).
pub fn help_text() -> String {
    "\
ltmrs — local MCP memory service

Usage: ltmrs [command] [options]

Commands (default with no arguments: stdio):
  (no args)               Serve MCP over stdio [--socket PATH]
  daemon                  Run the shared daemon: ensure serving and exit,
                          or serve inline with --foreground
  -lib, --library         Print a knowledge-base snapshot [--store PATH]
  -vis, --visualize       Run the visualizer [--fg] [-p PORT | --port PORT]
  --install-skill         Install/update the managed skill
  --install-shim          Install the opt-in legacy `lemma` shim
  --provision-models      Download + verify pinned E5 embedding artifacts
  -h, --help              Show this help
  -V, --version           Print the version

Options:
  --store PATH            Canonical store directory (for -lib)
  --socket PATH           Daemon socket path (for stdio mode)
  --foreground            Serve the daemon inline (with daemon)
  --daemon-idle-ms MS    Daemon idle shutdown in ms, 0 = forever
                          (with daemon; default 60000)
  --fg                    Visualizer in foreground (with -vis)
  -p PORT, --port PORT    Visualizer port 1-65535 (with -vis)

Exit codes: 0 success, 1 runtime failure, 2 usage error,
3 parsed but not yet implemented.

Commands are mutually exclusive; -h/--help and -V/--version win
anywhere. Repeatable options use the last value.
"
    .to_string()
}

/// Version text carrying the package version.
pub fn version_text() -> String {
    format!("ltmrs {}", env!("CARGO_PKG_VERSION"))
}

/// Print a knowledge-base snapshot of the store at `path`./// `None` is a usage error: ltmrs invents no default home for your data.
/// A missing path is also a usage error (a read must not create stores).
pub fn run_library(store: Option<String>) -> Result<String, CliError> {
    let path = store.ok_or_else(|| CliError::Usage("-lib requires --store <path>".to_string()))?;
    if !std::path::Path::new(&path).exists() {
        return Err(CliError::Usage(format!("no store at {path}")));
    }
    let repo = crate::service::repository::CanonicalRepository::open(&path)
        .map_err(|e| CliError::Runtime(e.message))?;
    let export = repo
        .export_snapshot()
        .map_err(|e| CliError::Runtime(e.message))?;
    let mut out = String::from("## Library Snapshot\n");
    out.push_str(&format!("Memories: {}\n", export.memories.len()));
    out.push_str(&format!("Relations: {}\n", export.relations.len()));
    for m in &export.memories {
        out.push_str(&format!("- [{}] {}\n", m.id, m.title));
    }
    Ok(out)
}

/// Install the managed native skill under `home` (`None`/empty = missing HOME).
/// Returns the human-readable result; refusals are runtime errors (the
/// arguments parsed fine — the environment or existing content blocks).
/// Foreign content and user-modified current files name the path and how
/// to proceed without data loss (back it up first — there is no --force).
pub fn install_skill_command(home: Option<String>) -> Result<String, CliError> {
    use crate::skills::installer::{InstallOutcome, install_native_skill};

    let home = home
        .filter(|h| !h.is_empty())
        .ok_or_else(|| CliError::Runtime("HOME is not set".to_string()))?;
    let path = crate::skills::installer::skill_path(std::path::Path::new(&home), "ltmrs");
    match install_native_skill(std::path::Path::new(&home))
        .map_err(|e| CliError::Runtime(format!("skill install failed: {e}")))?
    {
        // The recipe count keeps "installed" and "host loaded it" as separate
        // evidence fields (T-HOST-01): installation never claims host uptake.
        InstallOutcome::Installed => Ok(format!(
            "Installed skill at {}; host recipes: {} documented",
            path.display(),
            crate::skills::hosts::HOST_RECIPES.len()
        )),
        InstallOutcome::AlreadyCurrent => Ok(format!(
            "Skill already current at {}; host recipes: {} documented",
            path.display(),
            crate::skills::hosts::HOST_RECIPES.len()
        )),
        InstallOutcome::RefusedForeign => Err(CliError::Runtime(format!(
            "refused: {} is not managed by ltmrs; back it up, then remove it to install",
            path.display()
        ))),
        InstallOutcome::RefusedModified => Err(CliError::Runtime(format!(
            "refused: {} carries your edits on the current version; back it up, then remove it to install",
            path.display()
        ))),
    }
}

/// Install the opt-in legacy `lemma` shim under `home` (`None`/empty =
/// missing HOME). Returns the human-readable result: the outcome plus any
/// PATH collisions (a collision never blocks, but is always reported).
pub fn install_shim_command(home: Option<String>) -> Result<String, CliError> {
    use crate::skills::shim::{ShimOutcome, install_shim, shim_path};

    let home = home
        .filter(|h| !h.is_empty())
        .ok_or_else(|| CliError::Runtime("HOME is not set".to_string()))?;
    let exe = std::env::current_exe()
        .map_err(|e| CliError::Runtime(format!("cannot locate ltmrs binary: {e}")))?;
    let path = shim_path(std::path::Path::new(&home));
    let report = install_shim(std::path::Path::new(&home), &exe, None)
        .map_err(|e| CliError::Runtime(e.to_string()))?;
    let mut out = match report.outcome {
        ShimOutcome::Installed => {
            format!(
                "Installed legacy shim at {} (-> {})",
                path.display(),
                exe.display()
            )
        }
        ShimOutcome::AlreadyCurrent => {
            format!("Legacy shim already current at {}", path.display())
        }
    };
    if !report.collisions.is_empty() {
        let list = report
            .collisions
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "; warning: PATH already provides `lemma` at: {list} (the shim may be shadowed)"
        ));
    }
    Ok(out)
}

/// Download + verify the pinned E5 embedding artifacts into the managed
/// home's models directory (`None`/empty = missing HOME). Existing files
/// are re-verified, not re-downloaded; every digest must match or the
/// provision fails loudly (no partial model left enabled — the daemon only
/// enables dense when the full set verifies). Returns the human-readable
/// result: artifact id + revision + verified file count.
pub async fn provision_models_command(home: Option<String>) -> Result<String, CliError> {
    use crate::embeddings::artifacts::{ArtifactCache, HttpClient};
    use crate::embeddings::e5_small::E5SmallAdapter;
    use crate::embeddings::manifest::{E5_SMALL_ID, E5_SMALL_REVISION, e5_small_artifact};

    let home = home
        .filter(|h| !h.is_empty())
        .ok_or_else(|| CliError::Runtime("HOME is not set".to_string()))?;
    let models = std::path::Path::new(&home)
        .join(crate::frontend::serve::MANAGED_HOME_DIR)
        .join(crate::frontend::serve::MODELS_DIR_NAME);
    let cache = ArtifactCache::new(&models);
    // Bound the whole operation: the default client has no timeout, so a
    // stalled connection would hang provisioning forever instead of failing
    // loudly. Ten minutes per file is generous (the pinned set is ~120MB).
    let client: HttpClient = std::sync::Arc::new(
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(600))
            .build()
            .map_err(|e| CliError::Runtime(format!("cannot build network client: {e}")))?,
    );
    let _loaded = E5SmallAdapter::fetch_and_load(&cache, Some(&client))
        .await
        .map_err(|e| {
            CliError::Runtime(format!(
                "model provisioning failed: {e} (a digest mismatch fails closed: \
                 nothing half-verified is served; if local corruption is suspected, \
                 remove {} and retry)",
                models.display()
            ))
        })?;
    Ok(format!(
        "Provisioned {E5_SMALL_ID} revision {E5_SMALL_REVISION} at {} ({} files verified)",
        models.display(),
        e5_small_artifact().digests.len(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, CliError> {
        parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    /// No arguments starts stdio mode (upstream default).
    #[test]
    fn no_args_starts_stdio() {
        assert!(matches!(parse(&[]).unwrap(), Command::Stdio { .. }));
    }

    /// Every documented alias selects the same command.
    #[test]
    fn all_aliases_map() {
        assert!(matches!(parse(&["-h"]).unwrap(), Command::Help));
        assert!(matches!(parse(&["--help"]).unwrap(), Command::Help));
        assert!(matches!(parse(&["-V"]).unwrap(), Command::Version));
        assert!(matches!(parse(&["--version"]).unwrap(), Command::Version));
        assert!(matches!(parse(&["-lib"]).unwrap(), Command::Library { .. }));
        assert!(matches!(
            parse(&["--library"]).unwrap(),
            Command::Library { .. }
        ));
        assert!(matches!(
            parse(&["-vis"]).unwrap(),
            Command::Visualize { .. }
        ));
        assert!(matches!(
            parse(&["--visualize"]).unwrap(),
            Command::Visualize { .. }
        ));
        assert!(matches!(
            parse(&["--install-skill"]).unwrap(),
            Command::InstallSkill
        ));
    }

    /// Visualizer combos: foreground flag and validated port.
    #[test]
    fn visualizer_combos() {
        let Command::Visualize { foreground, port } = parse(&["-vis", "--fg"]).unwrap() else {
            panic!("expected Visualize");
        };
        assert!(foreground);
        assert_eq!(port, None);
        let Command::Visualize { foreground, port } =
            parse(&["--visualize", "-p", "8080"]).unwrap()
        else {
            panic!("expected Visualize");
        };
        assert!(!foreground);
        assert_eq!(port, Some(8080));
        let Command::Visualize { port, .. } = parse(&["-vis", "--port", "1"]).unwrap() else {
            panic!("expected Visualize");
        };
        assert_eq!(port, Some(1));
    }

    /// Port must be 1-65535 and numeric.
    #[test]
    fn port_validation() {
        assert!(parse(&["-vis", "-p", "0"]).is_err());
        assert!(parse(&["-vis", "-p", "65536"]).is_err());
        assert!(parse(&["-vis", "-p", "abc"]).is_err());
        assert!(parse(&["-vis", "-p", "65535"]).is_ok());
        // A port without -vis is a usage error, not silently ignored.
        assert!(parse(&["-p", "8080"]).is_err());
        assert!(parse(&["--fg"]).is_err());
    }

    /// Unknown flags and stray positionals are usage errors.
    #[test]
    fn unknown_flags_and_positionals_rejected() {
        assert!(parse(&["--frobnicate"]).is_err());
        assert!(parse(&["-x"]).is_err());
        assert!(parse(&["extra-positional"]).is_err());
        assert!(parse(&["-lib", "extra"]).is_err());
    }

    /// Conflicting top-level commands are usage errors in either order —
    /// never silent precedence.
    #[test]
    fn conflicting_commands_rejected() {
        assert!(parse(&["-vis", "-lib"]).is_err());
        assert!(parse(&["-lib", "--install-skill"]).is_err());
        assert!(parse(&["-lib", "--install-shim"]).is_err());
        assert!(parse(&["--install-shim", "--install-skill"]).is_err());
        assert!(parse(&["--install-skill", "-vis"]).is_err());
        assert!(parse(&["-vis", "--install-skill"]).is_err());
        assert!(parse(&["-vis", "--fg", "-p", "1", "--install-skill"]).is_err());
    }

    /// Options bind to their command: mismatches are usage errors.
    #[test]
    fn options_bound_to_commands() {
        assert!(parse(&["--store", "/tmp/s"]).is_err());
        assert!(parse(&["-vis", "--store", "/tmp/s"]).is_err());
        assert!(parse(&["-lib", "--socket", "/tmp/d.sock"]).is_err());
        assert!(parse(&["--install-skill", "--store", "/tmp/s"]).is_err());
        assert!(parse(&["--install-shim", "-p", "8080"]).is_err());
        assert!(parse(&["--provision-models", "--store", "/tmp/s"]).is_err());
    }

    /// --provision-models downloads the pinned E5 artifacts into the managed home.
    #[test]
    fn provision_models_alias() {
        assert!(matches!(
            parse(&["--provision-models"]).unwrap(),
            Command::ProvisionModels
        ));
    }

    /// Provisioning conflicts with every other command and takes no options —
    /// never silent precedence, never swallowed flags.
    #[test]
    fn provision_models_conflicts_and_no_options() {
        assert!(parse(&["--provision-models", "-lib"]).is_err());
        assert!(parse(&["-vis", "--provision-models"]).is_err());
        assert!(parse(&["--provision-models", "--install-skill"]).is_err());
        assert!(parse(&["--provision-models", "--install-shim"]).is_err());
        assert!(parse(&["--provision-models", "-p", "1"]).is_err());
        assert!(parse(&["--provision-models", "--socket", "/tmp/d.sock"]).is_err());
    }

    /// Provisioning needs a home: no invented model location (offline-safe check).
    #[tokio::test]
    async fn provision_models_requires_home() {
        let err = provision_models_command(None).await.unwrap_err();
        assert!(matches!(err, CliError::Runtime(_)), "got: {err}");
        assert!(err.to_string().contains("HOME"), "got: {err}");
    }

    /// Flag-like option values are rejected, never swallowed as paths.
    #[test]
    fn flag_like_values_rejected() {
        assert!(parse(&["--store", "-lib"]).is_err());
        assert!(parse(&["--socket", "-vis"]).is_err());
        assert!(parse(&["-lib", "--store", "--fg"]).is_err());
    }

    /// Repeatable options use the last value (pinned, conventional).
    #[test]
    fn repeated_options_last_wins() {
        let Command::Library { store } =
            parse(&["-lib", "--store", "/a", "--store", "/b"]).unwrap()
        else {
            panic!("expected Library");
        };
        assert_eq!(store.as_deref(), Some("/b"));
        let Command::Visualize { port, .. } = parse(&["-vis", "-p", "1", "-p", "2"]).unwrap()
        else {
            panic!("expected Visualize");
        };
        assert_eq!(port, Some(2));
    }

    /// Help/version short-circuit anywhere; help beats version.
    #[test]
    fn help_version_win_anywhere() {
        assert!(matches!(
            parse(&["--frobnicate", "-h"]).unwrap(),
            Command::Help
        ));
        assert!(matches!(
            parse(&["-vis", "--version"]).unwrap(),
            Command::Version
        ));
        assert!(matches!(parse(&["-h", "-V"]).unwrap(), Command::Help));
        assert!(matches!(parse(&["-V", "-h"]).unwrap(), Command::Help));
        // Even in value position: documented "win anywhere" semantics.
        assert!(matches!(
            parse(&["-lib", "--store", "-h"]).unwrap(),
            Command::Help
        ));
    }

    /// --store flows into the commands that need a store.
    #[test]
    fn store_option() {
        let Command::Library { store } = parse(&["-lib", "--store", "/tmp/s"]).unwrap() else {
            panic!("expected Library");
        };
        assert_eq!(store.as_deref(), Some("/tmp/s"));
        let Command::Stdio { socket } = parse(&["--socket", "/tmp/d.sock"]).unwrap() else {
            panic!("expected Stdio");
        };
        assert_eq!(socket.as_deref(), Some("/tmp/d.sock"));
    }

    /// Help text documents every flag; version carries the package version.
    #[test]
    fn help_and_version_text() {
        let help = help_text();
        for flag in [
            "-h",
            "--help",
            "-V",
            "--version",
            "-lib",
            "--library",
            "-vis",
            "--visualize",
            "--fg",
            "-p",
            "--port",
            "--install-skill",
            "--install-shim",
            "--provision-models",
            "--store",
            "--socket",
        ] {
            assert!(help.contains(flag), "help must document {flag}");
        }
        assert!(version_text().contains(env!("CARGO_PKG_VERSION")));
    }

    /// Exit codes: usage errors are 2, unimplemented slices are 3.
    #[test]
    fn exit_codes() {
        assert_eq!(CliError::Usage("x".into()).exit_code(), 2);
        assert_eq!(CliError::Unimplemented("x").exit_code(), 3);
        assert_eq!(CliError::Runtime("boom".into()).exit_code(), 1);
    }

    /// -lib without --store is a usage error (no invented default home).
    #[test]
    fn library_requires_store() {
        assert!(parse(&["-lib"]).is_ok());
        // Selection succeeds; execution validates the store below.
        assert_eq!(
            run_library(None).unwrap_err().to_string(),
            "-lib requires --store <path>"
        );
    }

    /// -lib on a real store prints its snapshot (empty store edge included).
    /// A missing path is a usage error, not an auto-created store.
    #[test]
    fn library_runs_on_real_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store").to_str().unwrap().to_string();
        assert_eq!(
            run_library(Some(path.clone())).unwrap_err().to_string(),
            format!("no store at {path}")
        );
        // Create the store, then the snapshot reads it.
        drop(crate::service::repository::CanonicalRepository::open(&path));
        let out = run_library(Some(path)).unwrap();
        assert!(out.contains("## Library Snapshot"));
        assert!(out.contains("Memories: 0"));
        // Unknown store paths fail as usage errors, not runtime errors.
        assert!(matches!(
            run_library(Some("/nonexistent-dir-xyz/store".to_string())),
            Err(CliError::Usage(_))
        ));
    }

    /// --install-skill installs idempotently under an explicit home.
    #[test]
    fn install_skill_command_installs_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap().to_string();
        let first = install_skill_command(Some(home.clone())).unwrap();
        assert!(
            first.contains("Installed"),
            "fresh install reports, got: {first}"
        );
        let second = install_skill_command(Some(home.clone())).unwrap();
        assert!(
            second.contains("current"),
            "reinstall is a no-op, got: {second}"
        );
        // The installed asset carries the ownership marker + content.
        let text = std::fs::read_to_string(
            dir.path()
                .join(".agents")
                .join("skills")
                .join("ltmrs")
                .join("SKILL.md"),
        )
        .unwrap();
        assert!(text.contains("ltmrs-skill"));
        assert!(text.contains("recall"));
    }

    /// Missing HOME and foreign content fail explicitly (exit 1 via Runtime).
    #[test]
    fn install_skill_command_refuses_cleanly() {
        assert!(matches!(
            install_skill_command(None),
            Err(CliError::Runtime(_))
        ));
        assert!(matches!(
            install_skill_command(Some(String::new())),
            Err(CliError::Runtime(_))
        ));
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap().to_string();
        let foreign = dir.path().join(".agents").join("skills").join("ltmrs");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("SKILL.md"), "# Mine\n").unwrap();
        let err = install_skill_command(Some(home)).unwrap_err();
        assert!(matches!(err, CliError::Runtime(_)));
        assert!(err.to_string().contains("refused"));
    }

    /// --install-shim selects the shim command and installs idempotently
    /// under an explicit home (missing HOME fails explicitly).
    #[test]
    fn install_shim_command_installs_idempotently() {
        assert_eq!(parse(&["--install-shim"]).unwrap(), Command::InstallShim);
        assert!(matches!(
            install_shim_command(None),
            Err(CliError::Runtime(_))
        ));
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap().to_string();
        let first = install_shim_command(Some(home.clone())).unwrap();
        assert!(
            first.contains("Installed"),
            "fresh install reports, got: {first}"
        );
        assert!(
            first.contains("lemma"),
            "names the legacy shim, got: {first}"
        );
        let second = install_shim_command(Some(home.clone())).unwrap();
        assert!(
            second.contains("current"),
            "reinstall is a no-op, got: {second}"
        );
        // The shim is a symlink to this very binary.
        let link = crate::skills::shim::shim_path(dir.path());
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            std::env::current_exe().unwrap()
        );
    }

    /// `daemon` runs the shared daemon process: bare form ensures it is
    /// serving and exits, `--foreground` serves inline (supervisors).
    #[test]
    fn daemon_command_parses() {
        assert_eq!(
            parse(&["daemon"]).unwrap(),
            Command::Daemon {
                foreground: false,
                idle_ms: None,
            }
        );
        assert_eq!(
            parse(&["daemon", "--foreground"]).unwrap(),
            Command::Daemon {
                foreground: true,
                idle_ms: None,
            }
        );
        assert_eq!(
            parse(&["daemon", "--daemon-idle-ms", "1500"]).unwrap(),
            Command::Daemon {
                foreground: false,
                idle_ms: Some(1500),
            }
        );
    }

    /// Daemon options bind to the daemon command; the daemon conflicts
    /// with every other command like the rest.
    #[test]
    fn daemon_options_and_conflicts() {
        assert!(parse(&["daemon", "--daemon-idle-ms"]).is_err());
        assert!(parse(&["daemon", "--daemon-idle-ms", "abc"]).is_err());
        // 0 = serve forever (matches DaemonConfig idle convention).
        assert_eq!(
            parse(&["daemon", "--daemon-idle-ms", "0"]).unwrap(),
            Command::Daemon {
                foreground: false,
                idle_ms: Some(0),
            }
        );
        assert!(parse(&["daemon", "-lib"]).is_err());
        assert!(parse(&["-lib", "daemon"]).is_err());
        assert!(parse(&["-vis", "--daemon"]).is_err());
        assert!(parse(&["--install-skill", "daemon"]).is_err());
        assert!(parse(&["daemon", "--socket", "/tmp/d.sock"]).is_err());
        assert!(parse(&["daemon", "--fg"]).is_err());
        assert!(parse(&["daemon", "extra-positional"]).is_err());
    }

    /// Help documents the daemon surface.
    #[test]
    fn help_documents_daemon() {
        let help = help_text();
        for flag in ["daemon", "--foreground", "--daemon-idle-ms"] {
            assert!(help.contains(flag), "help must document {flag}");
        }
    }
}
