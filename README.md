# ltmrs — Long Term Memory RS

**ltmrs** (the "long timers" pun) is a local memory service for LLM coding agents.
It persists facts, lessons, guides, attempts and knowledge relations, and recalls
them by identifier or meaning — over MCP, from any number of clients, on one
machine, with no cloud calls.

The agent decides what to save and how to reason. ltmrs stores explicit findings
and concise attempt summaries; it does not capture hidden chain-of-thought and
never calls a remote model.

## Status

**Release candidate v0.1-alpha** (`ltmrs 0.1.0-alpha`). Implementation
covers WP-12 plus post-WP-12 enhancements, with WP-13 evidence largely
executed. Release tracking (per-ticker status, evidence pointers,
open items) lives in [`docs/RELEASE.md`](docs/RELEASE.md); history in
[`CHANGELOG.md`](CHANGELOG.md). This repository contains:

- the reviewed implementation specification and test plan in [`plans/`](plans/),
- the frozen Lemma 0.21.0 baseline in
  [`baseline/lemma-0.21.0/`](baseline/lemma-0.21.0/) — live wire capture,
  tool inventory, upstream lock, behavior inventory, dependency/native-code
  audit and the proposed deviation ledger,
- a resolved, compile-validated candidate dependency set (Cargo.lock),
- the repository governance files.

The wire capture and tool inventory are real captures with provenance, not
hand-written goldens. All conformance matrix cells still start at `not_run` —
capturing the baseline is not passing conformance.

## Compatibility target

