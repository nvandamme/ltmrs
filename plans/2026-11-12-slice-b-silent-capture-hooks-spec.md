# Slice B — Silent-Capture Hooks: Spec

## Goal

Add an opt-in hook pair (`hook-inject` on session start, `hook-extract` on stop) that closes the passive-recall deficit vs the four Claude memory servers, without changing the explicit-tools-primary contract.

## Non-goals

No daemon protocol change. No new MCP tool. No `ResourceLimits` change. No shipped code under `tools/`. No change to the explicit recall→act→persist workflow. No auto-`guide_distill`/`memory_forget` from hooks. No network in the hook path.

## Architecture

Hook logic lives in `ltmrs-frontend` as two CLI subcommands (portable Rust, Windows-clean) that attach to the managed daemon over the existing IPC path and reuse the existing server-side `scan_snapshot` budget. Host wiring is generated JSON snippets installed by an `install-hooks` command mirroring the skill/shim installers.

Precedent mapping: yuvalsuede `Stop`/`PreCompact`/`SessionEnd` → our `extract`; kurovu146 `UserPromptSubmit` → our `inject` (+ `SessionStart` cold boot); mem0 `SessionStart`/`SessionEnd` → our pair. Hosts differ only in the JSON snippet that execs `ltmrs hook-inject` / `ltmrs hook-extract`; the Rust core is host-agnostic.

## Global constraints

- Explicit tools stay primary; hooks are opt-in and default OFF.
- `cargo fmt -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` gate any implementation.
- No shipped code under `tools/` (dev harness only). Windows path parity (AD-07). Crate DAG stays acyclic (frontend owns CLI + skills + hosts).
- Spec lives in `plans/` (tracked), not `.kilo/` (OMP has no kilo-code; user order 2026-11-12).

## Interfaces (frozen; implementers use exactly these names)

```rust
// crates/frontend/src/skills/hooks.rs (new)
pub const HOOKS_VERSION: &str;
pub struct HooksConfig { pub enabled: bool }
pub fn hooks_config_path(home: &Path) -> PathBuf;
pub fn load_hooks_config(home: &Path) -> HooksConfig;
pub fn install_hooks(home: &Path, exe: &Path) -> Result<InstallOutcome, InstallError>;
pub fn hook_snippet(host: &str, exe: &Path) -> Result<String, HooksError>;
pub struct HookInjectInput { pub query_hint: String, pub project: Option<String> }
pub struct HookExtractInput { pub transcript_tail: String, pub project: Option<String> }
pub async fn run_hook_inject(home: Option<String>, input: &HookInjectInput) -> HookOutput;
pub async fn run_hook_extract(home: Option<String>, input: &HookExtractInput) -> HookOutput;
pub struct HookOutput { pub exit_code: i32, pub stdout: String }
```

- `HookOutput.stdout` for inject is the host's `additionalContext` string; for extract it is empty on success.
- CLI: `Command::HookInject`, `Command::HookExtract`, `Command::InstallHooks` in `crates/frontend/src/cli.rs`; dispatch arms in `src/main.rs`.
- Transport: subcommands read one JSON object on stdin, write result on stdout, diagnostics on stderr, exit 0 even on daemon/budget failure (fail-open).

## Behavior

Phase 1 transport: parse `hook-inject`, `hook-extract` (stdin-JSON), `install-hooks`; `help_text` documents all three. Unknown host = loud error; disabled config = silent `(0, "")`. Missing/unparseable config ⇒ disabled (never invent a home).

Phase 2 inject: truncate hint to 2000 chars; project from hook cwd via `project_status` resolution else None. Cold (no live session) → `session_start` continuity recall truncated to 4000 chars; warm → lexical `semantic_search` top-5. Budget refusal / daemon-down ⇒ fail-open `(0, "")` + one stderr line. 5s end-to-end timeout. Never sends transcript bulk.

Phase 3 extract: last 8KB transcript, local secret-carrier pre-filter (best-effort; daemon authoritative), ≤5 candidates of 30–2000 chars, markdown, English. One `memory_add` per candidate (`source: "hook-extract"`, `confirm: false`); no `evidence` unless file+snippet cited; no `memory_update`/`forget`/`distill`. Per-call failure logged, never fatal; exit 0.

Phase 4 wiring: `install-hooks` writes `<home>/.ltmrs/hooks.json` atomically (temp+rename); refuses on unmanaged file; prints snippet path. Env override `LTMRS_HOOKS_ENABLED=0|1` (explicit wins; garbage ⇒ loud stderr + disabled). Skill doc: one opt-in paragraph, workflow list byte-identical. Hosts: `claude-code` first, others documented-not-verified stubs.

## Test plan

Unit (arg parsing, disabled no-op, snippet per host, garbage config ⇒ disabled) inline or `hooks_tests.rs` if <800 LOC. Integration via `start_local_daemon` on ephemeral home, lexical-only, no network: inject returns matching title; budget-0 ⇒ exit 0 empty; daemon down ⇒ exit 0 empty; extract stores 2 findings, rerun adds none, API key never stored.

## Plan-authority updates (one review)

Part I new § (opt-in, budget reuse, fail-open). Part II A-gap-1 item under WP-10/WP-13. Part III hook gates. `traceability.json` + `conformance_matrix.json` ledger entries. Skill text keeps recall→act→persist list byte-identical.

## Open questions (decided in spec)

OQ1 host schemas: ship `claude-code` first, others documented-not-verified. OQ2 live-session inject: `semantic_search` warm path (safe); `session_start` cold only, replay-safe. OQ3 `memory_add` fields: `confirm:false`, `fragment_type` pattern|lesson, `evidence` only with file+snippet. OQ4 scope: checkout `.ltmrs/project.json` when inside bound checkout, else daemon default. OQ5 timeout 5s initial, verify vs host docs in Phase 4. OQ6 one template per host behind `hook_snippet(host)`.

## Spec self-review

- Placeholders: none — OQs decided above, values concrete.
- Consistency: budget reuse vs no-new-budget holds; fail-open vs loud-errors holds (unknown host loud = install-time config error, runtime failures silent).
- Scope: single subsystem (frontend CLI + skills), daemon untouched except as client.
- Ambiguity: `importance`/render_budget belong to Slice D, not here — inject uses existing `semantic_search` ordering, no new ranking.
