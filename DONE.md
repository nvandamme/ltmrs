# DONE.md

Work completed on the current unreleased commit.
Content before `---` is instructions — do not modify. Add entries after `## Unreleased Commit`.

---

## Unreleased Commit

### WP-10a CLI parser + help/version + real -lib (uncommitted)

- `src/cli.rs` (new, lib): two-phase parser — help/version short-circuit
  anywhere (help wins), then collect-all/validate (conflicting commands
  error either order, options bound to commands, flag-like values
  rejected, repeats last-wins, port 1-65535, unknown/positionals error);
  exit codes 0/1/2/3; `-lib` executes for real (explicit `--store`,
  must-exist check so reads never create stores).
- `src/main.rs`: thin dispatch (help/version/-lib real; stdio/visualizer/
  skill arms explicit `Unimplemented` exit 3, never silent); stdout flush
  before exit; errors to stderr.
- Reviews (formal + functional): double PASS (9 checks + 8 hunts);
  restructured from single-pass after review caught silent-precedence
  bugs; live binary verified (help/version/usage/unimplemented/-lib).
- Tests: 15 parser/execution pins.
  (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`: 435 passed, 0 failed.)
- Notes: value-position -h quirk (documented); live-store -lib lock
  contention needs a UX decision with the daemon-lifecycle slice.

### WP-10b managed skill installer (uncommitted)

- `src/skills/installer.rs` (new) + `ltmrs_skill.md` (native asset):
  ownership markers (version + sha256), decision table (fresh install,
  idempotent reinstall, outdated update, modified/foreign refusal,
  explicit override), atomic temp+rename writes, numeric version order.
- `src/cli.rs` + `main.rs`: `--install-skill` wired (explicit home,
  exit 0 installed/current, exit 1 refusals with backup-first advice).
- Reviews (formal + functional): fixed spoofed-marker bypass (dotted-
  numeric validation), empty-content idempotency, numeric-order coverage,
  root-proof denial test, distinct refusal messages, version-bump rule.
- Tests: 7 decision branches + 3 hardening (spoof, empty, numeric) +
  2 CLI command pins.
  (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`: 447 passed, 0 failed.)
- Notes: legacy/upstream skill content NOT snapshotted (no mislabeling);
  explicit legacy path + host recipes remain (WP-10c/hosts).

### WP-10c stdio serving + daemon lifecycle (uncommitted)

- `src/frontend/serve.rs` (new): `resolve_home` (missing/empty HOME is a
  runtime error, never an invented store), `stdio_layout` pins
  `$HOME/.ltmrs/{store,sessions.json,search}` + runtime base,
  `start_local_daemon` (in-process `Daemon::start` + `UnixStream` pair +
  spawned `handle_connection`; caller holds the daemon for the singleton
  lock, `shutdown` persists sessions), `connect_remote` (fail-fast connect,
  handshake stays lazy so the snapshot prefetch runs in both modes),
  `serve_stdio` (rmcp `serve_server` over `transport::stdio`, `QuitReason`
  mapped, daemon shut down on every exit path).
- `src/main.rs`: `#[tokio::main]`; `Stdio` arm serves (exit 0 on clean stdin
  EOF, 1 on runtime failure). `Cargo.toml`: rmcp `transport-io` + tokio
  `io-std` (pure Rust, no new native deps).
- `tests/stdio_smoke.rs` (new): live binary over piped stdio — `initialize`
  returns `serverInfo.name == "ltmrs"`, stdin EOF exits 0, managed store
  created under isolated HOME.
- Reviews: formal (socket-mode prefetch loss → lazy handshake; init-failure
  shutdown skip → shutdown-on-every-path) + functional (`QuitReason` is not
  a `Result`; `waiting()` JoinError mapped; `load` missing-file safe).
- Validation: `cargo fmt -- --check`, `cargo clippy --all-targets --
  -D warnings`, full `cargo test` (see WP-10e entry for final counts).

### WP-10d visualizer (uncommitted)

- `src/visualizer/mod.rs`: loopback-only HTTP/1.1 (no new deps):
  `GET /` index (counts + escaped titles/fragments, links `/api/library`),
  `GET /api/library` canonical export JSON, 404/405/400 shapes,
  per-request store open (never a second long-lived handle next to a
  daemon), `DEFAULT_VIS_PORT = 18721` (ltmrs-native, no upstream default on
  record). `-vis` backgrounds via detached `setsid` child + listen-poll +
  ownership probe (refuses port squatters); `-vis --fg` serves until Ctrl-C.
  Missing store is a usage error (`no store at ...`, never created).
- `src/main.rs`: `Visualize` arm wired (prints URL / farewell, exit codes
  preserved). `Cargo.toml`: tokio `net` + `signal` (pure Rust).
- `tests/vis_smoke.rs` (new): background detach live — parent exits 0 with
  `serving at ... (pid ...)`, index serves, reported pid killed, port goes
  quiet. Live-verified also: fg serves + Ctrl-C farewell exit 0; 404/405.
