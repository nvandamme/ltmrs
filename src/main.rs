//! ltmrs binary: CLI dispatch over the frontend crate's argument parser.
//!
//! stdout carries protocol data and help/version text; diagnostics and
//! errors go to stderr. Exit codes follow `CliError::exit_code`.

// Benchmark harness (WP-12): exercised by its own unit tests, wired to no
// CLI command. Kept compiled in every profile; the allow covers its
// unreachable-from-`main` public items so `-D warnings` stays green.
#[allow(dead_code)]
mod bench;
mod visualizer;

use crate::visualizer::run_visualize;
use ltmrs_frontend::cli::{
    CliError, Command, help_text, install_hooks_command, install_shim_command,
    install_skill_command, mc_import_confirm_command, mc_import_preview_command,
    memory_sync_dispatch, mm_command, parse_args, project_adopt_command, project_bind_command,
    project_status_command, provision_models_command, run_library, version_text,
};
use ltmrs_frontend::frontend::serve::daemon::{
    daemon_idle_ms, daemon_socket_path, ensure_daemon_process, run_daemon_foreground, serve_stdio,
};
use ltmrs_frontend::frontend::serve::{home_dir, resolve_home, stdio_layout};
use ltmrs_frontend::skills::hooks::{HookExtractInput, HookInjectInput};

#[tokio::main]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let code = match parse_args(&argv) {
        Ok(Command::Help) => {
            print!("{help}", help = help_text());
            0
        }
        Ok(Command::Version) => {
            println!("{}", version_text());
            0
        }
        Ok(Command::Library { store }) => match run_library(store) {
            Ok(text) => {
                print!("{text}");
                0
            }
            Err(e) => fail(&e),
        },
        Ok(Command::Stdio { socket, project_id, idle_ms, hindsight }) => {
            let hindsight_flags = ltmrs_interchange::hindsight::HindsightCliFlags {
                url: hindsight.url,
                bank: hindsight.bank,
                tags: hindsight.tags,
                timeout_ms: hindsight.timeout_ms,
                scoping: hindsight.scoping,
                global_scope: hindsight.global_scope,
                outbox: hindsight.outbox,
            };
            match serve_stdio(socket, home_dir(), project_id, idle_ms, &hindsight_flags).await {
                Ok(()) => 0,
                Err(e) => fail(&e),
            }
        }
        Ok(Command::Daemon {
            foreground,
            idle_ms,
            hindsight,
        }) => {
            let home = home_dir();
            let idle = daemon_idle_ms(idle_ms);
            let hindsight_flags = ltmrs_interchange::hindsight::HindsightCliFlags {
                url: hindsight.url,
                bank: hindsight.bank,
                tags: hindsight.tags,
                timeout_ms: hindsight.timeout_ms,
                scoping: hindsight.scoping,
                global_scope: hindsight.global_scope,
                outbox: hindsight.outbox,
            };
            match resolve_home(home).map(|base| stdio_layout(&base)) {
                Err(e) => fail(&e),
                Ok(layout) => {
                    if foreground {
                        match run_daemon_foreground(&layout, idle, &hindsight_flags).await {
                            Ok(()) => 0,
                            Err(e) => fail(&e),
                        }
                    } else {
                        let exe = std::env::current_exe();
                        let result = match exe {
                            Ok(exe) => {
                                match ensure_daemon_process(&layout, &exe, idle, &hindsight_flags)
                                    .await
                                {
                                    Ok(()) => {
                                        println!(
                                            "daemon serving at {}",
                                            daemon_socket_path(&layout).display()
                                        );
                                        Ok(())
                                    }
                                    Err(e) => Err(e),
                                }
                            }
                            Err(e) => Err(CliError::Runtime(format!(
                                "cannot locate ltmrs binary: {e}"
                            ))),
                        };
                        match result {
                            Ok(()) => 0,
                            Err(e) => fail(&e),
                        }
                    }
                }
            }
        }
        Ok(Command::Visualize {
            foreground,
            port,
            vis_token,
        }) => match run_visualize(foreground, port, home_dir(), vis_token).await {
            Ok(text) => {
                println!("{text}");
                0
            }
            Err(e) => fail(&e),
        },
        Ok(Command::InstallSkill) => match install_skill_command(home_dir()) {
            Ok(text) => {
                println!("{text}");
                0
            }
            Err(e) => fail(&e),
        },
        Ok(Command::InstallShim) => match install_shim_command(home_dir()) {
            Ok(text) => {
                println!("{text}");
                0
            }
            Err(e) => fail(&e),
        },
        // Slice B hooks: read one JSON object on stdin, fail open.
        Ok(Command::HookInject { hooks_enabled }) => {
            let input = read_hook_stdin();
            let out = run_hook_inject_blocking(input, hooks_enabled).await;
            print!("{}", out.stdout);
            out.exit_code
        }
        Ok(Command::HookExtract { hooks_enabled }) => {
            let input = read_hook_stdin();
            let out = run_hook_extract_blocking(input, hooks_enabled).await;
            print!("{}", out.stdout);
            out.exit_code
        }
        Ok(Command::InstallHooks) => match install_hooks_command(home_dir()) {
            Ok(text) => {
                println!("{text}");
                0
            }
            Err(e) => fail(&e),
        },
        Ok(Command::McImportPreview { path, import_lib }) => {
            match mc_import_preview_command(path, import_lib) {
                Ok(text) => {
                    println!("{text}");
                    0
                }
                Err(e) => fail(&e),
            }
        }
        Ok(Command::Memory { action }) => match memory_sync_dispatch(home_dir(), &action) {
            Ok(text) => {
                println!("{text}");
                0
            }
            Err(e) => fail(&e),
        },
        Ok(Command::Mm { action, name }) => match mm_command(&action, name) {
            Ok(text) => {
                println!("{text}");
                0
            }
            Err(e) => fail(&e),
        },
        Ok(Command::McImportConfirm {
            path,
            digest,
            import_lib,
        }) => match mc_import_confirm_command(home_dir(), path, digest, import_lib).await {
            Ok(text) => {
                println!("{text}");
                0
            }
            Err(e) => fail(&e),
        },
        Ok(Command::ProvisionModels) => match provision_models_command(home_dir()).await {
            Ok(text) => {
                println!("{text}");
                0
            }
            Err(e) => fail(&e),
        },
        Ok(Command::ProjectBind { here, root, name }) => {
            match project_bind_command(
                here,
                root.as_deref(),
                name.as_deref(),
                std::env::var("HOME").ok(),
            )
            .await
            {
                Ok(text) => {
                    println!("{text}");
                    0
                }
                Err(e) => fail(&e),
            }
        }
        Ok(Command::ProjectStatus { root, project_id }) => {
            match project_status_command(root, project_id, std::env::var("HOME").ok()) {
                Ok(text) => {
                    println!("{text}");
                    0
                }
                Err(e) => fail(&e),
            }
        }
        Ok(Command::ProjectAdopt {
            here,
            root,
            slug,
            project_id,
        }) => {
            match project_adopt_command(
                here,
                root.as_deref(),
                &slug,
                std::env::var("HOME").ok(),
                project_id,
            )
            .await
            {
                Ok(text) => {
                    println!("{text}");
                    0
                }
                Err(e) => fail(&e),
            }
        }
        Err(e) => fail(&e),
    };
    flush_stdout();
    std::process::exit(code);
}

