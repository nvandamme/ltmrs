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

/// Light crates for the quick test tier: no candle/lance/arrow/rmcp in
/// their closures, so the tier builds and runs in seconds even cold.
const TEST_QUICK_CRATES: &[&str] = &[
    "ltmrs-domain",
    "ltmrs-service",
    "ltmrs-compat",
    "ltmrs-interchange",
];

/// Workspace members selectable via `test --package` (cargo package names).
const TEST_MEMBERS: &[&str] = &[
    "ltmrs",
    "ltmrs-domain",
    "ltmrs-service",
    "ltmrs-compat",
    "ltmrs-embeddings",
    "ltmrs-search",
    "ltmrs-interchange",
    "ltmrs-daemon",
    "ltmrs-frontend",
    "xtask",
];

/// Validate the `test` tier/package selection and build its child commands
/// (each entry is a full argv). Quick tier: light-crate lib suites only.
/// Full tier: the workspace gate. Package: that member's full test target.
fn test_commands(
    tier: &str,
    package: Option<&str>,
) -> Result<(Vec<Vec<String>>, Vec<String>), String> {
    if let Some(pkg) = package {
        if tier == "full" {
            return Err(
                "--package selects a single crate; omit --tier (or drop --package) for the full workspace gate"
                    .to_string(),
            );
        }
        if !TEST_MEMBERS.contains(&pkg) {
            return Err(format!(
                "unknown package '{pkg}' (expected one of: {})",
                TEST_MEMBERS.join(", ")
            ));
        }
        return Ok((
            vec![argv(&["cargo", "test", "-p", pkg])],
            vec![pkg.to_string()],
        ));
    }
    match tier {
        "quick" => Ok((
            TEST_QUICK_CRATES
                .iter()
                .map(|c| argv(&["cargo", "test", "-p", c, "--lib"]))
                .collect(),
            TEST_QUICK_CRATES
                .iter()
                .map(|c| c.strip_prefix("ltmrs-").unwrap_or(c).to_string())
                .collect(),
        )),
        "full" => Ok((
            vec![argv(&["cargo", "test", "--workspace"])],
            vec!["workspace".to_string()],
        )),
        other => Err(format!(
            "invalid tier '{other}' (expected 'quick' or 'full')"
        )),
    }
}

/// Tiered test runner (iteration speed): quick lib suites, the full
/// workspace gate, or one member's tests. Prints the tier banner on real
/// runs so a quick run is never mistaken for the gate.
pub fn run_test(tier: &str, package: Option<&str>, dry_run: bool) -> Result<i32, String> {
    let (commands, names) = test_commands(tier, package)?;
    if !dry_run {
        if package.is_some() {
            println!("test tier: single package (explicit scope, not the gate)");
        } else if tier == "full" {
            println!("test tier: full workspace gate");
        } else {
            println!("test tier: quick (light-crate lib suites; --tier full for the gate)");
        }
    }
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    run_children("test", &commands, &name_refs, dry_run)
}

/// T-BUILD-01 (RQ-21) deny tokens matched against workspace tree package
/// names. Families: Python (`python|pyo3`), Node (`napi|node`),
/// ONNX/foreign runtimes (`onnx|onnxruntime|ort|tract`), GGUF (`gguf`),
/// C++ database engines and client shims
/// (`rusqlite|libsqlite3-sys|sqlite|mysql|postgres|pq-sys|sqlx|diesel`),
/// GPU-only stacks
/// (`cuda|cudnn|cublas|nvrtc|nccl|mkl|openblas|metal|accelerate|vulkan|
/// libtorch|torch-sys`). Matching (see [`dist_audit_check_tree_packages`])
/// is component/prefix based, never bare substring: `winapi-util`
/// (Windows-only, in Cargo.lock but absent from the host tree) contains
/// `napi` yet never matches. `rusqlite` is allowed ONLY in the excluded
/// `crates/lemma-import` leaf: its presence anywhere in THIS workspace tree
/// is a violation, proven by the `cargo tree -i` inverse.
pub fn dist_audit_deny_tokens() -> &'static [&'static str] {
    &[
        "python",
        "pyo3",
        "napi",
        "node",
        "onnx",
        "onnxruntime",
        "ort",
        "tract",
        "gguf",
        "rusqlite",
        "libsqlite3-sys",
        "sqlite",
        "mysql",
        "postgres",
        "pq-sys",
        "sqlx",
        "diesel",
        "cuda",
        "cudnn",
        "cublas",
        "nvrtc",
        "nccl",
        "mkl",
        "openblas",
        "metal",
        "accelerate",
        "vulkan",
        "libtorch",
        "torch-sys",
    ]
}

