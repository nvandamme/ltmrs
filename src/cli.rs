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
    /// Print a knowledge-base snapshot from a store.
    Library { store: Option<String> },
    /// Run the visualizer (wired in a later slice).
    Visualize { foreground: bool, port: Option<u16> },
    /// Install/update the managed skill (wired in a later slice).
    InstallSkill,
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
    let mut port: Option<u16> = None;
    let mut positional: Vec<String> = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-lib" | "--library" => select(Selected::Library)?,
            "-vis" | "--visualize" => select(Selected::Visualize)?,
            "--install-skill" => select(Selected::InstallSkill)?,
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
    match selected {
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

/// Help text: documents every flag (tested to stay complete).
pub fn help_text() -> String {
    "\
ltmrs — local MCP memory service

Usage: ltmrs [command] [options]

Commands (default with no arguments: stdio):
  (no args)               Serve MCP over stdio [--socket PATH]
  -lib, --library         Print a knowledge-base snapshot [--store PATH]
  -vis, --visualize       Run the visualizer [--fg] [-p PORT | --port PORT]
  --install-skill         Install/update the managed skill
  -h, --help              Show this help
  -V, --version           Print the version

Options:
  --store PATH            Canonical store directory (for -lib)
  --socket PATH           Daemon socket path (for stdio mode)
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

/// Print a knowledge-base snapshot of the store at `path`.
/// `None` is a usage error: ltmrs invents no default home for your data.
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
}
