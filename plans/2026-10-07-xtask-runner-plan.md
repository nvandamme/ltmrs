# xtask Runner Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the approved `plans/2026-10-07-xtask-runner-design.md` as a private `xtask/` workspace member exposing all seven §20 verbs with zero new probing logic.

**Architecture:** Hand-rolled `std::env::args` parsing (no clap — no new lockfile crates) dispatching to per-verb runners; script verbs shell out to `tools/`, suite verbs run scoped `cargo test` selectors; every run writes a JSON run-record under `reports/` and propagates the child exit status. A global `--dry-run` flag prints the would-be command for testability.

**Tech Stack:** Rust edition 2024 (pinned toolchain), `serde_json` workspace edge only (no new Cargo.lock crates), existing `tools/` scripts, existing test suites as the execution mechanism.

## Global Constraints

- Rust edition 2024; pinned toolchain in `rust-toolchain.toml` unchanged.
- `cargo fmt -- --check` clean after every task.
- `cargo clippy --all-targets -- -D warnings` clean after every task.
- No new crates in `Cargo.lock` (`serde_json` edge only; `cargo tree -p xtask` must show no new packages).
- TDD: every behavior has a failing-first test; watch each fail for the right reason.
- Never conflate planned/inspected/executed/passed (evidence discipline).
- One reviewed commit per task; never amend published history; never `cargo clean`.
- `xtask` never enters any production dependency graph (no other crate may depend on it).

---

### Task 1: Scaffolding — alias, manifest, arg parsing, `--help`

