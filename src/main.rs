//! ltmrs binary: CLI dispatch over the library's argument parser.
//!
//! stdout carries protocol data and help/version text; diagnostics and
//! errors go to stderr. Exit codes follow `CliError::exit_code`.

use ltmrs::cli::{CliError, Command, help_text, parse_args, run_library, version_text};

fn main() {
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
        Ok(Command::Stdio { .. }) => fail(&CliError::Unimplemented(
            "stdio serving wires up with the daemon lifecycle",
        )),
        Ok(Command::Visualize { .. }) => fail(&CliError::Unimplemented("visualizer serving")),
        Ok(Command::InstallSkill) => fail(&CliError::Unimplemented("skill installer")),
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
