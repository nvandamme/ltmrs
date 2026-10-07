//! Suite-selector runners: capabilities, conformance, recovery (plus the
//! script verbs evidence, capture-lemma, benchmark, quality above).
//!
//! Each runner shells out via [`crate::run`], writes a JSON run-record under
//! `reports/`, and propagates the child exit code. `--dry-run`
//! short-circuits before spawning: it prints the would-be commands and
//! returns 0.

use crate::run::{execute, execute_with_env, write_record};

/// Args (after the program) for the release-evidence script.
///
/// The script takes boolean mode flags only; the `<version>` from
/// `evidence --release <version>` is CLI metadata (the bundle keeps its own
/// naming per the design), so it intentionally does not appear here. The
/// bundle always builds from a detached clean checkout at HEAD (never the
/// possibly-dirty worktree), so the artifact's commit identity stands on
/// its own.
fn evidence_argv(_release: &str, checkout: &str) -> Vec<String> {
    vec![
        "tools/gen_release_evidence.sh".to_string(),
        "--release".to_string(),
        "--locked".to_string(),
        "--clean-checkout".to_string(),
        checkout.to_string(),
    ]
}

/// Full command (program first) for the Lemma wire capture.
fn capture_argv(source: &str, home: &str, out: &str) -> Vec<String> {
    vec![
        "node".to_string(),
        "tools/capture_lemma.mjs".to_string(),
        "--repo".to_string(),
        source.to_string(),
        "--home".to_string(),
        home.to_string(),
        "--out".to_string(),
        out.to_string(),
    ]
}

/// Full command (program first) for the benchmark harness.
fn benchmark_argv(manifest: &str, out: &str, limit: Option<u64>) -> Vec<String> {
    let mut argv = vec![
        "python3".to_string(),
        "tools/bench_against_ltmrs.py".to_string(),
        "--ops".to_string(),
        manifest.to_string(),
        "--out".to_string(),
        out.to_string(),
    ];
    if let Some(n) = limit {
        argv.push("--limit".to_string());
        argv.push(n.to_string());
    }
    argv
}

/// Validate the `quality --split` value.
///
/// Only `"heldout"` and `"dev"` resolve; anything else is a hard error
/// naming the token.
fn run_quality_parts(split: &str) -> Result<String, String> {
    match split {
        "heldout" | "dev" => Ok(split.to_string()),
        _ => Err(format!(
            "unknown split '{split}' (expected 'heldout' or 'dev')"
        )),
    }
}

/// Command (program plus args after it) for the calibration-corpus generator.
///
/// The script takes zero args per its `__main__`.
fn quality_corpus_command() -> (String, Vec<String>) {
    (
        "python3".to_string(),
        vec!["tools/gen_calibration_corpus.py".to_string()],
    )
}

/// Honesty note for a split the wave script only half-honors: the corpus
/// generates both splits, but the wave filters to heldout internally, so
/// `--split dev` silently evaluates heldout. Callers print this when present.
fn quality_split_note(split: &str) -> Option<&'static str> {
    if split == "dev" {
        Some(
            "warning: --split dev runs the heldout-filtered wave (the wave script \
             filters to heldout internally); the corpus still generates both splits",
        )
    } else {
        None
    }
}
/// Command plus child-scoped env for the quality wave.
///
/// `probe_home` becomes `PROBE_HOME` for the child only via
/// [`crate::run::execute_with_env`], never the developer's real HOME.
/// `RATER_MODEL` passes through from the environment untouched by
/// inheritance.
fn quality_wave_command(
    probe_home: &str,
    _split: &str,
) -> (String, Vec<String>, Vec<(String, String)>) {
    (
        "python3".to_string(),
        vec!["tools/agent_quality_wave.py".to_string()],
        vec![("PROBE_HOME".to_string(), probe_home.to_string())],
    )
}

/// Workspace root, derived from this crate's manifest dir at compile time.
///
/// All `tools/` script paths and `reports/` output anchor here so every verb
/// behaves identically no matter which subdirectory invokes it. User-supplied
/// paths (`--manifest`, `--source`) pass through untouched, i.e. relative to
/// the invoker's working directory.
fn workspace_root() -> std::path::PathBuf {
    let raw = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    // Canonicalize away the `..` so printed paths are clean; fall back to
    // the raw join if the filesystem disagrees (cannot happen for a real
    // checkout, since the manifest dir always exists).
    std::fs::canonicalize(&raw).unwrap_or(raw)
}

