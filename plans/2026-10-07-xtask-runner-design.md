# xtask Development Runner — Design

**Status:** approved 2026-10-07 (approach A: thin uniform dispatcher over existing suites and scripts).
**Scope:** implements the §20 target interface of `plans/02_implementation_guide.md`
(`cargo xtask <verb>`) without adding probing logic, dependencies, or product behavior.

## Architecture

A private workspace member `xtask/` (Rust edition 2024, `publish = false`)
plus `.cargo/config.toml` with `[alias] xtask = "run --quiet -p xtask --"`.
Each verb is a subcommand that either shells out to its `tools/` script with
normalized flags or runs a scoped `cargo test` selector, then writes a JSON
run-record to `reports/xtask-<verb>-<timestamp>/` (gitignored, same
convention as release evidence bundles). Child exit statuses propagate
unchanged; the `xtask` crate never enters any production dependency graph
(no other crate may depend on it; verified by `cargo tree` in review).

## Verb mapping

- `capture-lemma --source <checkout>` → `node tools/capture_lemma.mjs --repo <checkout>`
  with xtask-provisioned sandbox `--home` (tempdir) and `--out` (baseline dir).
- `capabilities --candidate <name>` → scoped backend-gate suites
  (`cargo test -p ltmrs-service -p ltmrs-search` with the T-STORE/T-CONC/T-REC
  filters). Only `fjall-lance` resolves; `lance` errors explicitly — the
  losing path was removed post-AD-01 and is never silently substituted.
- `conformance --profile <name>` → compat + differential suites
  (`-p ltmrs-compat` plus the `differential_*` filters in `ltmrs-daemon`).
  Only `lemma-0.21.0` resolves.
- `recovery --suite durable` → kill/reopen + restore suites (service restore
  tests, interchange restore tests, `tests/daemon_lifecycle.rs`).
- `benchmark --manifest <file>` → `python3 tools/bench_against_ltmrs.py`
  with `HOME` sandbox and `--ops`/`--out` passthrough.
- `quality --split <name>` → `tools/agent_quality_wave.py` +
  `tools/gen_calibration_corpus.py` as documented in their headers.
- `evidence --release <version>` → `bash tools/gen_release_evidence.sh
  --release --locked` (version recorded in the run-record; the bundle keeps
  its own naming).

Unknown candidates, profiles, or splits are hard errors. No verb invents
results: selectors report the underlying suites' outcomes; scripts report
their own.

## Error handling

- Child non-zero exit → xtask exits non-zero with the child's stderr tail.
- Missing toolchains (`node`, `python3`) fail fast with a named-missing message.
- No manifest, lockfile, or source changes at runtime (read-only apart from
  `reports/` output).

## Testing

- Each verb: `--help` smoke test plus one happy-path execution asserting
  exit-status passthrough and run-record shape (JSON fields: verb, argv,
  started/finished, exit, artifact dir).
- Underlying suites keep their own tests; the runner claims no coverage
  beyond delegation (evidence discipline).
- Repo gates apply: `cargo fmt -- --check`, `cargo clippy --all-targets --
  -D warnings` clean.

## Non-goals

- No new capability probes, conformance logic, or recovery harnesses.
- No CI wiring, no publish, no changes to `tools/` scripts themselves.