ltmrs targets the complete supported tool/workflow surface of
[Lemma](https://github.com/xenitV1/lemma) `0.21.0`
(commit `d30a816632d0bc5d92907cbc51c1dc1010111986`), including its data
interchange, CLI aliases and skill workflow.

Dense semantic search and graph-aware context are **intentional enhancements**.
The release claim under qualification is therefore: *complete supported Lemma
API/workflow surface with documented retrieval enhancements*, not byte-identical
behavioral equivalence — pending the WP-13 gates (approvals + checklist). Known deviations are tracked in an explicit ledger
(`baseline/lemma-0.21.0/deviations.json`; all 12 owner-approved).

## Quick Start

Point an MCP host at the built binary over stdio (config file and key
names differ per host — Claude Desktop, Claude Code and opencode each
use their own shape; the server identifies as `ltmrs`):

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
`memory_add`, recalls with `memory_read` / `semantic_search`, and
follows the skill workflow (recall → act → persist) when the managed
skill is installed. No terminal command is needed for daily use; the
CLI surface below is for setup, snapshots and maintenance.

## Architecture (planned)

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
                                             LanceDB + Candle worker
```

- One binary: MCP stdio frontends, `ltmrs daemon`, CLI and optional visualizer.
- One daemon owns one physical store; multiple frontends share it.
- Canonical knowledge and derived search state are logically separated.
- Embeddings are computed locally with Candle; retrieval with LanceDB.
- Canonical backend is **B** Fjall + LanceDB (decided, AD-01
  `plans/AD-01_canonical_backend.md`, gated on hard correctness/atomicity
  tests — not on feature lists or throughput scores).

## How It Works

The host starts every call with live context: the server prefetches
a memory snapshot into its instructions, and `session_start`
pre-loads task-relevant memories. Knowledge flows along an explicit
pipeline — finding (`memory_add`) → pattern (`type: "pattern"`) →
guide (`guide_distill` → `guide_practice`) — across five fragment
types: `fact`, `pattern`, `lesson`, `warning`, `context`.

Saving is active, not archival: `memory_add` auto-redacts detected
secrets (pass `confirm: true` to store verbatim), flags distill
candidates, and auto-links topic overlaps. Explanations (`explain:
true` on `memory_read` / `semantic_search`) report the effective
mode, readiness and partial flags behind each answer.

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

There is no config file; behavior comes from flags plus `$HOME`.
`ltmrs --help` is authoritative for flags. Commands:

| Command | Purpose |
|---|---|
| (no args) | Serve MCP over stdio (default; spawns the daemon on demand) |
| `daemon [--foreground] [--daemon-idle-ms MS]` | Run the shared daemon (ensure-and-exit; foreground serves until idle/SIGTERM) |
| `--socket PATH` | Attach stdio to a running daemon instead of starting one |
| `-lib/--library --store PATH` | Print a knowledge-base snapshot (never creates stores) |
| `-vis/--visualize [--fg] [-p PORT]` | Run the library visualizer (default port 18721) |
| `--install-skill` | Install/update the managed agent skill |
| `--install-shim` | Install the opt-in legacy `lemma` executable shim |
| `--provision-models` | Fetch + verify E5 artifacts (only step using the network) |
| `-h/--help`, `-V/--version` | Help text (authoritative flag reference), version |

Install locations: skill → `~/.agents/skills/ltmrs/SKILL.md`
(idempotent, versioned, refuses foreign or user-modified files
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

## Constraints

- Rust-native core. The production CPU profile must not require Node, Python,
  ONNX Runtime or a C++ database engine (feature-resolved audit, not a blanket
  "100% Rust" claim).
- Local operation only. No remote database, cloud sync or hidden model calls.
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

Implementation starts with **WP-00** (freeze upstream baseline, resolve the CPU
dependency lock) and **WP-01** (domain model + reference interpreter). Do not
implement the full tool surface against an unproven storage transaction
assumption.

## Install

Prerequisites: a Rust stable toolchain. `rust-toolchain.toml` pins it
(currently 1.96.0 with rustfmt + clippy); rustup picks it up
automatically. No Node, Python, or system database engine is needed —
inference is CPU Candle, storage is Fjall + LanceDB.

```bash
cargo build                # debug binary at target/debug/ltmrs
cargo build --release      # release binary at target/release/ltmrs (~286MB)
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
`memory_read` / `semantic_search`, and follow the skill workflow
(recall → act → persist) if installed.

Beyond the host loop:

```bash
ltmrs --help                                   # authoritative flag reference
ltmrs -lib --store $HOME/.ltmrs/store          # knowledge-base snapshot
ltmrs -vis --fg                                # library visualizer (prints its token URL)
ltmrs --socket PATH                            # attach stdio to a running daemon
```

Backups run through the MCP tools (`backup_create` to a
directory, `backup_preview`, `backup_restore` with token + confirm);
see Operations above. Verification gates: `cargo test` (664 lib +
2 smoke suites), `cargo fmt -- --check`, `cargo clippy
--all-targets -- -D warnings`.

## Tools (29)

Short MCP names (`memory_read`, `memory_add`, `session_start`);
hosts may display them namespaced (e.g. `mcp_ltmrs_memory_add`).
Tool schemas are frozen from the Lemma 0.21.0 capture; behavior
deviations are ledgered (see Compatibility target).

| Family | Tools |
|---|---|
| Memory (10) | `memory_read`, `memory_add`, `memory_update`, `memory_feedback`, `memory_forget`, `memory_merge`, `memory_relate`, `memory_stats`, `memory_audit`, `memory_library` |
| Guides (7) | `guide_get`, `guide_practice`, `guide_create`, `guide_distill`, `guide_update`, `guide_forget`, `guide_merge` |
| Sessions (5) | `session_start`, `session_attempt`, `session_end`, `session_stats`, `suggestion_respond` |
| Intelligence (4) | `conflict_scan`, `proactive_analysis`, `project_analytics`, `semantic_search` |
| Backup, native (3) | `backup_create`, `backup_preview`, `backup_restore` |

Back up by asking: *"Back up my memory to this folder."* The
assistant creates a verified `.ltmrs-backup`, you move it where it
must survive, then `backup_preview` checks it: `ready` issues a
single-use confirmation token, `blocked` names the live connections
to close first (close them through their app, keep this connection
open, preview again). `backup_restore` with token + `confirm: true`
replaces the store — never merges — after writing a verified safety
backup first; restoring the safety file undoes a restore.

## Security

Local-first: everything stays under `$HOME/.ltmrs`; serving makes
no network calls (only `--provision-models` downloads, digest
verified). Fragments are scanned for secrets at add time and
redacted unless `confirm: true` stores them verbatim. The visualizer
binds `127.0.0.1` only and gates `/` and `/api/library` behind its
per-boot token (403 without it). Backups are unencrypted — keep
them in trusted storage, never on the disk being formatted.

## License

MIT OR Apache-2.0 — see [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE).
Upstream notices for translated MIT Lemma code are retained per the plan.