fn report_dir(verb: &str) -> Result<std::path::PathBuf, String> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let pid = std::process::id();
    Ok(workspace_root().join(format!("reports/xtask-{verb}-{now_ms}-{pid}")))
}

fn print_dry_run(program: &str, args: &[String]) {
    let words = std::iter::once(program.to_string())
        .chain(args.iter().cloned())
        .map(|w| {
            if w.contains(' ') {
                format!("'{}'", w.replace('\'', "'\\''"))
            } else {
                w
            }
        })
        .collect::<Vec<_>>();
    println!("would run: {}", words.join(" "));
}

/// Build a full child argv (program first) from literal words.
fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_string()).collect()
}

fn finish(
    verb: &str,
    argv: &[String],
    outcome: &crate::run::RunOutcome,
    dir: &std::path::Path,
    meta: Option<&serde_json::Value>,
) -> Result<i32, String> {
    write_record(dir, verb, argv, outcome, meta)
        .map_err(|e| format!("cannot write run-record: {e}"))?;
    if outcome.exit_code != 0 {
        eprintln!("{}", outcome.stderr_tail);
    }
    Ok(outcome.exit_code)
}

/// Build the release evidence bundle (or print the command with `dry_run`).
///
/// Live runs provision a throwaway checkout dir and pass it as
/// `--clean-checkout`, so the script generates from a detached HEAD
/// worktree; dry-run prints `<checkout-tmpdir>` where it would go.
pub fn run_evidence(release: &str, dry_run: bool) -> Result<i32, String> {
    let program = "bash";
    if dry_run {
        print_dry_run(program, &evidence_argv(release, "<checkout-tmpdir>"));
        return Ok(0);
    }
    let checkout =
        tempfile::tempdir().map_err(|e| format!("cannot provision checkout dir: {e}"))?;
    let checkout_str = checkout
        .path()
        .to_str()
        .ok_or("checkout path is not UTF-8")?
        .to_string();
    let args = evidence_argv(release, &checkout_str);
    let workdir = workspace_root();
    let outcome = execute(program, &args, &workdir);
    let dir = report_dir("evidence")?;
    let mut argv = vec![program.to_string()];
    argv.extend(args);
    // The CLI release is metadata only (the bundle keeps its own naming),
    // but the run-record keeps it so the design's "version recorded"
    // promise holds.
    let meta = serde_json::json!({"release": release});
    finish("evidence", &argv, &outcome, &dir, Some(&meta))
}

/// Capture a Lemma baseline into a sandbox (or print the command).
///
/// The capture product lives at `<report-dir>/baseline` (archived with the
/// run-record); only the throwaway sandbox HOME stays in a tempdir. Dry-run
/// provisions nothing and prints `<sandbox-tmpdir>` where a fresh tempdir
/// would go.
pub fn run_capture(source: &str, dry_run: bool) -> Result<i32, String> {
    let dir = report_dir("capture-lemma")?;
    let out_str = dir
        .join("baseline")
        .to_str()
        .ok_or("capture out path is not UTF-8")?
        .to_string();
    if dry_run {
        let argv = capture_argv(source, "<sandbox-tmpdir>", &out_str);
        let (program, args) = argv
            .split_first()
            .ok_or("capture argv is empty".to_string())?;
        print_dry_run(program, args);
        return Ok(0);
    }
    let home = tempfile::tempdir().map_err(|e| format!("cannot provision sandbox HOME: {e}"))?;
    let home_str = home
        .path()
        .to_str()
        .ok_or("sandbox HOME path is not UTF-8")?;
    std::fs::create_dir_all(dir.join("baseline"))
        .map_err(|e| format!("cannot create capture out dir: {e}"))?;
    let argv = capture_argv(source, home_str, &out_str);
    let (program, args) = argv
        .split_first()
        .ok_or("capture argv is empty".to_string())?;
    let workdir = workspace_root();
    let outcome = execute_with_env(program, args, &workdir, &[("HOME", home_str)]);
    finish("capture-lemma", &argv, &outcome, &dir, None)
}

/// Result file inside a benchmark report dir.
///
/// The harness opens `--out` for writing (file semantics), so the runner
/// must pass a file path, never the report directory itself.
fn benchmark_out_file(report_dir: &std::path::Path) -> std::path::PathBuf {
    report_dir.join("result.jsonl")
}

