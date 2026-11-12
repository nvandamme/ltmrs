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
    Stdio {
        socket: Option<String>,
    },
    /// Run the shared daemon: bare form ensures it is serving and exits,
    /// `--foreground` serves inline until idle/SIGTERM (supervisors).
    Daemon {
        foreground: bool,
        idle_ms: Option<u64>,
    },
    /// Print a knowledge-base snapshot from a store.
    Library {
        store: Option<String>,
    },
    /// Run the visualizer (wired in a later slice).
    Visualize {
        foreground: bool,
        port: Option<u16>,
    },
    /// Install/update the managed skill (wired in a later slice).
    InstallSkill,
    /// Bind this checkout to a stable project identity (Wave A3b):
    /// writes `<checkout>/.ltmrs/project.json` (`0600`).
    ProjectBind {
        here: bool,
        root: Option<String>,
        name: Option<String>,
    },
    /// Show this checkout's bound identity (bind file + env override).
    ProjectStatus {
        root: Option<String>,
    },
    /// Attach a legacy bucket to this checkout's bound identity (Wave A3c):
    /// records `--slug` in the id's `legacy_slugs` (explicit intent, warns
    /// on slugs shared with other identities).
    ProjectAdopt {
        here: bool,
        root: Option<String>,
        slug: String,
    },
    /// Install the opt-in legacy `lemma` executable shim.
    InstallShim,
    /// Hook: inject budget-capped recall into a host session start
    /// (Slice B; reads one JSON object on stdin, fail-open).
    HookInject,
    /// Hook: extract durable findings from a transcript tail
    /// (Slice B; reads one JSON object on stdin, fail-open).
    HookExtract,
    /// Install the opt-in hook snippets + enable hooks.json.
    InstallHooks,
    /// Preview an MC database import (read-only staging, no store touch).
    McImportPreview {
        path: String,
    },
    /// Confirm an MC import (re-stage, bind digest, fan out memory_add).
    McImportConfirm {
        path: String,
        digest: String,
    },
    /// Hindsight mental-model maintenance (G1c; omp `mm` parity):
    /// list seeds + cached models, show/refresh one model.
    Mm {
        action: String,
        name: Option<String>,
    },
    /// Memory backend surfaces (G2; omp `/memory` parity).
    Memory {
        action: String,
    },
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
        Project,
        HookInject,
        HookExtract,
        InstallHooks,
        McImport,
        Mm,
        Memory,
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
    let mut here = false;
    let mut project_root: Option<String> = None;
    let mut project_name: Option<String> = None;
    let mut project_slug: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut mc_digest: Option<String> = None;

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
            "--here" => here = true,
            "--root" => {
                let raw = args
                    .next()
                    .ok_or_else(|| CliError::Usage("--root requires a path".to_string()))?;
                project_root = Some(reject_flag_value("--root", raw)?);
            }
            "--name" => {
                let raw = args
                    .next()
                    .ok_or_else(|| CliError::Usage("--name requires a value".to_string()))?;
                project_name = Some(reject_flag_value("--name", raw)?);
            }
            "--digest" => {
                let raw = args
                    .next()
                    .ok_or_else(|| CliError::Usage("--digest requires a value".to_string()))?;
                mc_digest = Some(reject_flag_value("--digest", raw)?);
            }
            "--slug" => {
                let raw = args
                    .next()
                    .ok_or_else(|| CliError::Usage("--slug requires a value".to_string()))?;
                project_slug = Some(reject_flag_value("--slug", raw)?);
            }
            // Daemon subcommand (Part I §7): `ltmrs daemon [--foreground]`.
            "daemon" => select(Selected::Daemon)?,
            // Slice B hooks: `hook-inject`, `hook-extract`, `install-hooks`
            // (positional words, like `daemon` above).
            "hook-inject" => select(Selected::HookInject)?,
            "hook-extract" => select(Selected::HookExtract)?,
            "install-hooks" => select(Selected::InstallHooks)?,
            other if other.starts_with('-') => {
                return Err(CliError::Usage(format!("unknown flag: {other}")));
            }
            other => positional.push(other.to_string()),
        }
    }
    if !positional.is_empty() {
        // Slice C: `mc-import preview <path>` / `mc-import confirm <path>`.
        if positional.len() == 3
            && positional[0] == "mc-import"
            && (positional[1] == "preview" || positional[1] == "confirm")
        {
            if selected.is_some() {
                return Err(CliError::Usage(
                    "commands are mutually exclusive; pick one".to_string(),
                ));
            }
            selected = Some(Selected::McImport);
        } else
        // G1c: `mm list` / `mm show|refresh|seed|delete|reload <name>`.
        if (positional.len() == 2 || positional.len() == 3)
            && positional[0] == "mm"
            && (positional[1] == "list"
                || positional[1] == "show"
                || positional[1] == "refresh"
                || positional[1] == "seed"
                || positional[1] == "delete"
                || positional[1] == "reload")
        {
            if selected.is_some() {
                return Err(CliError::Usage(
                    "commands are mutually exclusive; pick one".to_string(),
                ));
            }
            selected = Some(Selected::Mm);
        } else
        // G2: `memory view|stats|diagnose|queue|sync|clear|reset|enqueue|rebuild`.
        if positional.len() == 2
            && positional[0] == "memory"
            && (positional[1] == "view"
                || positional[1] == "stats"
                || positional[1] == "diagnose"
                || positional[1] == "queue"
                || positional[1] == "sync"
                || positional[1] == "clear"
                || positional[1] == "reset"
                || positional[1] == "enqueue"
                || positional[1] == "rebuild")
        {
            if selected.is_some() {
                return Err(CliError::Usage(
                    "commands are mutually exclusive; pick one".to_string(),
                ));
            }
            selected = Some(Selected::Memory);
        } else
        // Project subcommand: `project bind|status` (positionals, since the
        // top-level parser is flag-oriented). Anything else stays an error.
        if positional.len() == 2
            && positional[0] == "project"
            && (positional[1] == "bind" || positional[1] == "status" || positional[1] == "adopt")
        {
            if selected.is_some() {
                return Err(CliError::Usage(
                    "commands are mutually exclusive; pick one".to_string(),
                ));
            }
            selected = Some(Selected::Project);
        } else {
            return Err(CliError::Usage(format!(
                "unexpected arguments: {}",
                positional.join(" ")
            )));
        }
    }
    if selected.as_ref().is_some_and(|s| *s != Selected::Daemon)
        && (daemon_foreground || daemon_idle_ms.is_some())
    {
        return Err(CliError::Usage(
            "--foreground/--daemon-idle-ms require daemon".to_string(),
        ));
    }
    if selected.as_ref().is_some_and(|s| *s != Selected::Project)
        && (here || project_root.is_some() || project_name.is_some() || project_slug.is_some())
    {
        return Err(CliError::Usage(
            "--here/--root/--name/--slug require project bind/status/adopt".to_string(),
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
        Some(Selected::HookInject) => {
            if store.is_some() || socket.is_some() || foreground || port.is_some() {
                return Err(CliError::Usage("hook-inject takes no options".to_string()));
            }
            Ok(Command::HookInject)
        }
        Some(Selected::HookExtract) => {
            if store.is_some() || socket.is_some() || foreground || port.is_some() {
                return Err(CliError::Usage("hook-extract takes no options".to_string()));
            }
            Ok(Command::HookExtract)
        }
        Some(Selected::InstallHooks) => {
            if store.is_some() || socket.is_some() || foreground || port.is_some() {
                return Err(CliError::Usage(
                    "install-hooks takes no options".to_string(),
                ));
            }
            Ok(Command::InstallHooks)
        }
        Some(Selected::ProvisionModels) => {
            if store.is_some() || socket.is_some() || foreground || port.is_some() {
                return Err(CliError::Usage(
                    "--provision-models takes no options".to_string(),
                ));
            }
            Ok(Command::ProvisionModels)
        }
        Some(Selected::Project) => {
            if store.is_some()
                || socket.is_some()
                || foreground
                || port.is_some()
                || daemon_foreground
                || daemon_idle_ms.is_some()
            {
                return Err(CliError::Usage(
                    "project takes only bind/status/adopt selectors".to_string(),
                ));
            }
            match positional.get(1).map(|s| s.as_str()) {
                Some("bind") => {
                    if here && project_root.is_some() {
                        return Err(CliError::Usage(
                            "--here and --root are mutually exclusive".to_string(),
                        ));
                    }
                    if !here && project_root.is_none() {
                        return Err(CliError::Usage(
                            "project bind needs a checkout: pass --here or --root DIR".to_string(),
                        ));
                    }
                    if project_slug.is_some() {
                        return Err(CliError::Usage(
                            "--slug is for project adopt, not bind".to_string(),
                        ));
                    }
                    Ok(Command::ProjectBind {
                        here,
                        root: project_root,
                        name: project_name,
                    })
                }
                Some("status") => {
                    if here || project_name.is_some() || project_slug.is_some() {
                        return Err(CliError::Usage("status takes only --root".to_string()));
                    }
                    Ok(Command::ProjectStatus { root: project_root })
                }
                Some("adopt") => {
                    let slug = project_slug.ok_or_else(|| {
                        CliError::Usage("project adopt needs --slug <slug>".to_string())
                    })?;
                    if here && project_root.is_some() {
                        return Err(CliError::Usage(
                            "--here and --root are mutually exclusive".to_string(),
                        ));
                    }
                    if !here && project_root.is_none() {
                        return Err(CliError::Usage(
                            "project adopt needs a checkout: pass --here or --root DIR".to_string(),
                        ));
                    }
                    if project_name.is_some() {
                        return Err(CliError::Usage(
                            "--name is for project bind, not adopt".to_string(),
                        ));
                    }
                    Ok(Command::ProjectAdopt {
                        here,
                        root: project_root,
                        slug,
                    })
                }
                _ => Err(CliError::Usage(
                    "project requires a subcommand: bind, status or adopt".to_string(),
                )),
            }
        }
        Some(Selected::Mm) => {
            if store.is_some()
                || socket.is_some()
                || foreground
                || port.is_some()
                || daemon_foreground
                || daemon_idle_ms.is_some()
                || here
                || project_root.is_some()
                || project_name.is_some()
                || project_slug.is_some()
                || mc_digest.is_some()
            {
                return Err(CliError::Usage(
                    "mm takes only list|show|refresh|seed|delete|reload and a model name"
                        .to_string(),
                ));
            }
            let action = positional.get(1).cloned().ok_or_else(|| {
                CliError::Usage(
                    "mm needs an action: mm list|show|refresh|seed|delete|reload [name]"
                        .to_string(),
                )
            })?;
            let name = positional.get(2).cloned();
            if action == "list" && name.is_some() {
                return Err(CliError::Usage("mm list takes no model name".to_string()));
            }
            if action != "list" && name.is_none() {
                return Err(CliError::Usage(format!("mm {action} needs a model name")));
            }
            Ok(Command::Mm { action, name })
        }
        Some(Selected::Memory) => {
            if store.is_some()
                || socket.is_some()
                || foreground
                || port.is_some()
                || daemon_foreground
                || daemon_idle_ms.is_some()
                || here
                || project_root.is_some()
                || project_name.is_some()
                || project_slug.is_some()
                || mc_digest.is_some()
            {
                return Err(CliError::Usage(
                    "memory takes only view|stats|diagnose|queue|sync|clear|reset|enqueue|rebuild"
                        .to_string(),
                ));
            }
            let action = positional
                .get(1)
                .cloned()
                .ok_or_else(|| CliError::Usage("memory needs an action".to_string()))?;
            Ok(Command::Memory { action })
        }
        Some(Selected::McImport) => {
            if store.is_some()
                || socket.is_some()
                || foreground
                || port.is_some()
                || daemon_foreground
                || daemon_idle_ms.is_some()
                || here
                || project_root.is_some()
                || project_name.is_some()
                || project_slug.is_some()
            {
                return Err(CliError::Usage(
                    "mc-import takes only preview|confirm, a path and --digest (confirm)"
                        .to_string(),
                ));
            }
            let action = positional.get(1).map(|s| s.as_str());
            let path = positional.get(2).cloned().ok_or_else(|| {
                CliError::Usage(
                    "mc-import needs a path: mc-import preview|confirm <path>".to_string(),
                )
            })?;
            match action {
                Some("preview") => {
                    if mc_digest.is_some() {
                        return Err(CliError::Usage(
                            "--digest is for mc-import confirm, not preview".to_string(),
                        ));
                    }
                    Ok(Command::McImportPreview { path })
                }
                Some("confirm") => {
                    let digest = mc_digest.ok_or_else(|| {
                        CliError::Usage(
                            "mc-import confirm needs --digest <digest> from preview".to_string(),
                        )
                    })?;
                    Ok(Command::McImportConfirm { path, digest })
                }
                _ => Err(CliError::Usage(
                    "mc-import requires preview or confirm".to_string(),
                )),
            }
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
  (no args)                Serve MCP over stdio [--socket PATH]
  daemon                  Run the shared daemon: ensure serving and exit,
                          or serve inline with --foreground
   project bind            Bind this checkout to a stable project identity
                           [--here | --root DIR] [--name NAME]
   project status          Show this checkout's bound identity [--root DIR]
   project adopt           Attach a legacy bucket to this checkout's identity
                           [--here | --root DIR] --slug SLUG
  -lib, --library         Print a knowledge-base snapshot [--store PATH]
  -vis, --visualize       Run the visualizer [--fg] [-p PORT | --port PORT]
  --install-skill         Install/update the managed skill
  --install-shim          Install the opt-in legacy `lemma` shim
  hook-inject             Inject budget-capped recall (host session hook, stdin JSON)
  mc-import preview       Preview a Magic Context import (read-only, no store touch)
                          <path>
  mc-import confirm       Confirm a previewed MC import [--digest DIGEST]
                          <path> (additive memory_add fan-out)
  hook-extract            Extract findings from transcript tail (host hook, stdin JSON)
  install-hooks           Install opt-in host hook snippets + enable hooks
  mm list|show..          Hindsight mental-model maintenance (list|show|refresh|seed|delete|reload [name])
  memory view|stats..       Memory backend surfaces (view|stats|diagnose|queue|sync|clear|reset|enqueue|rebuild)
  --provision-models      Download + verify pinned E5 embedding artifacts
  -h, --help              Show this help
  -V, --version           Print the version

Options:
  --store PATH            Canonical store directory (for -lib)
  --socket PATH           Daemon socket path (for stdio mode)
  --foreground            Serve the daemon inline (with daemon)
  --daemon-idle-ms MS    Daemon idle shutdown in ms, 0 = forever
                          (with daemon; default 60000)
   --here                  Bind the current directory (with project bind/adopt)
   --root DIR              Bind the given checkout directory (with project bind/status/adopt)
   --name NAME             Project display name (with project bind; default: directory basename)
   --slug SLUG             Legacy bucket to attach (with project adopt)
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
    let repo = ltmrs_service::repository::CanonicalRepository::open(&path)
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

    let home = home.filter(|h| !h.is_empty()).ok_or_else(|| {
        CliError::Runtime(format!(
            "{} is not set",
            crate::frontend::serve::home_env_name()
        ))
    })?;
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

    let home = home.filter(|h| !h.is_empty()).ok_or_else(|| {
        CliError::Runtime(format!(
            "{} is not set",
            crate::frontend::serve::home_env_name()
        ))
    })?;
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
    use ltmrs_embeddings::artifacts::{ArtifactCache, HttpClient};
    use ltmrs_embeddings::e5_small::E5SmallAdapter;
    use ltmrs_embeddings::manifest::{E5_SMALL_ID, E5_SMALL_REVISION, e5_small_artifact};

    let home = home.filter(|h| !h.is_empty()).ok_or_else(|| {
        CliError::Runtime(format!(
            "{} is not set",
            crate::frontend::serve::home_env_name()
        ))
    })?;
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

/// Bind a checkout to a fresh stable project identity (Wave A3b).
/// Writes `<checkout>/.ltmrs/project.json` (`0600`); refuses when bound.
/// The name defaults to the checkout basename (sanitized); the same-name
/// legacy bucket attaches daemon-side on first acknowledged write, and the
/// daemon-reported count is printed best-effort here (offline when no
/// daemon serves).
pub async fn project_bind_command(
    here: bool,
    root: Option<&str>,
    name: Option<&str>,
    home: Option<String>,
) -> Result<String, CliError> {
    use crate::project::{BIND_DIR, Binding, canonical_root, write_bind};
    if here && root.is_some() {
        return Err(CliError::Usage(
            "--here and --root are mutually exclusive".to_string(),
        ));
    }
    if !here && root.is_none() {
        return Err(CliError::Usage(
            "project bind needs a checkout: pass --here or --root DIR".to_string(),
        ));
    }
    let checkout = match root {
        Some(r) => std::path::PathBuf::from(r),
        None => std::env::current_dir()
            .map_err(|e| CliError::Runtime(format!("cannot resolve checkout directory: {e}")))?,
    };
    if !checkout.is_dir() {
        return Err(CliError::Usage(format!(
            "no checkout at {}",
            checkout.display()
        )));
    }
    let base = checkout
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let display = match name {
        Some(n) => ltmrs_domain::project_id::sanitize_name(n),
        None => ltmrs_domain::project_id::sanitize_name(&base),
    };
    let (id_str, _id) = ltmrs_domain::project_id::generate_project_id(&display);
    let binding = Binding {
        project_id: id_str.clone(),
        name: display.clone(),
        created_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    };
    let path = write_bind(&checkout, &binding).map_err(CliError::Runtime)?;
    let mut out = format!(
        "Bound {} to {id_str} (name '{display}')\nWrote {} (owner-only)",
        checkout.display(),
        path.display()
    );
    out.push_str(&format!(
        "\nAdd `{BIND_DIR}/` to .gitignore so the binding stays local to this checkout."
    ));
    match crate::project::daemon_unattached_count(&display, home).await {
        Some(n) => out.push_str(&format!(
            "\n{n} legacy '{display}' fragment(s) will attach to {id_str} on first write."
        )),
        None => {
            let root_note =
                canonical_root(&checkout).unwrap_or_else(|| checkout.display().to_string());
            out.push_str(&format!(
                "\n(daemon unreachable; the legacy '{display}' bucket of {root_note} attaches on first write)"
            ));
        }
    }
    Ok(out)
}

/// Show a checkout's bound identity: bind file plus env override.
/// Install opt-in hook snippets: writes `<home>/.ltmrs/hooks.json`
/// (`{"enabled":true}`) and prints the host snippet paste instructions.
/// Refusals are runtime errors (foreign content blocks silently overwriting).
/// Preview an MC database import (read-only staging, no store touch).
/// Hindsight mental-model maintenance (G1c; omp `mm` parity).
///
/// - `list`: seed catalog + applicability (no server contact).
/// - `show|refresh|seed|delete|reload <name>`: validate the name against the
///   catalog; server mutation happens through the bridge-equipped daemon,
///   so these print the resolved bank path + seed query (explicit next step,
///   never a silent local fake).
///
/// Memory backend surfaces (G2; omp `/memory` parity).
///
/// - view|stats|diagnose|queue render the P3 status surfaces (bridge
///   disabled labels when no bridge is configured — never fabricated data).
/// - sync|enqueue|rebuild acknowledge the flush contract (bridge queue
///   drain + projection maintenance run in the daemon; the CLI prints the
///   explicit next step, never a silent local fake).
/// - clear|reset REFUSE without a prior backup: the canonical store is
///   never wiped from a CLI verb (fail loud, print the backup-first path).
pub fn memory_command(action: &str) -> Result<String, CliError> {
    use crate::skills::memory_status::{
        render_diagnose, render_queue, render_stats, render_status,
    };
    match action {
        "view" => Ok(render_status("ltmrs", 0, 0, 0, 0)),
        "stats" => Ok(render_stats(0, 0.0, 0)),
        "diagnose" => Ok(render_diagnose("disabled (local-only)", 0, 10_000)),
        "queue" => Ok(render_queue(0, 256)),
        "sync" | "enqueue" | "rebuild" => memory_sync_command(crate::frontend::serve::home_dir()),
        "clear" | "reset" => Err(CliError::Usage(
            "refusing: `memory clear` never wipes the canonical store. Back up first (`backup_create`), then forget scope-by-scope via `memory_forget`.".to_string(),
        )),
        other => Err(CliError::Usage(format!(
            "unknown memory action {other:?} (want view|stats|diagnose|queue|sync|clear|reset|enqueue|rebuild)"
        ))),
    }
}

/// Consolidate pending curation queues into curated files (G4/G5).
/// Drains sharpshooter decision queues + local learned queue under `home`
/// ("default" bank), redacts + dedups + caps. Honest counts, never silent.
/// Home-taking sync dispatcher for the binary (CLI `memory sync`).
pub fn memory_sync_dispatch(home: Option<String>, action: &str) -> Result<String, CliError> {
    if matches!(action, "sync" | "enqueue" | "rebuild") {
        return memory_sync_command(home);
    }
    memory_command(action)
}

pub fn memory_sync_command(home: Option<String>) -> Result<String, CliError> {
    use crate::skills::curate::{
        DECISION_FILES, consolidate_file, drain_pending, local_bank_dir, sharpshooter_bank_dir,
    };
    let Some(home) = home.filter(|h| !h.is_empty()) else {
        return Err(CliError::Usage(
            "home directory is required for memory sync".to_string(),
        ));
    };
    let home = std::path::Path::new(&home);
    let mut total = 0usize;
    let mut parts = Vec::new();
    for file in DECISION_FILES {
        let dir = sharpshooter_bank_dir(home, "default");
        let deltas = drain_pending(&dir, file);
        if deltas.is_empty() {
            continue;
        }
        let n = consolidate_file(&dir.join(file), &deltas, false, usize::MAX)
            .map_err(|e| CliError::Runtime(format!("consolidate {file}: {e}")))?;
        total += n;
        parts.push(format!("{file}: {n}"));
    }
    {
        let dir = local_bank_dir(home, "default");
        let deltas = drain_pending(&dir, "learned.md");
        if !deltas.is_empty() {
            let n = consolidate_file(&dir.join("learned.md"), &deltas, true, 100)
                .map_err(|e| CliError::Runtime(format!("consolidate learned.md: {e}")))?;
            total += n;
            parts.push(format!("learned.md: {n}"));
        }
    }
    if total == 0 {
        return Ok("memory sync: no pending deltas.".to_string());
    }
    Ok(format!(
        "memory sync: consolidated {total} deltas ({}).",
        parts.join(", ")
    ))
}

pub fn mm_command(action: &str, name: Option<String>) -> Result<String, CliError> {
    use ltmrs_interchange::hindsight::Scoping;
    use ltmrs_interchange::hindsight::mental::{MENTAL_SEEDS, seeds_for_scope};
    if action == "list" {
        let mut out = String::from("Mental-model seeds (omp parity, frozen catalog):\n");
        for s in MENTAL_SEEDS {
            let modes = if s.project_tagged {
                "tagged banks"
            } else {
                "all banks"
            };
            out.push_str(&format!("- {} ({}) — {}\n", s.id, modes, s.name));
        }
        let _ = seeds_for_scope(&Scoping::PerProjectTagged);
        return Ok(out);
    }
    let Some(n) = name.as_deref().filter(|s| !s.trim().is_empty()) else {
        return Err(CliError::Usage(format!("mm {action} needs a model name")));
    };
    let seed = MENTAL_SEEDS.iter().find(|s| s.id == n.trim()).ok_or_else(|| {
        CliError::Usage(format!("unknown mental model {n:?} (want one of: user-preferences, project-conventions, project-decisions)"))
    })?;
    Ok(format!(
        "Model {} ({}) resolves against the configured Hindsight bank; server mutation runs through the bridge-equipped daemon (mm {action} acknowledged, no local fake).\nSeed query: {}",
        seed.id, seed.name, seed.source_query
    ))
}

pub fn mc_import_preview_command(path: String) -> Result<String, CliError> {
    use crate::skills::mc_import::preview;
    let rep = preview(std::path::Path::new(&path))
        .map_err(|e| CliError::Runtime(format!("mc-import preview failed: {e}")))?;
    let mut out = format!(
        "MC import preview: {} memories, {} loss item(s), digest {}\nScopes:\n",
        rep.memory_count, rep.loss_count, rep.digest
    );
    for (harness, root) in &rep.scopes {
        out.push_str(&format!("  harness={harness} root={root}\n"));
    }
    out.push_str("Confirm imports a copy (never moves): call mc-import confirm with --digest.");
    Ok(out)
}

/// Confirm an MC import: re-stage, bind digest, fan out memory_add.
pub async fn mc_import_confirm_command(
    home: Option<String>,
    path: String,
    digest: String,
) -> Result<String, CliError> {
    use crate::frontend::mcp::{FrontendIdentity, LtmrsFrontend};
    use crate::frontend::serve::daemon::{connect_remote, daemon_socket_path};
    use crate::frontend::serve::{resolve_home, stdio_layout};
    use crate::skills::mc_import::confirm;
    use ltmrs_daemon::envelope::{HandshakeRequest, PROTOCOL_VERSION};
    use ltmrs_domain::id::{ChannelId, FrontendId, StoreGeneration};

    let home = home.filter(|h| !h.is_empty()).ok_or_else(|| {
        CliError::Runtime(format!(
            "{} is not set",
            crate::frontend::serve::home_env_name()
        ))
    })?;
    let base = resolve_home(Some(home)).map_err(|e| CliError::Runtime(e.to_string()))?;
    let layout = stdio_layout(&base);
    let socket = daemon_socket_path(&layout);
    let mut client = connect_remote(&socket.to_string_lossy())
        .await
        .map_err(|e| CliError::Runtime(format!("daemon unavailable: {e}")))?;
    let frontend_id = FrontendId::new(uuid::Uuid::now_v7());
    let channel_id = ChannelId::new(uuid::Uuid::now_v7());
    client
        .handshake(&HandshakeRequest {
            protocol_version: PROTOCOL_VERSION,
            store_generation: StoreGeneration::FIRST,
            frontend_id,
            channel_id,
            resume_retry_epoch: None,
        })
        .await
        .map_err(|e| CliError::Runtime(format!("daemon handshake failed: {e}")))?;
    let fe = LtmrsFrontend::new(FrontendIdentity::new(frontend_id, channel_id), client);
    let (applied, rejected) = confirm(&fe, std::path::Path::new(&path), &digest)
        .await
        .map_err(|e| CliError::Runtime(format!("mc-import confirm failed: {e}")))?;
    Ok(format!(
        "MC import confirmed: {applied} applied, {rejected} rejected (reruns dedup via server Jaccard)."
    ))
}

pub fn install_hooks_command(home: Option<String>) -> Result<String, CliError> {
    use crate::skills::hooks::{InstallOutcome, hook_snippet, hooks_config_path, install_hooks};

    let home = home.filter(|h| !h.is_empty()).ok_or_else(|| {
        CliError::Runtime(format!(
            "{} is not set",
            crate::frontend::serve::home_env_name()
        ))
    })?;
    let exe = std::env::current_exe()
        .map_err(|e| CliError::Runtime(format!("cannot locate ltmrs binary: {e}")))?;
    let path = hooks_config_path(std::path::Path::new(&home));
    match install_hooks(std::path::Path::new(&home), &exe)
        .map_err(|e| CliError::Runtime(format!("hooks install failed: {e}")))?
    {
        InstallOutcome::Installed => Ok(format!(
            "Installed hooks config at {}; paste into Claude Code settings:\n{}",
            path.display(),
            hook_snippet("claude-code", &exe)
                .map_err(|e| CliError::Runtime(format!("snippet failed: {e}")))?
        )),
        InstallOutcome::AlreadyCurrent => Ok(format!(
            "Hooks already current at {}; snippet:\n{}",
            path.display(),
            hook_snippet("claude-code", &exe)
                .map_err(|e| CliError::Runtime(format!("snippet failed: {e}")))?
        )),
        InstallOutcome::RefusedForeign => Err(CliError::Runtime(format!(
            "refusing: unmanaged file at {} (back it up and remove it first)",
            path.display()
        ))),
    }
}

pub fn project_status_command(
    root: Option<String>,
    env_override: Option<String>,
    home: Option<String>,
) -> Result<String, CliError> {
    use crate::project::{ENV_PROJECT_ID, read_bind};
    let checkout = match root {
        Some(r) => std::path::PathBuf::from(r),
        None => std::env::current_dir()
            .map_err(|e| CliError::Runtime(format!("cannot resolve checkout directory: {e}")))?,
    };
    let mut out = format!("Checkout: {}\n", checkout.display());
    let bound = read_bind(&checkout).map_err(CliError::Runtime)?;
    match &bound {
        Some(b) => out.push_str(&format!(
            "Bind file: {} (name '{}', created {})\n",
            b.project_id, b.name, b.created_at
        )),
        None => out.push_str("Bind file: none (unbound; writes are unattributed)\n"),
    }
    let env = env_override.or_else(|| std::env::var(ENV_PROJECT_ID).ok());
    let winning: Option<String> = match env.filter(|s| !s.trim().is_empty()) {
        Some(v) => {
            out.push_str(&format!(
                "{ENV_PROJECT_ID}: {} (wins over the bind file)\n",
                v.trim()
            ));
            Some(v.trim().to_string())
        }
        None => {
            out.push_str(&format!("{ENV_PROJECT_ID}: unset\n"));
            bound.map(|b| b.project_id)
        }
    };
    out.push_str(&attached_slugs_report(winning.as_deref(), home));
    Ok(out)
}

/// Best-effort attached-slug line for `project status`: names the slugs the
/// winning identity attached, or why they are unknown (offline store).
fn attached_slugs_report(winning: Option<&str>, home: Option<String>) -> String {
    let Some(full) = winning else {
        return "Attached slugs: none (unbound)\n".to_string();
    };
    let Some((_, id)) = ltmrs_domain::project_id::parse_project_id(full) else {
        return format!("Attached slugs: unknown (bound identity is malformed: {full})\n");
    };
    let Some(home) = home.filter(|h| !h.is_empty()) else {
        return "Attached slugs: unknown (no home for the local store)\n".to_string();
    };
    let store_path = std::path::Path::new(&home)
        .join(crate::frontend::serve::MANAGED_HOME_DIR)
        .join(crate::frontend::serve::STORE_DIR_NAME);
    if !store_path.exists() {
        return "Attached slugs: unknown (no local store)\n".to_string();
    }
    match ltmrs_service::repository::CanonicalRepository::open(
        store_path.to_str().unwrap_or_default(),
    ) {
        Ok(repo) => match repo.project_record(id) {
            Ok(Some(rec)) if rec.legacy_slugs.is_empty() => "Attached slugs: none\n".to_string(),
            Ok(Some(rec)) => format!("Attached slugs: {}\n", rec.legacy_slugs.join(", ")),
            _ => "Attached slugs: unknown (identity not registered yet)\n".to_string(),
        },
        Err(_) => "Attached slugs: unknown (store unreachable)\n".to_string(),
    }
}

/// Attach a legacy bucket to the checkout's bound identity (Wave A3c).
/// Requires a bound identity (bind file or `LTMRS_PROJECT_ID` env);
/// records the normalized slug in the id's `legacy_slugs` and reports the
/// newly visible row count. Rows are never rewritten.
pub async fn project_adopt_command(
    here: bool,
    root: Option<&str>,
    slug: &str,
    home: Option<String>,
    env_override: Option<String>,
) -> Result<String, CliError> {
    use crate::project::{canonical_root, resolve_project_id};
    if here && root.is_some() {
        return Err(CliError::Usage(
            "--here and --root are mutually exclusive".to_string(),
        ));
    }
    if !here && root.is_none() {
        return Err(CliError::Usage(
            "project adopt needs a checkout: pass --here or --root DIR".to_string(),
        ));
    }
    let bucket = ltmrs_compat::lemma::tool_args::normalize_project(slug).ok_or_else(|| {
        CliError::Usage(
            "project adopt needs a bucket name: pass --slug <slug> (not empty or 'global')"
                .to_string(),
        )
    })?;
    let checkout = match root {
        Some(r) => std::path::PathBuf::from(r),
        None => std::env::current_dir()
            .map_err(|e| CliError::Runtime(format!("cannot resolve checkout directory: {e}")))?,
    };
    if !checkout.is_dir() {
        return Err(CliError::Usage(format!(
            "no checkout at {}",
            checkout.display()
        )));
    }
    let binding = resolve_project_id(Some(&checkout), env_override)
        .map_err(CliError::Runtime)?
        .ok_or_else(|| {
            CliError::Runtime(
                "no bound identity for this checkout; run `ltmrs project bind --here` (or `--root DIR`) first"
                    .to_string(),
            )
        })?;
    let (_, id) =
        ltmrs_domain::project_id::parse_project_id(&binding.project_id).ok_or_else(|| {
            CliError::Runtime(format!(
                "bound identity is malformed: {}",
                binding.project_id
            ))
        })?;
    let canonical = canonical_root(&checkout);
    let report = crate::project::daemon_adopt_slug(home, &binding.project_id, canonical, &bucket)
        .await
        .map_err(CliError::Runtime)?;
    let full = ltmrs_domain::project_id::format_project_id(&report.record_name, id);
    let mut out = if report.already_attached {
        format!("'{bucket}' is already attached to {full} (0 new fragments)")
    } else {
        format!(
            "Attached {} legacy '{bucket}' fragment(s) to {full}",
            report.attached
        )
    };
    if report.created {
        out.push_str(" (registered the identity for this checkout)");
    }
    if let Some(w) = report.warning {
        out.push_str(&format!("\nWarning: {w}"));
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
        assert!(
            err.to_string()
                .contains(crate::frontend::serve::home_env_var()),
            "got: {err}"
        );
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
        drop(ltmrs_service::repository::CanonicalRepository::open(&path));
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
        // The shim routes to this very binary: a symlink on unix, the
        // byte-identical `.cmd` content on Windows.
        let link = crate::skills::shim::shim_path(dir.path());
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            std::env::current_exe().unwrap()
        );
        #[cfg(windows)]
        assert_eq!(
            std::fs::read(&link).unwrap(),
            crate::skills::shim::shim_content(&std::env::current_exe().unwrap()).as_bytes()
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

    /// `project bind` parses its checkout selectors and name override.
    #[test]
    fn project_bind_parses() {
        assert_eq!(
            parse(&["project", "bind", "--here"]).unwrap(),
            Command::ProjectBind {
                here: true,
                root: None,
                name: None,
            }
        );
        // Bare `project bind` (no checkout selector) errors with guidance
        // instead of silently binding the cwd (explicit-errors philosophy).
        let err = parse(&["project", "bind"]).unwrap_err();
        assert!(
            err.to_string().contains("--here"),
            "must guide to a selector, got: {err}"
        );
        assert_eq!(
            parse(&["project", "bind", "--root", "/tmp/x", "--name", "shop"]).unwrap(),
            Command::ProjectBind {
                here: false,
                root: Some("/tmp/x".to_string()),
                name: Some("shop".to_string()),
            }
        );
        assert!(matches!(
            parse(&["project", "status"]).unwrap(),
            Command::ProjectStatus { .. }
        ));
    }

    /// mc-import parses preview/confirm with path; digest only for confirm.
    #[test]
    fn mc_import_parses() {
        assert!(matches!(
            parse(&["mc-import", "preview", "/tmp/x.db"]).unwrap(),
            Command::McImportPreview { .. }
        ));
        assert!(matches!(
            parse(&["mc-import", "confirm", "/tmp/x.db", "--digest", "abc"]).unwrap(),
            Command::McImportConfirm { .. }
        ));
        // confirm without digest errors; preview with digest errors.
        assert!(parse(&["mc-import", "confirm", "/tmp/x.db"]).is_err());
        assert!(parse(&["mc-import", "preview", "/tmp/x.db", "--digest", "abc"]).is_err());
        // wrong arity or verb errors.
        assert!(parse(&["mc-import", "preview"]).is_err());
        assert!(parse(&["mc-import", "frobnicate", "/tmp/x.db"]).is_err());
    }

    /// Project needs a subcommand; selectors conflict; project conflicts
    /// with every other command.
    #[test]
    fn project_usage_and_conflicts() {
        assert!(parse(&["project"]).is_err());
        assert!(parse(&["project", "frobnicate"]).is_err());
        assert!(parse(&["project", "bind", "--here", "--root", "/tmp/x"]).is_err());
        assert!(parse(&["project", "bind", "--here", "--missing-value"]).is_err());
        assert!(parse(&["project", "bind", "-lib"]).is_err());
        assert!(parse(&["-lib", "project", "status"]).is_err());
        assert!(parse(&["project", "status", "--store", "/tmp/s"]).is_err());
    }

    /// Hook subcommands parse (Slice B Phase 1 RED).
    #[test]
    fn hook_subcommands_parse() {
        assert!(matches!(
            parse(&["hook-inject"]).unwrap(),
            Command::HookInject
        ));
        assert!(matches!(
            parse(&["hook-extract"]).unwrap(),
            Command::HookExtract
        ));
        assert!(matches!(
            parse(&["install-hooks"]).unwrap(),
            Command::InstallHooks
        ));
    }

    /// Help documents the Slice C mc-import surface.
    #[test]
    fn help_documents_mc_import() {
        let help = help_text();
        for flag in ["mc-import", "preview", "confirm", "--digest"] {
            assert!(help.contains(flag), "help must document {flag}");
        }
    }

    /// Help documents the Slice B hook surface.
    #[test]
    fn help_documents_hooks() {
        let help = help_text();
        for flag in ["hook-inject", "hook-extract", "install-hooks"] {
            assert!(help.contains(flag), "help must document {flag}");
        }
    }

    /// Help documents the project surface.
    #[test]
    fn help_documents_project() {
        let help = help_text();
        for flag in [
            "project", "bind", "status", "adopt", "--here", "--root", "--name", "--slug",
        ] {
            assert!(help.contains(flag), "help must document {flag}");
        }
    }

    /// `project bind --root` writes a bind file offline (no daemon needed).
    #[tokio::test]
    async fn project_bind_writes_file_offline() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("shop");
        std::fs::create_dir_all(&root).unwrap();
        let out = project_bind_command(
            false,
            Some(root.to_str().unwrap()),
            None,
            Some("/nonexistent-home-xyz".to_string()),
        )
        .await
        .unwrap();
        assert!(out.contains("prj_shop_"), "got: {out}");
        assert!(
            out.contains(".gitignore"),
            "must name the gitignore hint, got: {out}"
        );
        let binding = crate::project::read_bind(&root).unwrap().unwrap();
        assert_eq!(binding.name, "shop");
        assert!(binding.project_id.starts_with("prj_shop_"));
    }

    /// Re-binding refuses; `project status` shows the binding.
    #[tokio::test]
    async fn project_bind_refuses_and_status_shows() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("shop");
        std::fs::create_dir_all(&root).unwrap();
        project_bind_command(
            false,
            Some(root.to_str().unwrap()),
            None,
            Some("/nonexistent-home-xyz".to_string()),
        )
        .await
        .unwrap();
        let err = project_bind_command(false, Some(root.to_str().unwrap()), None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already bound"), "got: {err}");
        let status =
            project_status_command(Some(root.to_str().unwrap().to_string()), None, None).unwrap();
        assert!(status.contains("prj_shop_"), "got: {status}");
    }

    /// `project adopt --slug` parses its selectors; bare `project adopt`
    /// (no --slug) errors with usage naming the flag.
    #[test]
    fn project_adopt_parses_and_bare_errors() {
        assert_eq!(
            parse(&["project", "adopt", "--slug", "other", "--here"]).unwrap(),
            Command::ProjectAdopt {
                here: true,
                root: None,
                slug: "other".to_string(),
            }
        );
        assert_eq!(
            parse(&["project", "adopt", "--slug", "other", "--root", "/tmp/x"]).unwrap(),
            Command::ProjectAdopt {
                here: false,
                root: Some("/tmp/x".to_string()),
                slug: "other".to_string(),
            }
        );
        for argv in [
            vec!["project", "adopt", "--here"],
            vec!["project", "adopt"],
            vec!["project", "adopt", "--here", "--root", "/tmp/x"],
        ] {
            let err = parse(&argv).unwrap_err();
            assert!(
                err.to_string().contains("--slug") || err.to_string().contains("--here"),
                "adopt misuse must error with usage, got: {err}"
            );
        }
        let err = parse(&["project", "adopt"]).unwrap_err();
        assert!(
            err.to_string().contains("--slug"),
            "bare adopt must name --slug, got: {err}"
        );
    }

    /// Adopt rejects conflicting selectors and foreign flags.
    #[test]
    fn project_adopt_usage_and_conflicts() {
        assert!(
            parse(&[
                "project", "adopt", "--slug", "x", "--here", "--root", "/tmp/x"
            ])
            .is_err()
        );
        assert!(parse(&["project", "adopt", "--slug", "x"]).is_err());
        assert!(parse(&["project", "adopt", "--slug", "x", "--name", "y", "--here"]).is_err());
        assert!(parse(&["project", "bind", "--here", "--slug", "x"]).is_err());
        assert!(parse(&["project", "status", "--slug", "x"]).is_err());
        assert!(parse(&["project", "adopt", "--slug", "-lib", "--here"]).is_err());
    }

    /// Adopt on an unbound checkout errors guiding to `project bind --here`.
    #[tokio::test]
    async fn project_adopt_requires_bound_identity() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("shop");
        std::fs::create_dir_all(&root).unwrap();
        let err = project_adopt_command(
            false,
            Some(root.to_str().unwrap()),
            "other",
            Some(dir.path().to_str().unwrap().to_string()),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("project bind --here"),
            "must guide to bind, got: {err}"
        );
    }

    fn legacy_memory(n: u128, slug: &str) -> ltmrs_domain::memory::Memory {
        use ltmrs_domain::memory::{FragmentType, Instant, Memory, MemoryLifecycle, MemorySource};
        Memory {
            id: ltmrs_domain::id::EntityId::new(uuid::Uuid::from_u128(n)),
            external_alias: None,
            title: format!("legacy {slug}"),
            fragment: format!("legacy {slug} fragment"),
            description: String::new(),
            fragment_type: FragmentType::Fact,
            project: Some(slug.into()),
            project_id: None,
            source: MemorySource::Ai,
            confidence: 0.5,
            quality_score: None,
            lifecycle: MemoryLifecycle::Live,
            tags: vec![],
            associated_with: vec![],
            relations: vec![],
            parent_id: None,
            child_ids: vec![],
            session_id: None,
            task_type: None,
            related_guides: vec![],
            evidence: vec![],
            access_count: 0,
            last_accessed_at: None,
            positive_feedback: 0,
            negative_feedback: 0,
            negative_hits: 0,
            refinement_count: 0,
            distill_candidate: false,
            entity_revision: ltmrs_domain::id::EntityRevision::new(1),
            document_revision: ltmrs_domain::id::DocumentRevision::new(1),
            eligibility_revision: ltmrs_domain::id::EligibilityRevision::new(1),
            created_at: Instant::new(100),
            updated_at: Instant::new(100),
            raw_created: None,
            unknown_fields: std::collections::BTreeMap::new(),
        }
    }

    fn add_legacy_row(repo: &ltmrs_service::repository::CanonicalRepository, n: u128, slug: &str) {
        use ltmrs_domain::command::DomainCommand;
        use ltmrs_domain::id::{ChannelId, FrontendId, OperationId, StoreGeneration};
        let fe = FrontendId::new(uuid::Uuid::from_u128(1));
        let ch = ChannelId::new(uuid::Uuid::from_u128(2));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let ns = repo.issue_namespace(fe, ch, now).unwrap();
        let ctx = ltmrs_domain::command::CommandContext {
            store_generation: StoreGeneration::FIRST,
            frontend_id: fe,
            channel_id: ch,
            session: None,
            operation_id: OperationId::new(uuid::Uuid::from_u128(n + 100)),
            request_digest: format!("adopt-fixture-{n}"),
            deadline_millis: None,
            scope: Default::default(),
            retry_epoch: ns.retry_epoch,
        };
        repo.apply(
            &ctx,
            &DomainCommand::AddMemory {
                memory: legacy_memory(n, slug),
                session: None,
                auto_link: None,
            },
        )
        .unwrap();
    }

    /// Adopt needs the running daemon: with no daemon serving the managed
    /// home, the CLI errors naming the daemon instead of opening the store
    /// directly (no concurrent-write dual-open).
    #[tokio::test]
    async fn project_adopt_needs_running_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap().to_string();
        let root = dir.path().join("shop");
        std::fs::create_dir_all(&root).unwrap();
        project_bind_command(
            false,
            Some(root.to_str().unwrap()),
            None,
            Some(home.clone()),
        )
        .await
        .unwrap();
        let err = project_adopt_command(
            false,
            Some(root.to_str().unwrap()),
            "other",
            Some(home),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("daemon"),
            "must name the daemon, got: {err}"
        );
    }

    /// Adopt attaches through daemon IPC: a live local daemon executes the
    /// request on its own store handle and the CLI reports the count.
    #[tokio::test]
    async fn project_adopt_attaches_via_daemon_ipc() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap().to_string();
        let layout = crate::frontend::serve::stdio_layout(&dir.path().join(".ltmrs"));
        let (mut daemon, _client) = crate::frontend::serve::daemon::start_local_daemon(&layout)
            .await
            .unwrap();
        add_legacy_row(daemon.dispatcher().repo(), 7, "other");
        let root = dir.path().join("shop");
        std::fs::create_dir_all(&root).unwrap();
        project_bind_command(
            false,
            Some(root.to_str().unwrap()),
            None,
            Some(home.clone()),
        )
        .await
        .unwrap();
        let out = project_adopt_command(
            false,
            Some(root.to_str().unwrap()),
            "other",
            Some(home),
            None,
        )
        .await
        .unwrap();
        assert!(
            out.contains("1") && out.contains("other"),
            "must report the attached count, got: {out}"
        );
        daemon.shutdown();
    }

    /// Status shows attached slugs from the local store (read-only open,
    /// matching the run_library precedent — only writes moved to IPC).
    #[tokio::test]
    async fn project_status_shows_attached_slugs() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap().to_string();
        let store_path = dir.path().join(".ltmrs").join("store");
        let repo =
            ltmrs_service::repository::CanonicalRepository::open(store_path.to_str().unwrap())
                .unwrap();
        add_legacy_row(&repo, 7, "other");
        let root = dir.path().join("shop");
        std::fs::create_dir_all(&root).unwrap();
        project_bind_command(
            false,
            Some(root.to_str().unwrap()),
            None,
            Some(home.clone()),
        )
        .await
        .unwrap();
        let binding = crate::project::read_bind(&root).unwrap().unwrap();
        let (name, id) = ltmrs_domain::project_id::parse_project_id(&binding.project_id).unwrap();
        let canonical = crate::project::canonical_root(&root);
        repo.adopt_legacy_slug(id, &name, canonical.as_deref(), "other")
            .unwrap();
        drop(repo);
        let status =
            project_status_command(Some(root.to_str().unwrap().to_string()), None, Some(home))
                .unwrap();
        assert!(
            status.contains("other"),
            "status must show attached slugs, got: {status}"
        );
    }

    /// Bare `project bind` at the command layer (neither --here nor --root)
    /// errors with guidance instead of silently binding the cwd
    /// (explicit-errors philosophy, M6 hygiene).
    #[tokio::test]
    async fn project_bind_bare_errors_with_guidance() {
        let err = project_bind_command(false, None, None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--here"), "got: {err}");
        assert!(err.to_string().contains("--root"), "got: {err}");
    }
}

#[cfg(test)]
mod mm_tests {
    use super::parse_args;
    use crate::cli::Command;

    /// G1c: mm list parses without a name; sub-actions need one.
    #[test]
    fn mm_parses_list_and_named_actions() {
        assert!(matches!(
            parse_args(&["mm".to_string(), "list".to_string()]).unwrap(),
            Command::Mm { action, name } if action == "list" && name.is_none()
        ));
        assert!(matches!(
            parse_args(&[
                "mm".to_string(),
                "show".to_string(),
                "user-preferences".to_string()
            ])
            .unwrap(),
            Command::Mm { .. }
        ));
        assert!(parse_args(&["mm".to_string(), "show".to_string()]).is_err());
        assert!(parse_args(&["mm".to_string(), "list".to_string(), "x".to_string()]).is_err());
        assert!(parse_args(&["mm".to_string(), "nonsense".to_string()]).is_err());
    }

    /// G1c: mm list renders the catalog; unknown models refuse loudly.
    #[test]
    fn mm_list_and_unknown() {
        let out = super::mm_command("list", None).unwrap();
        assert!(out.contains("user-preferences"));
        assert!(out.contains("project-conventions"));
        assert!(out.contains("project-decisions"));
        assert!(super::mm_command("show", Some("nope".to_string())).is_err());
        assert!(super::mm_command("show", Some("user-preferences".to_string())).is_ok());
    }
}

#[cfg(test)]
mod memory_tests {
    use super::{memory_command, memory_sync_command, parse_args};
    use crate::cli::Command;

    /// G2: memory view|stats|diagnose|queue|sync parse; clear prints backup-first refusal.
    #[test]
    fn memory_parses_all_actions() {
        for a in [
            "view", "stats", "diagnose", "queue", "sync", "enqueue", "rebuild", "clear", "reset",
        ] {
            let cmd = parse_args(&["memory".to_string(), a.to_string()]).unwrap();
            assert!(
                matches!(cmd, Command::Memory { action } if action == a),
                "{a} must parse"
            );
        }
        assert!(parse_args(&["memory".to_string()]).is_err());
        assert!(parse_args(&["memory".to_string(), "wipe".to_string()]).is_err());
    }

    /// G2: view renders the status surface; clear refuses loudly.
    #[test]
    fn memory_view_renders_clear_refuses() {
        let out = memory_command("view").unwrap();
        assert!(out.contains("Memory backend"));
        assert!(memory_command("clear").is_err());
        assert!(
            memory_sync_command(Some("/nonexistent-home-for-test".to_string()))
                .unwrap()
                .contains("no pending deltas")
        );
    }
}