**Files:**
- Create: `.cargo/config.toml`, `xtask/Cargo.toml`, `xtask/src/main.rs`
- Modify: root `Cargo.toml` (workspace `members` += `"xtask"`)
- Test: unit tests inside `xtask/src/main.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing (first task)
- Produces: `parse_args(Vec<String>) -> Result<Command, String>` where `Command` is the verb enum below (later tasks match on its variants — exact names: `CaptureLemma { source: String }`, `Capabilities { candidate: String }`, `Conformance { profile: String }`, `Recovery { suite: String }`, `Benchmark { manifest: String, limit: Option<u64> }`, `Quality { split: String }`, `Evidence { release: String }`, plus global `dry_run: bool` carried alongside, and `Help { verb: Option<String> }`); `fn usage() -> &'static str` (full help text).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn parses_evidence_verb() {
    let cmd = parse_args(vec!["xtask".into(), "evidence".into(), "--release".into(), "v0.1".into()]).unwrap();
    assert!(matches!(cmd, Command::Evidence { release, .. } if release == "v0.1"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p xtask parses_evidence_verb`
Expected: FAIL with "cannot find function `parse_args`" (nothing implemented yet)

- [ ] **Step 3: Write `.cargo/config.toml`, `xtask/Cargo.toml`, minimal `main.rs`**

```toml
# .cargo/config.toml
[alias]
xtask = "run --quiet -p xtask --"
```

```toml
# xtask/Cargo.toml
[package]
name = "xtask"
version = "0.1.0-alpha"
edition = "2024"
publish = false

[dependencies]
serde_json = { version = "1", features = ["preserve_order"] }
```

`main.rs` implements `Command`, `parse_args` (hand-rolled prefix matching; unknown verb/flag → `Err` naming the token; `--dry-run` sets the flag), `usage()`, and `fn main()` printing usage on empty/`--help` (exit 0) and errors to stderr (exit 2).

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p xtask 2>&1 | tail -n 3
cargo xtask --help 2>&1 | head -n 5
```

Expected: all unit tests PASS (parsing for all 7 verbs + `--dry-run` + unknown-verb error); `--help` prints usage.

- [ ] **Step 5: Verify no lockfile drift and commit**

```bash
git diff --stat Cargo.lock | head -n 3
cargo tree -p xtask --depth 1 --prefix none
git add .cargo/config.toml xtask/ Cargo.toml Cargo.lock
git commit -m "feat: add private xtask runner scaffolding (arg parsing only)"
```

Expected: lockfile shows no new package versions (serde_json edge only); tree shows std + serde_json path.

---

### Task 2: Run-record + dispatch core

**Files:**
- Create: `xtask/src/run.rs` (declared `mod run;` in `main.rs`)
- Modify: `xtask/src/main.rs` (wire each `Command` variant to a stub runner returning "not yet implemented" until Task 3–5 fills them — no, do not stub: this task implements ONLY the shared core; `main` arms call `run::execute(argv, workdir)` helper shape defined here)
- Test: `xtask/src/run.rs` unit tests

**Interfaces:**
- Consumes: `Command` from Task 1 (unchanged)
- Produces: `fn execute(program: &str, args: &[String], workdir: &std::path::Path) -> RunOutcome` with `struct RunOutcome { exit_code: i32, stdout_tail: String, stderr_tail: String, started_ms: u64, finished_ms: u64 }`; `fn write_record(dir: &std::path::Path, verb: &str, argv: &[String], outcome: &RunOutcome) -> std::io::Result<std::path::PathBuf>` writing `record.json` with fields `verb, argv, started_ms, finished_ms, exit_code` and returning its path. Later tasks call exactly these two functions and propagate `exit_code` as the process exit.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn failing_child_propagates_exit_code() {
    let out = execute("sh", &["-c".into(), "exit 3".into()], std::path::Path::new("/tmp"));
    assert_eq!(out.exit_code, 3);
}

#[test]
fn record_shape_has_required_fields() {
    let dir = tempfile_dir();
    let path = write_record(&dir, "evidence", &["--release".into(), "v0.1".into()], &fake_outcome()).unwrap();
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    for field in ["verb", "argv", "started_ms", "finished_ms", "exit_code"] {
        assert!(v.get(field).is_some(), "missing {field}");
    }
}
```

(`tempfile_dir()` and `fake_outcome()` are test-only helpers in the same test module — write them as part of this step. `tempfile` is a root dev-dependency; add it under `[dev-dependencies]` of `xtask/Cargo.toml` verbatim `tempfile = "3"`.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p xtask`
Expected: FAIL — `execute`/`write_record` do not exist.

- [ ] **Step 3: Implement `run.rs` with `std::process::Command`**

Capture both streams fully (pipes, not inheritance), tail to last 4 KiB each for the outcome struct, full status preserved. `write_record` creates the dir (all parents) and writes pretty JSON. No truncation of the exit code path: signal-kill maps to exit code 128+SIGKILL semantics documented in a comment (Rust `ExitStatus::code()` returns `None` on signal — record `exit_code: -1` plus `signal: true` field in that case).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p xtask 2>&1 | tail -n 3`
Expected: PASS, including the two new tests.

- [ ] **Step 5: Commit**

```bash
git add xtask/ Cargo.toml Cargo.lock
git commit -m "feat: add xtask run-record and dispatch core"
```

---

### Task 3: Script verbs — evidence, capture-lemma, benchmark

**Files:**
- Modify: `xtask/src/main.rs` (add `xtask/src/verbs.rs` with `pub fn run_evidence`, `pub fn run_capture`, `pub fn run_benchmark`, each `-> anyhow::Result<i32>` — no, no anyhow (new dep): return `Result<i32, String>`; wire the three match arms)
- Test: `xtask/src/verbs.rs` unit tests (argv construction) + `--dry-run` executions

**Interfaces:**
- Consumes: `Command::{Evidence, CaptureLemma, Benchmark}` + `execute`/`write_record` from Task 2
- Produces: three `pub fn` runners returning the child exit code; `--dry-run` prints the exact argv and returns 0 without executing (used by tests; later tasks reuse the flag)

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn evidence_argv_is_locked_release() {
    assert_eq!(evidence_argv("v0.1"), vec!["tools/gen_release_evidence.sh", "--release", "--locked"]);
}

#[test]
fn capture_argv_provisions_sandbox_paths() {
    let argv = capture_argv("/src/lemma", "/tmp/home-XYZ", "/tmp/out-XYZ");
    assert_eq!(argv[0], "node");
    assert!(argv.windows(2).any(|w| w == ["--repo", "/src/lemma"]));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p xtask`
Expected: FAIL — `evidence_argv`/`capture_argv` do not exist.

- [ ] **Step 3: Implement the three runners**

`run_evidence`: `execute("bash", ["tools/gen_release_evidence.sh", "--release", "--locked"])`, record under `reports/xtask-evidence-<timestamp>/`. `run_capture`: provision two tempdirs (sandbox home + out), `execute("node", ["tools/capture_lemma.mjs", "--repo", source, "--home", home, "--out", out])`. `run_benchmark`: `execute("python3", ["tools/bench_against_ltmrs.py", "--ops", manifest, "--out", <reports dir>, plus "--limit" when given])` with `HOME` set to a sandbox tempdir (do not use the developer's real HOME). `--dry-run` short-circuits before `execute`, printing `would run: <program> <args...>` and returning 0.

- [ ] **Step 4: Run tests + one live cheap execution**

```bash
cargo test -p xtask 2>&1 | tail -n 3
cargo xtask evidence --dry-run --release v0.1
```

Expected: unit tests PASS; dry-run prints the exact argv and exits 0 without creating any bundle.

- [ ] **Step 5: Commit**

```bash
git add xtask/
git commit -m "feat: add xtask evidence, capture-lemma and benchmark verbs"
```

---

### Task 4: Quality verb

**Files:**
- Modify: `xtask/src/verbs.rs` (add `pub fn run_quality`), `xtask/src/main.rs` (wire the arm)
- Test: argv/env unit tests + dry-run

**Interfaces:**
- Consumes: `Command::Quality`, `execute`/`write_record` from Task 2
- Produces: `pub fn run_quality(split: &str) -> Result<i32, String>` (only `"heldout"` and `"dev"` resolve; anything else is a hard error naming the token)

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn quality_rejects_unknown_split() {
    assert!(run_quality_parts("prod").is_err());
}

#[test]
fn quality_wave_env_uses_sandbox_probe_home() {
    let (prog, args, env) = quality_wave_command("/tmp/probe-XYZ", "heldout");
    assert_eq!(prog, "python3");
    assert!(args.contains(&"tools/agent_quality_wave.py".to_string()));
    assert_eq!(env.iter().find(|(k, _)| k == "PROBE_HOME"), Some(&("PROBE_HOME".to_string(), "/tmp/probe-XYZ".to_string())));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p xtask`
Expected: FAIL — `run_quality_parts`/`quality_wave_command` do not exist.

- [ ] **Step 3: Implement**

`run_quality`: reject unknown splits; run `python3 tools/gen_calibration_corpus.py` (zero-arg, per its `__main__`), then `python3 tools/agent_quality_wave.py` with `PROBE_HOME` set to a fresh sandbox tempdir (never the developer's real HOME; `RATER_MODEL` passes through from the environment untouched). Either child failing stops the verb and propagates its exit code. Document in a comment that model inference requires pre-provisioned artifacts/rater access (same precondition as running the scripts by hand).

- [ ] **Step 4: Run tests + dry-run**

```bash
cargo test -p xtask 2>&1 | tail -n 3
cargo xtask quality --dry-run --split heldout
```

Expected: PASS; dry-run prints both commands, executes nothing.

- [ ] **Step 5: Commit**

```bash
git add xtask/
git commit -m "feat: add xtask quality verb (corpus + wave pipeline)"
```

---

### Task 5: Suite-selector verbs — capabilities, conformance, recovery

**Files:**
- Modify: `xtask/src/verbs.rs` (add `pub fn run_capabilities`, `pub fn run_conformance`, `pub fn run_recovery`), `xtask/src/main.rs` (wire arms)
- Test: argv unit tests + dry-run per verb + ONE live execution (compat suite, fastest)

**Interfaces:**
- Consumes: `Command::{Capabilities, Conformance, Recovery}`, `execute`/`write_record` from Task 2
- Produces: three `pub fn` runners with these EXACT selector sets (unknown candidate/profile/suite is a hard error naming the token):
  - `capabilities fjall-lance`: `cargo test -p ltmrs-service -- namespace restore receipt contention durable barrier` then `cargo test -p ltmrs-search -- fts backend projector table` (stop at first failure, propagate its code). `lance` (or anything else) errors: "capabilities candidate removed post-AD-01 (losing path deleted); only fjall-lance resolves".
  - `conformance lemma-0.21.0`: `cargo test -p ltmrs-compat` then `cargo test -p ltmrs-daemon -- differential`. Anything else errors naming the supported profile.
  - `recovery durable`: `cargo test -p ltmrs-service -- restore replay kill soak fragmented` then `cargo test -p ltmrs-interchange -- restore` then `cargo test --test daemon_lifecycle`. Anything else errors naming the supported suite.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn capabilities_argv_pins_backend_suites() {
    let cmds = capability_commands("fjall-lance").unwrap();
    assert!(cmds.iter().any(|c| c.contains(&"-p".to_string()) && c.contains(&"ltmrs-service".to_string())));
    assert!(capability_commands("lance").is_err());
}

#[test]
fn conformance_argv_pins_compat_and_differential() {
    let cmds = conformance_commands("lemma-0.21.0").unwrap();
    assert_eq!(cmds.len(), 2);
    assert!(conformance_commands("lemma-0.22.0").is_err());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p xtask`
Expected: FAIL — `capability_commands`/`conformance_commands` do not exist.

- [ ] **Step 3: Implement the three runners** (each stops at the first failing child and returns its code; each child gets its own run-record entry under one `reports/xtask-<verb>-<timestamp>/` dir)

- [ ] **Step 4: Run unit tests, dry-runs, and one live execution**

```bash
cargo test -p xtask 2>&1 | tail -n 3
cargo xtask capabilities --dry-run --candidate fjall-lance
cargo xtask conformance --dry-run --profile lemma-0.21.0
cargo xtask recovery --dry-run --suite durable
cargo test -p ltmrs-compat 2>&1 | tail -n 2
```

Expected: unit PASS; all three dry-runs print exact argv, exit 0, execute nothing; live compat suite green (9 passed — proves the conformance path resolves to a real suite).

- [ ] **Step 5: Commit**

```bash
git add xtask/
git commit -m "feat: add xtask capabilities, conformance and recovery verbs"
```

---

### Task 6: Gate, §20 amendment, close-out

**Files:**
- Modify: `plans/02_implementation_guide.md` (§20: mark each verb delivered with its backing mechanism + the two refusal rules), `DONE.md` + `CHANGELOG.md` (per repo commit workflow)
- Test: none new (verification commands only)

**Interfaces:**
- Consumes: all verbs from Tasks 1–5
- Produces: committed close-out; §20 no longer claims an unimplemented interface

- [ ] **Step 1: Run the full gate**

```bash
cargo fmt -- --check && echo FMT-OK
cargo clippy --all-targets -- -D warnings 2>&1 | tail -n 2
cargo check --workspace 2>&1 | tail -n 2
cargo test -p xtask 2>&1 | tail -n 3
cargo xtask --help > /dev/null && echo HELP-OK
for v in "capabilities --candidate fjall-lance" "conformance --profile lemma-0.21.0" "recovery --suite durable" "evidence --help"; do cargo xtask $v --dry-run > /dev/null && echo "DRY-OK: $v" || echo "DRY-FAIL: $v"; done
```

Expected: everything green; every dry-run exits 0.

- [ ] **Step 2: Amend §20 with delivery notes**

Append to the §20 block: per-verb backing (script path or suite selectors), the two refusal rules (unknown candidate/profile/suite errors; `lance` removed post-AD-01), and the `--dry-run` flag. Keep the original target-interface text intact above the note (historical record).

- [ ] **Step 3: Commit per workflow (DONE → CHANGELOG → verify → commit → hash → bake → stop)**

Follow `AGENTS.md` commit workflow exactly. Stage ONLY: `xtask/`, `.cargo/config.toml`, root `Cargo.toml`/`Cargo.lock` (if changed by member addition), `plans/02_implementation_guide.md`, `DONE.md`, `CHANGELOG.md`. Verify `grep -c` on the date heading before committing.

## Out of scope (explicitly)

- New capability probes, conformance logic, or recovery harnesses (verbs delegate; nothing is re-implemented).
- CI wiring, publishing `xtask`, changes to any `tools/` script.
- `sccache`, remote caching, profiling the runner itself.