/// Read hook stdin (one JSON object; missing/unparseable => empty object downstream).
fn read_hook_stdin() -> String {
    use std::io::Read as _;
    let mut s = String::new();
    let _ = std::io::stdin().read_to_string(&mut s);
    s
}

/// Parse stdin JSON into HookInjectInput, run inject (main is async).
async fn run_hook_inject_blocking(
    raw: String,
    enabled: bool,
) -> ltmrs_frontend::skills::hooks::HookOutput {
    use ltmrs_frontend::skills::hooks::run_hook_inject_with;
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    let input = HookInjectInput {
        query_hint: v
            .get("query_hint")
            .or_else(|| v.get("prompt"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        project: v
            .get("project")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
    };
    run_hook_inject_with(home_dir(), &input, enabled).await
}

/// Parse stdin JSON into HookExtractInput, run extract (main is async).
async fn run_hook_extract_blocking(
    raw: String,
    enabled: bool,
) -> ltmrs_frontend::skills::hooks::HookOutput {
    use ltmrs_frontend::skills::hooks::run_hook_extract_with;
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_default();
    let input = HookExtractInput {
        transcript_tail: v
            .get("transcript_tail")
            .or_else(|| v.get("transcript"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        project: v
            .get("project")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string()),
    };
    run_hook_extract_with(home_dir(), &input, enabled).await
}

fn fail(e: &CliError) -> i32 {
    eprintln!("ltmrs: {e}");
    e.exit_code()
}

/// Flush stdout before exiting: `process::exit` skips destructors and
/// would otherwise truncate piped output.
fn flush_stdout() {
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
}
