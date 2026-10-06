//! ltmrs binary: CLI dispatch over the library's argument parser.
//!
//! stdout carries protocol data and help/version text; diagnostics and
//! errors go to stderr. Exit codes follow `CliError::exit_code`.

use ltmrs::cli::{
    CliError, Command, help_text, install_shim_command, install_skill_command, parse_args,
    provision_models_command, run_library, version_text,
};
use ltmrs::frontend::serve::{
    daemon_idle_ms, daemon_socket_path, ensure_daemon_process, resolve_home, run_daemon_foreground,
    serve_stdio, stdio_layout,
};
use ltmrs::visualizer::run_visualize;

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
        Ok(Command::Stdio { socket }) => {
            match serve_stdio(socket, std::env::var("HOME").ok()).await {
                Ok(()) => 0,
                Err(e) => fail(&e),
            }
        }
        Ok(Command::Daemon {
            foreground,
            idle_ms,
        }) => {
            let home = std::env::var("HOME").ok();
            let idle = daemon_idle_ms(idle_ms, std::env::var("LTMRS_DAEMON_IDLE_MS").ok());
            match resolve_home(home).map(|base| stdio_layout(&base)) {
                Err(e) => fail(&e),
                Ok(layout) => {
                    if foreground {
                        match run_daemon_foreground(&layout, idle).await {
                            Ok(()) => 0,
                            Err(e) => fail(&e),
                        }
                    } else {
                        let exe = std::env::current_exe();
                        let result = match exe {
                            Ok(exe) => match ensure_daemon_process(&layout, &exe, idle).await {
                                Ok(()) => {
                                    println!(
                                        "daemon serving at {}",
                                        daemon_socket_path(&layout).display()
                                    );
                                    Ok(())
                                }
                                Err(e) => Err(e),
                            },
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
        Ok(Command::Visualize { foreground, port }) => {
            match run_visualize(foreground, port, std::env::var("HOME").ok()).await {
                Ok(text) => {
                    println!("{text}");
                    0
                }
                Err(e) => fail(&e),
            }
        }
        Ok(Command::InstallSkill) => match install_skill_command(std::env::var("HOME").ok()) {
            Ok(text) => {
                println!("{text}");
                0
            }
            Err(e) => fail(&e),
        },
        Ok(Command::InstallShim) => match install_shim_command(std::env::var("HOME").ok()) {
            Ok(text) => {
                println!("{text}");
                0
            }
            Err(e) => fail(&e),
        },
        Ok(Command::ProvisionModels) => {
            match provision_models_command(std::env::var("HOME").ok()).await {
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

/// Report an error on stderr and return its exit code.
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