/// Run the benchmark harness with a sandboxed `HOME` (or print the command).
pub fn run_benchmark(manifest: &str, limit: Option<u64>, dry_run: bool) -> Result<i32, String> {
    let dir = report_dir("benchmark")?;
    let out_str = benchmark_out_file(&dir)
        .to_str()
        .ok_or("reports dir path is not UTF-8")?
        .to_string();
    let argv = benchmark_argv(manifest, &out_str, limit);
    let (program, args) = argv
        .split_first()
        .ok_or("benchmark argv is empty".to_string())?;
    if dry_run {
        print_dry_run(program, args);
        return Ok(0);
    }
    let sandbox = tempfile::tempdir().map_err(|e| format!("cannot provision sandbox HOME: {e}"))?;
    let sandbox_str = sandbox
        .path()
        .to_str()
        .ok_or("sandbox HOME path is not UTF-8")?;
    let workdir = workspace_root();
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create reports dir: {e}"))?;
    let outcome = execute_with_env(program, args, &workdir, &[("HOME", sandbox_str)]);
    finish("benchmark", &argv, &outcome, &dir, None)
}

/// Run the calibration corpus + quality wave pipeline (or print both commands).
///
/// Model inference requires pre-provisioned artifacts/rater access (same
/// precondition as running the scripts by hand): the corpus step is local
/// and deterministic, while the wave step calls the configured rater
/// endpoint.
pub fn run_quality(split: &str, dry_run: bool) -> Result<i32, String> {
    let split = run_quality_parts(split)?;
    if let Some(note) = quality_split_note(&split) {
        eprintln!("{note}");
    }
    let (corpus_prog, corpus_args) = quality_corpus_command();
    if dry_run {
        let (wave_prog, wave_args, _) = quality_wave_command("<sandbox-tmpdir>", &split);
        print_dry_run(&corpus_prog, &corpus_args);
        print_dry_run(&wave_prog, &wave_args);
        return Ok(0);
    }
    let probe =
        tempfile::tempdir().map_err(|e| format!("cannot provision sandbox PROBE_HOME: {e}"))?;
    let probe_str = probe
        .path()
        .to_str()
        .ok_or("sandbox PROBE_HOME path is not UTF-8")?;
    let (wave_prog, wave_args, wave_env) = quality_wave_command(probe_str, &split);
    let workdir = workspace_root();
    let dir = report_dir("quality")?;
    let mut corpus_argv = vec![corpus_prog.clone()];
    corpus_argv.extend(corpus_args.clone());
    let corpus_outcome = execute(&corpus_prog, &corpus_args, &workdir);
    let corpus_code = finish(
        "quality",
        &corpus_argv,
        &corpus_outcome,
        &dir.join("corpus"),
        None,
    )?;
    if corpus_code != 0 {
        return Ok(corpus_code);
    }
    let mut wave_argv = vec![wave_prog.clone()];
    wave_argv.extend(wave_args.clone());
    let env_refs: Vec<(&str, &str)> = wave_env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let wave_outcome = execute_with_env(&wave_prog, &wave_args, &workdir, &env_refs);
    finish(
        "quality",
        &wave_argv,
        &wave_outcome,
        &dir.join("wave"),
        None,
    )
}

/// Validate the `capabilities --candidate` value and build its child commands
/// (each a full argv, program first).
///
/// Only `"fjall-lance"` resolves; anything else (including the deleted
/// `"lance"` losing path) is a hard error naming the token.
fn capability_commands(candidate: &str) -> Result<Vec<Vec<String>>, String> {
    match candidate {
        "fjall-lance" => Ok(vec![
            argv(&[
                "cargo",
                "test",
                "-p",
                "ltmrs-service",
                "--",
                "namespace",
                "restore",
                "receipt",
                "contention",
                "durable",
                "barrier",
            ]),
            argv(&[
                "cargo",
                "test",
                "-p",
                "ltmrs-search",
                "--",
                "fts",
                "backend",
                "projector",
                "table",
            ]),
        ]),
        _ => Err(format!(
            "capabilities candidate removed post-AD-01 (losing path deleted); \
             only fjall-lance resolves (unknown candidate '{candidate}')"
        )),
    }
}