/// Parse `cargo tree --prefix none` output into package names: the first
/// whitespace-separated token of every line that names a versioned package.
pub fn dist_audit_tree_package_names(tree: &str) -> Vec<String> {
    tree.lines()
        .filter_map(|line| {
            let line = line.trim();
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            // Versioned package lines look like `name v1.2.3 ...`.
            let version = parts.next()?;
            if version.starts_with('v') && version[1..].starts_with(|c: char| c.is_ascii_digit()) {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Check parsed package names against the deny tokens. A package violates
/// iff its lowercase name equals a token, starts with a token (fused names
/// like `onnxruntime`, `cudart-sys`), or has a token as a hyphen-separated
/// component (`tract-onnx`, `napi-derive`). Bare-substring matching is
/// deliberately NOT used: `winapi-util` contains `napi` and must stay clean.
pub fn dist_audit_check_tree_packages(names: &[&str]) -> Result<String, String> {
    let mut hits = Vec::new();
    for name in names {
        let lower = name.to_lowercase();
        for token in dist_audit_deny_tokens() {
            if lower == *token
                || lower.starts_with(token)
                || lower.split('-').any(|comp| comp == *token)
            {
                hits.push(format!("{name} (token: {token})"));
                break;
            }
        }
    }
    if hits.is_empty() {
        Ok(format!(
            "no forbidden families in {} graphed packages",
            names.len()
        ))
    } else {
        Err(format!(
            "forbidden dependency families present:\n{}",
            hits.join("\n")
        ))
    }
}

/// Extract the pinned artifact filenames from an artifact-manifest source:
/// the key of every `digests.insert("name".into(), ...)` entry, in order.
/// This reads the REAL pin (e.g. `crates/embeddings/src/manifest.rs`), so
/// the model-dir check judges the actual provision contract, never a
/// hardcoded literal.
pub fn dist_audit_manifest_files(src: &str) -> Vec<String> {
    let mut files = Vec::new();
    let mut armed = false;
    for line in src.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("digests.insert(") {
            armed = true;
            // Single-line form: digests.insert("name".into(), ...).
            if let Some(name) = first_quoted_after(trimmed, "digests.insert(") {
                files.push(name);
                armed = false;
            }
            continue;
        }
        if armed {
            if let Some(name) = first_quoted(trimmed) {
                files.push(name);
                armed = false;
            } else if trimmed.starts_with(')') || trimmed.starts_with('}') {
                armed = false;
            }
        }
    }
    files
}

/// First `"..."` string after `marker` on the line, if any.
fn first_quoted_after(line: &str, marker: &str) -> Option<String> {
    let at = line.find(marker)? + marker.len();
    first_quoted(&line[at..])
}

/// First `"..."` string on the line, if the line opens with a quote
/// (after whitespace) — the shape of a manifest key entry.
fn first_quoted(line: &str) -> Option<String> {
    let rest = line.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Zero-tolerance scan for remote-namespace API usage in serving code.
/// The reqwest-string scan is blind to API usage, so this complements it:
/// any `lance_namespace` / remote-namespace / object-store-URL site in
/// production sources fails loudly. (`connect_remote` in serve.rs is the
/// local-IPC Unix-socket client and matches none of these tokens.)
pub fn dist_audit_check_remote_api_sites(sites: &[&str]) -> Result<String, String> {
    if sites.is_empty() {
        Ok("no lance-remote/namespace/object-store API usage in serving code".to_string())
    } else {
        Err(format!(
            "remote-namespace API usage outside provisioning/test code:\n{}",
            sites.join("\n")
        ))
    }
}

/// Check the (already grep-filtered) `cargo tree` hits: empty means the
/// forbidden families are absent; any line is a violation naming the hit.
pub fn dist_audit_check_tree_hits(grep_stdout: &str) -> Result<String, String> {
    if grep_stdout.trim().is_empty() {
        Ok("no forbidden families in the workspace dependency graph".to_string())
    } else {
        Err(format!(
            "forbidden dependency families present:\n{}",
            grep_stdout.trim()
        ))
    }
}

/// Inverse proof for the excluded leaf: `cargo tree -i rusqlite` run inside
/// the main workspace must FAIL (nonzero exit, "did not match any packages"
/// on stderr). Exit 0 with a tree means rusqlite leaked into the workspace.
pub fn dist_audit_check_rusqlite_inverse(exit_code: i32, output: &str) -> Result<String, String> {
    if exit_code != 0 {
        Ok("rusqlite is absent from the main workspace graph (excluded leaf only)".to_string())
    } else {
        Err(format!(
            "rusqlite resolves inside the main workspace (must live only in excluded crates/lemma-import):\n{}",
            output.trim()
        ))
    }
}

/// Every `reqwest` use site must be an explicit-provisioning path.
/// Known-good (provision-only): the artifact downloader
/// (`crates/embeddings/src/artifacts.rs`) and the `--provision-models`
/// command (`crates/frontend/src/cli.rs`). Anything else — daemon, service,
/// search hot paths, or unknown files — fails closed.
pub fn dist_audit_check_reqwest_sites(sites: &[&str]) -> Result<String, String> {
    const PROVISION_ONLY: &[&str] = &[
        "crates/embeddings/src/artifacts.rs",
        "crates/frontend/src/cli.rs",
    ];
    let mut violations = Vec::new();
    for site in sites {
        let path = site.split(':').next().unwrap_or(site);
        if !PROVISION_ONLY.contains(&path) {
            violations.push(site.to_string());
        }
    }
    if violations.is_empty() {
        Ok(format!(
            "reqwest call sites ({}): all provision-only",
            sites.len()
        ))
    } else {
        Err(format!(
            "reqwest used outside explicit provisioning:\n{}",
            violations.join("\n")
        ))
    }
}

/// The pinned model directory must contain `model.safetensors`; its absence
/// (e.g. an ONNX/GGUF file in its place) fails closed — no foreign weight
/// format is ever substituted (the loader opens exactly one filename).
pub fn dist_audit_check_model_dir(files: &[&str]) -> Result<String, String> {
    if files.contains(&"model.safetensors") {
        Ok("model dir carries the pinned model.safetensors".to_string())
    } else {
        Err(format!(
            "model dir lacks model.safetensors (foreign formats are never substituted; found: {})",
            files.join(", ")
        ))
    }
}

/// Source-level loader pin: every `model.<ext>` weight reference inside
/// `crates/embeddings` must be exactly `model.safetensors`. An
/// `model.onnx`/`model.gguf`/… reference fails the audit.
pub fn dist_audit_check_loader_pins(weight_refs: &[&str]) -> Result<String, String> {
    let mut violations = Vec::new();
    for r in weight_refs.iter().copied() {
        // A ref is a violation when it names model.<ext> with ext != safetensors.
        let mut rest = r;
        while let Some(i) = rest.find("model.") {
            let after = &rest[i + "model.".len()..];
            let ext: String = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            if !ext.is_empty() && ext != "safetensors" {
                violations.push(format!("model.{ext} in: {r}"));
                break;
            }
            rest = after;
        }
    }
    if violations.is_empty() {
        Ok(format!(
            "weight loader pins model.safetensors ({} refs checked)",
            weight_refs.len()
        ))
    } else {
        Err(format!(
            "non-safetensors weight reference in loader sources:\n{}",
            violations.join("\n")
        ))
    }
}

/// Remove `#[cfg(test)] mod <name> { ... }` regions from Rust source so
/// audit scans only judge production code. Test fixtures may name foreign
/// formats to prove they are ignored; that is evidence, not a violation.
/// Balanced-brace skipping ignores braces inside strings (regular, byte and
/// raw with any hash count), char literals and both comment styles.
pub fn dist_audit_strip_test_modules(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    while let Some(attr_at) = rest.find("#[cfg(test)]") {
        out.push_str(&rest[..attr_at]);
        let after_attr = &rest[attr_at + "#[cfg(test)]".len()..];
        match dist_audit_strip_one_test_mod(after_attr) {
            Some(consumed) => rest = &after_attr[consumed..],
            None => {
                // Not a test module after all: keep the attribute, continue.
                out.push_str("#[cfg(test)]");
                rest = after_attr;
            }
        }
    }
    out.push_str(rest);
    out
}

/// If `src` continues (after whitespace/comments) with `mod <ident> {`,
/// return the byte length through the matching close brace. Else None.
fn dist_audit_strip_one_test_mod(src: &str) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut i = 0;
    // Skip whitespace and comments.
    loop {
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if src[i..].starts_with("//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if src[i..].starts_with("/*") {
            let end = src[i..].find("*/")?;
            i += end + 2;
            continue;
        }
        break;
    }
    // Expect the `mod` keyword with word boundaries.
    if !src[i..].starts_with("mod") {
        return None;
    }
    let after_mod = i + 3;
    if after_mod < bytes.len()
        && ((bytes[after_mod] as char).is_alphanumeric() || bytes[after_mod] == b'_')
    {
        return None;
    }
    i = after_mod;
    while i < bytes.len() && (bytes[i] as char).is_whitespace() {
        i += 1;
    }
    // Expect an identifier, then the opening brace.
    let start_ident = i;
    while i < bytes.len() && ((bytes[i] as char).is_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    if i == start_ident {
        return None;
    }
    while i < bytes.len() && (bytes[i] as char).is_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'{' {
        return None;
    }
    // Consume balanced braces from the opening brace.
    let mut depth = 0usize;
    let mut j = i;
    while j < bytes.len() {
        let c = bytes[j] as char;
        if c == '"' || (c == 'r' && is_raw_string_start(&src[j..])) {
            j += skip_string(&src[j..])?;
            continue;
        }
        if c == 'b' && src[j..].starts_with("b\"") {
            j += skip_string(&src[j + 1..])? + 1;
            continue;
        }
        if c == '\'' {
            j += skip_char_literal(&src[j..])?;
            continue;
        }
        if c == '/' && src[j..].starts_with("//") {
            while j < bytes.len() && bytes[j] != b'\n' {
                j += 1;
            }
            continue;
        }
        if c == '/' && src[j..].starts_with("/*") {
            let end = src[j..].find("*/")?;
            j += end + 2;
            continue;
        }
        if c == '{' {
            depth += 1;
        } else if c == '}' {
            depth -= 1;
            if depth == 0 {
                return Some(j + 1);
            }
        }
        j += 1;
    }
    None
}

/// True when `src` starts a raw string: `r"`, `r#"..."#`, `r##"..."##`, …
fn is_raw_string_start(src: &str) -> bool {
    let mut hashes = 0usize;
    let bytes = src.as_bytes();
    if bytes.first() != Some(&b'r') {
        return false;
    }
    let mut i = 1;
    while i < bytes.len() && bytes[i] == b'#' {
        hashes += 1;
        i += 1;
    }
    i < bytes.len() && bytes[i] == b'"' && {
        let _ = hashes;
        true
    }
}

/// Byte length through the string starting at `src` (opening quote or the
/// `r` of a raw string). Handles escapes in cooked strings; raw strings end
/// at the matching quote-plus-hashes.
fn skip_string(src: &str) -> Option<usize> {
    let bytes = src.as_bytes();
    if bytes.first() == Some(&b'r') {
        let mut i = 1;
        let mut hashes = 0usize;
        while i < bytes.len() && bytes[i] == b'#' {
            hashes += 1;
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'"' {
            return None;
        }
        i += 1;
        let closer = format!("\"{}", "#".repeat(hashes));
        let end = src[i..].find(&closer)?;
        return Some(i + end + closer.len());
    }
    // Cooked (or byte) string starting at `"`.
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Byte length through a char literal starting at `'`. Handles escapes.
fn skip_char_literal(src: &str) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'\'' => return Some(i + 1),
            b'\n' => return None,
            _ => i += 1,
        }
    }
    None
}

/// Collect `model.<ext>` weight-file mentions (with 1-based line numbers)
/// from source text: only known weight-file extensions count, so struct
/// field access (`model.id`, `model.device`, `model.url_for`) is ignored.
/// The pinned `model.safetensors` is collected by the caller separately
/// and never reported here.
pub fn dist_audit_find_forbidden_weight_refs(src: &str) -> Vec<String> {
    const WEIGHT_EXTS: &[&str] = &[
        "onnx",
        "gguf",
        "bin",
        "pt",
        "h5",
        "ckpt",
        "pkl",
        "safetensors",
    ];
    let mut refs = Vec::new();
    for (n, line) in src.lines().enumerate() {
        let mut rest = line;
        while let Some(i) = rest.find("model.") {
            let after = &rest[i + "model.".len()..];
            let ext: String = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            if WEIGHT_EXTS.contains(&ext.as_str()) && ext != "safetensors" {
                refs.push(format!("{}: {line}", n + 1));
                break;
            }
            rest = after;
        }
    }
    refs
}
/// A scan command must exit 0 for its (possibly empty) output to mean
/// "clean". grep exits 1 on no-match, so the pipelines normalize exactly
/// that case; any other nonzero exit means the scan itself broke and must
/// fail the audit loudly instead of reporting a vacuous pass.
pub fn dist_audit_require_scan_ok(exit_code: i32, label: &str) -> Result<(), String> {
    if exit_code == 0 {
        Ok(())
    } else {
        Err(format!(
            "{label} scan command failed (exit {exit_code}); refusing to report a clean audit"
        ))
    }
}

/// Scope note (I1): `cargo tree --workspace` resolves the HOST-platform
/// graph (Linux here). Target-specific deps for other platforms (e.g. the
/// `windows-*`/`winapi-util` rows in Cargo.lock) are never listed, so they
/// are out of scope of this check by construction — the full
/// multi-platform set lives in Cargo.lock. The check parses the COMPLETE
/// tree output from a file (never a truncated tail) into package names.
fn dist_audit_tree_redirect(report_dir: &std::path::Path) -> (String, std::path::PathBuf) {
    let tree_file = report_dir.join("tree.txt");
    let tree_str = tree_file.display().to_string();
    (
        format!(
            "cargo tree --locked --workspace --prefix none --no-dev-dependencies > '{tree_str}'"
        ),
        tree_file,
    )
}

/// Full `cargo tree -i` inverse command for the rusqlite exclusion proof.
fn dist_audit_rusqlite_argv() -> Vec<String> {
    argv(&[
        "cargo", "tree", "--locked", "-i", "rusqlite", "--prefix", "none",
    ])
}

/// Usage scan backing the T-SEC-04 no-egress pin: lance remote-namespace
/// and object-store URL tokens that must appear NOWHERE in production
/// sources (the reqwest-string scan cannot see API usage). grep exits 1 on
/// no-match; `|| test $? -eq 1` normalizes exactly that case so a real
/// grep failure (exit 2+) still fails the audit loudly.
fn dist_audit_remote_api_pipeline() -> String {
    "grep -rn \"lance_namespace\\|RemoteNamespace\\|remote_namespace\\|NamespaceClient\\|RestAdapter\\|\\.remote(\\|remote://\\|s3://\\|gs://\\|az://\" crates/embeddings/src crates/frontend/src crates/daemon/src crates/service/src crates/search/src crates/domain/src crates/compat/src crates/interchange/src --include=\"*.rs\" || test $? -eq 1"
        .to_string()
}

/// Source scan backing the T-SEC-04 egress pin: every `reqwest` use site,
/// classified provision-only vs hot-path by [`dist_audit_check_reqwest_sites`].
/// grep exits 1 on no-match; `|| test $? -eq 1` normalizes exactly that case
/// so a real grep failure (exit 2+) still fails the audit loudly.
fn dist_audit_reqwest_pipeline() -> String {
    "grep -rn \"reqwest\" crates/embeddings/src crates/frontend/src crates/daemon/src crates/service/src crates/search/src crates/domain/src crates/compat/src crates/interchange/src --include=\"*.rs\" || test $? -eq 1"
        .to_string()
}

/// Production sources whose weight-file references the loader pin judges.
/// Test modules are stripped before scanning, so fixtures proving foreign
/// formats are ignored stay evidence, not violations.
fn dist_audit_embeddings_sources(
    workdir: &std::path::Path,
) -> Result<Vec<std::path::PathBuf>, String> {
    let dir = workdir.join("crates/embeddings/src");
    let mut files = Vec::new();
    let entries =
        std::fs::read_dir(&dir).map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read dir entry: {e}"))?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Scan production code (test modules excluded) for `model.<ext>` weight
/// references, returned as `file:line: text` hits for the loader pin.
fn dist_audit_scan_loader_refs(workdir: &std::path::Path) -> Result<Vec<String>, String> {
    let mut hits = Vec::new();
    for path in dist_audit_embeddings_sources(workdir)? {
        let src = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let stripped = dist_audit_strip_test_modules(&src);
        let name = path
            .strip_prefix(workdir)
            .unwrap_or(&path)
            .display()
            .to_string();
        for r in dist_audit_find_forbidden_weight_refs(&stripped) {
            hits.push(format!("{name}:{r}"));
        }
        // Count pinned safetensors refs so the report proves the scan ran.
        for (n, line) in stripped.lines().enumerate() {
            if line.contains("model.safetensors") {
                hits.push(format!("{name}:{}: PINNED {line}", n + 1));
            }
        }
    }
    Ok(hits)
}

fn dist_audit_skills_pipeline() -> String {
    "grep -rn \"reqwest\\|http\" crates/frontend/src/skills --include=\"*.rs\" || test $? -eq 1"
        .to_string()
}

/// Heavyweight T-BUILD-01 proofs (full mode only, wall time measured):
/// the locked CPU release build and the offline-capable build proof.
/// Offline scope, stated honestly: `cargo build --locked --offline` proves
/// the locked workspace compiles with NO network from the local cargo
/// cache. It does NOT prove a fresh-machine install (no prefetch, no
/// vendor dir) — that remains unproven and is labeled as such wherever
/// this check is reported.
fn dist_audit_release_argv() -> Vec<String> {
    argv(&["cargo", "build", "--locked", "--release", "--workspace"])
}

fn dist_audit_offline_argv() -> Vec<String> {
    argv(&["cargo", "build", "--locked", "--offline", "--workspace"])
}

/// Run the T-BUILD-01 distribution audit (fast inspection, or full mode
/// with the locked release + offline build proofs).
///
/// Fast mode (seconds): full-graph deny check over the host-platform
/// tree file, rusqlite inverse, reqwest site classification,
/// lance-remote API usage scan, loader pin, manifest-backed model-dir
/// check, skills egress scan. Full mode adds the locked release build and
/// the offline build (local cache only — fresh-machine install unproven)
/// with measured wall times. `dry_run` prints every would-be
/// command and executes nothing.
pub fn run_dist_audit(full: bool, dry_run: bool) -> Result<i32, String> {
    let mode = if full { "full" } else { "fast" };
    let rusqlite_argv = dist_audit_rusqlite_argv();
    let reqwest_cmd = dist_audit_reqwest_pipeline();
    let remote_cmd = dist_audit_remote_api_pipeline();
    let skills_cmd = dist_audit_skills_pipeline();
    if dry_run {
        println!(
            "would write: <reports>/tree.txt via cargo tree --locked --workspace --prefix none"
        );
        let (prog, args) = rusqlite_argv
            .split_first()
            .ok_or("rusqlite argv is empty")?;
        print_dry_run(prog, args);
        print_dry_run("sh", &[vec!["-c".to_string()], vec![reqwest_cmd]].concat());
        print_dry_run("sh", &[vec!["-c".to_string()], vec![remote_cmd]].concat());
        println!("would scan: crates/embeddings/src/*.rs (test modules excluded)");
        println!("would parse: crates/embeddings/src/manifest.rs digest inserts");
        print_dry_run("sh", &[vec!["-c".to_string()], vec![skills_cmd]].concat());
        if full {
            let (prog, args) = dist_audit_release_argv()
                .split_first()
                .map(|(p, a)| (p.clone(), a.to_vec()))
                .ok_or("release argv is empty")?;
            print_dry_run(&prog, &args);
            let (prog, args) = dist_audit_offline_argv()
                .split_first()
                .map(|(p, a)| (p.clone(), a.to_vec()))
                .ok_or("offline argv is empty")?;
            print_dry_run(&prog, &args);
        }
        return Ok(0);
    }
    let workdir = workspace_root();
    let dir = report_dir("dist-audit")?;
    // The tree redirect below writes into the report dir before any
    // run-record exists, so the directory must be created up front
    // (write_record creates it only at the end).
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create reports dir {}: {e}", dir.display()))?;
    let started = std::time::Instant::now();
    let mut report: Vec<String> = Vec::new();
    let mut failed = false;

    let (tree_cmd, tree_file) = dist_audit_tree_redirect(&dir);
    let tree = execute("sh", &["-c".into(), tree_cmd.clone()], &workdir);
    if let Err(e) = dist_audit_require_scan_ok(tree.exit_code, "deny-graph") {
        failed = true;
        report.push(format!("deny-graph: FAIL ({e})"));
    } else {
        match std::fs::read_to_string(&tree_file) {
            Err(e) => {
                failed = true;
                report.push(format!("deny-graph: FAIL (cannot read tree file: {e})"));
            }
            Ok(tree_text) => {
                let names = dist_audit_tree_package_names(&tree_text);
                let count = names.len();
                let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
                match dist_audit_check_tree_packages(&refs) {
                    Ok(_) => report.push(format!(
                        "deny-graph: PASS (host-platform graph, {count} packages, component/prefix rule)"
                    )),
                    Err(e) => {
                        failed = true;
                        report.push(format!("deny-graph: FAIL ({e})"));
                    }
                }
            }
        }
    }

    let (rusqlite_prog, rusqlite_args) = rusqlite_argv
        .split_first()
        .ok_or("rusqlite argv is empty")?;
    let rusqlite = execute(rusqlite_prog, rusqlite_args, &workdir);
    let mut rusqlite_output = rusqlite.stdout_tail.clone();
    rusqlite_output.push_str(&rusqlite.stderr_tail);
    match dist_audit_check_rusqlite_inverse(rusqlite.exit_code, &rusqlite_output) {
        Ok(note) => report.push(format!("rusqlite-exclusion: PASS ({note})")),
        Err(e) => {
            failed = true;
            report.push(format!("rusqlite-exclusion: FAIL ({e})"));
        }
    }

    let reqwest = execute("sh", &["-c".into(), reqwest_cmd.clone()], &workdir);
    if let Err(e) = dist_audit_require_scan_ok(reqwest.exit_code, "reqwest-sites") {
        failed = true;
        report.push(format!("reqwest-sites: FAIL ({e})"));
    } else {
        let sites: Vec<&str> = reqwest
            .stdout_tail
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        match dist_audit_check_reqwest_sites(&sites) {
            Ok(note) => report.push(format!("reqwest-sites: PASS ({note})")),
            Err(e) => {
                failed = true;
                report.push(format!("reqwest-sites: FAIL ({e})"));
            }
        }
    }

    let remote = execute("sh", &["-c".into(), remote_cmd.clone()], &workdir);
    if let Err(e) = dist_audit_require_scan_ok(remote.exit_code, "remote-apis") {
        failed = true;
        report.push(format!("remote-apis: FAIL ({e})"));
    } else {
        let sites: Vec<&str> = remote
            .stdout_tail
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        match dist_audit_check_remote_api_sites(&sites) {
            Ok(note) => report.push(format!("remote-apis: PASS ({note})")),
            Err(e) => {
                failed = true;
                report.push(format!("remote-apis: FAIL ({e})"));
            }
        }
    }

    match dist_audit_scan_loader_refs(&workdir) {
        Ok(hits) => {
            let refs: Vec<&str> = hits.iter().map(|s| s.as_str()).collect();
            match dist_audit_check_loader_pins(&refs) {
                Ok(note) => report.push(format!("loader-pin: PASS ({note})")),
                Err(e) => {
                    failed = true;
                    report.push(format!("loader-pin: FAIL ({e})"));
                }
            }
        }
        Err(e) => {
            failed = true;
            report.push(format!("loader-pin: FAIL ({e})"));
        }
    }

    let skills = execute("sh", &["-c".into(), skills_cmd.clone()], &workdir);
    if let Err(e) = dist_audit_require_scan_ok(skills.exit_code, "skills-egress") {
        failed = true;
        report.push(format!("skills-egress: FAIL ({e})"));
    } else {
        match dist_audit_check_tree_hits(&skills.stdout_tail) {
            Ok(_) => {
                report.push("skills-egress: PASS (no reqwest/http in skill installer)".to_string())
            }
            Err(e) => {
                failed = true;
                report.push(format!("skills-egress: FAIL ({e})"));
            }
        }
    }

    // The model-dir check judges the REAL provision contract: the digest
    // table parsed from crates/embeddings/src/manifest.rs must contain
    // model.safetensors and no foreign weight file (C1: never a hardcoded
    // literal — the input comes from the actual pin file).
    let manifest_path = workdir.join("crates/embeddings/src/manifest.rs");
    match std::fs::read_to_string(&manifest_path) {
        Err(e) => {
            failed = true;
            report.push(format!("model-dir: FAIL (cannot read manifest pin: {e})"));
        }
        Ok(manifest_src) => {
            let files = dist_audit_manifest_files(&manifest_src);
            let refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
            match dist_audit_check_model_dir(&refs) {
                Ok(_) => report.push(format!(
                    "model-dir: PASS (manifest pin: {} files present; loader opens exactly these)",
                    files.len()
                )),
                Err(e) => {
                    failed = true;
                    report.push(format!("model-dir: FAIL ({e})"));
                }
            }
        }
    }

    if full {
        let release_argv = dist_audit_release_argv();
        let (prog, args) = release_argv.split_first().ok_or("release argv is empty")?;
        let t0 = std::time::Instant::now();
        let release = execute(prog, args, &workdir);
        let wall = t0.elapsed().as_secs();
        if release.exit_code == 0 {
            report.push(format!("locked-release-build: PASS (wall {wall}s)"));
        } else {
            failed = true;
            report.push(format!(
                "locked-release-build: FAIL (exit {}; wall {wall}s; {})",
                release.exit_code, release.stderr_tail
            ));
        }
        let offline_argv = dist_audit_offline_argv();
        let (prog, args) = offline_argv.split_first().ok_or("offline argv is empty")?;
        let t0 = std::time::Instant::now();
        let offline = execute(prog, args, &workdir);
        let wall = t0.elapsed().as_secs();
        if offline.exit_code == 0 {
            report.push(format!(
                "offline-build: PASS (wall {wall}s; `cargo build --locked --offline`, local cache only — fresh-machine install remains unproven)"
            ));
        } else {
            failed = true;
            report.push(format!(
                "offline-build: FAIL (exit {}; wall {wall}s; {})",
                offline.exit_code, offline.stderr_tail
            ));
        }
    } else {
        report.push(
            "locked-release-build: NOT_RUN (fast mode; run --mode full for the locked release + offline proofs)"
                .to_string(),
        );
    }

    for line in &report {
        println!("{line}");
    }
    let wall_total = started.elapsed().as_secs();
    let meta = serde_json::json!({
        "mode": mode,
        "wall_secs": wall_total,
        "checks": report,
    });
    let mut argv = vec![
        "cargo".to_string(),
        "xtask".to_string(),
        "dist-audit".to_string(),
    ];
    if full {
        argv.push("--mode".to_string());
        argv.push("full".to_string());
    }
    let outcome = crate::run::RunOutcome {
        exit_code: if failed { 1 } else { 0 },
        stdout_tail: report.join("\n"),
        stderr_tail: String::new(),
        started_ms: 0,
        finished_ms: 0,
    };
    finish("dist-audit", &argv, &outcome, &dir, Some(&meta))?;
    if failed { Ok(1) } else { Ok(0) }
}

/// One traceability row in the shape the T-GATE-01 consistency gate judges.
///
/// Statuses are free-form strings on purpose: unknown spellings must fail
/// the vocabulary check loudly instead of being normalized away.
pub struct GateLedgerRow {
    pub id: String,
    pub tests: Vec<String>,
    pub status: String,
    pub evidence: String,
}

/// One conformance-matrix tool with its check cells in file order.
pub struct GateMatrixTool {
    pub name: String,
    pub checks: Vec<(String, String)>,
}

/// Parse the ledger: top-level aggregate status plus one row per requirement.
///
/// A missing `evidence` field defaults to empty (a passed row with no
/// evidence must then fail the evidence check, not the parser); a missing
/// id/tests/status is a shape error.
pub fn gate_audit_parse_ledger(text: &str) -> Result<(String, Vec<GateLedgerRow>), String> {
    let v: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| format!("traceability.json is not valid JSON: {e}"))?;
    let reqs = v
        .get("requirements")
        .and_then(|r| r.as_array())
        .ok_or("traceability.json: missing 'requirements' array")?;
    let status = v
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let mut rows = Vec::with_capacity(reqs.len());
    for r in reqs {
        let id = r
            .get("id")
            .and_then(|s| s.as_str())
            .ok_or("traceability.json: requirement without string 'id'")?
            .to_string();
        let tests = r
            .get("tests")
            .and_then(|t| t.as_array())
            .ok_or(format!("traceability.json: {id} without 'tests' array"))?
            .iter()
            .map(|t| {
                t.as_str()
                    .ok_or(format!("traceability.json: {id} has non-string test entry"))
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|s| s.to_string())
            .collect();
        let row_status = r
            .get("status")
            .and_then(|s| s.as_str())
            .ok_or(format!("traceability.json: {id} without string 'status'"))?
            .to_string();
        let evidence = r
            .get("evidence")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string();
        rows.push(GateLedgerRow {
            id,
            tests,
            status: row_status,
            evidence,
        });
    }
    if rows.is_empty() {
        return Err("traceability.json: no requirements".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    for row in &rows {
        if !seen.insert(row.id.clone()) {
            return Err(format!(
                "traceability.json: duplicate requirement id '{}'",
                row.id
            ));
        }
    }
    Ok((status, rows))
}

/// Parse the matrix: aggregate status, expected tool count, one entry per tool.
pub fn gate_audit_parse_matrix(text: &str) -> Result<(String, u64, Vec<GateMatrixTool>), String> {
    let v: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| format!("conformance_matrix.json is not valid JSON: {e}"))?;
    let status = v
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let expected = v
        .get("expected_tool_count")
        .and_then(|n| n.as_u64())
        .ok_or("conformance_matrix.json: missing 'expected_tool_count'")?;
    let tools_json = v
        .get("tools")
        .and_then(|t| t.as_array())
        .ok_or("conformance_matrix.json: missing 'tools' array")?;
    let mut tools = Vec::with_capacity(tools_json.len());
    for t in tools_json {
        let name = t
            .get("name")
            .and_then(|s| s.as_str())
            .ok_or("conformance_matrix.json: tool without string 'name'")?
            .to_string();
        let checks_json = t.get("checks").and_then(|c| c.as_object()).ok_or(format!(
            "conformance_matrix.json: tool {name} without 'checks'"
        ))?;
        let mut checks = Vec::with_capacity(checks_json.len());
        for (k, val) in checks_json {
            let st = val.as_str().ok_or(format!(
                "conformance_matrix.json: tool {name} check {k} is not a string"
            ))?;
            checks.push((k.clone(), st.to_string()));
        }
        tools.push(GateMatrixTool { name, checks });
    }
    Ok((status, expected, tools))
}

/// Advance one Unicode scalar from byte index `i` (stays on char
/// boundaries; byte stepping would split `→`/accents in evidence text).
fn gate_audit_next_char(text: &str, i: usize) -> usize {
    i + text[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1)
}

/// Scan free text for plan test IDs (`T-` + capitals + `-` + digits),
/// deduplicated in first-appearance order. No regex crate: the shape is
/// fixed enough for a manual scan.
pub fn gate_audit_find_test_ids(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == b'T' && bytes[i + 1] == b'-' {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j].is_ascii_uppercase() {
                j += 1;
            }
            if j > i + 2 && j < bytes.len() && bytes[j] == b'-' {
                let mut k = j + 1;
                while k < bytes.len() && bytes[k].is_ascii_digit() {
                    k += 1;
                }
                if k > j + 1
                    && (k == bytes.len() || (!bytes[k].is_ascii_alphanumeric() && bytes[k] != b'_'))
                {
                    let id = &text[i..k];
                    if !out.iter().any(|s: &String| s == id) {
                        out.push(id.to_string());
                    }
                    i = k;
                    continue;
                }
            }
        }
        i = gate_audit_next_char(text, i);
    }
    out
}

/// Split evidence on whitespace/quotes/backticks/parens and keep tokens
/// that look like repo paths (`plans/...`, `crates/...`, ...). Existence
/// is judged by the caller; this only extracts candidates.
pub fn gate_audit_path_tokens(text: &str) -> Vec<String> {
    const ROOTS: &[&str] = &[
        "plans/",
        "tools/",
        "crates/",
        "tests/",
        "src/",
        "reports/",
        "baseline/",
        "docs/",
        "experiments/",
    ];
    let mut out = Vec::new();
    for raw in text.split(|c: char| c.is_whitespace() || "\"'`()[],;".contains(c)) {
        let tok = raw
            .trim_end_matches(['.', ',', ':', ';', '!', '?'])
            .trim_start_matches("./");
        if ROOTS.iter().any(|r| tok.starts_with(r)) && !tok.ends_with('/') && tok.len() > 7 {
            let tok = tok.to_string();
            if !out.contains(&tok) {
                out.push(tok);
            }
        }
    }
    out
}

/// True when `needle` occurs in `haystack` with a non-identifier boundary
/// on its LEFT (case-insensitive), so `still` matches "still deferred" but
/// not "distilled". The right side stays open so inflections (`deferred`,
/// `remaining`, `gaps`, `skipped`) still match their stems.
pub fn gate_audit_contains_bounded(haystack: &str, needle: &str) -> bool {
    let hay = haystack.to_lowercase();
    let ndl = needle.to_lowercase();
    if ndl.is_empty() {
        return false;
    }
    let hb = hay.as_bytes();
    let nb = ndl.as_bytes();
    let mut i = 0;
    while i + nb.len() <= hb.len() {
        if hb[i..i + nb.len()] == *nb {
            let left_ok = i == 0 || (!hb[i - 1].is_ascii_alphanumeric() && hb[i - 1] != b'_');
            if left_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// True when partial-row evidence names its gaps (deferral, remaining,
/// exclusion or boundary language). The per-row matched marker is listed
/// in gate output so the marker set stays auditable, not gerrymandered.
pub fn gate_audit_has_gap(evidence: &str) -> bool {
    const GAP_MARKERS: &[&str] = &[
        "defer",
        "remain",
        "still",
        "not_run",
        "not run",
        "not executed",
        "skip",
        "without",
        "partial",
        "future",
        "unproven",
        "pending",
        "limited",
        "scoped",
        "refused explicitly",
        "not verified",
        "deviation",
        "outstanding",
        "uncovered",
        "missing",
        "not yet",
        "excluded",
        "exclusion",
        "waived",
        "gap",
        "stale",
        "incomplete",
        "unresolved",
    ];
    GAP_MARKERS
        .iter()
        .any(|m| gate_audit_contains_bounded(evidence, m))
}

/// True when not-executed-row evidence claims execution (pass/green/
/// shipped language that is not negated by a non-execution affidavit).
pub fn gate_audit_claims_execution(evidence: &str) -> bool {
    const CLAIM_MARKERS: &[&str] = &[
        "executed",
        "passed",
        "passing",
        "green",
        "verified",
        "complete",
        "shipped",
        "all tests",
    ];
    CLAIM_MARKERS
        .iter()
        .any(|m| gate_audit_contains_bounded(evidence, m))
}

/// Runnable inventory hits in evidence text: underscored symbols (real
/// test names are snake_case) present in the runnable set. Bare words
/// collide with prose, so they never match.
fn gate_audit_runnable_refs(evidence: &str, test_fns: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for f in test_fns {
        if f.len() >= 8 && f.contains('_') && gate_audit_contains_bounded(evidence, f) {
            out.push((*f).to_string());
        }
    }
    out
}

/// Verifiable artifacts named by evidence text: plan test IDs defined in
/// `plans_text`, runnable test fns from the repo inventory, existing repo
/// paths, and runnable command tokens. Empty means the evidence points at
/// nothing checkable.
pub fn gate_audit_evidence_matches(
    evidence: &str,
    plans_text: &str,
    test_fns: &[&str],
    existing_paths: &[&str],
) -> Vec<String> {
    let mut out = Vec::new();
    for id in gate_audit_find_test_ids(evidence) {
        if plans_text.contains(id.as_str()) {
            out.push(format!("test-id {id}"));
        }
    }
    // Underscored symbols only (see gate_audit_runnable_refs).
    for f in gate_audit_runnable_refs(evidence, test_fns) {
        out.push(format!("test-fn {f}"));
    }
    for p in gate_audit_path_tokens(evidence) {
        if existing_paths.contains(&p.as_str()) {
            out.push(format!("path {p}"));
        }
    }
    for cmd in [
        "cargo test",
        "cargo check",
        "cargo clippy",
        "cargo fmt",
        "cargo xtask",
        "dist-audit",
        "gen_release_evidence",
    ] {
        if gate_audit_contains_bounded(evidence, cmd) {
            out.push(format!("command {cmd}"));
        }
    }
    out
}

/// RQ status vocabulary: only the three plan statuses resolve; anything
/// else (`passed`, `complete`, `ok`, ...) is a mislabel and fails.
pub fn gate_audit_check_rq_vocabulary(rows: &[GateLedgerRow]) -> Result<String, String> {
    const KNOWN: &[&str] = &[
        "specified_not_executed",
        "executed_partial",
        "executed_passed",
    ];
    let mut bad = Vec::new();
    for row in rows {
        if !KNOWN.contains(&row.status.as_str()) {
            bad.push(format!("{}: '{}'", row.id, row.status));
        }
    }
    if bad.is_empty() {
        Ok(format!("{} rows, status vocabulary clean", rows.len()))
    } else {
        Err(format!(
            "unknown RQ statuses (mislabeled, not an executed state):\n{}",
            bad.join("\n")
        ))
    }
}

/// Matrix shape: tool count must equal `expected_tool_count` and every
/// tool must carry exactly the eight known check columns.
/// The eight conformance columns every matrix tool must carry.
pub fn gate_audit_matrix_columns() -> &'static [&'static str] {
    &[
        "name_and_schema",
        "defaults_and_validation",
        "output_and_errors",
        "state_transitions",
        "scope_and_concurrency",
        "retry_and_restart",
        "legacy_oracle",
        "enhancement_ledger",
    ]
}

pub fn gate_audit_check_matrix_shape(
    expected: u64,
    tools: &[GateMatrixTool],
) -> Result<String, String> {
    let mut bad = Vec::new();
    for tool in tools {
        let mut keys: Vec<&str> = tool.checks.iter().map(|(k, _)| k.as_str()).collect();
        keys.sort_unstable();
        let mut want = gate_audit_matrix_columns().to_vec();
        want.sort_unstable();
        if keys != want {
            bad.push(format!(
                "{}: {} checks, expected the 8 known columns",
                tool.name,
                tool.checks.len()
            ));
        }
    }
    if !bad.is_empty() {
        return Err(format!("matrix shape skew:\n{}", bad.join("\n")));
    }
    if tools.len() as u64 != expected {
        return Err(format!(
            "tool count {} != expected_tool_count {expected} (tools added or dropped silently)",
            tools.len()
        ));
    }
    let cells: usize = tools.iter().map(|t| t.checks.len()).sum();
    Ok(format!(
        "{expected} tools x 8 checks ({cells} cells), shape matches"
    ))
}

/// Cell status vocabulary: only executed/not_run family spellings resolve.
/// A `not_run` cell relabeled `passed` fails here (T-GATE-01 assertion).
pub fn gate_audit_check_cell_vocabulary(tools: &[GateMatrixTool]) -> Result<String, String> {
    const KNOWN: &[&str] = &[
        "executed_passed",
        "executed_partial",
        "specified_not_executed",
        "not_run",
    ];
    let mut bad = Vec::new();
    let mut passed = 0usize;
    let mut open = 0usize;
    for tool in tools {
        for (check, status) in &tool.checks {
            if !KNOWN.contains(&status.as_str()) {
                bad.push(format!("{} {check}: '{status}'", tool.name));
            } else if status == "executed_passed" {
                passed += 1;
            } else {
                open += 1;
            }
        }
    }
    if bad.is_empty() {
        Ok(format!(
            "cell vocabulary clean ({passed} executed_passed, {open} openly not-executed)"
        ))
    } else {
        Err(format!(
            "mislabeled conformance cells (unrun work passed off as passed):\n{}",
            bad.join("\n")
        ))
    }
}

/// Every `executed_passed` RQ needs test references plus evidence text
/// naming at least one verifiable artifact. Only runnable test fns count
/// (non-test fns and `#[ignore]`d tests never do); paths, commands and
/// plan-defined test IDs count as before. Per-row matches are listed so
/// a vacuous pass is impossible to hide. Grandfathered rows without any
/// match yield legacy WARNINGs instead of failing.
pub fn gate_audit_check_passed_evidence(
    rows: &[GateLedgerRow],
    plans_text: &str,
    test_fns: &[&str],
    existing_paths: &[&str],
) -> Result<(String, Vec<String>), String> {
    let mut bad = Vec::new();
    let mut warnings = Vec::new();
    let mut notes = Vec::new();
    for row in rows {
        if row.status != "executed_passed" {
            continue;
        }
        let grandfathered = gate_audit_is_grandfathered_row(&row.id);
        let fail_row = |msg: String, bad: &mut Vec<String>, warnings: &mut Vec<String>| {
            if grandfathered {
                warnings.push(format!(
                    "{}: {msg} (legacy evidence, refresh wave owns it)",
                    row.id
                ));
            } else {
                bad.push(format!("{}: {msg}", row.id));
            }
        };
        if row.tests.is_empty() {
            fail_row("no test references".to_string(), &mut bad, &mut warnings);
            continue;
        }
        if row.evidence.trim().is_empty() {
            fail_row("no evidence text".to_string(), &mut bad, &mut warnings);
            continue;
        }
        let hits = gate_audit_evidence_matches(&row.evidence, plans_text, test_fns, existing_paths);
        if hits.is_empty() {
            fail_row(
                "evidence names no verifiable artifact".to_string(),
                &mut bad,
                &mut warnings,
            );
        } else if hits.len() == 1 {
            notes.push(format!("{}: {}", row.id, hits[0]));
        } else {
            notes.push(format!(
                "{}: {} (+{} more)",
                row.id,
                hits[0],
                hits.len() - 1
            ));
        }
    }
    if bad.is_empty() {
        Ok((
            format!(
                "{} passed rows each name a verifiable artifact ({})",
                notes.len(),
                notes.join("; ")
            ),
            warnings,
        ))
    } else {
        Err(format!(
            "executed_passed rows without verifiable evidence:\n{}",
            bad.join("\n")
        ))
    }
}

/// Every `specified_not_executed` row must not claim execution. Empty
/// evidence and explicit non-execution affidavits (`not run`, `deferred`)
/// pass; execution claims fail.
pub fn gate_audit_check_not_executed_honest(rows: &[GateLedgerRow]) -> Result<String, String> {
    // Explicit non-execution affidavits: evidence carrying one of these is
    // honestly unrun even if it shares words with execution claims.
    const AFFIDAVIT: &[&str] = &[
        "not_run",
        "not run",
        "not executed",
        "not verified",
        "deferred",
        "remains specified",
        "no execution",
        "out of scope",
        "not started",
        "not yet",
    ];
    let mut bad = Vec::new();
    let mut honest = 0usize;
    for row in rows {
        if row.status != "specified_not_executed" {
            continue;
        }
        let ev = row.evidence.trim();
        if ev.is_empty() || AFFIDAVIT.iter().any(|m| gate_audit_contains_bounded(ev, m)) {
            honest += 1;
            continue;
        }
        if gate_audit_claims_execution(ev) {
            bad.push(format!("{}: evidence claims execution", row.id));
        } else {
            honest += 1;
        }
    }
    if bad.is_empty() {
        Ok(format!("{honest} not-executed rows claim no execution"))
    } else {
        Err(format!(
            "specified_not_executed rows whose evidence claims execution:\n{}",
            bad.join("\n")
        ))
    }
}

/// Every `executed_partial` row must name its gaps in the evidence text.
pub fn gate_audit_check_partial_gaps(rows: &[GateLedgerRow]) -> Result<String, String> {
    let mut bad = Vec::new();
    let mut count = 0usize;
    for row in rows {
        if row.status != "executed_partial" {
            continue;
        }
        if row.evidence.trim().is_empty() {
            bad.push(format!("{}: no evidence text, gaps unnamed", row.id));
        } else if !gate_audit_has_gap(&row.evidence) {
            bad.push(format!("{}: evidence names no gap", row.id));
        } else {
            count += 1;
        }
    }
    if bad.is_empty() {
        Ok(format!("{count} partial rows each name their gaps"))
    } else {
        Err(format!(
            "executed_partial rows without named gaps:\n{}",
            bad.join("\n")
        ))
    }
}

/// RQ IDs whose pre-C3 evidence text is grandfathered: under the strict
/// runnability rule they newly fail, but their rows are frozen, so the
/// gate emits an explicit legacy WARNING instead of failing on them.
/// (Fix round 1/2: RQ-01/02/03/11/17.)
pub fn gate_audit_is_grandfathered_row(id: &str) -> bool {
    matches!(id, "RQ-01" | "RQ-02" | "RQ-03" | "RQ-11" | "RQ-17")
}

/// xtask verb backing a plan test ID, if the ID names an automated gate.
/// Only automation-backed IDs resolve this way; every other ID must be
/// backed by a runnable test fn named in a referencing row's evidence.
pub fn gate_audit_verb_for_test(id: &str) -> Option<&'static str> {
    match id {
        "T-GATE-01" => Some("gate-audit"),
        "T-RELEASE-01" => Some("evidence"),
        "T-BUILD-01" | "T-SEC-04" => Some("dist-audit"),
        _ => None,
    }
}

/// Split one Rust source text into runnable test fns vs ignored test fns.
///
/// A fn counts as a test fn only when `#[test]`/`#[tokio::test]` attributes
/// precede it (attribute, blank and comment lines are skipped looking
/// back); it counts as ignored when `#[ignore]` is among them. Plain
/// production fns land in neither bucket and NEVER count as evidence.
pub fn gate_audit_test_fns_in_source(src: &str) -> (Vec<String>, Vec<String>) {
    let lines: Vec<&str> = src.lines().collect();
    let mut runnable = Vec::new();
    let mut ignored = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        let Some(fn_at) = gate_audit_fn_token_at(line) else {
            continue;
        };
        let mut attrs = Vec::new();
        attrs.push(line[..fn_at].to_string());
        let mut j = i as isize - 1;
        while j >= 0 {
            let t = lines[j as usize].trim();
            if t.starts_with("#[") {
                attrs.push(t.to_string());
                j -= 1;
            } else if t.is_empty() || t.starts_with("//") {
                j -= 1;
            } else {
                break;
            }
        }
        let blob = attrs.join(" ");
        if !blob.contains("#[test") && !blob.contains("#[tokio::test") {
            continue;
        }
        if let Some(name) = gate_audit_fn_name_after(line, fn_at) {
            if blob.contains("#[ignore") {
                if !ignored.contains(&name) {
                    ignored.push(name);
                }
            } else if !runnable.contains(&name) {
                runnable.push(name);
            }
        }
    }
    (runnable, ignored)
}

/// Byte index of the `fn` keyword starting a fn item on `line`, if any.
/// Left-bounded so `myfn` never matches; char-stepped for UTF-8 safety.
fn gate_audit_fn_token_at(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'f'
            && bytes[i + 1] == b'n'
            && (i + 2 == bytes.len() || bytes[i + 2] == b' ' || bytes[i + 2] == b'\t')
        {
            let left_ok = i == 0 || (!bytes[i - 1].is_ascii_alphanumeric() && bytes[i - 1] != b'_');
            if left_ok {
                return Some(i);
            }
        }
        i = gate_audit_next_char(line, i);
    }
    None
}

/// Identifier right after the `fn` keyword at `fn_at` (`fn name`, generics
/// and attributes already skipped by the caller shape).
fn gate_audit_fn_name_after(line: &str, fn_at: usize) -> Option<String> {
    let bytes = line.as_bytes();
    let mut j = fn_at + 2;
    while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
        j += 1;
    }
    let mut k = j;
    while k < bytes.len() && (bytes[k].is_ascii_alphanumeric() || bytes[k] == b'_') {
        k += 1;
    }
    if k > j && (bytes[j].is_ascii_alphabetic() || bytes[j] == b'_') {
        Some(line[j..k].to_string())
    } else {
        None
    }
}

/// Runnable backing for one T-* ID: an xtask verb, or runnable test fns
/// named in the evidence of a row listing the ID in its tests array.
/// None means the ID is defined in the plans but nothing runnable stands
/// behind it.
pub fn gate_audit_backing_for_test(
    id: &str,
    rows: &[GateLedgerRow],
    test_fns: &[&str],
) -> Option<String> {
    if let Some(verb) = gate_audit_verb_for_test(id) {
        return Some(format!("xtask verb `{verb}`"));
    }
    for row in rows {
        if row.tests.iter().any(|t| t == id) {
            let hits = gate_audit_runnable_refs(&row.evidence, test_fns);
            if hits.is_empty() {
                continue;
            }
            if hits.len() == 1 {
                return Some(format!("runnable test fn {} named by {}", hits[0], row.id));
            }
            return Some(format!(
                "runnable test fns named by {} ({} +{} more)",
                row.id,
                hits[0],
                hits.len() - 1
            ));
        }
    }
    None
}

/// Every T-* ID referenced by tests arrays or evidence text must be
/// defined in the plan test tables AND runnable: an xtask verb or a
/// non-ignored test fn behind a referencing row. A dangling ID (typo or
/// invented coverage) fails. An unrunnable ID fails when RQ-27/28 or a
/// non-grandfathered passed row references it; grandfathered rows and
/// partial rows yield explicit WARNINGs instead (the gate stays green).
pub fn gate_audit_check_test_ids_resolve(
    rows: &[GateLedgerRow],
    plans_text: &str,
    test_fns: &[&str],
) -> Result<(String, Vec<String>), String> {
    let mut ids: Vec<String> = Vec::new();
    let mut referrers: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut note_ref = |id: String, row: &str| {
        if !ids.contains(&id) {
            ids.push(id.clone());
        }
        let entry = referrers.entry(id).or_default();
        if !entry.iter().any(|r| r == row) {
            entry.push(row.to_string());
        }
    };
    for row in rows {
        for t in &row.tests {
            note_ref(t.clone(), &row.id);
        }
        for id in gate_audit_find_test_ids(&row.evidence) {
            note_ref(id, &row.id);
        }
    }
    let mut dangling = Vec::new();
    for id in &ids {
        if !plans_text.contains(id.as_str()) {
            dangling.push(id.clone());
        }
    }
    if !dangling.is_empty() {
        return Err(format!(
            "dangling test IDs (in no plan test table, runnable nowhere):\n{}",
            dangling.join("\n")
        ));
    }
    let mut verb_backed = 0usize;
    let mut test_backed = 0usize;
    let mut hard = Vec::new();
    let mut warnings = Vec::new();
    for id in &ids {
        match gate_audit_backing_for_test(id, rows, test_fns) {
            Some(note) => {
                if note.starts_with("xtask verb") {
                    verb_backed += 1;
                } else {
                    test_backed += 1;
                }
            }
            None => {
                let refs = referrers.get(id).cloned().unwrap_or_default();
                let owned = refs.iter().any(|r| r == "RQ-27" || r == "RQ-28");
                let hard_passed = refs.iter().any(|r| {
                    !gate_audit_is_grandfathered_row(r)
                        && rows
                            .iter()
                            .find(|x| &x.id == r)
                            .is_some_and(|x| x.status == "executed_passed")
                });
                if owned || hard_passed {
                    hard.push(format!(
                        "{id} has no runnable backing (referenced by {})",
                        refs.join(", ")
                    ));
                } else if refs.iter().all(|r| gate_audit_is_grandfathered_row(r)) {
                    warnings.push(format!(
                        "{id} has no runnable backing; referenced by {} (legacy evidence, refresh wave owns it)",
                        refs.join(", ")
                    ));
                } else {
                    warnings.push(format!(
                        "{id} has no runnable backing; referenced by partial {} (refresh belongs to the owning wave)",
                        refs.join(", ")
                    ));
                }
            }
        }
    }
    if !hard.is_empty() {
        return Err(format!(
            "test IDs with no runnable test/verb:\n{}",
            hard.join("\n")
        ));
    }
    Ok((
        format!(
            "{} referenced test IDs: {verb_backed} verb-backed, {test_backed} test-backed (T-GATE-01 via `cargo xtask gate-audit`, T-RELEASE-01 via the evidence bundle + checklist walk, T-BUILD-01/T-SEC-04 via `cargo xtask dist-audit`)",
            ids.len()
        ),
        warnings,
    ))
}

/// Aggregate statuses must not claim completion while rows/cells remain
/// open: no hiding unrun work behind a summary score (T-GATE-01 assertion).
pub fn gate_audit_check_aggregates_honest(
    ledger_status: &str,
    matrix_status: &str,
    rows: &[GateLedgerRow],
    tools: &[GateMatrixTool],
) -> Result<String, String> {
    const COMPLETE_WORDS: &[&str] = &[
        "complete",
        "completed",
        "passed",
        "all_passed",
        "fully_conformant",
        "released",
        "done",
    ];
    let ledger_open = rows
        .iter()
        .any(|r| r.status == "executed_partial" || r.status == "specified_not_executed");
    let matrix_open = tools
        .iter()
        .any(|t| t.checks.iter().any(|(_, s)| s != "executed_passed"));
    let mut bad = Vec::new();
    if ledger_open
        && COMPLETE_WORDS
            .iter()
            .any(|w| gate_audit_contains_bounded(ledger_status, w))
    {
        bad.push(format!(
            "traceability status '{ledger_status}' claims completion with open rows"
        ));
    }
    if matrix_open
        && COMPLETE_WORDS
            .iter()
            .any(|w| gate_audit_contains_bounded(matrix_status, w))
    {
        bad.push(format!(
            "matrix status '{matrix_status}' claims completion with open cells"
        ));
    }
    if bad.is_empty() {
        Ok(format!(
            "aggregates honest ('{ledger_status}' / '{matrix_status}' claim no completion)"
        ))
    } else {
        Err(format!(
            "aggregate scores hiding unrun work:\n{}",
            bad.join("\n")
        ))
    }
}

/// Backend/release decision report: AD-01 must read DECIDED (Fjall + Lance)
/// and RQ-02 must be passed with evidence naming AD-01.
pub fn gate_audit_check_decision_report(
    ad01_text: &str,
    rows: &[GateLedgerRow],
) -> Result<String, String> {
    let mut bad = Vec::new();
    for token in ["DECIDED", "Fjall", "Lance"] {
        if !ad01_text.contains(token) {
            bad.push(format!("AD-01 record lacks '{token}'"));
        }
    }
    match rows.iter().find(|r| r.id == "RQ-02") {
        None => bad.push("ledger has no RQ-02 row for the backend decision".to_string()),
        Some(row) => {
            if row.status != "executed_passed" {
                bad.push(format!("RQ-02 is '{}', not executed_passed", row.status));
            }
            if !row.evidence.contains("AD-01") {
                bad.push("RQ-02 evidence does not name AD-01".to_string());
            }
        }
    }
    if bad.is_empty() {
        Ok("AD-01 DECIDED (Fjall + Lance); RQ-02 passed with AD-01 evidence".to_string())
    } else {
        Err(format!(
            "backend decision report invalid:\n{}",
            bad.join("\n")
        ))
    }
}

/// Inventory of runnable test fn names under `crates/*/src`, `tests` and
/// `src` (any `target/` dir skipped): `#[test]`/`#[tokio::test]`-attributed
/// and not `#[ignore]`d, via [`gate_audit_test_fns_in_source`]. Reads names
/// only, never executes anything.
pub fn gate_audit_collect_test_fns(workdir: &std::path::Path) -> Result<Vec<String>, String> {
    fn visit(
        dir: &std::path::Path,
        out: &mut std::collections::HashSet<String>,
    ) -> Result<(), String> {
        let entries =
            std::fs::read_dir(dir).map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("cannot read dir entry: {e}"))?;
            let path = entry.path();
            if path.to_string_lossy().contains("target") {
                continue;
            }
            if path.is_dir() {
                visit(&path, out)?;
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let src = std::fs::read_to_string(&path)
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                let (runnable, _) = gate_audit_test_fns_in_source(&src);
                out.extend(runnable);
            }
        }
        Ok(())
    }
    let mut names = std::collections::HashSet::new();
    for sub in ["crates", "tests", "src"] {
        let dir = workdir.join(sub);
        if dir.is_dir() {
            visit(&dir, &mut names)?;
        }
    }
    let mut sorted: Vec<String> = names.into_iter().collect();
    sorted.sort();
    Ok(sorted)
}

