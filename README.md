# ltmrs — Long Term Memory RS

**ltmrs** (the "long timers" pun) is a local memory service for LLM coding agents.
It persists facts, lessons, guides, attempts and knowledge relations, and recalls
them by identifier or meaning — over MCP, from any number of clients, on one
machine, with no cloud calls.

The agent decides what to save and how to reason. ltmrs stores explicit findings
and concise attempt summaries; it does not capture hidden chain-of-thought and
never calls a remote model.

## Status

**v0.2-beta** (`ltmrs 0.2.0-beta`, tag `v0.2-beta`). Implementation covers
WP-00…WP-11 plus post-alpha enhancements (Burn tensor core, Hindsight
bridge, hooks, Magic Context import, Windows port, CUDA); WP-12
qualification is in progress — executed legs (BEIR SciFact full run, CUDA parity, Windows certification, Alpine power-rig trials, load/soak/contended records) and the open tickers are listed per-ticker in [`docs/RELEASE.md`](docs/RELEASE.md); history in
[`CHANGELOG.md`](CHANGELOG.md). This repository contains:

- the reviewed implementation specification and test plan in [`plans/`](plans/),
- the frozen Lemma 0.21.0 baseline in
  [`baseline/lemma-0.21.0/`](baseline/lemma-0.21.0/) — live wire capture,
  tool inventory, upstream lock, behavior inventory, dependency/native-code
  audit and the proposed deviation ledger,
- a resolved, compile-validated candidate dependency set (Cargo.lock),
- the repository governance files.

The wire capture and tool inventory are real captures with provenance, not
hand-written goldens. All BASELINE conformance cells start at `not_run` —
capturing the baseline is not passing conformance (ltmrs's own matrix in
`plans/conformance_matrix.json` tracks executed cells separately).

## Compatibility target