/// Validate the `conformance --profile` value and build its child commands
/// (each a full argv, program first).
///
/// Only `"lemma-0.21.0"` resolves; anything else is a hard error naming the
/// token.
fn conformance_commands(profile: &str) -> Result<Vec<Vec<String>>, String> {
    match profile {
        "lemma-0.21.0" => Ok(vec![
            argv(&["cargo", "test", "-p", "ltmrs-compat"]),
            argv(&["cargo", "test", "-p", "ltmrs-daemon", "--", "differential"]),
        ]),
        _ => Err(format!(
            "unknown profile '{profile}' (expected 'lemma-0.21.0')"
        )),
    }
}

/// Validate the `recovery --suite` value and build its child commands (each
/// a full argv, program first).
///
/// Only `"durable"` resolves; anything else is a hard error naming the
/// token.
fn recovery_commands(suite: &str) -> Result<Vec<Vec<String>>, String> {
    match suite {
        "durable" => Ok(vec![
            argv(&[
                "cargo",
                "test",
                "-p",
                "ltmrs-service",
                "--",
                "restore",
                "replay",
                "kill",
                "soak",
                "fragmented",
            ]),
            argv(&["cargo", "test", "-p", "ltmrs-interchange", "--", "restore"]),
            argv(&["cargo", "test", "--test", "daemon_lifecycle"]),
        ]),
        _ => Err(format!("unknown suite '{suite}' (expected 'durable')")),
    }
}

/// Run-record subdir names for the capabilities children, in command order.
const CAPABILITY_NAMES: &[&str] = &["service", "search"];
/// Run-record subdir names for the conformance children, in command order.
const CONFORMANCE_NAMES: &[&str] = &["compat", "differential"];
/// Run-record subdir names for the recovery children, in command order.
const RECOVERY_NAMES: &[&str] = &["service", "interchange", "daemon-lifecycle"];

/// Run child commands in order under one `reports/xtask-<verb>-<timestamp>/`
/// dir, stopping at the first failure and returning its code.
///
/// Each child gets its own run-record entry in `dir/<name>`. Commands and
/// names pair one-to-one; a length skew panics rather than silently
/// dropping a suite. With `dry_run`, prints every would-be command and
/// executes nothing (creating nothing).
fn run_children(
    verb: &str,
    commands: &[Vec<String>],
    names: &[&str],
    dry_run: bool,
) -> Result<i32, String> {
    assert_eq!(
        commands.len(),
        names.len(),
        "run_children commands/names length skew for verb '{verb}'"
    );
    if dry_run {
        for argv in commands {
            let (program, args) = argv.split_first().ok_or(format!("{verb} argv is empty"))?;
            print_dry_run(program, args);
        }
        return Ok(0);
    }
    let workdir = workspace_root();
    let dir = report_dir(verb)?;
    for (argv, name) in commands.iter().zip(names.iter()) {
        let (program, args) = argv.split_first().ok_or(format!("{verb} argv is empty"))?;
        let outcome = execute(program, args, &workdir);
        let code = finish(verb, argv, &outcome, &dir.join(name), None)?;
        if code != 0 {
            return Ok(code);
        }
    }
    Ok(0)
}

/// Run the backend-gate capability suites (or print both commands).
pub fn run_capabilities(candidate: &str, dry_run: bool) -> Result<i32, String> {
    let commands = capability_commands(candidate)?;
    run_children("capabilities", &commands, CAPABILITY_NAMES, dry_run)
}

/// Run the compat + differential conformance suites (or print both commands).
pub fn run_conformance(profile: &str, dry_run: bool) -> Result<i32, String> {
    let commands = conformance_commands(profile)?;
    run_children("conformance", &commands, CONFORMANCE_NAMES, dry_run)
}

/// Run the restore/kill/reopen recovery suites (or print all three commands).
pub fn run_recovery(suite: &str, dry_run: bool) -> Result<i32, String> {
    let commands = recovery_commands(suite)?;
    run_children("recovery", &commands, RECOVERY_NAMES, dry_run)
}

#[cfg(test)]
mod tests {
    use super::{
        CAPABILITY_NAMES, CONFORMANCE_NAMES, RECOVERY_NAMES, benchmark_argv, benchmark_out_file,
        capability_commands, capture_argv, conformance_commands, evidence_argv,
        quality_wave_command, recovery_commands, run_quality_parts,
    };

    #[test]
    fn evidence_argv_is_locked_release_from_clean_checkout() {
        assert_eq!(
            evidence_argv("v0.1", "/tmp/checkout-XYZ"),
            vec![
                "tools/gen_release_evidence.sh",
                "--release",
                "--locked",
                "--clean-checkout",
                "/tmp/checkout-XYZ"
            ]
        );
    }