/// Concatenated `plans/*.md` text backing the test-ID resolution check.
pub fn gate_audit_collect_plans_text(workdir: &std::path::Path) -> Result<String, String> {
    let dir = workdir.join("plans");
    let mut names = Vec::new();
    let entries =
        std::fs::read_dir(&dir).map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read dir entry: {e}"))?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "md") {
            names.push(path);
        }
    }
    names.sort();
    let mut text = String::new();
    for path in names {
        let src = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        text.push_str(&src);
        text.push('\n');
    }
    Ok(text)
}

/// Append one check verdict to the gate report, failing closed.
fn push_gate_check(
    report: &mut Vec<String>,
    failed: &mut bool,
    name: &str,
    result: Result<String, String>,
) {
    match result {
        Ok(note) => {
            report.push(format!("{name}: PASS ({note})"));
        }
        Err(e) => {
            *failed = true;
            let mut lines = e.lines();
            let first = lines.next().unwrap_or(&e);
            report.push(format!("{name}: FAIL ({first})"));
            for extra in lines {
                report.push(format!("  {extra}"));
            }
        }
    }
}

/// Run the T-GATE-01 consistency gate over the real ledger + matrix.
/// Parses `plans/traceability.json` + `plans/conformance_matrix.json` and
/// fails loudly on mislabeled, evidence-free or dangling references. This
/// validates CONSISTENCY, never execution: `not_run` cells and partial rows
/// with named gaps are legitimate passes. `dry_run` prints the would-be
/// reads and returns 0.
pub fn run_gate_audit(dry_run: bool) -> Result<i32, String> {
    if dry_run {
        println!(
            "would read: plans/traceability.json, plans/conformance_matrix.json, \
             plans/AD-01_canonical_backend.md, plans/*.md, \
             test-fn inventory (crates+tests+src, target/ skipped)"
        );
        return Ok(0);
    }
    let workdir = workspace_root();
    let dir = report_dir("gate-audit")?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create reports dir {}: {e}", dir.display()))?;
    let started = std::time::Instant::now();
    let mut report: Vec<String> = Vec::new();
    let mut failed = false;
    let mut push = |line: String, ok: bool| {
        if !ok {
            failed = true;
        }
        report.push(line);
    };

    let ledger_text = std::fs::read_to_string(workdir.join("plans/traceability.json"));
    let matrix_text = std::fs::read_to_string(workdir.join("plans/conformance_matrix.json"));
    let ad01_text = std::fs::read_to_string(workdir.join("plans/AD-01_canonical_backend.md"));
    let plans_text = gate_audit_collect_plans_text(&workdir);
    let test_fns = gate_audit_collect_test_fns(&workdir);

    let mut ledger_status = String::new();
    let mut rows: Vec<GateLedgerRow> = Vec::new();
    match ledger_text {
        Err(e) => push(
            format!("ledger-parse: FAIL (cannot read traceability.json: {e})"),
            false,
        ),
        Ok(text) => match gate_audit_parse_ledger(&text) {
            Err(e) => push(format!("ledger-parse: FAIL ({e})"), false),
            Ok((status, parsed)) => {
                ledger_status = status;
                rows = parsed;
                push(
                    format!("ledger-parse: PASS ({} requirement rows)", rows.len()),
                    true,
                );
            }
        },
    }

    let mut matrix_status = String::new();
    let mut expected: u64 = 0;
    let mut tools: Vec<GateMatrixTool> = Vec::new();
    match matrix_text {
        Err(e) => push(
            format!("matrix-parse: FAIL (cannot read conformance_matrix.json: {e})"),
            false,
        ),
        Ok(text) => match gate_audit_parse_matrix(&text) {
            Err(e) => push(format!("matrix-parse: FAIL ({e})"), false),
            Ok((status, count, parsed)) => {
                matrix_status = status;
                expected = count;
                tools = parsed;
                push(format!("matrix-parse: PASS ({} tools)", tools.len()), true);
            }
        },
    }

    let plans = match plans_text {
        Err(e) => {
            push(format!("plans-text: FAIL ({e})"), false);
            String::new()
        }
        Ok(text) => {
            push(
                format!(
                    "plans-text: PASS ({} bytes of plan test tables)",
                    text.len()
                ),
                true,
            );
            text
        }
    };

    let fns: Vec<String> = match test_fns {
        Err(e) => {
            push(format!("test-inventory: FAIL ({e})"), false);
            Vec::new()
        }
        Ok(names) => {
            push_gate_check(
                &mut report,
                &mut failed,
                "test-inventory",
                Ok(format!("{} runnable test fns", names.len())),
            );
            names
        }
    };
    let fn_refs: Vec<&str> = fns.iter().map(|s| s.as_str()).collect();

    let mut path_tokens = Vec::new();
    for row in &rows {
        for p in gate_audit_path_tokens(&row.evidence) {
            if !path_tokens.contains(&p) {
                path_tokens.push(p);
            }
        }
    }
    let existing: Vec<String> = path_tokens
        .into_iter()
        .filter(|p| workdir.join(p).exists())
        .collect();
    let existing_refs: Vec<&str> = existing.iter().map(|s| s.as_str()).collect();

    // Fail-closed: a check whose inputs failed to load reports FAIL, never a
    // vacuous PASS. An empty ledger/matrix (unreadable above) fails every
    // content check through the empty inputs.
    let checks: Vec<(&str, Result<String, String>)> = vec![
        ("rq-vocabulary", gate_audit_check_rq_vocabulary(&rows)),
        (
            "matrix-shape",
            gate_audit_check_matrix_shape(expected, &tools),
        ),
        ("cell-vocabulary", gate_audit_check_cell_vocabulary(&tools)),
        (
            "not-executed-honest",
            gate_audit_check_not_executed_honest(&rows),
        ),
        ("partial-gaps", gate_audit_check_partial_gaps(&rows)),
        (
            "aggregates-honest",
            gate_audit_check_aggregates_honest(&ledger_status, &matrix_status, &rows, &tools),
        ),
        (
            "decision-report",
            match ad01_text {
                Err(e) => Err(format!("cannot read AD-01 record: {e}")),
                Ok(text) => gate_audit_check_decision_report(&text, &rows),
            },
        ),
    ];
    for (name, result) in checks {
        push_gate_check(&mut report, &mut failed, name, result);
    }
    // Tuple checks additionally carry explicit WARNINGs (grandfathered rows
    // and partial-row references warn without failing the gate).
    for (name, result) in [
        (
            "passed-evidence",
            gate_audit_check_passed_evidence(&rows, &plans, &fn_refs, &existing_refs),
        ),
        (
            "test-id-resolve",
            gate_audit_check_test_ids_resolve(&rows, &plans, &fn_refs),
        ),
    ] {
        match result {
            Ok((note, warnings)) => {
                if warnings.is_empty() {
                    report.push(format!("{name}: PASS ({note})"));
                } else {
                    report.push(format!(
                        "{name}: PASS ({note}; {} warnings)",
                        warnings.len()
                    ));
                }
                for w in warnings {
                    report.push(format!("  WARNING {w}"));
                }
            }
            Err(e) => push_gate_check(&mut report, &mut failed, name, Err(e)),
        }
    }

    for line in &report {
        println!("{line}");
    }
    let wall_total = started.elapsed().as_secs();
    let meta = serde_json::json!({
        "wall_secs": wall_total,
        "checks": report,
    });
    let argv = vec![
        "cargo".to_string(),
        "xtask".to_string(),
        "gate-audit".to_string(),
    ];
    let outcome = crate::run::RunOutcome {
        exit_code: if failed { 1 } else { 0 },
        stdout_tail: report.join("\n"),
        stderr_tail: String::new(),
        started_ms: 0,
        finished_ms: 0,
    };
    finish("gate-audit", &argv, &outcome, &dir, Some(&meta))?;
    if failed { Ok(1) } else { Ok(0) }
}