- Reviews: formal (ownership probe, pre_exec doc, accept-loop continue) +
  functional (brace repair after review patch; `ToSocketAddrs` via
  `Ipv4Addr`; `'static` spawn bounds via owned `String`; pipe-hang in smoke
  test → spawn-based, never `wait_with_output` on inherited stderr).

### WP-10e host recipes (uncommitted)

- `src/skills/hosts.rs` (new): mechanism-based recipes
  (`managed-skill-dir`, `mcp-stdio`, `visualizer`) with exact skill path
  (pinned identical to the installer's `.agents/skills/ltmrs/SKILL.md`),
  exact activation commands, and `documented (supported recipe, not
  verified host behavior)` status — no invented host names/versions.
  `--install-skill` output now appends the documented recipe count, keeping
  "installed" and "host loaded it" separate (T-HOST-01).
- RED caught a test bug (status `"not verified"` contains `"verified"` →
  assertion now checks the disclaimer positively).
- Validation (all three slices): `cargo fmt -- --check` clean,
  `cargo clippy --all-targets -- -D warnings` clean, `cargo test`:
  461 lib + stdio_smoke (1) + vis_smoke (1) passed, 0 failed.

### WP-10 remainings: skill-workflow pins, legacy shim, server-name proof (uncommitted)

- Skill workflow vs compatibility names (`src/skills/installer.rs`):
  `skill_tool_tokens` extracts tool references from the native skill;
  `skill_workflow_tools_match_frozen_names` pins every referenced tool
  against `frozen_tools()` and the recall → act → persist core;
  `multilingual_guidance_is_separate_and_documented` pins the standalone
  multilingual section.
- Legacy shim (`src/skills/shim.rs`, `--install-shim` CLI command):
  opt-in `~/.local/bin/lemma` symlink to the running binary; foreign files
  AND foreign symlinks refuse (never replaced); PATH collisions reported
  (PATH order, shim excluded, non-executables ignored) but never block.
  `install_shim_command` + parser exclusivity + help coverage + live-binary
  verification (install → already-current; real collision with the legacy
  `~/.local/bin/lemma` on this machine warned correctly).
- Server-name independence (`src/frontend/mcp.rs`):
  `routing_needs_no_server_name` proves routing depends only on tool names
  across all 26 frozen tools (namespaces stay host-configured per hosts.rs).
- Reviews (formal + functional): own-shim spelling tolerance
  (`check_current` canonicalizes; `Path` equality normalizes `.` away, so
  the test uses a symlinked parent); AlreadyExists re-read for concurrent
  installers (matched on `ErrorKind`, never message text); empty-PATH-entry
  CWD guard; clippy `collapsible_if` collapsed to a let-chain.
- Trackers: `plans/02_implementation_guide.md` WP-10 tasks 1-8 checked
  (task 8 with recorded deviation: mechanism-based recipes, no per-host
  version claims).
- Validation: `cargo fmt -- --check` clean,
  `cargo clippy --all-targets -- -D warnings` clean, `cargo test`:
  470 lib + stdio_smoke (1) + vis_smoke (1) passed, 0 failed.

### WP-09 remainings: dense guide candidates + differential workflow replay (uncommitted)

- Dense guide-catalog leg (`QueryEmbedder::embed_passages` defaulted trait
  method; `ServiceQueryEmbedder` Passage-role batch override over the worker;
  `SearchBackend::embed_query_sync`/`embed_passages_sync` bridges;
  `guide_catalog_text` + `suggest_guides_dense` + wiring in `exec_guide_get`):
  cosine-ranked (ranking::cosine, >0.0 only) catalog guides append after the
  token matches (cap 5); any failure degrades to byte-identical token output;
  scores never displayed. Role separation enforced by types (passages never
  through the Query seam).
- Differential workflow replay (8-step recall -> act -> persist):
  `tools/capture_workflow.mjs` (synthetic fixture script over MCP stdio) +
  `tools/normalize_workflow.mjs` (placeholder normalization, tracked, byte-
  reproduces the fixture) + `tests/compat/lemma_0_21_0/workflow_fixture.json`
  (pinned upstream transcript + provenance + regen commands) +
  `differential_workflow_replay_matches_upstream` (data-driven replay, exact
  parity on adds/guides, structural sets/deltas with declared divergences:
  4 seed fragments, coaching blocks, read ordering, auto-detected techs,
  distill suggestion, cwd-derived projects).
- Notable upstream behaviors observed (recorded, not replicated): fuzzy
  duplicate refusal on near-identical adds; auto-link on add; seed catalog;
  duplicate guide entry quirk.
- Reviews: formal (data-driven replay args; tracked normalizer; byte-exact
  fixture reproduction) + functional (regex $$ replacements; frozen-clock
  1970 dates; (global) key pass-through; spawn_blocking for block_on;
  type_complexity alias; GuideCreateArgs has no Default).
- Validation: `cargo fmt -- --check` clean,
  `cargo clippy --all-targets -- -D warnings` clean, `cargo test`:
  476 lib + stdio_smoke (1) + vis_smoke (1) passed, 0 failed.