ltmrs targets the complete supported tool/workflow surface of
[Lemma](https://github.com/xenitV1/lemma) `0.21.0`
(commit `d30a816632d0bc5d92907cbc51c1dc1010111986`), including its data
interchange, CLI aliases and skill workflow.

Dense semantic search and graph-aware context are **intentional enhancements**.
The release claim under qualification is therefore: *complete supported Lemma
API/workflow surface with documented retrieval enhancements*, not byte-identical
behavioral equivalence — pending the WP-13 gates (approvals + checklist). Known deviations are tracked in an explicit ledger
(`baseline/lemma-0.21.0/deviations.json`; 17 entries — 14 owner-approved,
DEV-007 re-approval pending, DEV-016/017 proposed).

Beyond Lemma, ltmrs speaks the [oh-my-pi](https://github.com/can1357/oh-my-pi)
memory dialect: `memory_edit` tri-op, `learn` lesson capture, `reflect`
server-side synthesis, Hindsight bridge scoping, mental-model seeds,
`memory`/`mm` CLI groups, curated decision files and polyphonic recall
voices. The [gap table](#gap-table-multi-target) below maps every Lemma, omp and
bridge surface to its ltmrs counterpart; deliberate divergences are ledgered, not hidden.

## Quick Start

Point an MCP host at the built binary over stdio (config file and key
names differ per host — Claude Desktop, Claude Code and opencode each use
their own shape; the server identifies as `ltmrs`):

```json
{
  "mcpServers": {
    "ltmrs": {
      "command": "/path/to/ltmrs"
    }
  }
}
```

Then ask your assistant to remember something. It saves findings with
`memory_add`, recalls with `memory_read` / `semantic_search`, captures
lessons with `learn`, synthesizes with `reflect` (bridge-backed), and
follows the skill workflow (recall → act → persist) when the managed
skill is installed. No terminal command is needed for daily use; the
CLI surface below is for setup, snapshots and maintenance.

## Architecture

```text
MCP host A -> ltmrs stdio frontend --+
MCP host B -> ltmrs stdio frontend --+-> private local IPC -> ltmrs daemon
CLI / visualizer client -----------+                         |
                                               domain commands and snapshots
                                                             |
                                              selected canonical repository
                                                             |
                                          versioned lexical/vector projection
                                                             |
                                             LanceDB + Burn worker
```

- One binary: MCP stdio frontends, `ltmrs daemon`, CLI and optional visualizer.
- One daemon owns one physical store; multiple frontends share it.
- Canonical knowledge and derived search state are logically separated.
- Embeddings are computed locally with Burn (Candle→Burn 2026-11-12, tree Candle-free); retrieval with LanceDB.
- Canonical backend is **B** Fjall + LanceDB (decided, AD-01
  `plans/AD-01_canonical_backend.md`, gated on hard correctness/atomicity
  tests — not on feature lists or throughput scores).
- Optional Hindsight bridge: best-effort dual-write + scoped recall fusion
  (never blocks or fails tools; local-first when disabled or down).

## How It Works

The host starts every call with live context: the server prefetches
a memory snapshot into its instructions, and `session_start`
pre-loads task-relevant memories. Knowledge flows along an explicit
pipeline — finding (`memory_add`) → pattern (`type: "pattern"`) →
guide (`guide_distill` → `guide_practice`) — across five fragment
types: `fact`, `pattern`, `lesson`, `warning`, `context`.

Saving is active, not archival: `memory_add` auto-redacts detected
secrets (pass `confirm: true` to store verbatim), flags distill
candidates, and auto-links topic overlaps. Lessons land lesson-typed
(`learn` captures them durably; optional `guide` creates a managed
guide second — lesson-first, skill failure is partial, never a
rollback). Explanations (`explain: true` on `memory_read` /
`semantic_search`) report the effective mode, readiness and partial
flags behind each answer.

Recall blends lexical + dense + fact + temporal voices through
reciprocal-rank fusion (advisory fact/temporal legs at quarter weight each;
text-similarity legs stay primary), with a generation-keyed result
cache (5-minute TTL; writes bump the generation, so hits are never
stale). `reflect` synthesizes over the Hindsight bank when the bridge
is configured; without it, it degrades honestly instead of failing.

## Dense retrieval (E5)

Lexical recall works with no model. Dense hybrid retrieval needs the
pinned E5 artifacts, provisioned explicitly (the only step that uses
the network):

```bash
ltmrs --provision-models   # download + digest-verify 6 pinned files into $HOME/.ltmrs/models
```

- Provisioning is idempotent (existing files are re-verified, not
  re-downloaded) and fails closed: any digest mismatch aborts loudly
  and nothing half-verified is served. If local corruption is
  suspected, remove `$HOME/.ltmrs/models` and retry.
- On the next start the daemon auto-detects the verified set and
  serves hybrid; without it (absent, partial or corrupt cache) it
  serves lexical-only and says so on stderr. Either way every
  `semantic_search` answer carries its effective mode (`hybrid` vs
  `lexical-fallback`) with `dense_ready`, plus `partial` while
  projection work is still pending.
- Indexing is asynchronous: writes return immediately and a commit
  wake-up drives the background worker at once (up to 100 jobs per
  pass); the maintenance interval (default 300s) remains as the
  fallback for retries and repair. Until a write is projected,
  `semantic_search` answers carry `partial: true` with the pending
  count instead of silently presenting incomplete results. Serving
  itself makes no network calls (verified by strace over stdio runs).
- Operating costs when E5 is enabled: two resident model adapters
  (~1GB RAM for the 470MB artifact set) and digest verification plus
  weight loading at startup. Deterministic oversize inputs stay
  lexically indexed with vector retry pending; fallback relevance
  scores are display values, not probabilities.

## Operations

All state lives under the managed home `$HOME/.ltmrs`:

| Path | Contents |
|---|---|
| `store/` | Canonical Fjall store (source of truth) |
| `sessions.json` | Frontend session registry (persisted on shutdown) |
| `search/` | Lance projection (derived, rebuildable) |
| `models/` | Verified E5 artifacts (`--provision-models` target) |
| `hindsight-outbox/` | Durable bridge retain outbox (when `--hindsight-outbox` set) |
| `sharpshooter/<bank>/` | Curated decision files (`architecture/product/style.md` + pending queues) |
| `memories/<bank>/` | Curated lesson files (`learned.md` + pending queue) |
| `managed-skills/` | Host-managed skills (installer-guarded, 64k cap) — under the agent home |

There is no config file; behavior comes from flags plus `$HOME`.
`ltmrs --help` is authoritative for flags. Commands:

| Command | Purpose |
|---|---|
| (no args) | Serve MCP over stdio (default; spawns the daemon on demand) — accepts `--socket`, `--project-id`, `--daemon-idle-ms`, `--hindsight-*` for the spawn it performs |
| `daemon [--foreground] [--daemon-idle-ms MS] [--hindsight-*]` | Run the shared daemon (ensure-and-exit; foreground serves inline). Hindsight: `--hindsight-url URL --hindsight-bank BANK` enable the bridge; optional `--hindsight-tags CSV`, `--hindsight-timeout-ms MS`, `--hindsight-scoping MODE`, `--hindsight-global-scope`, `--hindsight-outbox DIR` |
| `--socket PATH` | Attach stdio to a running daemon instead of starting one |
| `--project-id ID` | Stable identity override (with `project status|adopt`, default stdio) |
| `-lib/--library --store PATH` | Print a knowledge-base snapshot (never creates stores) |
| `-vis/--visualize [--fg] [-p PORT] [--vis-token T]` | Run the library visualizer (default port 18721; fresh token unless given) |
| `--install-skill` | Install/update the managed agent skill |
| `--install-shim` | Install the opt-in legacy `lemma` executable shim |
| `--provision-models` | Fetch + verify E5 artifacts (only step using the network) |
| `memory view\|stats\|diagnose\|queue` | Render backend status surfaces (bridge-disabled labels when local-only) |
| `memory sync\|enqueue\|rebuild` | Consolidate pending curation queues into curated files (honest counts) |
| `memory clear\|reset` | Refuse loudly (canonical store is never wiped from a CLI verb) |
| `mm list\|show\|refresh\|seed\|delete\|reload [name]` | Hindsight mental-model maintenance (frozen 3-seed catalog) |
| `hook-inject` / `hook-extract` [`--no-hooks`] | Budget-capped recall / transcript finding extraction (host hooks, stdin JSON, fail-open; `--no-hooks` opts out per-invocation) |
| `install-hooks` | Install opt-in host hook snippets + enable hooks |
| `mc-import preview\|confirm <path> [--import-lib P] [--digest D]` | Preview/confirm a Magic Context import (additive fan-out; `--import-lib` overrides the cdylib path) |
| `-h/--help`, `-V/--version` | Help text (authoritative flag reference), version |

Install locations: skill → `~/.agents/skills/ltmrs/SKILL.md`;
managed skills → `~/.agents/managed-skills/<name>/SKILL.md`
(idempotent, versioned, refuse foreign or user-modified files
instead of overwriting); shim → `~/.local/bin/lemma` symlink
(refuses non-managed paths, warns on PATH collisions).

Exit codes: 0 success, 1 runtime failure, 2 usage error,
3 parsed-but-unimplemented slice. stdout carries protocol data;
diagnostics go to stderr.

The visualizer mints a per-boot access token and prints its URL in
the foreground (`http://127.0.0.1:PORT/?token=…`); `/` and
`/api/library` return 403 without it.

Backup and restore are native MCP tools (`backup_create` to a
destination directory, `backup_preview`, `backup_restore` with a
single-use confirmation token plus explicit confirm). Backups are
logical exports from a consistent canonical snapshot; model weights
and derived indexes are excluded by default. Restore replaces —
never merges — under a staged-generation switch.

### Hindsight bridge

Opt-in via `daemon` flags (CLI-first; no env switches):
`--hindsight-url URL --hindsight-bank BANK` enable the bridge;
optional `--hindsight-tags CSV`, `--hindsight-timeout-ms MS`,
`--hindsight-scoping global|per-project|per-project-tagged`,
`--hindsight-global-scope`, `--hindsight-outbox DIR`.
Disabled by default: tools behave byte-identically. Flags forward to
spawned daemons explicitly (never inherited env).

- Dual-write fan-out post-commit (never blocks/fails tools); scoped
  recall fusion appended after canonical results, never into ranking.
- Bank derivation per mode (shared base for global/tagged, base-label
  for per-project); recall tag filters for tagged mode; global
  (untagged) writes need `--hindsight-global-scope` + a mode other than per-project.
- `reflect` synthesizes over the bank (30s timeout, "" when down).
- Durable outbox (when `OUTBOX` set): each retain persists to disk
  before send; failures retry 5s→30s→5m then stay on disk; 7-day
  expiry → quarantine, 30-day quarantine delete.
- Mental-model seeds: `user-preferences` (untagged, all banks),
  `project-conventions`, `project-decisions` (tagged); `mm` CLI
  lists and resolves them; synthesis runs server-side.

### Curated files ( Sharpshooter + local pipeline)

The daemon never calls a model. Hosts (or `hook-extract`) enqueue
extracted deltas; `memory sync` (or the next scheduled pass)
redacts, dedups and consolidates them into curated files:

- `sharpshooter/<bank>/{architecture,product,style}.md` (+ `.pending.jsonl` queues).
- `memories/<bank>/learned.md` (newest-first, 100-cap, secret-redacted).

## Constraints

- Rust-native core. The production CPU profile must not require Node, Python,
  ONNX Runtime or a C++ database engine (feature-resolved audit, not a blanket
  "100% Rust" claim).
- Local operation only. No remote database, cloud sync or hidden model calls.
  The Hindsight bridge is the single explicit exception, opt-in and best-effort.
- CLI-first configuration: every operator switch is a CLI flag (`--hindsight-*`,
  `--daemon-idle-ms`, `--project-id`, `--vis-token`, `--no-hooks`, `--import-lib`,
  `max_bytes` tool arg). No `LTMRS_*` env switches (platform `$HOME`/`USERPROFILE`
  for home resolution and test-harness `LTMRS_BENCH_MACHINE`/`LTMRS_PROBE_MODELS`/
  `LTMRS_BEIR_DIR` for `#[ignore]`d runs excepted — see `benchmarks.toml`).
- OSI-approved dependencies; model weights and tokenizer licenses audited
  separately.
- First supported target: local Linux x86_64 CPU. CUDA is an optional,
  independently tested feature.

## Plans

The specification is the source of truth for implementation:

| Document | Purpose |
|---|---|
| [Part I — Design and concepts](plans/01_design_and_concepts.md) | Normative contracts: consistency, sessions, graph, projections, models, retrieval, compatibility, safety, requirements RQ-01…RQ-28 |
| [Part II — Implementation guide](plans/02_implementation_guide.md) | Ordered work packages WP-00…WP-13, delivery slices S0…S8, repository layout |
| [Part III — Quality, tests, benchmarks, conformance](plans/03_quality_tests_benchmarks_conformance.md) | Hard gates, test catalog, recovery/security suites, benchmark strategy, release checklist |
| [traceability.json](plans/traceability.json) | Machine-readable requirements → work packages → tests mapping |
| [conformance_matrix.json](plans/conformance_matrix.json) | Per-tool conformance inventory (all `not_run` until executed) |
| omp gap closure (local-only spec, not committed) | omp ↔ ltmrs gap analysis + closure plan — delivered per TODO (Slices 1–5, G1–G7) |

Implementation status (per TODO): WP-00…WP-11 delivered (slices S0–S7, 33 tools
served); open work is WP-12 qualification and WP-13 release closure. The
canonical backend gate stays on hard correctness/atomicity tests — not on
feature lists or throughput scores.

## Install

Prerequisites: a Rust stable toolchain. `rust-toolchain.toml` pins it
(currently 1.99.0 with rustfmt + clippy); rustup picks it up
automatically. No Node, Python, or system database engine is needed —
inference is CPU Burn, storage is Fjall + LanceDB.

```bash
cargo build                # debug binary at target/debug/ltmrs
cargo build --release      # release binary at target/release/ltmrs
```

Optional, for dense retrieval (the only step that uses the network):

```bash
ltmrs --provision-models   # verified E5 artifacts into $HOME/.ltmrs/models
```

Optional, for agent hosts: `ltmrs --install-skill` installs the
managed skill (`--install-shim` adds the opt-in legacy `lemma` shim).

## Usage

Run the server (it speaks MCP over stdio and identifies as `ltmrs`):

```bash
ltmrs                      # serve; store auto-creates under $HOME/.ltmrs
```

Point an MCP host at the binary over stdio (exact key names are
host-configured — Claude Code, OpenCode and Codex each use their own
config shape — with the command pointing at your built binary).
Then, from the host: save findings with `memory_add`, recall with
`memory_read` / `semantic_search`, capture lessons with `learn`,
synthesize with `reflect` (bridge-backed), and follow the skill workflow
(recall → act → persist) if installed.

Beyond the host loop:

```bash
ltmrs --help                                   # authoritative flag reference
ltmrs -lib --store $HOME/.ltmrs/store          # knowledge-base snapshot
ltmrs -vis --fg                                # library visualizer (prints its token URL)
ltmrs --socket PATH                            # attach stdio to a running daemon
ltmrs memory view                              # backend status surfaces
ltmrs memory sync                              # consolidate curation queues
ltmrs mm list                                  # mental-model seed catalog
```

Backups run through the MCP tools (`backup_create` to a
directory, `backup_preview`, `backup_restore` with token + confirm);
see Operations above. Verification gates: `cargo test`, `cargo fmt -- --check`, `cargo clippy
--all-targets -- -D warnings`.

## Tools (33)

Short MCP names (`memory_read`, `memory_add`, `learn`, `reflect`,
`session_start`); hosts may display them namespaced (e.g.
`mcp_ltmrs_memory_add`). Tool schemas are frozen from the Lemma
0.21.0 capture; behavior deviations are ledgered (see Compatibility
target).

| Family | Tools |
|---|---|
| Memory (10) | `memory_read`, `memory_add`, `memory_update`, `memory_feedback`, `memory_forget`, `memory_merge`, `memory_relate`, `memory_stats`, `memory_audit`, `memory_library` |
| Guides (7) | `guide_get`, `guide_practice`, `guide_create`, `guide_distill`, `guide_update`, `guide_forget`, `guide_merge` |
| Sessions (5) | `session_start`, `session_attempt`, `session_end`, `session_stats`, `suggestion_respond` |
| Intelligence (5) | `conflict_scan`, `proactive_analysis`, `project_analytics`, `semantic_search`, `reflect` |
| omp-compat, native (2) | `memory_edit` (tri-op alias), `learn` (lesson + optional guide) |
| Backup, native (4) | `backup_create`, `backup_preview`, `backup_restore`, `backup_import_lemma` |

Back up by asking: *"Back up my memory to this folder."* The
assistant creates a verified `.ltmrs-backup`, you move it where it
must survive, then `backup_preview` checks it: `ready` issues a
single-use confirmation token, `blocked` names the live connections
to close first (close them through their app, keep this connection
open, preview again). `backup_restore` with token + `confirm: true`
replaces the store — never merges — after writing a verified safety
backup first; restoring the safety file undoes a restore.

Capture a lesson by asking: *"Learn this: always quote SQL identifiers."*
The assistant stores a lesson-typed memory first, then (if asked with
a guide name) creates the managed guide second — skill failure is
partial, never a rollback. Empty lessons reject loudly.

## Gap table (multi-target)

Every compatibility surface ltmrs claims, by target: Lemma 0.21.0 (the pinned
legacy contract, AD-03), the omp `memory` dialect and its backends, Magic
Context, and the legacy `lemma` CLI. Deviation ids point into
`baseline/lemma-0.21.0/deviations.json`.

| Target | Surface | ltmrs counterpart | Status / notes |
|---|---|---|---|
| Lemma 0.21.0 | 33 MCP tools + frozen schemas | served verbatim from the wire capture | matrix: 33 cells executed_passed |
| Lemma 0.21.0 | `semantic_search` ranking | hybrid RRF + native scorer | `hybrid:false` serves the upstream TF-IDF reference profile (DEV-011); `explain:true` adds the upstream-shaped recall explanation (DEV-012); DEV-001/005 enhancements |
| Lemma 0.21.0 | secret redaction on `memory_add` | `confirm:true` overrides non-critical secrets only | critical credentials (PEM, GitHub PAT, AWS AKIA) always redacted (DEV-002) |
| Lemma 0.21.0 | session model | sessions bound to authenticated frontend channels | no daemon-global current session (DEV-003) |
| Lemma 0.21.0 | `source` provenance | strict enum user\|ai\|web\|paper\|book\|code | truly-unknown values coerce to ai (DEV-004) |
| Lemma 0.21.0 | project attribution | stable `prj_<name>_<ulid>` + `project bind\|status\|adopt` | cwd is a fallback clue, never an identity (DEV-006) |
| Lemma 0.21.0 | add dedup + conflict handling | Jaccard 0.80 similarity contract; conflicting adds refused pre-commit with advice | DEV-007 re-approval pending; DEV-015 has no override flag |
| Lemma 0.21.0 | pagination | typed validation errors for out-of-range limit/top_k/offset | no silent clamps (DEV-013) |
| Lemma 0.21.0 | tools/list | static frozen schemas, verbatim | memory context in responses, never injected descriptions (DEV-014) |
| Lemma 0.21.0 | consolidate weights | MIN(confidence, 0.1) on all write paths | parity restored 2026-11-13; flat-0.05 bug fixed (DEV-017) |
| Lemma 0.21.0 | instructional UX | no seed fragments, no coaching blocks, no auto-detect | host concern (DEV-008); skill-side guidance landed in SKILL.md 0.2.0 |
| Lemma 0.21.0 | host skill install | recipes at `~/.agents/skills/ltmrs/SKILL.md`, installer-guarded | per-host verification pending (DEV-009) |
| Lemma 0.21.0 | `.lemma-backup` archives | `backup_import_lemma` (preview + confirm, loss-accounted) | isolated runtime cdylib; SQLite never in the build graph (DEV-010) |
| Lemma 0.21.0 | CLI aliases | `-lib`, `-vis`, `--install-skill`, `-h`, `-V` | verbatim (WP-10) |
| Lemma 0.21.0 | deviation ledger | `baseline/lemma-0.21.0/deviations.json` | 17 entries: 14 approved, DEV-007 re-approval pending, DEV-016/017 proposed |
| omp `memory` | `recall` / `retain` / `reflect` tools | `memory_read`+`semantic_search` / `memory_add` / `reflect` | `reflect` needs the bridge; otherwise honest empty synthesis |
| omp `memory` | `memory_edit` tri-op + importance clamp | `memory_edit` native tool | importance-only rejects (confidence is earned); fact rows editable by design |
| omp `memory` | `learn` + optional skill | `learn` native tool (lesson + `guide_create` second) | 100-cap learned queue + dedup + redaction live in the add path |
| omp `memory` | `manage_skill` CRUD | `learn --guide` + installer guards | No MCP tool: the daemon never writes host files; host uses CLI/lib |
| omp `memory` | `/memory view\|stats\|diagnose\|queue` | P3 renderers + `memory` CLI | Bridge-disabled labels, never fabricated data |
| omp `memory` | `/memory sync\|enqueue\|rebuild` | `memory sync` (real consolidation) | Drains queues + consolidates; bridge flush runs in the daemon |
| omp `memory` | `/memory clear\|reset` | Refuses loudly | Canonical store is never wiped from a CLI verb |
| omp `memory` | `/memory mm …` | `mm` CLI (frozen 3-seed catalog) | Synthesis runs server-side via the bridge |
| omp `memory` | `memory://` URLs | `memory_library` + `-lib` snapshot | Host-side scheme; server exposes the snapshot, not the protocol |
| omp `memory` | `local` backend pipeline | Curated files (`memories/`, `sharpshooter/`) | Daemon never calls a model; hosts enqueue deltas, daemon consolidates |
| omp `memory` (sharpshooter) | decision files | `sharpshooter/<bank>/*.md` + pending queues | Same mechanism; extraction stays host-side |
| omp `memory` (hindsight) | `hindsight` backend | Opt-in bridge (dual-write + scoped recall + reflect) | 7 CLI flags; scoping/globalScope/outbox/mental models landed |
| omp `memory` (mnemopi) | `mnemopi` backend | Canonical Fjall store (no separate SQLite mode) | One store, not two; no `noEmbeddings` toggle needed (lexical fallback is automatic) |
| omp `memory` | auto-recall / auto-retain | `session_start` preload + `hook-extract` | No silent background writes from the server; host opts in |
| omp `memory` | polyphonic / enhanced recall / proactive linking | RRF fact+temporal voices (quarter weight) + generation-keyed cache | Graph voice landed: bounded relation-graph expansion (supersession/contradiction/relatedness, engine stage 8); cache drops on generation bump |
| omp `memory` | hook event bus | Slice B CLI hooks (`hook-inject`/`hook-extract`) | Different layer (CLI entry points, not in-process events) |
| omp `memory` | bank scoping global/per-project/tagged | G1a `Scoping` + bank derivation + tag filters | Project label = lowercase basename (omp parity) |
| Magic Context | `.mc` bundle import | `mc-import preview\|confirm <path> [--digest D]` | read-only preview, version fence, loss accounting; isolated cdylib, CLI-first (Slice C) |
| legacy lemma CLI | `lemma` executable | `--install-shim` → `~/.local/bin/lemma` (Windows `lemma.cmd`) | refuses non-managed paths, warns on PATH collisions |

## Known limits

Status per ticker lives in `docs/RELEASE.md`; the honest open list:

- Long-run stability: 4-segment soak executed (2000 ops / 0 failures, RSS stable,
  `reports/wp12-soak-01/`); the 24h clock soak is open.
- Power-loss durability: the libvirt rig **models** power loss (guest dirty pages
  discarded on `virsh destroy`; host caches intact — plans/03 §4.1 doctrine).
  Built and proven 2026-11-13: canary written+synced survived destroy/reboot;
  50MB mid-write load rebooted into journal-orphan recovery with the canary
  intact (rigtest1); trials `smoke10`, `smoke11` (12 steps green) and `stale1`
  pass (`reports/power-wp12-*/verdict.json`); `power clean --stale` sweep
  (`reports/power-clean-stale/`). Hardware qualification on a dedicated machine
  remains open.
- Multi-host matrices: Linux R0 primary, Windows certified on-host (daemon +
  T-SEC-01 security suite + full gate, 2026-10-08/2026-11-13), Alpine rig guest
  for power legs; the broader host matrix is not_run, and Windows per-host skill
  activation is not covered.
- Quality waves: upstream-manual full-corpus waves open; BEIR beyond SciFact
  open (SciFact full run closed 2026-11-13: 5183/5183, CPU 1246s + CUDA 123s).
- Deviation review: 17-entry ledger — 14 owner-approved; DEV-007 re-approval
  pending, DEV-016/017 proposed (`baseline/lemma-0.21.0/deviations.json`).

## Security

Local-first: everything stays under `$HOME/.ltmrs`; serving makes
no network calls (only `--provision-models` downloads, digest
verified). Fragments are scanned for secrets at add time and
redacted unless `confirm: true` stores them verbatim. The visualizer
binds `127.0.0.1` only and gates `/` and `/api/library` behind its
per-boot token (403 without it). Backups are unencrypted — keep
them in trusted storage, never on the disk being formatted.
Curated files redact the same secret patterns before write; refused
bridge payloads never touch the canonical store.

## License

MIT OR Apache-2.0 — see [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
Upstream notices for translated MIT Lemma code are retained per the plan.