#[cfg(test)]
mod tests {
    use super::{
        CAPABILITY_NAMES, CONFORMANCE_NAMES, RECOVERY_NAMES, benchmark_argv, benchmark_out_file,
        capability_commands, capture_argv, conformance_commands, evidence_argv,
        quality_wave_command, recovery_commands, run_quality_parts, test_commands,
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
        assert_eq!(super::run_test("quick", None, true).unwrap(), 0);
        assert_eq!(super::run_test("full", None, true).unwrap(), 0);
        assert_eq!(
            super::run_test("quick", Some("ltmrs-daemon"), true).unwrap(),
            0
        );
    }

    #[test]
    fn test_commands_tiers_and_package() {
        let (cmds, names) = test_commands("quick", None).unwrap();
        assert_eq!(cmds.len(), 4);
        assert!(cmds[0].iter().any(|a| a == "ltmrs-domain"));
        assert!(cmds.iter().all(|c| c.contains(&"--lib".to_string())));
        assert_eq!(names, vec!["domain", "service", "compat", "interchange"]);
        let (cmds, names) = test_commands("full", None).unwrap();
        assert_eq!(
            cmds,
            vec![vec![
                "cargo".to_string(),
                "test".to_string(),
                "--workspace".to_string()
            ]]
        );
        assert_eq!(names, vec!["workspace".to_string()]);
        let (cmds, names) = test_commands("quick", Some("ltmrs-daemon")).unwrap();
        assert_eq!(
            cmds,
            vec![vec![
                "cargo".to_string(),
                "test".to_string(),
                "-p".to_string(),
                "ltmrs-daemon".to_string()
            ]]
        );
        assert_eq!(names, vec!["ltmrs-daemon".to_string()]);
        assert!(test_commands("fast", None).is_err());
        assert!(test_commands("quick", Some("nope")).is_err());
        assert!(test_commands("full", Some("ltmrs-daemon")).is_err());
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

    // Wave C2 (T-BUILD-01 / RQ-21): distribution-audit detectors. Each test
    // feeds either the real-tree shape (must pass) or a deliberately-broken
    // fixture (must fail) so the verb proves it detects violations instead
    // of vacuously passing.
    #[test]
    fn dist_audit_token_list_names_every_forbidden_family() {
        let tokens = super::dist_audit_deny_tokens().join(" ");
        for token in [
            "python",
            "pyo3",
            "napi",
            "node",
            "onnx",
            "onnxruntime",
            "ort",
            "tract",
            "gguf",
            "rusqlite",
            "libsqlite3-sys",
            "sqlite",
            "mysql",
            "postgres",
            "pq-sys",
            "sqlx",
            "diesel",
            "cuda",
            "cudnn",
            "cublas",
            "nvrtc",
            "nccl",
            "mkl",
            "openblas",
            "metal",
            "accelerate",
            "vulkan",
            "libtorch",
            "torch-sys",
        ] {
            assert!(
                tokens.split(' ').any(|t| t == token),
                "deny token list must name '{token}': {tokens}"
            );
        }
    }

    #[test]
    fn dist_audit_tree_package_names_parses_lines() {
        let tree = "lance v12.0.0\nlancedb v0.39.0 (*)\nwinapi-util v0.1.6\n";
        assert_eq!(
            super::dist_audit_tree_package_names(tree),
            vec!["lance", "lancedb", "winapi-util"]
        );
    }

    #[test]
    fn dist_audit_clean_tree_packages_pass() {
        // winapi-util contains `napi` as a bare substring but must NOT match:
        // matching is component/prefix based, and it is a Windows-only dep
        // (in Cargo.lock, absent from the host-platform tree) either way.
        let names = ["lance", "lancedb", "zstd-sys", "winapi-util", "tokio"];
        assert!(super::dist_audit_check_tree_packages(&names).is_ok());
    }

    #[test]
    fn dist_audit_forbidden_tree_package_fails() {
        for (name, token) in [
            ("onnxruntime", "onnx"),
            ("rusqlite", "rusqlite"),
            ("cudart-sys", "cuda"),
            ("napi-derive", "napi"),
        ] {
            let names = ["lance", name];
            let err = super::dist_audit_check_tree_packages(&names).unwrap_err();
            assert!(err.contains(name), "must name the hit: {err}");
            assert!(err.contains(token), "must name the token: {err}");
        }
    }

    #[test]
    fn dist_audit_manifest_files_parses_insert_set() {
        let src = "pub fn e5_small_artifact() -> ModelArtifact {\n    let mut digests = HashMap::new();\n    digests.insert(\n        \"config.json\".into(),\n        \"abc\".into(),\n    );\n    digests.insert(\"model.safetensors\".into(), \"def\".into());\n    digests.insert(\"model.onnx\".into(), \"evil\".into());\n}\n";
        assert_eq!(
            super::dist_audit_manifest_files(src),
            vec!["config.json", "model.safetensors", "model.onnx"]
        );
    }

    #[test]
    fn dist_audit_manifest_files_empty_when_no_inserts() {
        assert!(super::dist_audit_manifest_files("fn main() {}\n").is_empty());
    }

    #[test]
    fn dist_audit_remote_api_clean_passes() {
        // Zero tolerance: clean means the usage grep found nothing.
        // (`connect_remote` in serve.rs is the local-IPC Unix-socket
        // client and matches none of the remote-namespace tokens.)
        let sites: [&str; 0] = [];
        assert!(super::dist_audit_check_remote_api_sites(&sites).is_ok());
    }

    #[test]
    fn dist_audit_remote_api_hit_fails() {
        // A lance remote-namespace client in serving code must fail loudly
        // (the reqwest-string scan is blind to API usage).
        let sites = [
            "crates/search/src/search/table.rs:99: let db = lance_namespace::RestAdapter::new(url)",
        ];
        let err = super::dist_audit_check_remote_api_sites(&sites).unwrap_err();
        assert!(err.contains("RestAdapter"), "must name the API: {err}");
    }

    #[test]
    fn dist_audit_empty_hits_pass_for_skills_predicate() {
        assert!(super::dist_audit_check_tree_hits("").is_ok());
        assert!(super::dist_audit_check_tree_hits("  \n").is_ok());
        let err = super::dist_audit_check_tree_hits("reqwest::Client\n").unwrap_err();
        assert!(err.contains("reqwest"), "must name the hit: {err}");
    }

    #[test]
    fn dist_audit_rusqlite_absent_passes() {
        let stderr = "error: package ID specification `rusqlite` did not match any packages\n";
        assert!(super::dist_audit_check_rusqlite_inverse(101, stderr).is_ok());
    }

    #[test]
    fn dist_audit_rusqlite_present_fails() {
        let stdout = "rusqlite v0.37.0\n└── ltmrs-lemma-import v0.1.0\n";
        let err = super::dist_audit_check_rusqlite_inverse(0, stdout).unwrap_err();
        assert!(err.contains("rusqlite"), "must name the leak: {err}");
    }

    #[test]
    fn dist_audit_reqwest_provision_only_passes() {
        let sites = [
            "crates/embeddings/src/artifacts.rs:143: client: &reqwest::Client,",
            "crates/frontend/src/cli.rs:563: reqwest::Client::builder()",
        ];
        assert!(super::dist_audit_check_reqwest_sites(&sites).is_ok());
    }

    #[test]
    fn dist_audit_reqwest_hot_path_fails() {
        let sites = [
            "crates/embeddings/src/artifacts.rs:143: client: &reqwest::Client,",
            "crates/daemon/src/server.rs:10: reqwest::get(url).await",
        ];
        let err = super::dist_audit_check_reqwest_sites(&sites).unwrap_err();
        assert!(err.contains("server.rs"), "must name the hot path: {err}");
    }

    #[test]
    fn dist_audit_unknown_reqwest_site_fails_closed() {
        let sites = ["crates/search/src/engine.rs:1: use reqwest;"];
        let err = super::dist_audit_check_reqwest_sites(&sites).unwrap_err();
        assert!(
            err.contains("engine.rs"),
            "unknown sites fail closed: {err}"
        );
    }

    #[test]
    fn dist_audit_model_dir_pinned_passes() {
        let files = [
            "config.json",
            "tokenizer_config.json",
            "special_tokens_map.json",
            "tokenizer.json",
            "sentencepiece.bpe.model",
            "model.safetensors",
        ];
        assert!(super::dist_audit_check_model_dir(&files).is_ok());
    }

    #[test]
    fn dist_audit_model_dir_missing_safetensors_fails() {
        // Wrong-format provision: an ONNX file can never substitute for the
        // pinned weights; the directory fails closed without model.safetensors.
        let files = ["config.json", "tokenizer_config.json", "model.onnx"];
        let err = super::dist_audit_check_model_dir(&files).unwrap_err();
        assert!(
            err.contains("model.safetensors"),
            "must name the pin: {err}"
        );
    }

    #[test]
    fn dist_audit_dry_run_prints_without_executing() {
        assert_eq!(super::run_dist_audit(false, true).unwrap(), 0);
        assert_eq!(super::run_dist_audit(true, true).unwrap(), 0);
    }

    #[test]
    fn dist_audit_loader_pins_safetensors_only() {
        let refs = [
            "crates/embeddings/src/e5_small.rs:84: let weights_path = dir.join(\"model.safetensors\");",
            "crates/embeddings/src/manifest.rs:48: \"model.safetensors\".into(),",
        ];
        assert!(super::dist_audit_check_loader_pins(&refs).is_ok());
    }

    #[test]
    fn dist_audit_loader_rejects_onnx_weight_ref() {
        let refs =
            ["crates/embeddings/src/e5_small.rs:84: let weights_path = dir.join(\"model.onnx\");"];
        let err = super::dist_audit_check_loader_pins(&refs).unwrap_err();
        assert!(err.contains("model.onnx"), "must name the file: {err}");
    }

    #[test]
    fn dist_audit_scan_failure_refuses_clean_report() {
        assert!(super::dist_audit_require_scan_ok(0, "deny-graph").is_ok());
        let err = super::dist_audit_require_scan_ok(2, "deny-graph").unwrap_err();
        assert!(err.contains("deny-graph"), "must name the scan: {err}");
    }

    #[test]
    fn dist_audit_strip_test_modules_removes_cfg_test_region() {
        let src = "use x;\n#[cfg(test)]\nmod tests {\n    fn f() { join(\"model.onnx\"); }\n}\nfn prod() {}\n";
        let stripped = super::dist_audit_strip_test_modules(src);
        assert!(!stripped.contains("model.onnx"), "test region must go");
        assert!(stripped.contains("fn prod()"), "production code must stay");
    }

    #[test]
    fn dist_audit_strip_test_modules_survives_braces_in_strings() {
        let src = "fn prod() { let s = \"}{\"; }\n#[cfg(test)]\nmod tests {\n    fn f() { let t = \"{\"; }\n}\n";
        let stripped = super::dist_audit_strip_test_modules(src);
        assert!(stripped.contains("fn prod()"), "production code must stay");
        assert!(!stripped.contains("fn f()"), "test region must go");
    }

    #[test]
    fn dist_audit_strip_test_modules_survives_raw_string_json() {
        let src = "fn prod() {}\n#[cfg(test)]\nmod tests {\n    fn f() {\n        let j = r#\"{\"architectures\": [\"X\"], \"nested\": {\"a\": 1}}\"#;\n        let _ = (1, 2);\n    }\n}\nfn tail() {}\n";
        let stripped = super::dist_audit_strip_test_modules(src);
        assert!(stripped.contains("fn prod()"), "production code must stay");
        assert!(
            stripped.contains("fn tail()"),
            "code after the region must stay"
        );
        assert!(!stripped.contains("architectures"), "test region must go");
    }

    #[test]
    fn dist_audit_find_forbidden_weight_refs_flags_only_foreign_ext() {
        let refs = super::dist_audit_find_forbidden_weight_refs(
            "a\nlet w = dir.join(\"model.safetensors\");\nlet o = dir.join(\"model.onnx\");\n",
        );
        assert_eq!(refs.len(), 1);
        assert!(refs[0].contains("model.onnx"), "must name it: {:?}", refs);
    }

    #[test]
    fn dist_audit_find_ignores_struct_field_access() {
        let refs = super::dist_audit_find_forbidden_weight_refs(
            "let url = model.url_for(file);\nlet id = model.id.clone();\nlet d = model.device;\n",
        );
        assert!(
            refs.is_empty(),
            "field access is not a weight file: {:?}",
            refs
        );
    }

    // Wave C3 (T-GATE-01 / RQ-28): requirement/evidence matrix consistency
    // gate. Each test feeds either an honest ledger shape (must pass) or a
    // deliberately-broken fixture (must fail) so the verb proves it detects
    // mislabeling instead of vacuously passing.
    fn gate_row<const N: usize>(
        id: &str,
        tests: [&str; N],
        status: &str,
        evidence: &str,
    ) -> super::GateLedgerRow {
        super::GateLedgerRow {
            id: id.to_string(),
            tests: tests.iter().map(|s| (*s).to_string()).collect(),
            status: status.to_string(),
            evidence: evidence.to_string(),
        }
    }

    #[test]
    fn gate_audit_passed_row_without_evidence_fails() {
        let rows = [gate_row("RQ-27", ["T-RELEASE-01"], "executed_passed", "")];
        let err =
            super::gate_audit_check_passed_evidence(&rows, "T-RELEASE-01", &[], &[]).unwrap_err();
        assert!(err.contains("RQ-27"), "must name the row: {err}");
    }

    #[test]
    fn gate_audit_passed_row_with_vacuous_evidence_fails() {
        let rows = [gate_row(
            "RQ-27",
            ["T-RELEASE-01"],
            "executed_passed",
            "Works great, trust us.",
        )];
        let err =
            super::gate_audit_check_passed_evidence(&rows, "T-RELEASE-01", &[], &[]).unwrap_err();
        assert!(err.contains("RQ-27"), "must name the row: {err}");
    }

    #[test]
    fn gate_audit_passed_row_naming_test_fn_passes() {
        let rows = [gate_row(
            "RQ-11",
            ["T-RANK-01"],
            "executed_passed",
            "ranking.rs: rrf_hand_computed_one_based, mmr coverage",
        )];
        let fns = ["rrf_hand_computed_one_based"];
        assert!(super::gate_audit_check_passed_evidence(&rows, "", &fns, &[]).is_ok());
    }

    #[test]
    fn gate_audit_passed_row_naming_decision_file_passes() {
        let rows = [gate_row(
            "RQ-02",
            ["T-GATE-01"],
            "executed_passed",
            "AD-01 decided Option B; see plans/AD-01_canonical_backend.md",
        )];
        let paths = ["plans/AD-01_canonical_backend.md"];
        assert!(super::gate_audit_check_passed_evidence(&rows, "", &[], &paths).is_ok());
    }

    #[test]
    fn gate_audit_not_run_cell_marked_passed_fails() {
        let tools = [super::GateMatrixTool {
            name: "memory_read".to_string(),
            checks: vec![("scope_and_concurrency".to_string(), "passed".to_string())],
        }];
        let err = super::gate_audit_check_cell_vocabulary(&tools).unwrap_err();
        assert!(err.contains("memory_read"), "must name the tool: {err}");
        assert!(err.contains("passed"), "must name the mislabel: {err}");
    }

    #[test]
    fn gate_audit_honest_not_run_cell_passes() {
        let tools = [super::GateMatrixTool {
            name: "memory_read".to_string(),
            checks: vec![
                ("name_and_schema".to_string(), "executed_passed".to_string()),
                ("scope_and_concurrency".to_string(), "not_run".to_string()),
            ],
        }];
        assert!(super::gate_audit_check_cell_vocabulary(&tools).is_ok());
    }

    #[test]
    fn gate_audit_dangling_test_id_fails() {
        let rows = [gate_row(
            "RQ-99",
            ["T-FROB-99"],
            "executed_partial",
            "covers T-FROB-99, remaining work deferred",
        )];
        let err = super::gate_audit_check_test_ids_resolve(&rows, "T-GATE-01 T-RELEASE-01", &[])
            .unwrap_err();
        assert!(
            err.contains("T-FROB-99"),
            "must name the dangling ID: {err}"
        );
    }

    #[test]
    fn gate_audit_plans_defined_test_id_passes() {
        let rows = [gate_row(
            "RQ-28",
            ["T-GATE-01"],
            "executed_partial",
            "gate verb run green, checklist gaps deferred",
        )];
        assert!(
            super::gate_audit_check_test_ids_resolve(&rows, "T-GATE-01 procedure", &[]).is_ok()
        );
    }

    #[test]
    fn gate_audit_partial_without_gap_fails() {
        let rows = [gate_row(
            "RQ-04",
            ["T-CONC-01"],
            "executed_partial",
            "Complete coverage; every test executed and passing.",
        )];
        let err = super::gate_audit_check_partial_gaps(&rows).unwrap_err();
        assert!(err.contains("RQ-04"), "must name the row: {err}");
    }

    #[test]
    fn gate_audit_partial_naming_gap_passes() {
        let rows = [gate_row(
            "RQ-04",
            ["T-CONC-01"],
            "executed_partial",
            "T-CONC-01 disjoint-field races still deferred.",
        )];
        assert!(super::gate_audit_check_partial_gaps(&rows).is_ok());
    }

    #[test]
    fn gate_audit_not_executed_claiming_execution_fails() {
        let rows = [gate_row(
            "RQ-27",
            ["T-RELEASE-01"],
            "specified_not_executed",
            "Executed and green, all tests passing.",
        )];
        let err = super::gate_audit_check_not_executed_honest(&rows).unwrap_err();
        assert!(err.contains("RQ-27"), "must name the row: {err}");
    }

    #[test]
    fn gate_audit_not_executed_empty_or_deferred_passes() {
        let rows = [
            gate_row("RQ-27", ["T-RELEASE-01"], "specified_not_executed", ""),
            gate_row(
                "RQ-28",
                ["T-GATE-01"],
                "specified_not_executed",
                "Deferred to a later wave, not run.",
            ),
        ];
        assert!(super::gate_audit_check_not_executed_honest(&rows).is_ok());
    }

    #[test]
    fn gate_audit_aggregate_complete_with_open_rows_fails() {
        let rows = [gate_row("RQ-04", ["T-X"], "executed_partial", "deferred")];
        let tools = [super::GateMatrixTool {
            name: "t".to_string(),
            checks: vec![("c".to_string(), "not_run".to_string())],
        }];
        let err = super::gate_audit_check_aggregates_honest("complete", "planned", &rows, &tools)
            .unwrap_err();
        assert!(err.contains("complete"), "must name the aggregate: {err}");
    }

    #[test]
    fn gate_audit_honest_aggregate_passes() {
        let rows = [gate_row("RQ-04", ["T-X"], "executed_partial", "deferred")];
        let tools = [super::GateMatrixTool {
            name: "t".to_string(),
            checks: vec![("c".to_string(), "not_run".to_string())],
        }];
        assert!(
            super::gate_audit_check_aggregates_honest(
                "implementation_in_progress",
                "planned_inventory_not_wire_capture",
                &rows,
                &tools
            )
            .is_ok()
        );
    }

    #[test]
    fn gate_audit_decision_report_proposed_fails() {
        let rows = [gate_row(
            "RQ-02",
            ["T-GATE-01"],
            "executed_passed",
            "see AD-01",
        )];
        let err = super::gate_audit_check_decision_report("Status: PROPOSED", &rows).unwrap_err();
        assert!(
            err.contains("DECIDED"),
            "must name the required state: {err}"
        );
    }

    #[test]
    fn gate_audit_decision_report_decided_passes() {
        let rows = [gate_row(
            "RQ-02",
            ["T-GATE-01"],
            "executed_passed",
            "AD-01 decided Option B (Fjall + Lance)",
        )];
        let ad01 = "Status: DECIDED\nCanonical state lives in Fjall keyspaces. Lance is the projection store.";
        assert!(super::gate_audit_check_decision_report(ad01, &rows).is_ok());
    }

    #[test]
    fn gate_audit_matrix_shape_skew_fails() {
        let tools = [super::GateMatrixTool {
            name: "memory_read".to_string(),
            checks: vec![("name_and_schema".to_string(), "executed_passed".to_string())],
        }];
        let err = super::gate_audit_check_matrix_shape(29, &tools).unwrap_err();
        assert!(err.contains("memory_read"), "must name the tool: {err}");
    }

    #[test]
    fn gate_audit_find_test_ids_scans_shapes() {
        assert_eq!(
            super::gate_audit_find_test_ids("covers T-GATE-01 and T-RELEASE-01, not T- or T-x"),
            vec!["T-GATE-01", "T-RELEASE-01"]
        );
        assert!(super::gate_audit_find_test_ids("no ids here").is_empty());
    }

    #[test]
    fn gate_audit_unknown_rq_status_fails() {
        let rows = [gate_row(
            "RQ-27",
            ["T-RELEASE-01"],
            "passed",
            "bundle green",
        )];
        let err = super::gate_audit_check_rq_vocabulary(&rows).unwrap_err();
        assert!(err.contains("RQ-27"), "must name the row: {err}");
    }

    #[test]
    fn gate_audit_dry_run_prints_without_executing() {
        assert_eq!(super::run_gate_audit(true).unwrap(), 0);
    }

    #[test]
    fn gate_audit_scanners_survive_unicode_evidence() {
        // Real RQ-04/08 evidence carries `→`/`∥`/accents; byte stepping
        // split them and panicked on the real ledger (C3 GREEN run).
        let ev = "A→B ∥ B→A, exactly one winner (barrière suprême), covers T-GATE-01";
        assert_eq!(
            super::gate_audit_find_test_ids(ev),
            vec!["T-GATE-01".to_string()]
        );
        assert!(super::gate_audit_has_gap("résumé deferred → later"));
        assert!(!super::gate_audit_claims_execution(
            "état stable, not run → later"
        ));
    }

    // Fix round 1/2 (SPEC Req-2): runnability. A T-ID resolves only via an
    // xtask verb or a non-#[ignore] test fn; grandfathered rows warn.
    #[test]
    fn gate_audit_text_parser_marks_test_and_ignored() {
        let src = "fn plain_helper() {}\n#[test]\nfn runnable_one() {}\n#[test]\n#[ignore]\nfn skipped_one() {}\n#[tokio::test]\nasync fn runnable_two() {}\n/// documented\n#[test]\nfn documented() {}\n";
        let (runnable, ignored) = super::gate_audit_test_fns_in_source(src);
        for name in ["runnable_one", "runnable_two", "documented"] {
            assert!(
                runnable.contains(&name.to_string()),
                "{name} must be runnable"
            );
        }
        assert_eq!(ignored, vec!["skipped_one".to_string()]);
        assert!(!runnable.iter().any(|n| n == "plain_helper"));
        assert!(!ignored.iter().any(|n| n == "plain_helper"));
    }

    #[test]
    fn gate_audit_verb_table_pins_automation_ids() {
        assert_eq!(
            super::gate_audit_verb_for_test("T-GATE-01"),
            Some("gate-audit")
        );
        assert_eq!(
            super::gate_audit_verb_for_test("T-RELEASE-01"),
            Some("evidence")
        );
        assert_eq!(
            super::gate_audit_verb_for_test("T-BUILD-01"),
            Some("dist-audit")
        );
        assert_eq!(
            super::gate_audit_verb_for_test("T-SEC-04"),
            Some("dist-audit")
        );
        assert_eq!(super::gate_audit_verb_for_test("T-EMB-01"), None);
    }

    #[test]
    fn gate_audit_ignored_only_evidence_fails() {
        // live_e5_embed_against_provisioned_cache is really #[ignore]d: with
        // a runnable inventory lacking it, evidence naming only it fails.
        let rows = [gate_row(
            "RQ-23",
            ["T-BENCH-99"],
            "executed_passed",
            "reference run live_e5_embed_against_provisioned_cache green",
        )];
        let err =
            super::gate_audit_check_passed_evidence(&rows, "T-BENCH-99", &[], &[]).unwrap_err();
        assert!(err.contains("RQ-23"), "must name the row: {err}");
    }

    #[test]
    fn gate_audit_non_test_fn_never_counts() {
        // durability_barrier is production code, never #[test]-attributed: a
        // runnable inventory has no such entry, so the evidence must fail.
        let rows = [gate_row(
            "RQ-23",
            ["T-BENCH-99"],
            "executed_passed",
            "barrier via durability_barrier with SyncAll",
        )];
        let err = super::gate_audit_check_passed_evidence(&rows, "", &["some_other_fn"], &[])
            .unwrap_err();
        assert!(err.contains("RQ-23"), "must name the row: {err}");
    }

    #[test]
    fn gate_audit_verb_mapped_id_resolves_without_test_fn() {
        let rows = [gate_row(
            "RQ-28",
            ["T-GATE-01"],
            "executed_partial",
            "gate verb run green, gaps deferred",
        )];
        let (note, warnings) =
            super::gate_audit_check_test_ids_resolve(&rows, "T-GATE-01 procedure", &[]).unwrap();
        assert!(warnings.is_empty(), "no warnings expected: {warnings:?}");
        assert!(note.contains("verb"), "must cite the verb: {note}");
    }

    #[test]
    fn gate_audit_id_backed_by_runnable_fn_resolves() {
        let rows = [gate_row(
            "RQ-04",
            ["T-CONC-99"],
            "executed_partial",
            "one winner via concurrent_opposite_supersession_edges_keep_graph_acyclic, rest deferred",
        )];
        let fns = ["concurrent_opposite_supersession_edges_keep_graph_acyclic"];
        let (_, warnings) =
            super::gate_audit_check_test_ids_resolve(&rows, "T-CONC-99 table", &fns).unwrap();
        assert!(warnings.is_empty(), "no warnings expected: {warnings:?}");
    }

    #[test]
    fn gate_audit_unbacked_id_fails_for_owned_rows() {
        let rows = [gate_row(
            "RQ-27",
            ["T-BENCH-99"],
            "executed_partial",
            "bench tails deferred, not run",
        )];
        let err =
            super::gate_audit_check_test_ids_resolve(&rows, "T-BENCH-99 table", &[]).unwrap_err();
        assert!(err.contains("T-BENCH-99"), "must name the ID: {err}");
    }

    #[test]
    fn gate_audit_unbacked_id_warns_for_grandfathered_row() {
        let rows = [gate_row(
            "RQ-03",
            ["T-STORE-03"],
            "executed_passed",
            "WP-00 baseline: pinned manifest (cargo check OK)",
        )];
        let (_, warnings) =
            super::gate_audit_check_test_ids_resolve(&rows, "T-STORE-03 table", &[]).unwrap();
        assert_eq!(warnings.len(), 1, "exactly one warning: {warnings:?}");
        assert!(
            warnings[0].contains("RQ-03") && warnings[0].contains("T-STORE-03"),
            "must name row + ID: {warnings:?}"
        );
        assert!(
            warnings[0].contains("legacy evidence"),
            "must carry the legacy phrase: {warnings:?}"
        );
    }

    #[test]
    fn gate_audit_unbacked_id_warns_for_partial_row() {
        let rows = [gate_row(
            "RQ-09",
            ["T-EMB-99"],
            "executed_partial",
            "reference equivalence loosened with rationale, GPU profile deferred",
        )];
        let (_, warnings) =
            super::gate_audit_check_test_ids_resolve(&rows, "T-EMB-99 table", &[]).unwrap();
        assert_eq!(warnings.len(), 1, "exactly one warning: {warnings:?}");
        assert!(
            warnings[0].contains("T-EMB-99") && warnings[0].contains("RQ-09"),
            "must name ID + row: {warnings:?}"
        );
    }

    #[test]
    fn gate_audit_grandfathered_passed_row_without_match_warns() {
        let rows = [gate_row(
            "RQ-01",
            ["T-STORE-01"],
            "executed_passed",
            "ancient lore, no checkable artifacts",
        )];
        let (_, warnings) = super::gate_audit_check_passed_evidence(&rows, "", &[], &[]).unwrap();
        assert_eq!(warnings.len(), 1, "exactly one warning: {warnings:?}");
        assert!(
            warnings[0].contains("RQ-01"),
            "must name the row: {warnings:?}"
        );
    }
}