    #[test]
    fn capture_argv_provisions_sandbox_paths() {
        let argv = capture_argv("/src/lemma", "/tmp/home-XYZ", "/tmp/out-XYZ");
        assert_eq!(argv[0], "node");
        assert!(argv.windows(2).any(|w| w == ["--repo", "/src/lemma"]));
    }

    #[test]
    fn benchmark_argv_passes_manifest_and_limit() {
        let argv = benchmark_argv("ops.json", "/tmp/out-XYZ/result.jsonl", None);
        assert_eq!(argv[0], "python3");
        assert!(argv.windows(2).any(|w| w == ["--ops", "ops.json"]));
        assert!(!argv.iter().any(|a| a == "--limit"));
        let argv = benchmark_argv("ops.json", "/tmp/out-XYZ/result.jsonl", Some(50));
        assert!(argv.windows(2).any(|w| w == ["--limit", "50"]));
    }

    #[test]
    fn benchmark_out_is_a_file_inside_the_report_dir() {
        let out = benchmark_out_file(std::path::Path::new("/tmp/r"));
        assert_eq!(out, std::path::Path::new("/tmp/r/result.jsonl"));
    }

    #[test]
    fn dry_run_returns_zero_without_executing() {
        assert_eq!(super::run_evidence("v0.1", true).unwrap(), 0);
        assert_eq!(super::run_capture("/src/lemma", true).unwrap(), 0);
        assert_eq!(super::run_benchmark("ops.json", None, true).unwrap(), 0);
        assert_eq!(super::run_quality("heldout", true).unwrap(), 0);
        assert_eq!(super::run_capabilities("fjall-lance", true).unwrap(), 0);
        assert_eq!(super::run_conformance("lemma-0.21.0", true).unwrap(), 0);
        assert_eq!(super::run_recovery("durable", true).unwrap(), 0);
    }

    #[test]
    fn capabilities_argv_pins_backend_suites() {
        let cmds = capability_commands("fjall-lance").unwrap();
        assert_eq!(cmds.len(), 2);
        assert_eq!(CAPABILITY_NAMES.len(), 2);
        assert!(
            cmds.iter()
                .any(|c| c.contains(&"-p".to_string()) && c.contains(&"ltmrs-service".to_string()))
        );
        assert!(capability_commands("lance").is_err());
    }

    #[test]
    fn conformance_argv_pins_compat_and_differential() {
        let cmds = conformance_commands("lemma-0.21.0").unwrap();
        assert_eq!(cmds.len(), 2);
        assert_eq!(CONFORMANCE_NAMES.len(), 2);
        assert!(conformance_commands("lemma-0.22.0").is_err());
    }

    #[test]
    fn recovery_argv_pins_restore_suites() {
        let cmds = recovery_commands("durable").unwrap();
        assert_eq!(cmds.len(), 3);
        assert_eq!(RECOVERY_NAMES.len(), 3);
        assert!(recovery_commands("flaky").is_err());
    }

    #[test]
    fn quality_rejects_unknown_split() {
        assert!(run_quality_parts("prod").is_err());
    }

    #[test]
    fn dev_split_warns_but_heldout_stays_silent() {
        let note = super::quality_split_note("dev").expect("dev must warn");
        assert!(note.contains("heldout"), "note must name heldout: {note}");
        assert!(super::quality_split_note("heldout").is_none());
    }

    #[test]
    fn workspace_root_anchors_tools_and_reports() {
        let root = super::workspace_root();
        assert!(
            root.join("Cargo.toml").is_file(),
            "root must hold the workspace manifest: {}",
            root.display()
        );
        assert!(
            root.join("tools/gen_release_evidence.sh").is_file(),
            "root must anchor tools scripts: {}",
            root.display()
        );
    }
    #[test]
    fn quality_wave_env_uses_sandbox_probe_home() {
        let (prog, args, env) = quality_wave_command("/tmp/probe-XYZ", "heldout");
        assert_eq!(prog, "python3");
        assert!(args.contains(&"tools/agent_quality_wave.py".to_string()));
        assert_eq!(
            env.iter().find(|(k, _)| k == "PROBE_HOME"),
            Some(&("PROBE_HOME".to_string(), "/tmp/probe-XYZ".to_string()))
        );
    }
}
